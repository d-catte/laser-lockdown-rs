#![allow(static_mut_refs)]
use crate::sd_utils;
use crate::signals::Command;
use core::fmt::{Display, Formatter, Write};
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::once_lock::OnceLock;
use embassy_sync::signal::Signal;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::sha::Sha256;
use heapless::{String, Vec};
use nb::block;
use picoserve::extract::{FailedToExtractEntireBodyAsStringError, Form, FromRequest};
use picoserve::io::Read;
use picoserve::request::{ReadAllBodyError, RequestBody, RequestParts};
use picoserve::response::{
    Connection, ErrorWithStatusCode, IntoResponse, Response, ResponseWriter, StatusCode,
};
use picoserve::routing::{get_service, PathRouter};
use picoserve::{Config, ResponseSent, Router};
use serde::{Deserialize, Serialize};
use static_cell::StaticCell;

pub static USERS: OnceLock<Mutex<CriticalSectionRawMutex, Vec<UserInfo, 32>>> = OnceLock::new();
pub static LOGS: OnceLock<Mutex<CriticalSectionRawMutex, String<4096>>> = OnceLock::new();
pub static PSWD: OnceLock<Mutex<CriticalSectionRawMutex, [u8; 32]>> = OnceLock::new();
const SALT_STR: &str = env!("SALT");
pub const SALT: [u8; 16] = {
    let bytes = SALT_STR.as_bytes();
    let mut array = [0u8; 16];
    let mut i = 0;
    while i < 16 {
        array[i] = bytes[i];
        i += 1;
    }
    array
};
pub static TOKEN: Mutex<CriticalSectionRawMutex, Option<String<64>>> = Mutex::new(None);
pub static CMD: OnceLock<&'static Signal<CriticalSectionRawMutex, Command>> = OnceLock::new();
pub static RAND: OnceLock<Mutex<CriticalSectionRawMutex, Trng>> = OnceLock::new();
pub static _RNG_SOURCE: OnceLock<TrngSource<'static>> = OnceLock::new();
static CONFIG: Config = Config::const_default().keep_connection_alive();
static mut COOKIE_BUF: String<128> = String::new();
pub static HTML_DATA: StaticCell<&'static str> = StaticCell::new();

/// The port that the webserver will be opened on; This is port 80 for http
const WEB_PORT: u16 = 80;

