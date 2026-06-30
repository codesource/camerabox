//! Web UI (the landing page) and the JSON / control API.
//!
//! The page is a small static shell; a vanilla-JS script polls `/api/status`
//! and updates the DOM in place (no full reload, so it never disturbs an open
//! dropdown or an in-progress toggle). Optional HTTP Basic Auth (from config)
//! protects every route. The firmware-update endpoints come from
//! [`crate::update`].

use std::sync::Arc;

use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::camera::{self, AppState, CameraMode, ControlError};
use crate::net;
use crate::update;

/// Build the complete application router, with Basic Auth if configured.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/login", get(login_page))
        .route("/favicon.svg", get(favicon))
        .route("/favicon.ico", get(favicon))
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/account", post(set_account))
        .route("/api/status", get(status))
        .route("/api/cameras/:id/enable", post(enable_camera))
        .route("/api/cameras/:id/disable", post(disable_camera))
        .route("/api/cameras/:id/mode", post(set_camera_mode))
        .route("/api/cameras/:id/auth", post(set_camera_auth))
        .route("/api/network", get(network_status))
        .route("/api/network/scan", post(network_scan))
        .route("/api/network/hotspot", post(network_hotspot))
        .route("/api/network/connect", post(network_connect))
        .route("/api/network/profile/add", post(profile_add))
        .route("/api/network/profile/remove", post(profile_remove))
        .route("/api/network/profile/connect", post(profile_connect))
        .route("/api/system", get(system_info))
        .route("/api/logs", get(logs))
        .route("/api/hostname", post(set_hostname))
        .merge(update::router())
        .layer(middleware::from_fn_with_state(state.clone(), require_session))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Authentication (form login + session cookie)
// ---------------------------------------------------------------------------

/// Gate every route on a valid session, except the login page + login API.
/// Unauthenticated API calls get 401; pages redirect to `/login`.
async fn require_session(State(state): State<Arc<AppState>>, req: Request, next: Next) -> Response {
    let path = req.uri().path();
    if path == "/login" || path == "/api/login" || path.starts_with("/favicon") {
        return next.run(req).await;
    }
    let authed = cookie_value(req.headers(), "session")
        .map(|t| state.auth.validate(&t))
        .unwrap_or(false);
    if authed {
        next.run(req).await
    } else if path.starts_with("/api") {
        (StatusCode::UNAUTHORIZED, Json(json!({ "error": "unauthorized" }))).into_response()
    } else {
        Redirect::to("/login").into_response()
    }
}

#[derive(Deserialize)]
struct LoginReq {
    username: String,
    password: String,
}

async fn login(State(state): State<Arc<AppState>>, Json(r): Json<LoginReq>) -> Response {
    match state.auth.login(&r.username, &r.password) {
        Some(token) => (
            StatusCode::OK,
            [(
                header::SET_COOKIE,
                format!("session={token}; HttpOnly; Path=/; Max-Age=43200; SameSite=Lax"),
            )],
            Json(json!({ "status": "ok" })),
        )
            .into_response(),
        None => (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid username or password" })),
        )
            .into_response(),
    }
}

async fn logout(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers, "session") {
        state.auth.logout(&token);
    }
    (
        StatusCode::OK,
        [(
            header::SET_COOKIE,
            "session=; HttpOnly; Path=/; Max-Age=0".to_string(),
        )],
        Json(json!({ "status": "ok" })),
    )
        .into_response()
}

#[derive(Deserialize)]
struct AccountReq {
    #[serde(default)]
    username: Option<String>,
    password: String,
}

async fn set_account(State(state): State<Arc<AppState>>, Json(r): Json<AccountReq>) -> Response {
    let user = r.username.unwrap_or_else(|| state.auth.username());
    match state.auth.set_credentials(&user, &r.password) {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn login_page() -> Html<&'static str> {
    Html(LOGIN_HTML)
}

async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON)
}

const FAVICON: &str = r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 32 32"><rect width="32" height="32" rx="8" fill="#2563eb"/><g fill="none" stroke="#fff" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><rect x="6" y="11" width="13" height="11" rx="2.5"/><path d="M22 14l5-3v11l-5-3z"/></g></svg>"##;

/// Extract a named cookie value from the request headers.
fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(rest) = part.strip_prefix(name) {
            if let Some(val) = rest.strip_prefix('=') {
                return Some(val.to_string());
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// API model
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct StatusResponse {
    pub hostname: String,
    pub ip_address: String,
    pub uptime: u64,
    pub cameras: Vec<CameraStatus>,
}

#[derive(Serialize)]
pub struct CameraStatus {
    pub id: String,
    pub device_path: String,
    pub name: Option<String>,
    /// Desired state (user toggle).
    pub enabled: bool,
    /// Actual state (ustreamer process alive).
    pub running: bool,
    pub port: Option<u16>,
    pub stream_url: Option<String>,
    pub pid: Option<u32>,
    pub mjpeg: bool,
    pub resolution: String,
    pub fps: u32,
    pub modes: Vec<CameraMode>,
    /// Stream protected by HTTP Basic Auth?
    pub protected: bool,
    /// Stream auth credentials (shown to the logged-in admin who set them).
    pub stream_user: Option<String>,
    pub stream_password: Option<String>,
}

async fn build_status(state: &AppState, host: &str) -> StatusResponse {
    let cams = state.cameras.read().await;
    let mut cameras = Vec::with_capacity(cams.len());
    for c in cams.iter() {
        let rt = c.runtime.lock().await;
        let settings = c.settings.lock().await;
        let stream_url = c.port.map(|p| format!("http://{}:{}/stream", host, p));
        cameras.push(CameraStatus {
            id: c.device.id.clone(),
            device_path: c.device.path.display().to_string(),
            name: c.device.name.clone(),
            enabled: c.enabled,
            running: rt.running,
            port: c.port,
            stream_url,
            pid: rt.pid,
            mjpeg: c.device.mjpeg,
            resolution: settings.resolution.clone(),
            fps: settings.fps,
            modes: c.device.modes.clone(),
            protected: settings.user.is_some(),
            stream_user: settings.user.clone(),
            stream_password: settings.password.clone(),
        });
    }

    StatusResponse {
        hostname: net::current_hostname(),
        ip_address: host.to_string(),
        uptime: state.started.elapsed().as_secs(),
        cameras,
    }
}

/// Host the client connected to (from the `Host` header), or the config fallback.
fn request_host(headers: &HeaderMap, fallback: &str) -> String {
    headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(host_without_port)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| fallback.to_string())
}

fn host_without_port(host: &str) -> String {
    if let Some(rest) = host.strip_prefix('[') {
        if let Some(end) = rest.find(']') {
            return format!("[{}]", &rest[..end]);
        }
    }
    match host.rsplit_once(':') {
        Some((h, _port)) if !h.is_empty() => h.to_string(),
        _ => host.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn status(State(state): State<Arc<AppState>>, headers: HeaderMap) -> Json<StatusResponse> {
    let host = request_host(&headers, &state.config.device_ip);
    Json(build_status(&state, &host).await)
}

#[derive(Deserialize)]
struct ModeRequest {
    resolution: String,
    fps: u32,
}

async fn enable_camera(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    map_result(camera::set_enabled(&state, &id, true).await)
}

async fn disable_camera(State(state): State<Arc<AppState>>, Path(id): Path<String>) -> Response {
    map_result(camera::set_enabled(&state, &id, false).await)
}

async fn set_camera_mode(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<ModeRequest>,
) -> Response {
    map_result(camera::set_mode(&state, &id, &req.resolution, req.fps).await)
}

#[derive(Deserialize)]
struct StreamAuthRequest {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    password: Option<String>,
}

async fn set_camera_auth(
    State(state): State<Arc<AppState>>,
    Path(id): Path<String>,
    Json(req): Json<StreamAuthRequest>,
) -> Response {
    map_result(camera::set_stream_auth(&state, &id, req.user, req.password).await)
}

fn map_result(result: Result<(), ControlError>) -> Response {
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(ControlError::NotFound) => (
            StatusCode::NOT_FOUND,
            Json(json!({ "error": "camera not found" })),
        )
            .into_response(),
        Err(ControlError::Unsupported) => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "unsupported resolution/fps" })),
        )
            .into_response(),
    }
}

// ---------------------------------------------------------------------------
// Network handlers
// ---------------------------------------------------------------------------

