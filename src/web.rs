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
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::camera::{self, AppState, CameraMode, ControlError};
use crate::net;
use crate::update;

/// Build the complete application router, with Basic Auth if configured.
pub fn router(state: Arc<AppState>) -> Router {
    let routes = Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .route("/api/cameras/:id/enable", post(enable_camera))
        .route("/api/cameras/:id/disable", post(disable_camera))
        .route("/api/cameras/:id/mode", post(set_camera_mode))
        .route("/api/network", get(network_status))
        .route("/api/network/scan", post(network_scan))
        .route("/api/network/hotspot", post(network_hotspot))
        .route("/api/network/connect", post(network_connect))
        .route("/api/network/profile/add", post(profile_add))
        .route("/api/network/profile/remove", post(profile_remove))
        .route("/api/network/profile/connect", post(profile_connect))
        .route("/api/system", get(system_info))
        .route("/api/logs", get(logs))
        .merge(update::router())
        .with_state(state.clone());

    match state.config.basic_auth() {
        Some((user, pass)) => {
            let expected = Arc::new(format!("Basic {}", STANDARD.encode(format!("{user}:{pass}"))));
            routes.layer(middleware::from_fn(move |req: Request, next: Next| {
                let expected = expected.clone();
                async move { basic_auth(&expected, req, next).await }
            }))
        }
        None => routes,
    }
}