pub async fn web_task<R: PathRouter>(stack: Stack<'static>, router: &Router<R>) -> ! {
    let mut tcp_rx_buffer = [0; 4096];
    let mut tcp_tx_buffer = [0; 4096];
    let mut http_buffer = [0; 4096];

    picoserve::Server::new(&router.shared(), &CONFIG, &mut http_buffer)
        .listen_and_serve(0, stack, WEB_PORT, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}

/// Starts the integrated admin panel
pub async fn start_web_server(stack: Stack<'static>, html: &'static str) {
    let router = Router::new()
        .nest("/logs", logs_router())
        .nest("/users", users_router())
        .nest("/auth", auth_router())
        .route(
            "/",
            get_service(picoserve::response::File::html(html)),
        );

    web_task(stack, &router).await
}

/// All log endpoints
fn logs_router<S>() -> Router<impl PathRouter<S>, S> {
    Router::new()
        .route("/", picoserve::routing::get(get_logs))
        .route("/clear", picoserve::routing::post(clear_logs))
}

/// All user endpoints
fn users_router<S>() -> Router<impl PathRouter<S>, S> {
    Router::new()
        .route("/", picoserve::routing::get(get_users))
        .route("/add", picoserve::routing::post(add_user))
        .route("/remove", picoserve::routing::post(remove_user))
        .route("/update", picoserve::routing::post(update_user))
}

/// All authentication endpoints
fn auth_router<S>() -> Router<impl PathRouter<S>, S> {
    Router::new()
        .route("/login", picoserve::routing::post(login))
        .route("/logout", picoserve::routing::post(logout))
        .route("/change_password", picoserve::routing::post(change_password))
}

/// Gets the logs
async fn get_logs(_auth: AuthenticatedUser) -> impl IntoResponse {
    picoserve::response::Json(LOGS.get().await.lock().await.clone())
}

/// Clears the log file
async fn clear_logs(_auth: AuthenticatedUser) -> impl IntoResponse {
    let mut logs = LOGS.get().await.lock().await;
    logs.clear();
    CMD.get().await.signal(Command::ClearLog);
    picoserve::response::Redirect::to("/")
}

/// Sets the mode to add_user mode
async fn add_user(_auth: AuthenticatedUser) -> impl IntoResponse {
    CMD.get().await.signal(Command::AddUserMode);
    picoserve::response::Redirect::to("/")
}

/// Updates the specified user's name
async fn update_user(AuthenticatedUserInfo { body }: AuthenticatedUserInfo) -> impl IntoResponse {
    let mut users = USERS.get().await.lock().await;

    if let Some(user) = users.iter_mut().find(|u| u.id == body.id) {
        // Don't cause unnecessary SD writing
        if user.name != body.name {
            user.name = body.name;
            CMD.get().await.signal(Command::UpdateUser {
                id: body.id,
                name: user.name.clone(),
            });
        }
    }

    picoserve::response::Redirect::to("/")
}

/// Removes a user in the database
async fn remove_user(AuthenticatedUserID { body }: AuthenticatedUserID) -> impl IntoResponse {
    let mut users = USERS.get().await.lock().await;

    if let Some(pos) = users.iter().position(|u| u.id == body.id) {
        users.swap_remove(pos);
        CMD.get().await.signal(Command::RemoveUser { id: body.id });
    }

    picoserve::response::Redirect::to("/")
}

/// Gets all users in the database
async fn get_users(_auth: AuthenticatedUser) -> impl IntoResponse {
    let users = USERS.get().await.lock().await.clone();
    picoserve::response::Json(users)
}

async fn login(Form(body): Form<LoginRequest>) -> impl IntoResponse {
    // Compute hash
    let computed = {
        hash_password(&body.password).await
    };

    // Compare with stored hash
    let valid = {
        let stored = PSWD.get().await.lock().await;
        *stored == computed
    };

    if !valid {
        return Response::empty(StatusCode::UNAUTHORIZED).with_header("Content-Type", "text/plain");
    }

    // Generate session token
    let token = generate_session_token().await;

    // Store session token
    let mut session = TOKEN.lock().await;
    *session = Some(token.clone());
    drop(session);

    // Write cookie into static buffer
    let cookie_str = unsafe {
        COOKIE_BUF.clear();
        write!(
            COOKIE_BUF,
            "session={}; Path=/; HttpOnly; SameSite=Strict",
            token.as_str()
        )
        .unwrap();
        COOKIE_BUF.as_str()
    };

    // Return response
    Response::empty(StatusCode::OK).with_header("Set-Cookie", cookie_str)
}

async fn logout(_auth: AuthenticatedUser) -> impl IntoResponse {
    // Remove token
    let mut token = TOKEN.lock().await;
    *token = None;
    drop(token);

    // Expire cookie
    Response::empty(StatusCode::OK).with_header(
        "Set-Cookie",
        "session=deleted; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path=/; HttpOnly; SameSite=Strict",
    )
}

/// Hashes the password using the hardware SHA256 algorithm
pub async fn hash_password(password: &str) -> [u8; 32] {
    let mut sha = sd_utils::SHA_INSTANCE.get().await.lock().await;
    let mut hasher = sha.start::<Sha256>();
    let mut data = SALT.as_slice();
    while !data.is_empty() {
        data = block!(hasher.update(data)).unwrap();
    }

    let mut data = password.as_bytes();
    while !data.is_empty() {
        data = block!(hasher.update(data)).unwrap();
    }

    let mut output = [0u8; 32];
    block!(hasher.finish(&mut output)).unwrap();

    output
}

/// Hashes the user's id so that it can be stored securely on the SD card
pub async fn hash_id(value: u32) -> [u8; 32] {
    let mut sha = sd_utils::SHA_INSTANCE.get().await.lock().await;
    let mut hasher = sha.start::<Sha256>();

    let mut salt_data = SALT.as_slice();
    while !salt_data.is_empty() {
        salt_data = block!(hasher.update(salt_data)).unwrap();
    }

    let val_bytes = value.to_be_bytes();
    let mut data = &val_bytes[..];

    while !data.is_empty() {
        data = block!(hasher.update(data)).unwrap();
    }

    let mut output = [0u8; 32];
    block!(hasher.finish(&mut output)).unwrap();

    output
}

/// Generates a session token for the user viewing the website.
/// This allows the user to stay logged in when sending REST requests
async fn generate_session_token() -> String<64> {
    let mut bytes = [0u8; 32];
    RAND.get().await.lock().await.read(&mut bytes);

    let mut token: String<64> = String::new();

    for b in &bytes {
        use core::fmt::Write;
        write!(token, "{:02x}", b).ok();
    }

    token
}

/// Changes the password to a new password
async fn change_password(
    AuthenticatedLoginRequest { body }: AuthenticatedLoginRequest,
) -> impl IntoResponse {
    let new_hash = hash_password(body.password.as_str()).await;

    {
        let mut hash_lock = PSWD.get().await.lock().await;
        *hash_lock = new_hash;
    }

    CMD.get().await.signal(Command::SetPassword {
        hash: new_hash,
    });

    picoserve::response::Redirect::to("/")
}

// Extractors

pub struct AuthenticatedUser;

#[derive(Debug)]
pub enum AuthError {
    MissingCookie,
    InvalidUtf8,
    NoSession,
    TokenMismatch,
    FormError,
}

impl IntoResponse for AuthError {
    async fn write_to<R, W>(
        self,
        connection: Connection<'_, R>,
        response_writer: W,
    ) -> Result<ResponseSent, W::Error>
    where
        R: Read,
        W: ResponseWriter<Error = R::Error>,
    {
        // Minimal 401 response
        let res =
            Response::empty(StatusCode::UNAUTHORIZED).with_header("Content-Type", "text/plain");

        res.write_to(connection, response_writer).await
    }
}

impl Display for AuthError {
    fn fmt(&self, f: &mut Formatter<'_>) -> core::fmt::Result {
        match self {
            AuthError::MissingCookie => write!(f, "Missing cookie"),
            AuthError::InvalidUtf8 => write!(f, "Invalid UTF-8 in cookie"),
            AuthError::NoSession => write!(f, "No session token found in cookie"),
            AuthError::TokenMismatch => write!(f, "Session token does not match"),
            AuthError::FormError => write!(f, "Form Error"),
        }
    }
}

impl ErrorWithStatusCode for AuthError {
    fn status_code(&self) -> StatusCode {
        StatusCode::UNAUTHORIZED
    }
}

impl<'r, State> FromRequest<'r, State> for AuthenticatedUser {
    type Rejection = AuthError;

    async fn from_request<R: Read>(
        _state: &'r State,
        request_parts: RequestParts<'r>,
        _request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Get the Cookie header
        let cookie_header = request_parts
            .headers()
            .get("Cookie")
            .ok_or(AuthError::MissingCookie)?;

        let cookie_str =
            str::from_utf8(cookie_header.as_raw()).map_err(|_| AuthError::InvalidUtf8)?;

        // Find session= token
        let token = cookie_str
            .split(';')
            .map(str::trim)
            .find_map(|c: &str| c.strip_prefix("session="))
            .ok_or(AuthError::NoSession)?;

        // Compare with stored token
        let stored = TOKEN.lock().await;
        if Some(token) != stored.as_deref() {
            return Err(AuthError::TokenMismatch);
        }

        Ok(AuthenticatedUser)
    }
}

pub struct AuthenticatedUserInfo {
    pub body: UserInfo,
}

impl<'r, State> FromRequest<'r, State> for AuthenticatedUserInfo {
    type Rejection = AuthError;

    async fn from_request<R: Read>(
        _state: &'r State,
        request_parts: RequestParts<'r>,
        request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Check cookie header for authentication
        let cookie_header = request_parts
            .headers()
            .get("Cookie")
            .ok_or(AuthError::MissingCookie)?;

        let cookie_str =
            core::str::from_utf8(cookie_header.as_raw()).map_err(|_| AuthError::InvalidUtf8)?;

        let token = cookie_str
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix("session="))
            .ok_or(AuthError::NoSession)?;

        let stored = TOKEN.lock().await;
        if Some(token) != stored.as_deref() {
            return Err(AuthError::TokenMismatch);
        }
        drop(stored);

        // Create Form
        let user_info = <UserInfo>::from_request(_state, request_parts, request_body)
            .await
            .map_err(|_| AuthError::FormError)?;

        Ok(Self { body: user_info })
    }
}