fn net_result(result: net::NetResult<()>) -> Response {
    match result {
        Ok(()) => (StatusCode::OK, Json(json!({ "status": "ok" }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

#[derive(Deserialize)]
struct IfaceReq {
    iface: String,
}

async fn network_status() -> Json<net::NetworkStatus> {
    Json(net::status().await)
}

async fn network_scan(Json(r): Json<IfaceReq>) -> Response {
    match net::scan(&r.iface).await {
        Ok(networks) => (StatusCode::OK, Json(json!({ "networks": networks }))).into_response(),
        Err(e) => (StatusCode::BAD_REQUEST, Json(json!({ "error": e }))).into_response(),
    }
}

async fn network_hotspot(Json(r): Json<IfaceReq>) -> Response {
    net_result(net::start_hotspot(&r.iface).await)
}

async fn network_connect(Json(p): Json<net::ConnectParams>) -> Response {
    net_result(net::connect(&p).await)
}

#[derive(Deserialize)]
struct ProfileAddReq {
    name: String,
    ssid: String,
    password: String,
}

async fn profile_add(Json(r): Json<ProfileAddReq>) -> Response {
    net_result(net::add_profile(&r.name, &r.ssid, &r.password).await)
}

#[derive(Deserialize)]
struct ProfileNameReq {
    name: String,
}

async fn profile_remove(Json(r): Json<ProfileNameReq>) -> Response {
    net_result(net::remove_profile(&r.name))
}

#[derive(Deserialize)]
struct ProfileConnectReq {
    iface: String,
    name: String,
    #[serde(default = "default_true")]
    dhcp: bool,
}

fn default_true() -> bool {
    true
}

async fn profile_connect(Json(r): Json<ProfileConnectReq>) -> Response {
    net_result(net::connect_profile(&r.iface, &r.name, r.dhcp).await)
}

// ---------------------------------------------------------------------------
// System overview + logs
// ---------------------------------------------------------------------------

async fn system_info() -> Json<crate::sys::SystemInfo> {
    Json(crate::sys::info().await)
}

async fn logs() -> Json<serde_json::Value> {
    Json(json!({ "lines": crate::logs::snapshot() }))
}

#[derive(Deserialize)]
struct HostnameReq {
    name: String,
}

async fn set_hostname(Json(r): Json<HostnameReq>) -> Response {
    net_result(net::set_hostname(&r.name).await)
}

// ---------------------------------------------------------------------------
// Static page: shell + CSS + the JS that renders/updates from /api/status.
// ---------------------------------------------------------------------------

const LOGIN_HTML: &str = r#"<!DOCTYPE html>
<html lang="en"><head>
<meta charset="utf-8"><meta name="viewport" content="width=device-width, initial-scale=1">
<title>camera-box — Sign in</title>
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="alternate icon" href="/favicon.ico">
<style>
:root{--primary:#2563eb;--txt:#111827;--muted:#6b7280;--border:#e5e7eb}
*{box-sizing:border-box}
body{margin:0;min-height:100vh;display:grid;place-items:center;background:#f5f7fb;color:var(--txt);
 font-family:'Inter',system-ui,-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif;-webkit-font-smoothing:antialiased}
.card{background:#fff;border:1px solid var(--border);border-radius:22px;padding:36px 32px;width:350px;
 box-shadow:0 12px 44px rgba(16,24,40,.10);animation:in .35s ease}
@keyframes in{from{opacity:0;transform:translateY(10px)}to{opacity:1;transform:none}}
.logo{width:56px;height:56px;border-radius:17px;background:var(--primary);color:#fff;display:grid;place-items:center;margin:0 auto 18px;
 box-shadow:0 8px 20px rgba(37,99,235,.35)}
h1{font-size:22px;margin:0;text-align:center;letter-spacing:-.02em}
.sub{color:var(--muted);font-size:14px;text-align:center;margin:6px 0 24px}
label{display:block;font-size:13px;font-weight:600;color:var(--muted);margin:14px 0 6px}
input{width:100%;height:48px;border:1px solid var(--border);border-radius:12px;padding:0 14px;font-size:15px;background:#fff;color:var(--txt)}
input:focus{outline:none;border-color:var(--primary);box-shadow:0 0 0 4px rgba(37,99,235,.14)}
button{width:100%;height:50px;margin-top:22px;background:var(--primary);color:#fff;border:0;border-radius:14px;
 font-size:15px;font-weight:600;cursor:pointer;transition:.15s}
button:hover{background:#1d57d6}button:active{transform:translateY(1px)}
.err{color:#dc2626;font-size:13px;text-align:center;margin-top:14px;min-height:16px}
</style></head>
<body>
<form class="card" onsubmit="return signin(event)">
<div style="display:flex;justify-content:flex-end;margin:-8px -4px 0 0"><select id="lang" onchange="setL(this.value)" style="height:34px;border:1px solid var(--border);border-radius:10px;background:#fff;color:var(--muted);font:inherit;font-size:13px;font-weight:600;padding:0 8px;cursor:pointer"><option value="en">EN</option><option value="fr">FR</option><option value="de">DE</option><option value="it">IT</option></select></div>
<div class="logo"><svg width="28" height="28" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M23 7l-7 5 7 5V7z"/><rect x="1" y="5" width="15" height="14" rx="3"/></svg></div>
<h1 id="h">Welcome back</h1>
<div class="sub" id="sub">Sign in to manage your camera-box</div>
<label id="lu">Username</label><input id="u" autocomplete="username" autofocus>
<label id="lp">Password</label><input id="p" type="password" autocomplete="current-password">
<button type="submit" id="btn">Sign in</button>
<div class="err" id="err"></div>
</form>
<script>
var T={en:{wb:"Welcome back",sub:"Sign in to manage your camera-box",u:"Username",p:"Password",s:"Sign in",inv:"Invalid username or password",ne:"Network error"},
 fr:{wb:"Bon retour",sub:"Connectez-vous pour gérer votre camera-box",u:"Nom d'utilisateur",p:"Mot de passe",s:"Se connecter",inv:"Nom d'utilisateur ou mot de passe invalide",ne:"Erreur réseau"},
 de:{wb:"Willkommen zurück",sub:"Melden Sie sich an, um Ihre camera-box zu verwalten",u:"Benutzername",p:"Passwort",s:"Anmelden",inv:"Ungültiger Benutzername oder Passwort",ne:"Netzwerkfehler"},
 it:{wb:"Bentornato",sub:"Accedi per gestire la tua camera-box",u:"Nome utente",p:"Password",s:"Accedi",inv:"Nome utente o password non validi",ne:"Errore di rete"}};
var lg; try{lg=localStorage.getItem('cb_lang');}catch(e){} if(!lg||!T[lg]){var nv=(navigator.language||'en').slice(0,2).toLowerCase();lg=T[nv]?nv:'en';}
function g(id){return document.getElementById(id);}
function L(k){return (T[lg]||T.en)[k];}
function applyL(){document.documentElement.lang=lg;g('h').textContent=L('wb');g('sub').textContent=L('sub');g('lu').textContent=L('u');g('lp').textContent=L('p');g('btn').textContent=L('s');g('lang').value=lg;}
function setL(v){lg=v;try{localStorage.setItem('cb_lang',v);}catch(e){} applyL();}
function signin(e){ e.preventDefault();
  fetch('/api/login',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({username:g('u').value,password:g('p').value})})
   .then(function(r){ if(r.ok) location.href='/'; else g('err').textContent=L('inv'); })
   .catch(function(){ g('err').textContent=L('ne'); });
  return false; }
applyL();
</script>
</body></html>
"#;

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>camera-box</title>
<link rel="icon" type="image/svg+xml" href="/favicon.svg">
<link rel="alternate icon" href="/favicon.ico">
<style>
:root{--bg:#f5f7fb;--card:#fff;--primary:#2563eb;--success:#16a34a;--warn:#f59e0b;--danger:#dc2626;
 --txt:#111827;--muted:#6b7280;--border:#e5e7eb;
 --shadow:0 1px 2px rgba(16,24,40,.04),0 4px 16px rgba(16,24,40,.06);--shadow-h:0 10px 30px rgba(16,24,40,.12)}
*{box-sizing:border-box}
body{margin:0;background:var(--bg);color:var(--txt);-webkit-font-smoothing:antialiased;
 font-family:'Inter',system-ui,-apple-system,'Segoe UI',Roboto,Helvetica,Arial,sans-serif}
a{color:var(--primary);text-decoration:none}
.topbar{position:sticky;top:0;z-index:30;background:rgba(255,255,255,.82);backdrop-filter:blur(12px);border-bottom:1px solid var(--border)}
.bar-inner{max-width:1400px;margin:0 auto;display:flex;align-items:center;gap:16px;padding:11px 22px;flex-wrap:wrap}
.brand{display:flex;align-items:center;gap:10px;font-weight:700;font-size:17px;letter-spacing:-.01em}
.brand .ic{width:32px;height:32px;border-radius:10px;background:var(--primary);color:#fff;display:grid;place-items:center;box-shadow:0 6px 16px rgba(37,99,235,.3)}
.tabs{display:flex;gap:3px;flex:1;flex-wrap:wrap}
.tabs button{display:flex;align-items:center;gap:8px;border:0;background:transparent;color:var(--muted);font:inherit;font-size:14px;font-weight:600;padding:9px 13px;border-radius:11px;cursor:pointer;transition:.15s}
.tabs button:hover{background:#eef2f8;color:var(--txt)}
.tabs button.active{background:#e8effe;color:var(--primary)}
.right{display:flex;align-items:center;gap:9px}
.iconbtn{width:40px;height:40px;border-radius:12px;border:1px solid var(--border);background:#fff;color:var(--muted);display:grid;place-items:center;cursor:pointer;transition:.15s}
.iconbtn:hover{color:var(--txt);box-shadow:var(--shadow)}
.user{display:flex;align-items:center;gap:9px;padding:5px 12px 5px 5px;border:1px solid var(--border);border-radius:999px;background:#fff;font-size:13px;font-weight:600;color:var(--txt)}
.user .av{width:28px;height:28px;border-radius:999px;background:#e8effe;color:var(--primary);display:grid;place-items:center;font-weight:700;font-size:13px}
.wrap{max-width:1400px;margin:0 auto;padding:30px 22px 70px}
.page{animation:fade .28s ease}
.hidden{display:none}
@keyframes fade{from{opacity:0;transform:translateY(8px)}to{opacity:1;transform:none}}
.title{font-size:28px;margin:0 0 4px;letter-spacing:-.025em;font-weight:700}
.subtitle{color:var(--muted);font-size:15px;margin:0 0 24px}
.sec{font-size:16px;font-weight:700;margin:30px 0 14px;letter-spacing:-.01em}
.grid{display:grid;gap:18px}
.cols-4{grid-template-columns:repeat(4,1fr)}.cols-2{grid-template-columns:repeat(2,1fr)}
.cols-auto{grid-template-columns:repeat(auto-fill,minmax(290px,1fr))}
@media(max-width:1150px){.cols-4{grid-template-columns:repeat(2,1fr)}}
@media(max-width:700px){.cols-4,.cols-2{grid-template-columns:1fr}}
.card{background:var(--card);border:1px solid var(--border);border-radius:18px;padding:20px;box-shadow:var(--shadow);transition:transform .18s,box-shadow .18s}
.card.hov:hover{box-shadow:var(--shadow-h);transform:translateY(-2px)}
.ic-circle{width:46px;height:46px;border-radius:14px;display:grid;place-items:center;flex:0 0 auto}
.ic-blue{background:#e8effe;color:#2563eb}.ic-green{background:#e7f6ec;color:#16a34a}.ic-purple{background:#f1ebfe;color:#7c3aed}
.ic-orange{background:#fef3e2;color:#d97706}.ic-red{background:#fdeaea;color:#dc2626}.ic-gray{background:#eef1f5;color:#6b7280}
.statusbig{font-size:20px;font-weight:700;margin:10px 0 4px;letter-spacing:-.01em}
.muted{color:var(--muted);font-size:14px}
.btn{display:inline-flex;align-items:center;justify-content:center;gap:8px;min-height:42px;padding:0 16px;border-radius:13px;border:1px solid var(--border);background:#fff;color:var(--txt);font:inherit;font-weight:600;font-size:14px;cursor:pointer;transition:.15s}
.btn:hover{box-shadow:var(--shadow)}.btn:active{transform:translateY(1px)}
.btn.primary{background:var(--primary);border-color:var(--primary);color:#fff}.btn.primary:hover{background:#1d57d6}
.btn.danger{background:var(--danger);border-color:var(--danger);color:#fff}
.btn.sm{min-height:38px;padding:0 13px;font-size:13px}
.btn[disabled]{opacity:.5;cursor:not-allowed}
.btn svg,.tabs svg,.iconbtn svg,.brand svg{width:18px;height:18px}
.pill{display:inline-flex;align-items:center;gap:6px;font-size:12px;font-weight:700;padding:4px 11px;border-radius:999px}
.pill.green{background:#e7f6ec;color:#15803d}.pill.gray{background:#eef1f5;color:#6b7280}.pill.orange{background:#fef3e2;color:#b45309}.pill.red{background:#fdeaea;color:#dc2626}
.dot{width:7px;height:7px;border-radius:999px;background:currentColor;box-shadow:0 0 0 3px rgba(22,163,74,.18)}
input,select{width:100%;min-height:44px;border:1px solid var(--border);border-radius:12px;padding:0 13px;font:inherit;font-size:14px;background:#fff;color:var(--txt)}
input:focus,select:focus{outline:none;border-color:var(--primary);box-shadow:0 0 0 4px rgba(37,99,235,.13)}
label{display:block;font-size:13px;font-weight:600;color:var(--muted);margin:13px 0 6px}
.row{display:flex;gap:12px;flex-wrap:wrap;align-items:center}
.qa{display:grid;grid-template-columns:repeat(4,1fr);gap:14px}
@media(max-width:900px){.qa{grid-template-columns:repeat(2,1fr)}}
.qa button{display:flex;flex-direction:column;align-items:center;gap:11px;padding:20px;background:#fff;border:1px solid var(--border);border-radius:16px;cursor:pointer;transition:.18s;font:inherit;font-weight:600;font-size:14px;color:var(--txt);box-shadow:var(--shadow)}
.qa button:hover{box-shadow:var(--shadow-h);transform:translateY(-2px)}
.progress{height:9px;border-radius:999px;background:#eef1f5;overflow:hidden;margin-top:12px}
.progress>i{display:block;height:100%;border-radius:999px;background:var(--primary);transition:width .5s}
.bars-good>i{background:var(--success)}.bars-warn>i{background:var(--warn)}.bars-bad>i{background:var(--danger)}
.expander{margin-top:16px}
.expander>summary{cursor:pointer;color:var(--muted);font-weight:600;font-size:13px;list-style:none;display:flex;align-items:center;gap:7px;padding:9px 0;user-select:none}
.expander>summary::-webkit-details-marker{display:none}
.expander>summary svg{width:15px;height:15px;transition:transform .2s}
.expander[open]>summary svg{transform:rotate(180deg)}
.card.expander[open]{box-shadow:var(--shadow)}
.kvs{display:grid;grid-template-columns:auto 1fr;gap:7px 16px;font-size:13px;margin-top:6px}
.kvs .k{color:var(--muted)}.kvs .v{color:var(--txt);text-align:right;font-variant-numeric:tabular-nums;word-break:break-all}
.seg{display:inline-flex;border:1px solid var(--border);border-radius:12px;overflow:hidden;background:#fff;box-shadow:var(--shadow)}
.seg button{border:0;background:#fff;padding:9px 12px;cursor:pointer;color:var(--muted);display:grid;place-items:center}
.seg button.active{background:#e8effe;color:var(--primary)}
.toggle{position:relative;width:46px;height:28px;border-radius:999px;background:#d1d5db;border:0;cursor:pointer;transition:.2s;flex:0 0 auto}
.toggle.on{background:var(--success)}
.toggle::after{content:"";position:absolute;top:3px;left:3px;width:22px;height:22px;border-radius:999px;background:#fff;transition:.2s;box-shadow:0 1px 3px rgba(0,0,0,.25)}
.toggle.on::after{transform:translateX(18px)}
.cam-card .thumb{height:158px;border-radius:14px;background:linear-gradient(135deg,#eef2f8,#dfe6f0);display:grid;place-items:center;color:#9aa7bd;position:relative;overflow:hidden}
.cam-card .thumb .live{position:absolute;top:11px;left:11px}
.netlist{margin-top:12px;max-height:170px;overflow:auto;border:1px solid var(--border);border-radius:12px}
.netlist:empty{display:none}
.netlist div{padding:9px 12px;cursor:pointer;font-size:14px;border-bottom:1px solid var(--border)}
.netlist div:last-child{border-bottom:0}.netlist div:hover{background:#f3f6fb}
.empty{text-align:center;padding:46px 20px;color:var(--muted)}
.empty .ic-circle{margin:0 auto 16px;width:64px;height:64px;border-radius:20px}
.logbox{max-height:64vh;overflow:auto;font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace;font-size:12.5px;line-height:1.7}
.logline{display:block;padding:3px 10px;border-radius:7px;white-space:pre-wrap;word-break:break-word}
.logline:hover{background:#f3f6fb}
.ll-INFO{color:#2563eb}.ll-WARN{color:#b45309}.ll-ERROR{color:#dc2626}.ll-DEBUG{color:#6b7280}.ll-TRACE{color:#9ca3af}
.toast{position:fixed;right:22px;bottom:22px;display:flex;flex-direction:column;gap:10px;z-index:60}
.toast .t{background:#fff;border:1px solid var(--border);border-left:4px solid var(--success);border-radius:13px;padding:13px 16px;box-shadow:var(--shadow-h);font-size:14px;font-weight:500;max-width:320px;transition:opacity .3s;animation:slidein .25s}
.toast .t.err{border-left-color:var(--danger)}
@keyframes slidein{from{opacity:0;transform:translateX(24px)}to{opacity:1;transform:none}}
</style>
</head>
<body>
<div class="topbar"><div class="bar-inner">
<div class="brand"><span class="ic"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M23 7l-7 5 7 5V7z"/><rect x="1" y="5" width="15" height="14" rx="3"/></svg></span>camera-box</div>
<nav class="tabs" id="tabs"></nav>
<div class="right">
<select id="langsel" title="Language" onchange="setLang(this.value)" style="min-height:40px;width:auto;padding:0 10px;border-radius:12px;font-weight:600;color:var(--muted)"><option value="en">EN</option><option value="fr">FR</option><option value="de">DE</option><option value="it">IT</option></select>
<button class="iconbtn" title="Help" onclick="toast(t('help_tip'))" id="help"></button>
<div class="user"><span class="av">A</span><span id="hostlabel">camera-box</span></div>
<button class="iconbtn" title="Log out" onclick="logout()" id="logoutbtn"></button>
</div>
</div></div>
<main class="wrap">
<section class="page" id="p-dashboard"></section>
<section class="page hidden" id="p-cameras"></section>
<section class="page hidden" id="p-network"></section>
<section class="page hidden" id="p-system"></section>
<section class="page hidden" id="p-logs"></section>
<section class="page hidden" id="p-settings"></section>
</main>
<div class="toast" id="toast"></div>
<script>
// ===== icons (inline SVG, offline) =====
var IC={
 dashboard:'<rect x="3" y="3" width="7" height="9" rx="1.5"/><rect x="14" y="3" width="7" height="5" rx="1.5"/><rect x="14" y="12" width="7" height="9" rx="1.5"/><rect x="3" y="16" width="7" height="5" rx="1.5"/>',
 cameras:'<path d="M23 7l-7 5 7 5V7z"/><rect x="1" y="5" width="15" height="14" rx="2"/>',
 wifi:'<path d="M5 12.55a11 11 0 0 1 14 0"/><path d="M8.5 16.1a6 6 0 0 1 7 0"/><path d="M2 8.82a15 15 0 0 1 20 0"/><line x1="12" y1="20" x2="12.01" y2="20"/>',
 system:'<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6"/><line x1="9" y1="1" x2="9" y2="4"/><line x1="15" y1="1" x2="15" y2="4"/><line x1="9" y1="20" x2="9" y2="23"/><line x1="15" y1="20" x2="15" y2="23"/><line x1="20" y1="9" x2="23" y2="9"/><line x1="20" y1="14" x2="23" y2="14"/><line x1="1" y1="9" x2="4" y2="9"/><line x1="1" y1="14" x2="4" y2="14"/>',
 logs:'<path d="M14 2H6a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8z"/><path d="M14 2v6h6"/><line x1="8" y1="13" x2="16" y2="13"/><line x1="8" y1="17" x2="16" y2="17"/><line x1="8" y1="9" x2="10" y2="9"/>',
 settings:'<line x1="4" y1="21" x2="4" y2="14"/><line x1="4" y1="10" x2="4" y2="3"/><line x1="12" y1="21" x2="12" y2="12"/><line x1="12" y1="8" x2="12" y2="3"/><line x1="20" y1="21" x2="20" y2="16"/><line x1="20" y1="12" x2="20" y2="3"/><line x1="1" y1="14" x2="7" y2="14"/><line x1="9" y1="8" x2="15" y2="8"/><line x1="17" y1="16" x2="23" y2="16"/>',
 help:'<circle cx="12" cy="12" r="10"/><path d="M9.1 9a3 3 0 0 1 5.8 1c0 2-3 3-3 3"/><line x1="12" y1="17" x2="12.01" y2="17"/>',
 logout:'<path d="M9 21H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h4"/><polyline points="16 17 21 12 16 7"/><line x1="21" y1="12" x2="9" y2="12"/>',
 globe:'<circle cx="12" cy="12" r="10"/><line x1="2" y1="12" x2="22" y2="12"/><path d="M12 2a15 15 0 0 1 0 20 15 15 0 0 1 0-20z"/>',
 drive:'<line x1="22" y1="12" x2="2" y2="12"/><path d="M5.4 5.1 2 12v6a2 2 0 0 0 2 2h16a2 2 0 0 0 2-2v-6l-3.4-6.9A2 2 0 0 0 16.8 4H7.2a2 2 0 0 0-1.8 1.1z"/><line x1="6" y1="16" x2="6.01" y2="16"/><line x1="10" y1="16" x2="10.01" y2="16"/>',
 thermo:'<path d="M14 14.76V3.5a2.5 2.5 0 0 0-5 0v11.26a4.5 4.5 0 1 0 5 0z"/>',
 chip:'<rect x="4" y="4" width="16" height="16" rx="2"/><rect x="9" y="9" width="6" height="6"/><line x1="9" y1="2" x2="9" y2="4"/><line x1="15" y1="2" x2="15" y2="4"/><line x1="9" y1="20" x2="9" y2="22"/><line x1="15" y1="20" x2="15" y2="22"/><line x1="20" y1="9" x2="22" y2="9"/><line x1="20" y1="15" x2="22" y2="15"/><line x1="2" y1="9" x2="4" y2="9"/><line x1="2" y1="15" x2="4" y2="15"/>',
 plus:'<line x1="12" y1="5" x2="12" y2="19"/><line x1="5" y1="12" x2="19" y2="12"/>',
 grid:'<rect x="3" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="3" width="7" height="7" rx="1.5"/><rect x="14" y="14" width="7" height="7" rx="1.5"/><rect x="3" y="14" width="7" height="7" rx="1.5"/>',
 list:'<line x1="8" y1="6" x2="21" y2="6"/><line x1="8" y1="12" x2="21" y2="12"/><line x1="8" y1="18" x2="21" y2="18"/><line x1="3" y1="6" x2="3.01" y2="6"/><line x1="3" y1="12" x2="3.01" y2="12"/><line x1="3" y1="18" x2="3.01" y2="18"/>',
 refresh:'<polyline points="23 4 23 10 17 10"/><polyline points="1 20 1 14 7 14"/><path d="M3.5 9a9 9 0 0 1 14.8-3.4L23 10M1 14l4.6 4.4A9 9 0 0 0 20.5 15"/>',
 search:'<circle cx="11" cy="11" r="8"/><line x1="21" y1="21" x2="16.65" y2="16.65"/>',
 download:'<path d="M21 15v4a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2v-4"/><polyline points="7 10 12 15 17 10"/><line x1="12" y1="15" x2="12" y2="3"/>',
 check:'<path d="M22 11.08V12a10 10 0 1 1-5.93-9.14"/><polyline points="22 4 12 14.01 9 11.01"/>',
 alert:'<path d="M10.29 3.86 1.82 18a2 2 0 0 0 1.71 3h16.94a2 2 0 0 0 1.71-3L13.71 3.86a2 2 0 0 0-3.42 0z"/><line x1="12" y1="9" x2="12" y2="13"/><line x1="12" y1="17" x2="12.01" y2="17"/>',
 chevron:'<polyline points="6 9 12 15 18 9"/>',
 play:'<polygon points="6 4 20 12 6 20 6 4"/>',
 link:'<path d="M10 13a5 5 0 0 0 7.54.54l3-3a5 5 0 0 0-7.07-7.07l-1.72 1.71"/><path d="M14 11a5 5 0 0 0-7.54-.54l-3 3a5 5 0 0 0 7.07 7.07l1.71-1.71"/>',
 user:'<path d="M20 21v-2a4 4 0 0 0-4-4H8a4 4 0 0 0-4 4v2"/><circle cx="12" cy="7" r="4"/>',
 trash:'<polyline points="3 6 5 6 21 6"/><path d="M19 6v14a2 2 0 0 1-2 2H7a2 2 0 0 1-2-2V6m3 0V4a2 2 0 0 1 2-2h4a2 2 0 0 1 2 2v2"/>'
};
function ic(n,sz){ return '<svg class="ic" width="'+(sz||18)+'" height="'+(sz||18)+'" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round">'+(IC[n]||'')+'</svg>'; }

// ===== i18n =====
var LANG={
 en:{nav_dashboard:"Dashboard",nav_cameras:"Cameras",nav_network:"Network",nav_system:"System",nav_logs:"Logs",nav_settings:"Settings",help:"Help",logout:"Log out",help_tip:"Plug in USB cameras and set up Wi-Fi from the Network tab.",
  welcome:"Welcome home",all_good:"Everything looks good. Your camera-box is running normally.",needs_sub:"A few things need your attention.",
  internet:"Internet",wifi:"Wi-Fi",cameras:"Cameras",device_health:"Device health",connected:"Connected",hotspot_mode:"Hotspot mode",offline:"Offline",not_connected:"Not connected",
  connected_to:"Connected to {x}.",providing_wifi:"Providing its own Wi-Fi network.",not_conn_net:"Not connected to a network.",hotspot_on_sub:"Your hotspot is on.",hotspot_off:"Hotspot off",using_x:"Using {x}.",no_hotspot:"No hotspot running.",
  run_diag:"Run diagnostics",manage_wifi:"Manage Wi-Fi",online:"Online",offlineword:"offline",cams_have:"You have {n} camera(s).",no_cams_added:"No cameras added yet.",open_cameras:"Open cameras",view_system:"View system",
  excellent:"Excellent",needs_attention:"Needs attention",quick_actions:"Quick actions",add_camera:"Add camera",scan_wifi:"Scan Wi-Fi",change_password:"Change password",
  status:"Status",attention_needed:"Attention needed",all_great:"Everything looks great",no_action:"No action is required.",
  cam_offline:"Camera offline",cam_offline_d:"{x} is on but not streaming yet.",not_connected_d:"This device is not connected to a network.",set_up_wifi:"Set up Wi-Fi",running_warm:"Running warm",running_warm_d:"Temperature is {x} °C.",
  diag_done:"All systems checked — everything looks good.",addcam_tip:"Plug a USB camera into the device — it appears here automatically.",
  cams_connected:"{n} camera(s) connected",no_cams_yet:"No cameras yet",no_cams_desc:"Plug a USB camera into the device and it will appear here automatically.",
  live:"Live",off:"Off",starting:"Starting…",camera_w:"Camera",quality:"Quality",preview:"Preview",advanced:"Advanced details",device:"Device",stream:"Stream",port:"Port",process:"Process",usb_id:"USB id",video_stream:"Video Stream",passthrough:"passthrough",
  quality_updated:"Quality updated.",cam_on:"Camera turned on.",cam_turned_off:"Camera turned off.",action_failed:"Action failed",
  network_sub:"Manage how your camera-box connects to Wi-Fi.",current_conn:"Current connection",signal_exc:"Signal: Excellent",built_in:"built-in",wifi_setup:"Wi-Fi setup",adapter:"Adapter",scan:"Scan",scanning:"Scanning…",start_hotspot:"Start hotspot",
  ssid_label:"Network name (SSID)",password:"Password",adv_static:"Advanced (static IP)",addressing:"Addressing",dhcp_auto:"Automatic (DHCP)",static_ip:"Static IP",static_ip_label:"Static IP (a.b.c.d/nn)",for_static:"for static only",save_as_opt:"Save as (optional)",connect:"Connect",
  saved_networks:"Saved networks",connect_using:"Connect using",no_saved:"No saved networks yet.",no_networks:"No networks found",connecting:"Connecting…",starting_hotspot:"Starting hotspot…",done:"Done.",failed:"Failed",conn_changed:"The connection changed — you may need to reconnect.",scan_failed:"Scan failed",
  your_device:"Your device",processor:"Processor",memory:"Memory",storage:"Storage",temperature:"Temperature",network_activity:"Network activity",measuring:"measuring…",interface:"Interface",down:"Down",up:"Up",
  model:"Model",firmware:"Firmware",hostname:"Hostname",local_time:"Local time",system_uptime:"System uptime",cpu:"CPU",disk:"Disk",
  logs_sub:"Recent activity from your device.",search_logs:"Search logs",all_levels:"All levels",autoscroll:"Auto-scroll",onword:"On",offword:"Off",download:"Download",no_log_lines:"No log lines.",
  settings_sub:"Manage your device.",general:"General",reachable_as:"Reachable as {x}.local",device_name:"Device name",save:"Save",security:"Security",username:"Username",new_password:"New password",update_password:"Update password",about:"About",uptime:"Uptime",
  name_saved:"Device name saved.",password_updated:"Password updated.",stream_protection:"Stream protection",protected_w:"Protected",auth_user_ph:"username (blank = off)",auth_saved:"Stream protection updated.",copy_link:"Copy link",link_copied:"Stream link copied.",credentials:"Login",protected_stream:"Protected Stream",show_login:"Show login information",copy_creds:"Copy credentials",creds_copied:"Credentials copied."},
 fr:{nav_dashboard:"Tableau de bord",nav_cameras:"Caméras",nav_network:"Réseau",nav_system:"Système",nav_logs:"Journaux",nav_settings:"Réglages",help:"Aide",logout:"Se déconnecter",help_tip:"Branchez des caméras USB et configurez le Wi-Fi dans l'onglet Réseau.",
  welcome:"Bienvenue",all_good:"Tout fonctionne bien. Votre camera-box fonctionne normalement.",needs_sub:"Quelques points nécessitent votre attention.",
  internet:"Internet",wifi:"Wi-Fi",cameras:"Caméras",device_health:"État de l'appareil",connected:"Connecté",hotspot_mode:"Mode point d'accès",offline:"Hors ligne",not_connected:"Non connecté",
  connected_to:"Connecté à {x}.",providing_wifi:"Diffuse son propre réseau Wi-Fi.",not_conn_net:"Non connecté à un réseau.",hotspot_on_sub:"Votre point d'accès est actif.",hotspot_off:"Point d'accès désactivé",using_x:"Utilise {x}.",no_hotspot:"Aucun point d'accès actif.",
  run_diag:"Lancer un diagnostic",manage_wifi:"Gérer le Wi-Fi",online:"en ligne",offlineword:"hors ligne",cams_have:"Vous avez {n} caméra(s).",no_cams_added:"Aucune caméra ajoutée.",open_cameras:"Ouvrir les caméras",view_system:"Voir le système",
  excellent:"Excellent",needs_attention:"Attention requise",quick_actions:"Actions rapides",add_camera:"Ajouter une caméra",scan_wifi:"Rechercher un Wi-Fi",change_password:"Changer le mot de passe",
  status:"État",attention_needed:"Attention requise",all_great:"Tout va bien",no_action:"Aucune action requise.",
  cam_offline:"Caméra hors ligne",cam_offline_d:"{x} est activée mais ne diffuse pas encore.",not_connected_d:"Cet appareil n'est connecté à aucun réseau.",set_up_wifi:"Configurer le Wi-Fi",running_warm:"Température élevée",running_warm_d:"La température est de {x} °C.",
  diag_done:"Tous les systèmes vérifiés — tout va bien.",addcam_tip:"Branchez une caméra USB sur l'appareil — elle apparaît automatiquement ici.",
  cams_connected:"{n} caméra(s) connectée(s)",no_cams_yet:"Aucune caméra",no_cams_desc:"Branchez une caméra USB et elle apparaîtra automatiquement ici.",
  live:"En direct",off:"Arrêt",starting:"Démarrage…",camera_w:"Caméra",quality:"Qualité",preview:"Aperçu",advanced:"Détails avancés",device:"Appareil",stream:"Flux",port:"Port",process:"Processus",usb_id:"ID USB",video_stream:"Flux vidéo",passthrough:"direct",
  quality_updated:"Qualité mise à jour.",cam_on:"Caméra activée.",cam_turned_off:"Caméra désactivée.",action_failed:"Échec de l'action",
  network_sub:"Gérez la connexion Wi-Fi de votre camera-box.",current_conn:"Connexion actuelle",signal_exc:"Signal : Excellent",built_in:"intégré",wifi_setup:"Configuration Wi-Fi",adapter:"Adaptateur",scan:"Rechercher",scanning:"Recherche…",start_hotspot:"Activer le point d'accès",
  ssid_label:"Nom du réseau (SSID)",password:"Mot de passe",adv_static:"Avancé (IP statique)",addressing:"Adressage",dhcp_auto:"Automatique (DHCP)",static_ip:"IP statique",static_ip_label:"IP statique (a.b.c.d/nn)",for_static:"pour IP statique uniquement",save_as_opt:"Enregistrer sous (facultatif)",connect:"Connecter",
  saved_networks:"Réseaux enregistrés",connect_using:"Se connecter via",no_saved:"Aucun réseau enregistré.",no_networks:"Aucun réseau trouvé",connecting:"Connexion…",starting_hotspot:"Activation du point d'accès…",done:"Terminé.",failed:"Échec",conn_changed:"La connexion a changé — vous devrez peut-être vous reconnecter.",scan_failed:"Échec de la recherche",
  your_device:"Votre appareil",processor:"Processeur",memory:"Mémoire",storage:"Stockage",temperature:"Température",network_activity:"Activité réseau",measuring:"mesure…",interface:"Interface",down:"Réception",up:"Émission",
  model:"Modèle",firmware:"Micrologiciel",hostname:"Nom d'hôte",local_time:"Heure locale",system_uptime:"Temps de fonctionnement",cpu:"Processeur",disk:"Disque",
  logs_sub:"Activité récente de votre appareil.",search_logs:"Rechercher dans les journaux",all_levels:"Tous les niveaux",autoscroll:"Défilement auto",onword:"Activé",offword:"Désactivé",download:"Télécharger",no_log_lines:"Aucune ligne de journal.",
  settings_sub:"Gérez votre appareil.",general:"Général",reachable_as:"Accessible via {x}.local",device_name:"Nom de l'appareil",save:"Enregistrer",security:"Sécurité",username:"Nom d'utilisateur",new_password:"Nouveau mot de passe",update_password:"Mettre à jour",about:"À propos",uptime:"Disponibilité",
  name_saved:"Nom de l'appareil enregistré.",password_updated:"Mot de passe mis à jour.",stream_protection:"Protection du flux",protected_w:"Protégé",auth_user_ph:"utilisateur (vide = désactivé)",auth_saved:"Protection du flux mise à jour.",copy_link:"Copier le lien",link_copied:"Lien du flux copié.",credentials:"Identifiants",protected_stream:"Flux protégé",show_login:"Afficher les identifiants",copy_creds:"Copier les identifiants",creds_copied:"Identifiants copiés."},
 de:{nav_dashboard:"Übersicht",nav_cameras:"Kameras",nav_network:"Netzwerk",nav_system:"System",nav_logs:"Protokolle",nav_settings:"Einstellungen",help:"Hilfe",logout:"Abmelden",help_tip:"Schließen Sie USB-Kameras an und richten Sie WLAN im Tab Netzwerk ein.",
  welcome:"Willkommen zu Hause",all_good:"Alles in Ordnung. Ihre camera-box läuft normal.",needs_sub:"Einige Dinge erfordern Ihre Aufmerksamkeit.",
  internet:"Internet",wifi:"WLAN",cameras:"Kameras",device_health:"Gerätezustand",connected:"Verbunden",hotspot_mode:"Hotspot-Modus",offline:"Offline",not_connected:"Nicht verbunden",
  connected_to:"Verbunden mit {x}.",providing_wifi:"Stellt ein eigenes WLAN bereit.",not_conn_net:"Mit keinem Netzwerk verbunden.",hotspot_on_sub:"Ihr Hotspot ist aktiv.",hotspot_off:"Hotspot aus",using_x:"Verwendet {x}.",no_hotspot:"Kein Hotspot aktiv.",
  run_diag:"Diagnose ausführen",manage_wifi:"WLAN verwalten",online:"Online",offlineword:"offline",cams_have:"Sie haben {n} Kamera(s).",no_cams_added:"Noch keine Kameras hinzugefügt.",open_cameras:"Kameras öffnen",view_system:"System ansehen",
  excellent:"Ausgezeichnet",needs_attention:"Achtung erforderlich",quick_actions:"Schnellaktionen",add_camera:"Kamera hinzufügen",scan_wifi:"WLAN suchen",change_password:"Passwort ändern",
  status:"Status",attention_needed:"Achtung erforderlich",all_great:"Alles bestens",no_action:"Keine Aktion erforderlich.",
  cam_offline:"Kamera offline",cam_offline_d:"{x} ist an, streamt aber noch nicht.",not_connected_d:"Dieses Gerät ist mit keinem Netzwerk verbunden.",set_up_wifi:"WLAN einrichten",running_warm:"Wird warm",running_warm_d:"Die Temperatur beträgt {x} °C.",
  diag_done:"Alle Systeme geprüft — alles in Ordnung.",addcam_tip:"Schließen Sie eine USB-Kamera an — sie erscheint hier automatisch.",
  cams_connected:"{n} Kamera(s) verbunden",no_cams_yet:"Noch keine Kameras",no_cams_desc:"Schließen Sie eine USB-Kamera an, sie erscheint hier automatisch.",
  live:"Live",off:"Aus",starting:"Startet…",camera_w:"Kamera",quality:"Qualität",preview:"Vorschau",advanced:"Erweiterte Details",device:"Gerät",stream:"Stream",port:"Port",process:"Prozess",usb_id:"USB-ID",video_stream:"Videostream",passthrough:"Durchleitung",
  quality_updated:"Qualität aktualisiert.",cam_on:"Kamera eingeschaltet.",cam_turned_off:"Kamera ausgeschaltet.",action_failed:"Aktion fehlgeschlagen",
  network_sub:"Verwalten Sie die WLAN-Verbindung Ihrer camera-box.",current_conn:"Aktuelle Verbindung",signal_exc:"Signal: Ausgezeichnet",built_in:"integriert",wifi_setup:"WLAN-Einrichtung",adapter:"Adapter",scan:"Suchen",scanning:"Suche…",start_hotspot:"Hotspot starten",
  ssid_label:"Netzwerkname (SSID)",password:"Passwort",adv_static:"Erweitert (statische IP)",addressing:"Adressierung",dhcp_auto:"Automatisch (DHCP)",static_ip:"Statische IP",static_ip_label:"Statische IP (a.b.c.d/nn)",for_static:"nur für statische IP",save_as_opt:"Speichern als (optional)",connect:"Verbinden",
  saved_networks:"Gespeicherte Netzwerke",connect_using:"Verbinden über",no_saved:"Noch keine gespeicherten Netzwerke.",no_networks:"Keine Netzwerke gefunden",connecting:"Verbinde…",starting_hotspot:"Hotspot wird gestartet…",done:"Fertig.",failed:"Fehlgeschlagen",conn_changed:"Die Verbindung hat sich geändert — möglicherweise müssen Sie sich erneut verbinden.",scan_failed:"Suche fehlgeschlagen",
  your_device:"Ihr Gerät",processor:"Prozessor",memory:"Arbeitsspeicher",storage:"Speicher",temperature:"Temperatur",network_activity:"Netzwerkaktivität",measuring:"messe…",interface:"Schnittstelle",down:"Empfang",up:"Senden",
  model:"Modell",firmware:"Firmware",hostname:"Hostname",local_time:"Ortszeit",system_uptime:"Systemlaufzeit",cpu:"CPU",disk:"Festplatte",
  logs_sub:"Letzte Aktivität Ihres Geräts.",search_logs:"Protokolle durchsuchen",all_levels:"Alle Stufen",autoscroll:"Auto-Scroll",onword:"Ein",offword:"Aus",download:"Herunterladen",no_log_lines:"Keine Protokollzeilen.",
  settings_sub:"Verwalten Sie Ihr Gerät.",general:"Allgemein",reachable_as:"Erreichbar als {x}.local",device_name:"Gerätename",save:"Speichern",security:"Sicherheit",username:"Benutzername",new_password:"Neues Passwort",update_password:"Passwort aktualisieren",about:"Über",uptime:"Laufzeit",
  name_saved:"Gerätename gespeichert.",password_updated:"Passwort aktualisiert.",stream_protection:"Stream-Schutz",protected_w:"Geschützt",auth_user_ph:"Benutzer (leer = aus)",auth_saved:"Stream-Schutz aktualisiert.",copy_link:"Link kopieren",link_copied:"Stream-Link kopiert.",credentials:"Zugangsdaten",protected_stream:"Geschützter Stream",show_login:"Anmeldedaten anzeigen",copy_creds:"Zugangsdaten kopieren",creds_copied:"Zugangsdaten kopiert."},
 it:{nav_dashboard:"Dashboard",nav_cameras:"Telecamere",nav_network:"Rete",nav_system:"Sistema",nav_logs:"Registri",nav_settings:"Impostazioni",help:"Aiuto",logout:"Esci",help_tip:"Collega telecamere USB e configura il Wi-Fi nella scheda Rete.",
  welcome:"Bentornato",all_good:"Tutto a posto. La tua camera-box funziona normalmente.",needs_sub:"Alcune cose richiedono la tua attenzione.",
  internet:"Internet",wifi:"Wi-Fi",cameras:"Telecamere",device_health:"Stato del dispositivo",connected:"Connesso",hotspot_mode:"Modalità hotspot",offline:"Offline",not_connected:"Non connesso",
  connected_to:"Connesso a {x}.",providing_wifi:"Fornisce la propria rete Wi-Fi.",not_conn_net:"Non connesso a una rete.",hotspot_on_sub:"Il tuo hotspot è attivo.",hotspot_off:"Hotspot disattivato",using_x:"Usa {x}.",no_hotspot:"Nessun hotspot attivo.",
  run_diag:"Esegui diagnostica",manage_wifi:"Gestisci Wi-Fi",online:"Online",offlineword:"offline",cams_have:"Hai {n} telecamera/e.",no_cams_added:"Nessuna telecamera aggiunta.",open_cameras:"Apri telecamere",view_system:"Vedi sistema",
  excellent:"Eccellente",needs_attention:"Richiede attenzione",quick_actions:"Azioni rapide",add_camera:"Aggiungi telecamera",scan_wifi:"Cerca Wi-Fi",change_password:"Cambia password",
  status:"Stato",attention_needed:"Richiede attenzione",all_great:"Tutto a posto",no_action:"Nessuna azione richiesta.",
  cam_offline:"Telecamera offline",cam_offline_d:"{x} è attiva ma non trasmette ancora.",not_connected_d:"Questo dispositivo non è connesso a nessuna rete.",set_up_wifi:"Configura Wi-Fi",running_warm:"Temperatura elevata",running_warm_d:"La temperatura è di {x} °C.",
  diag_done:"Tutti i sistemi verificati — tutto a posto.",addcam_tip:"Collega una telecamera USB al dispositivo — apparirà qui automaticamente.",
  cams_connected:"{n} telecamera/e connessa/e",no_cams_yet:"Nessuna telecamera",no_cams_desc:"Collega una telecamera USB e apparirà qui automaticamente.",
  live:"In diretta",off:"Spenta",starting:"Avvio…",camera_w:"Telecamera",quality:"Qualità",preview:"Anteprima",advanced:"Dettagli avanzati",device:"Dispositivo",stream:"Flusso",port:"Porta",process:"Processo",usb_id:"ID USB",video_stream:"Flusso video",passthrough:"diretto",
  quality_updated:"Qualità aggiornata.",cam_on:"Telecamera attivata.",cam_turned_off:"Telecamera disattivata.",action_failed:"Azione non riuscita",
  network_sub:"Gestisci come la tua camera-box si connette al Wi-Fi.",current_conn:"Connessione attuale",signal_exc:"Segnale: Eccellente",built_in:"integrato",wifi_setup:"Configurazione Wi-Fi",adapter:"Adattatore",scan:"Cerca",scanning:"Ricerca…",start_hotspot:"Avvia hotspot",
  ssid_label:"Nome rete (SSID)",password:"Password",adv_static:"Avanzate (IP statico)",addressing:"Indirizzamento",dhcp_auto:"Automatico (DHCP)",static_ip:"IP statico",static_ip_label:"IP statico (a.b.c.d/nn)",for_static:"solo per IP statico",save_as_opt:"Salva come (facoltativo)",connect:"Connetti",
  saved_networks:"Reti salvate",connect_using:"Connetti tramite",no_saved:"Nessuna rete salvata.",no_networks:"Nessuna rete trovata",connecting:"Connessione…",starting_hotspot:"Avvio hotspot…",done:"Fatto.",failed:"Non riuscito",conn_changed:"La connessione è cambiata — potrebbe essere necessario riconnettersi.",scan_failed:"Ricerca non riuscita",
  your_device:"Il tuo dispositivo",processor:"Processore",memory:"Memoria",storage:"Archiviazione",temperature:"Temperatura",network_activity:"Attività di rete",measuring:"misurazione…",interface:"Interfaccia",down:"Download",up:"Upload",
  model:"Modello",firmware:"Firmware",hostname:"Nome host",local_time:"Ora locale",system_uptime:"Tempo di attività",cpu:"CPU",disk:"Disco",
  logs_sub:"Attività recente del dispositivo.",search_logs:"Cerca nei registri",all_levels:"Tutti i livelli",autoscroll:"Scorrimento auto",onword:"Attivo",offword:"Disattivo",download:"Scarica",no_log_lines:"Nessuna riga di registro.",
  settings_sub:"Gestisci il tuo dispositivo.",general:"Generale",reachable_as:"Raggiungibile come {x}.local",device_name:"Nome dispositivo",save:"Salva",security:"Sicurezza",username:"Nome utente",new_password:"Nuova password",update_password:"Aggiorna password",about:"Informazioni",uptime:"Tempo attività",
  name_saved:"Nome del dispositivo salvato.",password_updated:"Password aggiornata.",stream_protection:"Protezione flusso",protected_w:"Protetto",auth_user_ph:"utente (vuoto = off)",auth_saved:"Protezione flusso aggiornata.",copy_link:"Copia link",link_copied:"Link del flusso copiato.",credentials:"Credenziali",protected_stream:"Flusso protetto",show_login:"Mostra credenziali",copy_creds:"Copia credenziali",creds_copied:"Credenziali copiate."}
};
var lang; try{lang=localStorage.getItem('cb_lang');}catch(e){} if(!lang||!LANG[lang]){var nv=(navigator.language||'en').slice(0,2).toLowerCase();lang=LANG[nv]?nv:'en';}
function t(k){var d=LANG[lang]||LANG.en;return (k in d)?d[k]:(k in LANG.en?LANG.en[k]:k);}
function tf(k,v){return t(k).split('{x}').join(v).split('{n}').join(v);}
function setLang(v){lang=v;try{localStorage.setItem('cb_lang',v);}catch(e){} L.built=false; applyChrome(); buildTabs(); go(page);}
function applyChrome(){ var ls=el('langsel'); if(ls)ls.value=lang; var hp=el('help'); if(hp)hp.title=t('help'); var lo=el('logoutbtn'); if(lo)lo.title=t('logout'); }

// ===== helpers =====
var page='dashboard', netPrev=null, sysAdvOpen=false;
var S={status:null,system:null,network:null};
var L={q:'',level:'',auto:true,lines:[],built:false};
function el(id){return document.getElementById(id);}
function esc(t){var d=document.createElement('div');d.textContent=(t==null?'':t);return d.innerHTML;}
function api(m,p,b){return fetch(p,{method:m,headers:b?{'Content-Type':'application/json'}:{},body:b?JSON.stringify(b):undefined});}
function getJSON(p){return api('GET',p).then(function(r){return r.json();});}
function logout(){api('POST','/api/logout').then(function(){location.href='/login';});}
function toast(msg,err){var c=el('toast');var t=document.createElement('div');t.className='t'+(err?' err':'');t.textContent=msg;c.appendChild(t);setTimeout(function(){t.style.opacity=0;setTimeout(function(){t.remove();},320);},2800);}
function fmtUptime(s){var d=Math.floor(s/86400),h=Math.floor((s%86400)/3600),m=Math.floor((s%3600)/60);return d>0?d+'d '+h+'h':h>0?h+'h '+m+'m':m+'m';}
function fmtBytes(n){var u=['B','KB','MB','GB','TB'],i=0;n=n||0;while(n>=1024&&i<u.length-1){n/=1024;i++;}return n.toFixed(i?1:0)+' '+u[i];}
function camName(c){var n=c.name||'USB Camera';n=n.replace(/\s*\([0-9a-f]{4}:[0-9a-f]{4}\)/i,'');var p=n.split(':');if(p.length===2&&p[0].trim()===p[1].trim())n=p[0];return (n.trim()||'USB Camera');}
function netInfo(){var n=S.network||{interfaces:[]};var cl=n.interfaces.filter(function(i){return i.mode==='client'&&i.ip;})[0];var ap=n.interfaces.filter(function(i){return i.mode==='ap';})[0];return{client:cl,ap:ap,online:!!cl};}

// ===== top nav =====
var TABS=[['dashboard','dashboard'],['cameras','cameras'],['network','wifi'],['system','system'],['logs','logs'],['settings','settings']];
function buildTabs(){ el('tabs').innerHTML=TABS.map(function(tb){return '<button data-p="'+tb[0]+'" onclick="go(\''+tb[0]+'\')"'+(tb[0]===page?' class="active"':'')+'>'+ic(tb[1],18)+t('nav_'+tb[0])+'</button>'; }).join(''); }
buildTabs();
el('help').innerHTML=ic('help',18); el('logoutbtn').innerHTML=ic('logout',18); applyChrome();
function go(p){ page=p;
  document.querySelectorAll('.tabs button').forEach(function(b){b.classList.toggle('active',b.dataset.p===p);});
  document.querySelectorAll('.page').forEach(function(s){s.classList.add('hidden');});
  var sec=el('p-'+p); sec.classList.remove('hidden'); sec.style.animation='none'; void sec.offsetHeight; sec.style.animation='';
  load(p);
}
function load(p){ if(p==='dashboard')loadDashboard();else if(p==='cameras')loadCameras();else if(p==='network')loadNetwork();else if(p==='system')loadSystem();else if(p==='logs')loadLogs();else if(p==='settings')loadSettings(); }

// ===== Dashboard =====
function loadDashboard(){ Promise.all([getJSON('/api/status'),getJSON('/api/system'),getJSON('/api/network')]).then(function(a){S.status=a[0];S.system=a[1];S.network=a[2];renderDashboard();}).catch(function(){}); }
function sumCard(color,icon,title,status,desc,action,act){
 return '<div class="card hov"><div class="ic-circle ic-'+color+'">'+ic(icon,22)+'</div>'+
  '<div class="muted" style="margin-top:14px;font-weight:600">'+esc(title)+'</div>'+
  '<div class="statusbig">'+status+'</div><div class="muted" style="min-height:40px;line-height:1.4">'+esc(desc)+'</div>'+
  '<button class="btn sm" style="margin-top:8px" onclick="qaDo(\''+act+'\')">'+esc(action)+'</button></div>';
}
function qaBtn(color,icon,label,act){ return '<button onclick="qaDo(\''+act+'\')"><div class="ic-circle ic-'+color+'">'+ic(icon,22)+'</div>'+esc(label)+'</button>'; }
function qaDo(act){ if(act.indexOf('go:')===0)go(act.slice(3));
 else if(act==='diag'){toast(t('diag_done'));}
 else if(act==='scan'){go('network');setTimeout(function(){var b=document.querySelector('#p-network .scanbtn');if(b)b.click();},350);}
 else if(act==='addcam'){toast(t('addcam_tip'));}
}
function renderDashboard(){
 var ni=netInfo(), sys=S.system||{}, cams=(S.status&&S.status.cameras)||[];
 var live=cams.filter(function(c){return c.running;}).length, off=cams.filter(function(c){return c.enabled&&!c.running;}).length;
 var alerts=[];
 cams.forEach(function(c){ if(c.enabled&&!c.running) alerts.push({t:t('cam_offline'),d:tf('cam_offline_d',camName(c)),go:'cameras',act:t('open_cameras'),k:'orange'}); });
 if(!ni.online&&!ni.ap) alerts.push({t:t('not_connected'),d:t('not_connected_d'),go:'network',act:t('set_up_wifi'),k:'red'});
 if(sys.temperature_c!=null&&sys.temperature_c>78) alerts.push({t:t('running_warm'),d:tf('running_warm_d',sys.temperature_c.toFixed(0)),go:'system',act:t('view_system'),k:'orange'});
 var ok=alerts.length===0;
 var inet=ni.online?{p:'green',s:t('connected'),d:tf('connected_to',ni.client.ssid||'')}:ni.ap?{p:'orange',s:t('hotspot_mode'),d:t('providing_wifi')}:{p:'red',s:t('offline'),d:t('not_conn_net')};
 var wifi=ni.ap?{s:esc(ni.ap.ssid||'Hotspot'),d:t('hotspot_on_sub')}:{s:t('hotspot_off'),d:ni.client?tf('using_x',ni.client.ssid):t('no_hotspot')};
 var health=(sys.temperature_c!=null&&sys.temperature_c>78)||sys.cpu_percent>92?t('needs_attention'):t('excellent');
 var h='<h1 class="title">'+t('welcome')+'</h1><p class="subtitle">'+(ok?t('all_good'):t('needs_sub'))+'</p>';
 h+='<div class="grid cols-4">'+
   sumCard('green','globe',t('internet'),'<span class="pill '+inet.p+'"><span class="dot"></span>'+inet.s+'</span>',inet.d,t('run_diag'),'diag')+
   sumCard('blue','wifi',t('wifi'),esc(wifi.s),wifi.d,t('manage_wifi'),'go:network')+
   sumCard('purple','cameras',t('cameras'),(live+' '+t('online'))+(off?' · '+off+' '+t('offlineword'):''),cams.length?tf('cams_have',cams.length):t('no_cams_added'),t('open_cameras'),'go:cameras')+
   sumCard('orange','system',t('device_health'),health,esc(sys.model||''),t('view_system'),'go:system')+
 '</div>';
 h+='<h2 class="sec">'+t('quick_actions')+'</h2><div class="qa">'+
   qaBtn('purple','plus',t('add_camera'),'addcam')+qaBtn('blue','search',t('scan_wifi'),'scan')+
   qaBtn('green','refresh',t('run_diag'),'diag')+qaBtn('orange','settings',t('change_password'),'go:settings')+'</div>';
 h+='<h2 class="sec">'+(ok?t('status'):t('attention_needed'))+'</h2>';
 if(ok){ h+='<div class="card hov"><div class="row" style="gap:14px"><div class="ic-circle ic-green">'+ic('check',22)+'</div><div><div class="statusbig" style="margin:0">'+t('all_great')+'</div><div class="muted">'+t('no_action')+'</div></div></div></div>'; }
 else { h+='<div class="grid cols-2">'+alerts.map(function(a){return '<div class="card hov"><div class="row" style="gap:14px;align-items:flex-start"><div class="ic-circle ic-'+a.k+'">'+ic('alert',22)+'</div><div style="flex:1"><div class="statusbig" style="margin:0;font-size:16px">'+esc(a.t)+'</div><div class="muted" style="margin:6px 0 14px;line-height:1.4">'+esc(a.d)+'</div><button class="btn primary sm" onclick="go(\''+a.go+'\')">'+esc(a.act)+'</button></div></div></div>';}).join('')+'</div>'; }
 el('p-dashboard').innerHTML=h;
}

// ===== Cameras =====
function loadCameras(){ getJSON('/api/status').then(function(s){S.status=s;renderCameras();}).catch(function(){}); }
function emptyState(icon,title,desc,action,act){ return '<div class="card" style="margin-top:18px"><div class="empty"><div class="ic-circle ic-blue">'+ic(icon,28)+'</div><div style="font-size:18px;font-weight:700;color:var(--txt)">'+esc(title)+'</div><div style="margin:8px 0 18px">'+esc(desc)+'</div>'+(action?'<button class="btn primary" onclick="qaDo(\''+act+'\')">'+ic('plus',18)+esc(action)+'</button>':'')+'</div></div>'; }
function renderCameras(){
 var cams=(S.status&&S.status.cameras)||[];
 var h='<div class="row" style="justify-content:space-between"><div><h1 class="title">'+t('nav_cameras')+'</h1><p class="subtitle" style="margin:0">'+(cams.length?tf('cams_connected',cams.length):t('no_cams_yet'))+'</p></div>'+
   '<div class="row"><button class="btn primary" onclick="qaDo(\'addcam\')">'+ic('plus',18)+t('add_camera')+'</button></div></div>';
 if(!cams.length){ el('p-cameras').innerHTML=h+emptyState('cameras',t('no_cams_yet'),t('no_cams_desc'),t('add_camera'),'addcam'); return; }
 h+='<div class="grid cols-auto" style="margin-top:18px">'+cams.map(camCard).join('')+'</div>';
 el('p-cameras').innerHTML=h;
}
function camCard(c){
 var pill=!c.enabled?'<span class="pill gray">'+t('off')+'</span>':c.running?'<span class="pill green"><span class="dot"></span>'+t('live')+'</span>':'<span class="pill orange">'+t('starting')+'</span>';
 var thumb=(c.running&&c.stream_url&&!c.protected)?'<img src="'+esc(c.stream_url)+'" style="width:100%;height:100%;object-fit:cover" onerror="this.replaceWith(document.createTextNode(\'\'))">':ic('cameras',34);
 var q='';
 if(c.modes&&c.modes.length){ c.modes.forEach(function(m){var r=m.width+'x'+m.height;m.fps.forEach(function(f){var v=r+'@'+f;q+='<option value="'+v+'"'+((r===c.resolution&&f===c.fps)?' selected':'')+'>'+r+' · '+f+' fps</option>';});}); }
 var quality=c.modes&&c.modes.length?'<select onchange="camMode(this,\''+esc(c.id)+'\')">'+q+'</select>':'<div class="muted">'+esc(c.resolution)+' · '+c.fps+' fps</div>';
 var lock=c.protected?' <span class="pill gray" style="font-size:10px;padding:2px 7px">🔒 '+t('protected_w')+'</span>':'';
 return '<div class="card cam-card hov">'+
  '<div class="thumb">'+thumb+'<div class="live">'+pill+'</div></div>'+
  '<div class="row" style="justify-content:space-between;margin-top:14px;gap:10px"><div><div style="font-weight:700;font-size:15px">'+esc(camName(c))+lock+'</div><div class="muted" style="font-size:13px">'+(c.running?(c.resolution+' · '+c.fps+' fps'):t('camera_w'))+'</div></div>'+
   '<button class="toggle '+(c.enabled?'on':'')+'" onclick="camToggle(this,\''+esc(c.id)+'\','+(!c.enabled)+')"></button></div>'+
  '<label style="margin:14px 0 6px">'+t('quality')+'</label>'+quality+
  '<div class="row" style="margin-top:14px">'+(c.running&&c.stream_url?'<a class="btn primary sm" href="'+esc(c.stream_url)+'" target="_blank">'+ic('play',16)+t('preview')+'</a>':'<button class="btn sm" disabled>'+ic('play',16)+t('preview')+'</button>')+(c.stream_url?'<button class="btn sm" onclick="copyLink(\''+esc(c.stream_url)+'\')">'+ic('link',16)+t('copy_link')+'</button>':'')+'</div>'+
  (c.stream_user?'<div style="margin-top:12px;font-weight:600;font-size:14px;display:flex;align-items:center;gap:6px">🔒 '+t('protected_stream')+'</div>'+
   '<details class="expander" style="margin-top:2px"><summary>'+ic('chevron',16)+t('show_login')+'</summary>'+
     '<div class="kvs" style="margin-top:8px;grid-template-columns:auto 1fr">'+
       '<div class="k">'+t('username')+'</div><div class="v" style="text-align:left"><code>'+esc(c.stream_user)+'</code></div>'+
       '<div class="k">'+t('password')+'</div><div class="v" style="text-align:left"><code>'+esc(c.stream_password||'')+'</code></div>'+
     '</div>'+
     '<div class="row" style="margin-top:12px"><button class="btn sm" data-u="'+esc(c.stream_user)+'" data-p="'+esc(c.stream_password||'')+'" onclick="copyCreds(this)">📋 '+t('copy_creds')+'</button></div>'+
   '</details>':'')+
  '<details class="expander"><summary>'+ic('chevron',16)+t('advanced')+'</summary><div class="kvs">'+
   '<div class="k">'+t('device')+'</div><div class="v">'+esc(c.device_path)+'</div>'+
   '<div class="k">'+t('stream')+'</div><div class="v">'+t('video_stream')+(c.mjpeg?' ('+t('passthrough')+')':'')+'</div>'+
   '<div class="k">'+t('port')+'</div><div class="v">'+(c.port||'—')+'</div>'+
   '<div class="k">'+t('process')+'</div><div class="v">'+(c.pid||'—')+'</div>'+
   '<div class="k">'+t('usb_id')+'</div><div class="v">'+esc(c.id)+'</div></div>'+
   '<div style="margin-top:12px"><label style="margin:0 0 6px">'+t('stream_protection')+'</label>'+
   '<div class="row"><input class="su" placeholder="'+t('auth_user_ph')+'" value="'+esc(c.stream_user||'')+'" style="flex:1;min-width:90px"><input class="sp" type="password" placeholder="'+t('password')+'" style="flex:1;min-width:90px"><button class="btn sm" onclick="camAuth(this,\''+esc(c.id)+'\')">'+t('save')+'</button></div></div>'+
   '</details></div>';
}
function camAuth(btn,id){var c=btn.closest('.card');var u=c.querySelector('.su').value,p=c.querySelector('.sp').value;api('POST','/api/cameras/'+encodeURIComponent(id)+'/auth',{user:u||undefined,password:p||undefined}).then(function(){toast(t('auth_saved'));loadCameras();}).catch(function(){toast(t('action_failed'),1);});}
function copyText(text,msg){
 function done(){toast(msg);}
 if(navigator.clipboard&&navigator.clipboard.writeText){navigator.clipboard.writeText(text).then(done,function(){fbCopy(text,done);});}
 else fbCopy(text,done);
}
function fbCopy(text,done){var ta=document.createElement('textarea');ta.value=text;ta.style.position='fixed';ta.style.opacity='0';document.body.appendChild(ta);ta.focus();ta.select();try{document.execCommand('copy');done();}catch(e){toast(text);}document.body.removeChild(ta);}
function copyLink(url){copyText(url,t('link_copied'));}
function copyCreds(btn){copyText(btn.dataset.u+':'+btn.dataset.p,t('creds_copied'));}
function camMode(sel,id){var v=sel.value.split('@');api('POST','/api/cameras/'+encodeURIComponent(id)+'/mode',{resolution:v[0],fps:parseInt(v[1],10)}).then(function(){toast(t('quality_updated'));loadCameras();});}
function camToggle(btn,id,on){ btn.classList.toggle('on'); var card=btn.closest('.card'); var sel=card?card.querySelector('select'):null;
 var u='/api/cameras/'+encodeURIComponent(id),p;
 if(on){var b=sel?{resolution:sel.value.split('@')[0],fps:parseInt(sel.value.split('@')[1],10)}:null;p=(b?api('POST',u+'/mode',b):Promise.resolve()).then(function(){return api('POST',u+'/enable');});}
 else p=api('POST',u+'/disable');
 p.then(function(){toast(on?t('cam_on'):t('cam_turned_off'));loadCameras();}).catch(function(){toast(t('action_failed'),1);});
}

// ===== Network =====
function loadNetwork(){ getJSON('/api/network').then(function(n){S.network=n;renderNetwork();}).catch(function(){}); }
function cardOf(b){return b.closest('.card');}
function renderNetwork(){
 var n=S.network||{interfaces:[],profiles:[]}, ni=netInfo();
 var h='<h1 class="title">'+t('nav_network')+'</h1><p class="subtitle">'+t('network_sub')+'</p>';
 h+='<div class="card"><div class="row" style="gap:14px;align-items:flex-start"><div class="ic-circle ic-green">'+ic('wifi',22)+'</div><div style="flex:1"><div class="muted" style="font-weight:600">'+t('current_conn')+'</div>';
 if(ni.client) h+='<div class="statusbig">'+esc(ni.client.ssid||t('connected'))+'</div><div class="row" style="gap:16px"><span class="pill green"><span class="dot"></span>'+t('connected')+'</span><span class="muted">'+t('signal_exc')+'</span></div>';
 else if(ni.ap) h+='<div class="statusbig">'+esc(ni.ap.ssid||'Hotspot')+'</div><span class="pill orange">'+t('hotspot_mode')+'</span>';
 else h+='<div class="statusbig">'+t('not_connected')+'</div><span class="pill red">'+t('offline')+'</span>';
 h+='<details class="expander"><summary>'+ic('chevron',16)+t('advanced')+'</summary><div class="kvs">';
 n.interfaces.forEach(function(it){ h+='<div class="k">'+esc(it.name)+(it.primary?' ('+t('built_in')+')':'')+'</div><div class="v">'+esc(it.mode)+(it.ip?' · '+esc(it.ip):'')+'</div><div class="k">MAC</div><div class="v">'+esc(it.mac)+'</div>'; });
 h+='</div></details></div></div></div>';
 n.interfaces.forEach(function(it){ h+=netActionCard(it); });
 h+='<h2 class="sec">'+t('saved_networks')+'</h2><div class="card">';
 if(n.profiles&&n.profiles.length){ h+='<div class="row" style="margin-bottom:6px"><span class="muted">'+t('connect_using')+'</span><select id="pif" style="max-width:170px">'+n.interfaces.map(function(i){return '<option>'+esc(i.name)+'</option>';}).join('')+'</select></div>';
  h+=n.profiles.map(function(p){return '<div class="row" style="justify-content:space-between;padding:11px 0;border-bottom:1px solid var(--border)"><div class="row" style="gap:10px">'+ic('wifi',18)+'<b>'+esc(p)+'</b></div><div class="row"><button class="btn sm" onclick="profConnect(\''+esc(p)+'\')">'+t('connect')+'</button><button class="btn sm" onclick="profRemove(\''+esc(p)+'\')" style="color:var(--danger)">'+ic('trash',15)+'</button></div></div>';}).join('');
 } else h+='<div class="empty" style="padding:24px"><div class="muted">'+t('no_saved')+'</div></div>';
 h+='</div>';
 el('p-network').innerHTML=h;
}
function netActionCard(it){
 var ap=it.primary&&it.ap_capable;
 return '<div class="card" style="margin-top:18px"><div class="row" style="justify-content:space-between"><h2 style="margin:0;font-size:16px">'+(it.primary?t('wifi_setup'):t('adapter')+' '+esc(it.name))+'</h2>'+
  '<div class="row"><button class="btn sm scanbtn" onclick="doScan(this,\''+esc(it.name)+'\')">'+ic('search',16)+t('scan')+'</button>'+(ap?'<button class="btn sm" onclick="doHotspot(this,\''+esc(it.name)+'\')">'+ic('wifi',16)+t('start_hotspot')+'</button>':'')+'</div></div>'+
  '<div class="netlist"></div>'+
  '<label>'+t('ssid_label')+'</label><input class="ssid"><label>'+t('password')+'</label><input class="pass" type="password">'+
  '<details class="expander"><summary>'+ic('chevron',16)+t('adv_static')+'</summary>'+
   '<label>'+t('addressing')+'</label><select class="dhcp"><option value="1">'+t('dhcp_auto')+'</option><option value="0">'+t('static_ip')+'</option></select>'+
   '<label>'+t('static_ip_label')+'</label><input class="addr" placeholder="'+t('for_static')+'">'+
   '<label>'+t('save_as_opt')+'</label><input class="saveas"></details>'+
  '<div class="row" style="margin-top:16px"><button class="btn primary" onclick="doConnect(this,\''+esc(it.name)+'\')">'+t('connect')+'</button></div></div>';
}
function actP(promise){ return promise.then(function(r){return r.json().catch(function(){return{};}).then(function(d){return{ok:r.ok,d:d};});}).then(function(x){toast(x.ok?t('done'):((x.d&&x.d.error)||t('failed')),!x.ok);setTimeout(loadNetwork,1600);}).catch(function(){toast(t('conn_changed'),1);}); }
function doScan(btn,iface){ btn.disabled=true; var o=btn.innerHTML; btn.textContent=t('scanning');
 api('POST','/api/network/scan',{iface:iface}).then(function(r){return r.json();}).then(function(d){ btn.disabled=false; btn.innerHTML=o;
  var list=cardOf(btn).querySelector('.netlist'); list.innerHTML='';
  (d.networks||[]).forEach(function(nw){var row=document.createElement('div');row.innerHTML='<b>'+esc(nw.ssid)+'</b> <span class="muted">'+nw.signal_dbm+' dBm'+(nw.secured?' · 🔒':'')+'</span>';row.onclick=function(){cardOf(btn).querySelector('.ssid').value=nw.ssid;};list.appendChild(row);});
  if(!(d.networks||[]).length)list.innerHTML='<div class="muted" style="padding:10px 12px">'+t('no_networks')+'</div>';
 }).catch(function(){btn.disabled=false;btn.innerHTML=o;toast(t('scan_failed'),1);}); }
function doConnect(btn,iface){var c=cardOf(btn);var body={iface:iface,ssid:c.querySelector('.ssid').value,password:c.querySelector('.pass').value,dhcp:c.querySelector('.dhcp').value==='1'};var a=c.querySelector('.addr').value;if(!body.dhcp&&a)body.address=a;var s=c.querySelector('.saveas').value;if(s)body.save_as=s;toast(t('connecting'));actP(api('POST','/api/network/connect',body));}
function doHotspot(btn,iface){toast(t('starting_hotspot'));actP(api('POST','/api/network/hotspot',{iface:iface}));}
function profConnect(p){var pif=el('pif');actP(api('POST','/api/network/profile/connect',{iface:pif?pif.value:undefined,name:p}));}
function profRemove(p){actP(api('POST','/api/network/profile/remove',{name:p}));}

// ===== System =====
function loadSystem(){ getJSON('/api/system').then(function(s){S.system=s;renderSystem();}).catch(function(){}); }
function metricCard(color,icon,title,big,pct,cls){ return '<div class="card"><div class="row" style="gap:10px"><div class="ic-circle ic-'+color+'" style="width:38px;height:38px;border-radius:11px">'+ic(icon,18)+'</div><span class="muted" style="font-weight:600">'+esc(title)+'</span></div><div class="statusbig" style="font-size:18px;margin:12px 0 0">'+esc(big)+'</div><div class="progress '+cls+'"><i style="width:'+Math.max(2,Math.min(100,pct))+'%"></i></div></div>'; }
function renderSystem(){
 var s=S.system||{},now=Date.now();
 var cpu=Math.round(s.cpu_percent||0),memPct=s.mem_total?Math.round(s.mem_used/s.mem_total*100):0,diskPct=s.disk_total?Math.round(s.disk_used/s.disk_total*100):0,temp=s.temperature_c;
 var bad=(temp!=null&&temp>78)||cpu>92||diskPct>92, health=bad?{w:t('needs_attention'),c:'orange'}:{w:t('excellent'),c:'green'};
 var h='<h1 class="title">'+t('nav_system')+'</h1><p class="subtitle">'+esc(s.model||t('your_device'))+'</p>';
 h+='<div class="card"><div class="row" style="gap:14px"><div class="ic-circle ic-'+health.c+'">'+ic('check',22)+'</div><div><div class="muted" style="font-weight:600">'+t('device_health')+'</div><div class="statusbig" style="margin:2px 0 0">'+health.w+'</div></div></div></div>';
 h+='<div class="grid cols-4" style="margin-top:18px">'+
   metricCard('blue','chip',t('processor'),cpu+'%',cpu,cpu>85?'bars-bad':cpu>65?'bars-warn':'bars-good')+
   metricCard('purple','chip',t('memory'),fmtBytes(s.mem_used)+' / '+fmtBytes(s.mem_total),memPct,memPct>85?'bars-bad':memPct>65?'bars-warn':'bars-good')+
   metricCard('green','drive',t('storage'),fmtBytes(s.disk_used)+' / '+fmtBytes(s.disk_total),diskPct,diskPct>85?'bars-bad':diskPct>65?'bars-warn':'bars-good')+
   metricCard('orange','thermo',t('temperature'),(temp==null?'—':temp.toFixed(0)+' °C'),(temp==null?0:Math.min(100,temp)),(temp==null?'bars-good':temp>72?'bars-bad':temp>60?'bars-warn':'bars-good'))+
 '</div>';
 var bw;
 if(netPrev){var dt=(now-netPrev.t)/1000||1;bw='<table style="width:100%;font-size:14px"><tr style="color:var(--muted);text-align:left"><th style="padding:6px 0">'+t('interface')+'</th><th style="text-align:right">'+t('down')+'</th><th style="text-align:right">'+t('up')+'</th></tr>';(s.net||[]).forEach(function(nw){var p=netPrev.map[nw.iface];var rx=p?Math.max(0,(nw.rx_bytes-p.rx)/dt):0,tx=p?Math.max(0,(nw.tx_bytes-p.tx)/dt):0;bw+='<tr><td style="padding:6px 0">'+esc(nw.iface)+'</td><td style="text-align:right">'+fmtBytes(rx)+'/s</td><td style="text-align:right">'+fmtBytes(tx)+'/s</td></tr>';});bw+='</table>';}else bw='<div class="muted">'+t('measuring')+'</div>';
 var m={};(s.net||[]).forEach(function(nw){m[nw.iface]={rx:nw.rx_bytes,tx:nw.tx_bytes};});netPrev={t:now,map:m};
 h+='<h2 class="sec">'+t('network_activity')+'</h2><div class="card">'+bw+'</div>';
 h+='<details class="expander card" style="margin-top:18px" ontoggle="sysAdvOpen=this.open"'+(sysAdvOpen?' open':'')+'><summary>'+ic('chevron',16)+t('advanced')+'</summary><div class="kvs">'+
   '<div class="k">'+t('model')+'</div><div class="v">'+esc(s.model)+'</div><div class="k">'+t('firmware')+'</div><div class="v">camera-box '+esc(s.version)+'</div>'+
   '<div class="k">'+t('hostname')+'</div><div class="v">'+esc((S.status&&S.status.hostname)||'')+'</div><div class="k">'+t('local_time')+'</div><div class="v">'+esc(s.local_time)+'</div>'+
   '<div class="k">'+t('system_uptime')+'</div><div class="v">'+fmtUptime(s.uptime||0)+'</div><div class="k">'+t('cpu')+'</div><div class="v">'+cpu+'%</div>'+
   '<div class="k">'+t('memory')+'</div><div class="v">'+fmtBytes(s.mem_used)+' / '+fmtBytes(s.mem_total)+'</div><div class="k">'+t('disk')+'</div><div class="v">'+fmtBytes(s.disk_used)+' / '+fmtBytes(s.disk_total)+'</div>'+
  '</div></details>';
 el('p-system').innerHTML=h;
}

// ===== Logs =====
function logLevel(l){ if(/ ERROR/.test(l))return'ERROR'; if(/ WARN/.test(l))return'WARN'; if(/ DEBUG/.test(l))return'DEBUG'; if(/ TRACE/.test(l))return'TRACE'; return'INFO'; }
function buildLogs(){
 el('p-logs').innerHTML='<h1 class="title">'+t('nav_logs')+'</h1><p class="subtitle">'+t('logs_sub')+'</p>'+
  '<div class="card"><div class="row" style="margin-bottom:14px">'+
   '<div style="position:relative;flex:1;min-width:180px"><span style="position:absolute;left:12px;top:50%;transform:translateY(-50%);color:var(--muted)">'+ic('search',16)+'</span><input id="logq" placeholder="'+t('search_logs')+'" style="padding-left:38px" oninput="L.q=this.value;renderLogs()"></div>'+
   '<select id="loglvl" style="max-width:160px" onchange="L.level=this.value;renderLogs()"><option value="">'+t('all_levels')+'</option><option>INFO</option><option>WARN</option><option>ERROR</option><option>DEBUG</option></select>'+
   '<button class="btn sm" id="autobtn" onclick="L.auto=!L.auto;el(\'autobtn\').innerHTML=icRefresh()">'+icRefresh()+'</button>'+
   '<button class="btn sm" onclick="downloadLogs()">'+ic('download',16)+t('download')+'</button></div>'+
  '<div class="logbox" id="logbox"></div></div>';
 L.built=true;
}
function icRefresh(){return ic('refresh',16)+t('autoscroll')+': '+(L.auto?t('onword'):t('offword'));}
function loadLogs(){ if(!L.built)buildLogs(); getJSON('/api/logs').then(function(d){L.lines=d.lines||[];renderLogs();}).catch(function(){}); }
function renderLogs(){
 var box=el('logbox'); if(!box)return; var atBottom=box.scrollTop+box.clientHeight>=box.scrollHeight-30;
 var q=L.q.toLowerCase();
 var html=L.lines.filter(function(l){ if(q&&l.toLowerCase().indexOf(q)<0)return false; if(L.level&&logLevel(l)!==L.level)return false; return true; })
   .map(function(l){ return '<span class="logline ll-'+logLevel(l)+'">'+esc(l)+'</span>'; }).join('');
 box.innerHTML=html||'<div class="muted" style="padding:12px">'+t('no_log_lines')+'</div>';
 if(L.auto&&atBottom) box.scrollTop=box.scrollHeight;
}
function downloadLogs(){var b=new Blob([L.lines.join('\n')],{type:'text/plain'});var a=document.createElement('a');a.href=URL.createObjectURL(b);a.download='camera-box-logs.txt';a.click();}

// ===== Settings =====
function loadSettings(){ Promise.all([getJSON('/api/status'),getJSON('/api/system')]).then(function(a){S.status=a[0];S.system=a[1];renderSettings();}).catch(function(){}); }
function renderSettings(){
 var st=S.status||{},sy=S.system||{};
 var h='<h1 class="title">'+t('nav_settings')+'</h1><p class="subtitle">'+t('settings_sub')+'</p><div class="grid cols-2">';
 h+='<div class="card"><div class="row" style="gap:12px;margin-bottom:4px"><div class="ic-circle ic-blue" style="width:38px;height:38px;border-radius:11px">'+ic('settings',18)+'</div><h2 style="margin:0;font-size:16px">'+t('general')+'</h2></div>'+
  '<div class="muted" style="font-size:13px">'+tf('reachable_as','<b>'+esc(st.hostname||'')+'</b>')+'</div>'+
  '<label>'+t('device_name')+'</label><input id="hn" value="'+esc(st.hostname||'')+'"><div class="row" style="margin-top:14px"><button class="btn primary" onclick="saveHostname()">'+t('save')+'</button></div></div>';
 h+='<div class="card"><div class="row" style="gap:12px;margin-bottom:4px"><div class="ic-circle ic-green" style="width:38px;height:38px;border-radius:11px">'+ic('user',18)+'</div><h2 style="margin:0;font-size:16px">'+t('security')+'</h2></div>'+
  '<label>'+t('username')+'</label><input id="au" placeholder="admin"><label>'+t('new_password')+'</label><input id="ap" type="password"><div class="row" style="margin-top:14px"><button class="btn primary" onclick="savePassword()">'+t('update_password')+'</button></div></div>';
 h+='<div class="card"><div class="row" style="gap:12px;margin-bottom:4px"><div class="ic-circle ic-purple" style="width:38px;height:38px;border-radius:11px">'+ic('help',18)+'</div><h2 style="margin:0;font-size:16px">'+t('about')+'</h2></div>'+
  '<div class="kvs"><div class="k">'+t('device')+'</div><div class="v">'+esc(sy.model||'')+'</div><div class="k">'+t('firmware')+'</div><div class="v">camera-box '+esc(sy.version||'')+'</div><div class="k">'+t('uptime')+'</div><div class="v">'+fmtUptime(sy.uptime||0)+'</div></div></div>';
 h+='</div>';
 el('p-settings').innerHTML=h;
}
function saveHostname(){api('POST','/api/hostname',{name:el('hn').value}).then(function(r){return r.json().then(function(d){return{ok:r.ok,d:d};});}).then(function(x){toast(x.ok?t('name_saved'):((x.d&&x.d.error)||t('failed')),!x.ok);});}
function savePassword(){var u=el('au').value,p=el('ap').value;api('POST','/api/account',{username:u||undefined,password:p}).then(function(r){return r.json().then(function(d){return{ok:r.ok,d:d};});}).then(function(x){toast(x.ok?t('password_updated'):((x.d&&x.d.error)||t('failed')),!x.ok);if(x.ok)el('ap').value='';});}

// ===== poll =====
var tickN=0;
function poll(){ tickN++;
 if(page==='system') loadSystem();              // refresh System every 1s
 if(tickN%5===0){                                // everything else every 5s
   getJSON('/api/status').then(function(s){ el('hostlabel').textContent=s.hostname||'camera-box'; }).catch(function(){});
   if(page==='dashboard')loadDashboard(); else if(page==='logs')loadLogs();
 }
}
getJSON('/api/status').then(function(s){ el('hostlabel').textContent=s.hostname||'camera-box'; }).catch(function(){});
go('dashboard'); setInterval(poll,1000);
</script>
</body>
</html>
"#;