/// HTTP Basic Auth gate. Compares the full `Authorization` header value.
async fn basic_auth(expected: &str, req: Request, next: Next) -> Response {
    let provided = req
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if provided == Some(expected) {
        next.run(req).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Basic realm=\"camera-box\"")],
            "401 Unauthorized",
        )
            .into_response()
    }
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
        });
    }

    StatusResponse {
        hostname: state.hostname.clone(),
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

// ---------------------------------------------------------------------------
// Static page: shell + CSS + the JS that renders/updates from /api/status.
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>camera-box</title>
<style>
body{font-family:system-ui,Segoe UI,sans-serif;margin:0;background:#0f1115;color:#e6e6e6}
header{background:#171a21;padding:16px 20px;border-bottom:1px solid #2a2f3a}
h1{margin:0;font-size:18px;letter-spacing:.5px}
.meta{color:#9aa4b2;font-size:13px;margin-top:6px}
main{padding:20px;display:grid;gap:16px;grid-template-columns:repeat(auto-fill,minmax(320px,1fr))}
.card{background:#171a21;border:1px solid #2a2f3a;border-radius:8px;padding:14px}
.card h2{margin:0 0 6px;font-size:15px}
.kv{color:#9aa4b2;font-size:13px;margin:3px 0}
code{background:#1c2029;padding:2px 6px;border-radius:4px;color:#cbd5e1}
a{color:#5cb3ff;text-decoration:none}a:hover{text-decoration:underline}
.badge{font-size:11px;font-weight:700;padding:2px 8px;border-radius:10px;margin-left:6px}
.on{background:#16331f;color:#46d369}.off{background:#3a1c1c;color:#ffb86b}.idle{background:#2a2f3a;color:#9aa4b2}
.ctrl{margin-top:10px;display:flex;gap:8px;flex-wrap:wrap;align-items:center}
select,button{background:#1c2029;color:#e6e6e6;border:1px solid #2a2f3a;border-radius:6px;padding:6px 8px;font-size:13px}
button{cursor:pointer}button:hover{border-color:#5cb3ff}
.empty{color:#9aa4b2;padding:24px}
footer{padding:12px 20px;color:#6b7280;font-size:12px;border-top:1px solid #2a2f3a}
</style>
</head>
<body>
<header>
<h1>camera-box</h1>
<div class="meta" id="meta">Loading…</div>
</header>
<main id="cams"></main>
<p class="empty" id="empty" style="display:none">No USB cameras connected.</p>
<footer>Live status from <a href="/api/status">/api/status</a> (updates every 5s, never interrupts your selection) &middot; <a href="/api/version">/api/version</a></footer>
<script>
var cards = {}; // id -> card element

function fmtUptime(s){
  var d=Math.floor(s/86400), h=Math.floor((s%86400)/3600), m=Math.floor((s%3600)/60), sec=s%60;
  if(d>0) return d+'d '+h+'h '+m+'m';
  if(h>0) return h+'h '+m+'m '+sec+'s';
  return m+'m '+sec+'s';
}
function esc(t){ var d=document.createElement('div'); d.textContent=(t==null?'':t); return d.innerHTML; }

function rawPost(path, body){
  return fetch(path, {
    method:'POST',
    headers: body ? {'Content-Type':'application/json'} : {},
    body: body ? JSON.stringify(body) : undefined
  });
}
function modeBody(card){
  var sel = card.querySelector('select.mode');
  if(!sel) return null;
  var v = sel.value.split('@');
  return {resolution: v[0], fps: parseInt(v[1], 10)};
}
function setEnabled(card, id, on){
  var url = '/api/cameras/' + encodeURIComponent(id);
  var p;
  if(on){
    // Turn on with the chosen resolution/fps: apply the mode, then enable.
    var body = modeBody(card);
    p = (body ? rawPost(url+'/mode', body) : Promise.resolve())
          .then(function(){ return rawPost(url+'/enable'); });
  } else {
    p = rawPost(url+'/disable');
  }
  p.then(tick);
}
function applyMode(card, id){
  var body = modeBody(card);
  if(!body) return;
  rawPost('/api/cameras/' + encodeURIComponent(id) + '/mode', body).then(tick);
}

function createCard(c){
  var card = document.createElement('div'); card.className='card';
  var h = document.createElement('h2');
  h.innerHTML = '<span class="name"></span><span class="badge"></span>';
  card.appendChild(h);
  var kp = document.createElement('div'); kp.className='kv'; kp.innerHTML='Device: <code class="path"></code>'; card.appendChild(kp);
  var ks = document.createElement('div'); ks.className='kv'; ks.innerHTML='Stream: <span class="streamval"></span>'; card.appendChild(ks);
  var kf = document.createElement('div'); kf.className='kv'; kf.innerHTML='Format: <span class="fmt"></span> &middot; PID: <span class="pid"></span>'; card.appendChild(kf);
  var ctrl = document.createElement('div'); ctrl.className='ctrl';
  if(c.modes && c.modes.length){
    var sel = document.createElement('select'); sel.className='mode';
    c.modes.forEach(function(m){
      var res = m.width+'x'+m.height;
      m.fps.forEach(function(f){ sel.add(new Option(res+' @ '+f+' fps', res+'@'+f)); });
    });
    var ap = document.createElement('button'); ap.textContent='Apply';
    ap.onclick = function(){ applyMode(card, c.id); };
    ctrl.appendChild(sel); ctrl.appendChild(ap);
  } else {
    var fx = document.createElement('span'); fx.className='kv modefixed'; ctrl.appendChild(fx);
  }
  var tg = document.createElement('button'); tg.className='toggle'; ctrl.appendChild(tg);
  card.appendChild(ctrl);
  return card;
}

function updateCard(card, c){
  card.querySelector('.name').textContent = c.name || 'USB Camera';
  var bd = card.querySelector('.badge');
  if(!c.enabled){ bd.textContent='disabled'; bd.className='badge idle'; }
  else if(c.running){ bd.textContent='streaming'; bd.className='badge on'; }
  else { bd.textContent='starting…'; bd.className='badge off'; }
  card.querySelector('.path').textContent = c.device_path;
  var sv = card.querySelector('.streamval');
  if(c.stream_url){ sv.innerHTML=''; var a=document.createElement('a'); a.href=c.stream_url; a.textContent=c.stream_url; sv.appendChild(a); }
  else { sv.textContent='—'; }
  card.querySelector('.fmt').textContent = c.mjpeg ? 'MJPEG (passthrough)' : 'raw → JPEG';
  card.querySelector('.pid').textContent = (c.pid==null ? '—' : c.pid);
  var sel = card.querySelector('select.mode');
  if(sel && document.activeElement !== sel){ sel.value = c.resolution+'@'+c.fps; }
  var fx = card.querySelector('.modefixed');
  if(fx){ fx.textContent = 'Mode: '+c.resolution+' @ '+c.fps+' fps (fixed)'; }
  var tg = card.querySelector('.toggle');
  tg.textContent = c.enabled ? 'Disable' : 'Enable';
  tg.onclick = function(){ setEnabled(card, c.id, !c.enabled); };
}

function render(list){
  var main = document.getElementById('cams');
  document.getElementById('empty').style.display = list.length ? 'none' : '';
  var seen = {};
  list.forEach(function(c){
    seen[c.id] = true;
    var card = cards[c.id];
    if(!card){ card = createCard(c); cards[c.id] = card; main.appendChild(card); }
    updateCard(card, c);
  });
  Object.keys(cards).forEach(function(id){
    if(!seen[id]){ cards[id].remove(); delete cards[id]; }
  });
}

function tick(){
  return fetch('/api/status').then(function(r){ return r.json(); }).then(function(s){
    document.getElementById('meta').innerHTML =
      'Device <b>'+esc(s.hostname)+'</b> &middot; IP <b>'+esc(s.ip_address)+'</b> &middot; Uptime '+
      fmtUptime(s.uptime)+' &middot; '+s.cameras.length+' USB camera(s)';
    render(s.cameras);
  }).catch(function(){});
}

tick();
setInterval(tick, 5000);
</script>
</body>
</html>
"#;