pub struct AuthenticatedUserID {
    pub body: UserID,
}

impl<'r, State> FromRequest<'r, State> for AuthenticatedUserID {
    type Rejection = AuthError;

    async fn from_request<R: Read>(
        _state: &'r State,
        request_parts: RequestParts<'r>,
        request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Check cookie header for authentication
        let cookie_header = request_parts
            .headers()
            .get("Cookie")
            .ok_or(AuthError::MissingCookie)?;

        let cookie_str =
            core::str::from_utf8(cookie_header.as_raw()).map_err(|_| AuthError::InvalidUtf8)?;

        let token = cookie_str
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix("session="))
            .ok_or(AuthError::NoSession)?;

        let stored = TOKEN.lock().await;
        if Some(token) != stored.as_deref() {
            return Err(AuthError::TokenMismatch);
        }
        drop(stored);

        // Create Form
        let user_id = <UserID>::from_request(_state, request_parts, request_body)
            .await
            .map_err(|_| AuthError::FormError)?;

        Ok(Self { body: user_id })
    }
}

pub struct AuthenticatedLoginRequest {
    pub body: LoginRequest,
}

impl<'r, State> FromRequest<'r, State> for AuthenticatedLoginRequest {
    type Rejection = AuthError;

    async fn from_request<R: Read>(
        _state: &'r State,
        request_parts: RequestParts<'r>,
        request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Check cookie header for authentication
        let cookie_header = request_parts
            .headers()
            .get("Cookie")
            .ok_or(AuthError::MissingCookie)?;

        let cookie_str =
            core::str::from_utf8(cookie_header.as_raw()).map_err(|_| AuthError::InvalidUtf8)?;

        let token = cookie_str
            .split(';')
            .map(str::trim)
            .find_map(|c| c.strip_prefix("session="))
            .ok_or(AuthError::NoSession)?;

        let stored = TOKEN.lock().await;
        if Some(token) != stored.as_deref() {
            return Err(AuthError::TokenMismatch);
        }
        drop(stored);

        // Create Form
        let login_request = <LoginRequest>::from_request(_state, request_parts, request_body)
            .await
            .map_err(|_| AuthError::FormError)?;

        Ok(Self {
            body: login_request,
        })
    }
}

