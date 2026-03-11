use crate::web::routing::get_service;
use core::fmt::Write;
use crate::signals::Command;
use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::Duration;
use esp_alloc as _;
use esp_hal::rng::{Trng, TrngSource};
use esp_hal::sha::Sha256;
use heapless::{String, Vec};
use nb::block;
use picoserve::request::RequestParts;
use picoserve::response::{IntoResponse, IntoResponseWithState, Json, Redirect, Response, StatusCode};
use picoserve::{AppRouter, Router, response::File, routing, AppWithStateBuilder};
use picoserve::extract::FromRequestParts;
use picoserve::response::with_state::WithStateUpdate;
use serde::{Deserialize, Serialize};
use crate::extractors::LoginExtract;
use crate::sd_utils;

pub struct Application;

impl AppWithStateBuilder for Application {
    type State = AppState;
    type PathRouter = impl routing::PathRouter<AppState>;

    fn build_app(self) -> Router<Self::PathRouter, AppState> {
        /*
        Router::new()
            .route(
                "/",
                get_service(File::html(include_str!("index.html"))),
            )
            .route("/login", routing::post(login))
            .route("/logout", routing::post(|Authenticated(state)| async move {
                logout(state).await
            }))
            .route("/logs", routing::post(|Authenticated(state)| async move {
                get_logs(state).await
            }))
            .route("/clear_logs", routing::post(|Authenticated(state)| async move {
                clear_logs(state).await
            }))
            .route("/users", routing::get(|Authenticated(state)| async move {
                get_users(state).await
            }))
            .route("/add_user", routing::post(|Authenticated(state)| async move {
                add_user(state).await
            }))
            .route("/remove_user", routing::post(|Authenticated(state), Json(body)| async move {
                remove_user(state, body).await
            }))
            .route("/update_user", routing::post(|Authenticated(state), Json(body)| async move {
                update_user(state, body).await
            }))
            .route("/clear_users", routing::get(|Authenticated(state)| async move {
                clear_users(state).await
            }))
            .route("/change_password", routing::post(|Authenticated(state), Json(body)| async move {
                reset_password(state, body).await
            }))

         */

        Router::new()
    }
}

/// The port that the webserver will be opened on; Typically this is port 80 for http and 443 for https
const WEB_PORT: u16 = 80;

pub async fn web_task(
    stack: Stack<'static>,
    router: &'static AppRouter<Application>,
    config: &'static picoserve::Config,
    state: &'static AppState,
) -> ! {
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(
        &router.shared().with_state(state),
        &config,
        &mut http_buffer
    )
        .listen_and_serve(0, stack, WEB_PORT, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}

struct StateExtractor<'a>(&'a AppState);

impl<'r> FromRequestParts<'r, AppState> for StateExtractor<'r> {
    type Rejection = core::convert::Infallible;

    async fn from_request_parts(
        state: &'r AppState,
        _parts: &RequestParts<'r>,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(state))
    }
}

struct Authenticated<'a>(&'a AppState);

impl<'r> FromRequestParts<'r, AppState> for Authenticated<'r> {
    type Rejection = StatusCode;

    async fn from_request_parts(
        state: &'r AppState,
        parts: &RequestParts<'r>,
    ) -> Result<Self, Self::Rejection> {

        if is_authenticated(state, parts).await {
            Ok(Self(state))
        } else {
            Err(StatusCode::UNAUTHORIZED)
        }
    }
}

pub struct WebApp {
    pub router: &'static AppRouter<Application>,
    pub config: &'static picoserve::Config,
}

impl WebApp {
    pub fn new() -> Self {
        let app = Application {};

        let router = picoserve::make_static!(AppRouter<Application>, app.build_app());

        let config = picoserve::make_static!(
            picoserve::Config,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Duration::from_secs(5),
                read_request: Duration::from_secs(1),
                write: Duration::from_secs(1),
                persistent_start_read_request: Duration::from_secs(1),
            })
            .keep_connection_alive()
        );

        Self { router, config }
    }
}

pub struct AppState {
    pub users: Mutex<CriticalSectionRawMutex, Vec<User, 32>>,
    pub logs: Mutex<CriticalSectionRawMutex, String<4096>>,
    pub password_hash: Mutex<CriticalSectionRawMutex, [u8; 32]>,
    pub password_salt: Mutex<CriticalSectionRawMutex, [u8; 16]>,
    pub session_token: Mutex<CriticalSectionRawMutex, Option<String<64>>>,
    pub commands: Signal<CriticalSectionRawMutex, Command>,
    pub rand: Mutex<CriticalSectionRawMutex, Trng>,
    pub _rng_source: TrngSource<'static>,
}

pub struct User {
    pub id: u32,
    pub name: String<32>,
}

/// If the user is contained in the user cache
pub async fn valid_user(app: &AppState, id: u32) -> bool {
    let users = app.users.lock().await;
    for user in users.iter() {
        if user.id == id {
            return true;
        }
    }
    false
}

