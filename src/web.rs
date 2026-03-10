use embassy_net::Stack;
use embassy_sync::blocking_mutex::raw::CriticalSectionRawMutex;
use embassy_sync::mutex::Mutex;
use embassy_sync::signal::Signal;
use embassy_time::Duration;
use esp_alloc as _;
use picoserve::{AppBuilder, AppRouter, Router, response::File, routing};
use picoserve::request::Request;
use picoserve::response::{IntoResponse, Json, Response, StatusCode};
use static_cell::StaticCell;
use heapless::{Vec, String, format};
use rand_core::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use crate::signals::Command;

pub static APP_STATE: StaticCell<AppState> = StaticCell::new();

pub struct Application {
    state: &'static AppState,
}

impl AppBuilder for Application {
    type PathRouter = impl routing::PathRouter;

    fn build_app(self) -> Router<Self::PathRouter> {
        Router::new().route(
            "/",
            routing::get_service(File::html(include_str!("index.html"))),
        )
            .route("/login", routing::post(login))
            .route("/logout", routing::post(logout))
            .route("/logs", routing::get(get_logs))
            .route("/clear_logs", routing::post(clear_logs))
            .route("/users", routing::get(get_users))
            .route("/add_user", routing::post(add_user))
            .route("/remove_user", routing::post(remove_user))
            .route("/update_user", routing::post(update_user))
            .route("/clear_users", routing::post(clear_users))
            .route("/change_password", routing::post(reset_password))
    }
}

/// The port that the webserver will be opened on; Typically this is port 80 for http and 443 for https
const WEB_PORT: i32 = 80;

pub async fn web_task(
    stack: Stack<'static>,
    router: &'static AppRouter<Application>,
    config: &'static picoserve::Config<Duration>,
) -> ! {
    let mut tcp_rx_buffer = [0; 1024];
    let mut tcp_tx_buffer = [0; 1024];
    let mut http_buffer = [0; 2048];

    picoserve::Server::new(router, config, &mut http_buffer)
        .listen_and_serve(0, stack, WEB_PORT, &mut tcp_rx_buffer, &mut tcp_tx_buffer)
        .await
        .into_never()
}

pub struct WebApp {
    pub router: &'static Router<<Application as AppBuilder>::PathRouter>,
    pub config: &'static picoserve::Config<Duration>,
}