// Forms
#[derive(Deserialize, Serialize, Clone)]
pub struct UserInfo {
    pub(crate) id: [u8; 32],
    pub(crate) name: String<32>,
}

impl<'r, State> FromRequest<'r, State> for UserInfo {
    type Rejection = FailedToExtractEntireBodyAsStringError;

    async fn from_request<R: Read>(
        _state: &'r State,
        _parts: RequestParts<'r>,
        request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Read the body as &str
        let s = <&str>::from_request(_state, _parts, request_body).await?;

        // Parse JSON using serde_json_core
        let (user_info, _) = serde_json_core::from_str::<UserInfo>(s).map_err(|_| {
            FailedToExtractEntireBodyAsStringError::FailedToExtractEntireBody(
                ReadAllBodyError::UnexpectedEof,
            )
        })?; // I'm just using this as a placeholder

        Ok(user_info)
    }
}

#[derive(Deserialize, Serialize)]
pub struct UserID {
    id: [u8; 32],
}

impl<'r, State> FromRequest<'r, State> for UserID {
    type Rejection = FailedToExtractEntireBodyAsStringError;

    async fn from_request<R: Read>(
        _state: &'r State,
        _parts: RequestParts<'r>,
        request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Read the body as &str
        let s = <&str>::from_request(_state, _parts, request_body).await?;

        // Parse JSON using serde_json_core
        let (user_info, _) = serde_json_core::from_str::<UserID>(s).map_err(|_| {
            FailedToExtractEntireBodyAsStringError::FailedToExtractEntireBody(
                ReadAllBodyError::UnexpectedEof,
            )
        })?; // I'm just using this as a placeholder

        Ok(user_info)
    }
}

#[derive(Deserialize, Serialize)]
pub struct LoginRequest {
    password: String<64>,
}

impl<'r, State> FromRequest<'r, State> for LoginRequest {
    type Rejection = FailedToExtractEntireBodyAsStringError;

    async fn from_request<R: Read>(
        _state: &'r State,
        _parts: RequestParts<'r>,
        request_body: RequestBody<'r, R>,
    ) -> Result<Self, Self::Rejection> {
        // Read the body as &str
        let s = <&str>::from_request(_state, _parts, request_body).await?;

        // Parse JSON using serde_json_core
        let (user_info, _) = serde_json_core::from_str::<LoginRequest>(s).map_err(|_| {
            FailedToExtractEntireBodyAsStringError::FailedToExtractEntireBody(
                ReadAllBodyError::UnexpectedEof,
            )
        })?; // I'm just using this as a placeholder

        Ok(user_info)
    }
}

// API
/// If the user is contained in the user cache
pub async fn valid_user(id: [u8; 32]) -> bool {
    let users = USERS.get().await.lock().await;
    users.iter().any(|u| u.id == id)
}