/// Hashes the password using the hardware SHA256 algorithm
pub async fn hash_password(password: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut sha = sd_utils::SHA_INSTANCE.get().await.lock().await;
    let mut hasher = sha.start::<Sha256>();
    let mut data = salt.as_slice();
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

/// Generates a session token for the user viewing the website.
/// This allows the user to stay logged in when sending REST requests
async fn generate_session_token(app: &AppState) -> String<64> {
    let mut bytes = [0u8; 32];
    app.rand.lock().await.read(&mut bytes);

    let mut token: String<64> = String::new();

    for b in &bytes {
        use core::fmt::Write;
        write!(token, "{:02x}", b).ok();
    }

    token
}

/// Check's if the user's password is valid.
/// If it is, a session token is created
async fn login_old(
    extract: LoginExtract<'_>
) -> impl IntoResponse {
    let state = extract.state;
    let body = extract.body;
    let salt = state.password_salt.lock().await;
    let computed = hash_password(body.password, &*salt).await;
    drop(salt);

    let stored = state.password_hash.lock().await;
    let valid = *stored == computed;
    drop(stored);

    if !valid {
        return Response::empty(StatusCode::UNAUTHORIZED)
            .with_header("Content-Type", "text/plain");
    }

    let token = generate_session_token(&state).await;

    let mut session = state.session_token.lock().await;
    *session = Some(token.clone());
    drop(session);

    let mut cookie: String<128> = String::new();
    write!(cookie, "session={}; Path=/; HttpOnly; SameSite=Strict", token.as_str()).unwrap();

    Response::empty(StatusCode::OK)
        .with_header("Set-Cookie", cookie.as_str())
}

/// Invalidates the session token
async fn logout(state: &AppState) -> impl IntoResponse {
    // Clear session token on server
    let mut session = state.session_token.lock().await;
    *session = None;
    drop(session);

    // Expire cookie
    Response::empty(StatusCode::OK).with_header(
        "Set-Cookie",
        "session=deleted; Max-Age=0; Expires=Thu, 01 Jan 1970 00:00:00 GMT; Path=/; HttpOnly; SameSite=Strict",
    )
}

/// Resets the user password with a new one
async fn reset_password(
    state: &AppState,
    body: ResetPasswordRequest<'_>,
) -> impl IntoResponse {

    if body.new_password.len() < 8 {
        return StatusCode::BAD_REQUEST;
    }

    let mut new_salt = [0u8; 16];
    state.rand.lock().await.read(&mut new_salt);

    let new_hash = hash_password(body.new_password, &new_salt).await;

    let mut salt_lock = state.password_salt.lock().await;
    *salt_lock = new_salt;

    let mut hash_lock = state.password_hash.lock().await;
    *hash_lock = new_hash;

    state.commands.signal(Command::SetPassword {
        hash: new_hash,
        salt: new_salt,
    });

    StatusCode::OK
}

/// Gets a list of all allowed users in the database
async fn get_users(
    state: &AppState,
) -> impl IntoResponse {
    // Convert to response format
    let mut response_vec: Vec<UserResponse, 64> = Vec::new();

    // Lock users
    let users_lock = state.users.lock().await;

    for user in users_lock.iter() {
        let _ = response_vec.push(UserResponse {
            id: user.id,
            name: user.name.clone(),
        });
    }

    Json(response_vec)
}

/// Puts the device in "add user" mode
async fn add_user(
    state: &AppState,
) -> impl IntoResponse {
    state.commands.signal(Command::AddUserMode);

    StatusCode::OK
}

async fn update_user(
    state: &AppState,
    body: UserResponse,
) -> impl IntoResponse {
    let mut users = state.users.lock().await;
    for user in users.iter_mut() {
        if user.id == body.id {
            if user.name == body.name {
                // Don't cause unnecessary SD writing
                break;
            }
            user.name.clear();
            user.name.push_str(&body.name).unwrap();
            state.commands.signal(Command::UpdateUser {
                id: body.id,
                name: user.name.clone(),
            });
            break;
        }
    }

    StatusCode::OK
}

/// Removes the specified user from the database
async fn remove_user(
    state: &AppState,
    body: RemoveUserRequest,
) -> impl IntoResponse {
    let mut users = state.users.lock().await;

    if let Some(pos) = users.iter().position(|u| u.id == body.id) {
        users.swap_remove(pos);

        state
            .commands
            .signal(Command::RemoveUser { id: body.id });

        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Removes all users from the database
async fn clear_users(
    state: &AppState,
) -> impl IntoResponse {
    let mut users = state.users.lock().await;

    users.clear();

    state.commands.signal(Command::RemoveAllUsers);

    StatusCode::OK
}

/// Gets the last 4096 characters of the logs
async fn get_logs(
    state: &AppState,
) -> impl IntoResponse {
    let logs_copy = {
        let logs_lock = state.logs.lock().await;
        logs_lock.clone()
    };

    Json(LogsResponse {
        logs: logs_copy,
    })
}

/// Deletes all logs
async fn clear_logs(
    state: &AppState,
) -> impl IntoResponse {
    let mut logs = state.logs.lock().await;

    logs.clear();

    state.commands.signal(Command::ClearLog);

    StatusCode::OK
}

// Request Structures
#[derive(Deserialize)]
pub struct LoginRequest<'a> {
    password: &'a str,
}

#[derive(Deserialize)]
struct ResetPasswordRequest<'a> {
    current: &'a str,
    new_password: &'a str,
}

#[derive(Serialize)]
pub struct UserResponse {
    id: u32,
    name: String<32>,
}

#[derive(Deserialize)]
pub struct RemoveUserRequest {
    pub id: u32,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub logs: String<4096>,
}