impl WebApp {
    pub fn new(state: &'static AppState) -> Self {
        let app = Application { state };

        let router = picoserve::make_static!(
            AppRouter<Application>,
            app.build_app()
        );

        let config = picoserve::make_static!(
            picoserve::Config<Duration>,
            picoserve::Config::new(picoserve::Timeouts {
                start_read_request: Some(Duration::from_secs(5)),
                read_request: Some(Duration::from_secs(1)),
                write: Some(Duration::from_secs(1)),
                persistent_start_read_request: Some(Duration::from_secs(1)),
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

// TODO A possible improvement to efficiency could be to use hardware SHA256
/// Hashes the password using the software SHA256 algorithm
pub fn hash_password(password: &str, salt: &[u8; 16]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(salt);
    hasher.update(password.as_bytes());
    let result = hasher.finalize();

    let mut out = [0u8; 32];
    out.copy_from_slice(&result);
    out
}

/// Generates a session token for the user viewing the website.
/// This allows the user to stay logged in when sending REST requests
fn generate_session_token() -> String<64> {
    let mut bytes = [0u8; 32];
    let mut rng = rand::rng();
    rng.fill_bytes(&mut bytes);

    let mut token: String<64> = String::new();

    for b in &bytes {
        use core::fmt::Write;
        write!(token, "{:02x}", b).ok();
    }

    token
}

/// Checks if the user's session token is valid
async fn is_authenticated<R: picoserve::io::Read>(
    app: &Application,
    req: &Request<'_, R>,
) -> bool {
    let state = app.state;

    let Some(cookie_header) = req.headers().get("Cookie") else {
        return false;
    };

    let cookie_str = match core::str::from_utf8(cookie_header) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // Look for "session="
    if let Some(token) = cookie_str.strip_prefix("session=") {
        let stored = state.session_token.lock().await;
        if let Some(stored_token) = &*stored {
            return stored_token.as_str() == token;
        }
    }

    false
}

/// Check's if the user's password is valid.
/// If it is, a session token is created
async fn login<R: picoserve::io::Read>(
    app: &Application,
    mut req: Request<'_, R>,
) -> impl IntoResponse {

    // Parse JSON body
    let body: LoginRequest = match req.json().await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    let salt = app.state.password_salt.lock().await;
    let computed = hash_password(body.password, &*salt);
    drop(salt);

    let stored = app.state.password_hash.lock().await;
    let valid = *stored == computed;
    drop(stored);

    if !valid {
        return StatusCode::UNAUTHORIZED;
    }

    // Generate session token
    let token = generate_session_token();

    let mut session = app.state.session_token.lock().await;
    *session = Some(token.clone());
    drop(session);

    // Proper cookie response
    Response::ok(())
        .with_header(
            "Set-Cookie",
            &format!("session={}; Path=/; HttpOnly; SameSite=Strict", token),
        )
}

/// Invalidates the session token
async fn logout(app: &Application) -> impl IntoResponse {
    // Clear session token on server
    let mut session = app.state.session_token.lock().await;
    *session = None;
    drop(session);

    // Expire cookie
    Response::ok(())
        .with_header(
            "Set-Cookie",
            "session=deleted; Max-Age=0; Path=/; HttpOnly; SameSite=Strict",
        )
}

/// Resets the user password with a new one
async fn reset_password<R: picoserve::io::Read>(
    app: &Application,
    mut req: Request<'_, R>,
) -> impl IntoResponse {

    let state = app.state;

    let body: ResetPasswordRequest = match req.json().await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    if body.new_password.len() < 8 {
        return StatusCode::BAD_REQUEST;
    }

    let mut new_salt = [0u8; 16];

    {
        rand::rng().fill_bytes(&mut new_salt);
    }

    let new_hash = hash_password(body.new_password, &new_salt);

    {
        let mut salt_lock = state.password_salt.lock().await;
        *salt_lock = new_salt;
    }

    {
        let mut hash_lock = state.password_hash.lock().await;
        *hash_lock = new_hash;
    }

    state.commands.signal(Command::SetPassword {
        hash: new_hash,
        salt: new_hash,
    });

    StatusCode::OK
}

/// Gets a list of all allowed users in the database
async fn get_users<R: picoserve::io::Read>(
    app: &Application,
    req: Request<'_, R>,
) -> impl IntoResponse {

    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    let state = app.state;

    // Lock users
    let users_lock = state.users.lock().await;

    // Convert to response format
    let mut response_vec: Vec<UserResponse, 64> = Vec::new();

    for user in users_lock.iter() {
        let _ = response_vec.push(UserResponse {
            id: user.id,
            name: user.name.as_str(),
        });
    }

    Json(response_vec)
}

/// Puts the device in "add user" mode
async fn add_user<R: picoserve::io::Read>(
    app: &Application,
    req: Request<'_, R>,
) -> impl IntoResponse {

    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    app.state.commands.signal(Command::AddUserMode);

    StatusCode::OK
}

async fn update_user<R: picoserve::io::Read>(
    app: &Application,
    mut req: Request<'_, R>,
) -> impl IntoResponse {
    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    let body: UserResponse = match req.json().await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    let mut users = app.state.users.lock().await;
    for user in users.iter_mut() {
        if user.id == body.id {
            if user.name == body.name {
                // Don't cause unnecessary SD writing
                break
            }
            user.name.clear();
            user.name.push_str(&body.name).unwrap();
            app.state.commands.signal(Command::UpdateUser { id: body.id, name: user.name.clone() });
            break;
        }
    }
}

/// Removes the specified user from the database
async fn remove_user<R: picoserve::io::Read>(
    app: &Application,
    mut req: Request<'_, R>,
) -> impl IntoResponse {

    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    let body: RemoveUserRequest = match req.json().await {
        Ok(b) => b,
        Err(_) => return StatusCode::BAD_REQUEST,
    };

    let state = app.state;
    let mut users = state.users.lock().await;

    if let Some(pos) = users.iter().position(|u| u.id == body.id) {
        users.swap_remove(pos);

        app.state.commands.signal(Command::RemoveUser { id: body.id });

        StatusCode::OK
    } else {
        StatusCode::NOT_FOUND
    }
}

/// Removes all users from the database
async fn clear_users<R: picoserve::io::Read>(
    app: &Application,
    req: Request<'_, R>,
) -> impl IntoResponse {

    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    let state = app.state;
    let mut users = state.users.lock().await;

    users.clear();

    app.state.commands.signal(Command::RemoveAllUsers);

    StatusCode::OK
}

/// Gets the last 4096 characters of the logs
async fn get_logs<R: picoserve::io::Read>(
    app: &Application,
    req: Request<'_, R>,
) -> impl IntoResponse {

    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    let state = app.state;
    let logs_lock = state.logs.lock().await;

    let response = LogsResponse {
        logs: logs_lock.as_str(),
    };

    Json(response)
}

/// Deletes all logs
async fn clear_logs<R: picoserve::io::Read>(
    app: &Application,
    req: Request<'_, R>,
) -> impl IntoResponse {

    if !is_authenticated(app, &req).await {
        return StatusCode::UNAUTHORIZED;
    }

    let state = app.state;
    let mut logs = state.logs.lock().await;

    logs.clear();

    app.state.commands.signal(Command::ClearLog);

    StatusCode::OK
}

// Request Structures
#[derive(Deserialize)]
struct LoginRequest<'a> {
    password: &'a str,
}

#[derive(Deserialize)]
struct ResetPasswordRequest<'a> {
    current: &'a str,
    new_password: &'a str,
}

#[derive(Serialize)]
pub struct UserResponse<'a> {
    pub id: u32,
    pub name: &'a str,
}

#[derive(Deserialize)]
pub struct RemoveUserRequest {
    pub id: u32,
}

#[derive(Serialize)]
pub struct LogsResponse<'a> {
    pub logs: &'a str,
}