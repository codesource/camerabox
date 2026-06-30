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
        .route("/api/login", post(login))
        .route("/api/logout", post(logout))
        .route("/api/account", post(set_account))
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
    if path == "/login" || path == "/api/login" {
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
<title>camera-box — sign in</title>
<style>
body{font-family:system-ui,Segoe UI,sans-serif;margin:0;height:100vh;display:flex;align-items:center;justify-content:center;background:#0f1115;color:#e6e6e6}
.box{background:#171a21;border:1px solid #2a2f3a;border-radius:10px;padding:28px;width:280px}
h1{margin:0 0 4px;font-size:18px}.sub{color:#9aa4b2;font-size:13px;margin-bottom:16px}
label{display:block;font-size:12px;color:#9aa4b2;margin:10px 0 4px}
input{width:100%;box-sizing:border-box;background:#1c2029;color:#e6e6e6;border:1px solid #2a2f3a;border-radius:6px;padding:9px 10px;font-size:14px}
button{margin-top:16px;width:100%;background:#1f6feb;color:#fff;border:0;border-radius:6px;padding:10px;font-size:14px;cursor:pointer}
button:hover{background:#388bfd}
.err{color:#ff7a7a;font-size:13px;margin-top:10px;min-height:16px}
</style></head>
<body>
<form class="box" onsubmit="return signin(event)">
<h1>camera-box</h1><div class="sub">Sign in to manage your device</div>
<label>Username</label><input id="u" autocomplete="username" autofocus>
<label>Password</label><input id="p" type="password" autocomplete="current-password">
<button type="submit">Sign in</button>
<div class="err" id="err"></div>
</form>
<script>
function signin(e){
  e.preventDefault();
  fetch('/api/login',{method:'POST',headers:{'Content-Type':'application/json'},
    body:JSON.stringify({username:document.getElementById('u').value,password:document.getElementById('p').value})})
   .then(function(r){ if(r.ok){ location.href='/'; } else { document.getElementById('err').textContent='Invalid username or password'; } })
   .catch(function(){ document.getElementById('err').textContent='Network error'; });
  return false;
}
</script>
</body></html>
"#;

const INDEX_HTML: &str = r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>camera-box</title>
<style>
:root{--bg:#0f1115;--panel:#171a21;--line:#2a2f3a;--muted:#9aa4b2;--txt:#e6e6e6;--accent:#1f6feb}
*{box-sizing:border-box}
body{font-family:system-ui,Segoe UI,sans-serif;margin:0;background:var(--bg);color:var(--txt)}
header{background:var(--panel);border-bottom:1px solid var(--line);display:flex;align-items:center;gap:12px;padding:10px 16px;flex-wrap:wrap}
.brand{font-weight:700;font-size:16px;letter-spacing:.5px}
.host{color:var(--muted);font-size:13px}
nav{display:flex;gap:4px;flex-wrap:wrap}
nav button{background:transparent;border:0;color:var(--muted);padding:8px 12px;border-radius:6px;font-size:14px;cursor:pointer}
nav button:hover{color:var(--txt);background:#1c2029}
nav button.active{color:#fff;background:var(--accent)}
.spacer{flex:1}
.ghost{background:transparent;border:1px solid var(--line);color:var(--muted);padding:7px 12px;border-radius:6px;cursor:pointer;font-size:13px}
.ghost:hover{color:var(--txt);border-color:var(--accent)}
main{padding:18px}
.tab.hidden{display:none}
.grid{display:grid;gap:14px;grid-template-columns:repeat(auto-fill,minmax(300px,1fr))}
.card{background:var(--panel);border:1px solid var(--line);border-radius:8px;padding:14px}
.card h2{margin:0 0 8px;font-size:15px;display:flex;align-items:center}
.kv{color:var(--muted);font-size:13px;margin:3px 0}
.kv b{color:var(--txt);font-weight:600}
code{background:#1c2029;padding:2px 6px;border-radius:4px;color:#cbd5e1}
a{color:#5cb3ff;text-decoration:none}a:hover{text-decoration:underline}
.badge{font-size:11px;font-weight:700;padding:2px 8px;border-radius:10px;margin-left:6px}
.on{background:#16331f;color:#46d369}.off{background:#3a1c1c;color:#ffb86b}.idle{background:#2a2f3a;color:#9aa4b2}
.ctrl{margin-top:10px;display:flex;gap:8px;flex-wrap:wrap;align-items:center}
select,button,input{background:#1c2029;color:var(--txt);border:1px solid var(--line);border-radius:6px;padding:7px 9px;font-size:13px}
button{cursor:pointer}button:hover{border-color:var(--accent)}
button.primary{background:var(--accent);border-color:var(--accent);color:#fff}
label{font-size:12px;color:var(--muted);display:block;margin:8px 0 3px}
.row{display:flex;gap:8px;flex-wrap:wrap;align-items:end}
.metric{font-size:22px;font-weight:700;margin:2px 0}
.bar{height:6px;border-radius:4px;background:#1c2029;overflow:hidden;margin-top:6px}
.bar>i{display:block;height:100%;background:var(--accent)}
table{border-collapse:collapse;width:100%;font-size:13px}
th,td{text-align:left;padding:6px 8px;border-bottom:1px solid var(--line)}
th{color:var(--muted)}
pre#logbox{background:#0b0d11;border:1px solid var(--line);border-radius:8px;padding:12px;font-size:12px;line-height:1.5;max-height:72vh;overflow:auto;white-space:pre-wrap}
.empty{color:var(--muted);padding:24px}
.muted{color:var(--muted);font-size:12px}
.netlist{margin-top:8px;max-height:150px;overflow:auto;border:1px solid var(--line);border-radius:6px}
.netlist div{padding:6px 9px;cursor:pointer;font-size:13px;border-bottom:1px solid var(--line)}
.netlist div:hover{background:#1c2029}
.msg{font-size:12px;min-height:15px;margin-top:6px}.msg.ok{color:#46d369}.msg.err{color:#ff7a7a}
</style>
</head>
<body>
<header>
<div class="brand">camera-box</div>
<div class="host" id="host">…</div>
<nav id="nav">
<button data-tab="cameras" class="active">Cameras</button>
<button data-tab="network">Network</button>
<button data-tab="system">System</button>
<button data-tab="logs">Logs</button>
<button data-tab="settings">Settings</button>
</nav>
<div class="spacer"></div>
<button class="ghost" onclick="logout()">Log out</button>
</header>
<main>
<section class="tab" id="tab-cameras">
<div class="grid" id="cams"></div>
<p class="empty" id="cams-empty" style="display:none">No USB cameras connected.</p>
</section>
<section class="tab hidden" id="tab-network"><div class="grid" id="netgrid"></div></section>
<section class="tab hidden" id="tab-system"><div class="grid" id="sysgrid"></div></section>
<section class="tab hidden" id="tab-logs"><pre id="logbox">…</pre></section>
<section class="tab hidden" id="tab-settings"><div class="grid" id="setgrid"></div></section>
</main>
<script>
var active='cameras', cards={}, netPrev=null;

function esc(t){ var d=document.createElement('div'); d.textContent=(t==null?'':t); return d.innerHTML; }
function fmtUptime(s){ var d=Math.floor(s/86400),h=Math.floor((s%86400)/3600),m=Math.floor((s%3600)/60),x=Math.floor(s%60);
  return d>0?d+'d '+h+'h '+m+'m':h>0?h+'h '+m+'m '+x+'s':m+'m '+x+'s'; }
function fmtBytes(n){ var u=['B','KB','MB','GB','TB'],i=0; n=n||0; while(n>=1024&&i<u.length-1){n/=1024;i++;} return n.toFixed(i?1:0)+' '+u[i]; }
function api(method,path,body){ return fetch(path,{method:method,headers:body?{'Content-Type':'application/json'}:{},body:body?JSON.stringify(body):undefined}); }
function getJSON(path){ return api('GET',path).then(function(r){return r.json();}); }
function logout(){ api('POST','/api/logout').then(function(){ location.href='/login'; }); }
function card(title,inner){ return '<div class="card"><h2>'+esc(title)+'</h2>'+inner+'</div>'; }
function bar(pct){ pct=Math.max(0,Math.min(100,pct)); return '<div class="bar"><i style="width:'+pct+'%"></i></div>'; }

document.querySelectorAll('#nav button').forEach(function(b){
  b.onclick=function(){
    active=b.dataset.tab;
    document.querySelectorAll('#nav button').forEach(function(x){ x.classList.toggle('active', x===b); });
    document.querySelectorAll('.tab').forEach(function(s){ s.classList.add('hidden'); });
    document.getElementById('tab-'+active).classList.remove('hidden');
    if(active==='network') loadNetwork();
    else if(active==='settings') loadSettings();
    else poll();
  };
});

// ---------------- Cameras ----------------
function modeBody(c){ var sel=c.querySelector('select.mode'); if(!sel) return null; var v=sel.value.split('@'); return {resolution:v[0],fps:parseInt(v[1],10)}; }
function camEnable(c,id,on){
  var u='/api/cameras/'+encodeURIComponent(id), p;
  if(on){ var b=modeBody(c); p=(b?api('POST',u+'/mode',b):Promise.resolve()).then(function(){return api('POST',u+'/enable');}); }
  else p=api('POST',u+'/disable');
  p.then(poll);
}
function camApply(c,id){ var b=modeBody(c); if(b) api('POST','/api/cameras/'+encodeURIComponent(id)+'/mode',b).then(poll); }
function createCard(c){
  var el=document.createElement('div'); el.className='card';
  el.innerHTML='<h2><span class="name"></span><span class="badge"></span></h2>'+
    '<div class="kv">Device: <code class="path"></code></div>'+
    '<div class="kv">Stream: <span class="streamval"></span></div>'+
    '<div class="kv">Format: <span class="fmt"></span> · PID: <span class="pid"></span></div>'+
    '<div class="ctrl"></div>';
  var ctrl=el.querySelector('.ctrl');
  if(c.modes&&c.modes.length){
    var sel=document.createElement('select'); sel.className='mode';
    c.modes.forEach(function(m){ var r=m.width+'x'+m.height; m.fps.forEach(function(f){ sel.add(new Option(r+' @ '+f+' fps', r+'@'+f)); }); });
    var ap=document.createElement('button'); ap.textContent='Apply'; ap.onclick=function(){ camApply(el,c.id); };
    ctrl.appendChild(sel); ctrl.appendChild(ap);
  } else { var fx=document.createElement('span'); fx.className='kv modefixed'; ctrl.appendChild(fx); }
  var tg=document.createElement('button'); tg.className='toggle'; ctrl.appendChild(tg);
  return el;
}
function updateCard(el,c){
  el.querySelector('.name').textContent=c.name||'USB Camera';
  var bd=el.querySelector('.badge');
  if(!c.enabled){ bd.textContent='disabled'; bd.className='badge idle'; }
  else if(c.running){ bd.textContent='streaming'; bd.className='badge on'; }
  else { bd.textContent='starting…'; bd.className='badge off'; }
  el.querySelector('.path').textContent=c.device_path;
  var sv=el.querySelector('.streamval');
  if(c.stream_url){ sv.innerHTML=''; var a=document.createElement('a'); a.href=c.stream_url; a.textContent=c.stream_url; sv.appendChild(a); }
  else sv.textContent='—';
  el.querySelector('.fmt').textContent=c.mjpeg?'MJPEG (passthrough)':'raw → JPEG';
  el.querySelector('.pid').textContent=(c.pid==null?'—':c.pid);
  var sel=el.querySelector('select.mode');
  if(sel&&document.activeElement!==sel) sel.value=c.resolution+'@'+c.fps;
  var fx=el.querySelector('.modefixed'); if(fx) fx.textContent='Mode: '+c.resolution+' @ '+c.fps+' fps (fixed)';
  var tg=el.querySelector('.toggle'); tg.textContent=c.enabled?'Disable':'Enable'; tg.onclick=function(){ camEnable(el,c.id,!c.enabled); };
}
function renderCameras(list){
  var g=document.getElementById('cams'); document.getElementById('cams-empty').style.display=list.length?'none':'';
  var seen={};
  list.forEach(function(c){ seen[c.id]=true; var el=cards[c.id]; if(!el){ el=createCard(c); cards[c.id]=el; g.appendChild(el); } updateCard(el,c); });
  Object.keys(cards).forEach(function(id){ if(!seen[id]){ cards[id].remove(); delete cards[id]; } });
}

// ---------------- System ----------------
function loadSystem(){
  getJSON('/api/system').then(function(s){
    var now=Date.now(), bw;
    if(netPrev){ var dt=(now-netPrev.t)/1000||1;
      bw='<table><tr><th>Interface</th><th>&darr; down</th><th>&uarr; up</th></tr>';
      s.net.forEach(function(n){ var p=netPrev.map[n.iface];
        var rx=p?Math.max(0,(n.rx_bytes-p.rx)/dt):0, tx=p?Math.max(0,(n.tx_bytes-p.tx)/dt):0;
        bw+='<tr><td>'+esc(n.iface)+'</td><td>'+fmtBytes(rx)+'/s</td><td>'+fmtBytes(tx)+'/s</td></tr>'; });
      bw+='</table>';
    } else bw='<div class="muted">measuring…</div>';
    var map={}; s.net.forEach(function(n){ map[n.iface]={rx:n.rx_bytes,tx:n.tx_bytes}; }); netPrev={t:now,map:map};
    var memPct=s.mem_total?Math.round(s.mem_used/s.mem_total*100):0;
    var diskPct=s.disk_total?Math.round(s.disk_used/s.disk_total*100):0;
    document.getElementById('sysgrid').innerHTML=
      card('Device','<div class="kv"><b>'+esc(s.model)+'</b></div><div class="kv">camera-box v'+esc(s.version)+'</div><div class="kv">'+esc(s.local_time)+'</div>')+
      card('Uptime','<div class="metric">'+fmtUptime(s.uptime)+'</div>')+
      card('CPU','<div class="metric">'+s.cpu_percent.toFixed(0)+'%</div>'+bar(s.cpu_percent))+
      card('Memory','<div class="metric">'+fmtBytes(s.mem_used)+' / '+fmtBytes(s.mem_total)+'</div>'+bar(memPct))+
      card('Disk','<div class="metric">'+fmtBytes(s.disk_used)+' / '+fmtBytes(s.disk_total)+'</div>'+bar(diskPct))+
      card('Temperature','<div class="metric">'+(s.temperature_c==null?'—':s.temperature_c.toFixed(1)+' °C')+'</div>')+
      card('Bandwidth',bw);
  }).catch(function(){});
}

// ---------------- Logs ----------------
function loadLogs(){
  getJSON('/api/logs').then(function(d){
    var box=document.getElementById('logbox');
    var atBottom=box.scrollTop+box.clientHeight>=box.scrollHeight-20;
    box.textContent=(d.lines||[]).join('\n');
    if(atBottom) box.scrollTop=box.scrollHeight;
  }).catch(function(){});
}

// ---------------- Network ----------------
function loadNetwork(){
  getJSON('/api/network').then(function(s){
    var g=document.getElementById('netgrid'); g.innerHTML='';
    s.interfaces.forEach(function(it){ g.appendChild(ifaceCard(it)); });
    g.appendChild(profilesCard(s));
  }).catch(function(){});
}
function act(el, promise){
  var msg=el.querySelector('.msg'); if(msg){ msg.className='msg'; msg.textContent='Working…'; }
  promise.then(function(r){ return r.json().catch(function(){return{};}).then(function(d){ return {ok:r.ok,d:d}; }); })
    .then(function(x){ if(msg){ msg.className='msg '+(x.ok?'ok':'err'); msg.textContent=x.ok?'Done.':((x.d&&x.d.error)||'Failed'); } setTimeout(loadNetwork,1500); })
    .catch(function(){ if(msg){ msg.className='msg err'; msg.textContent='Network error (interface may have changed)'; } });
}
function ifaceCard(it){
  var el=document.createElement('div'); el.className='card';
  var badge=it.mode==='ap'?'<span class="badge on">hotspot</span>':it.mode==='client'?'<span class="badge idle">client</span>':'<span class="badge off">'+esc(it.mode)+'</span>';
  el.innerHTML='<h2>'+esc(it.name)+(it.primary?' <span class="muted">(built-in)</span>':'')+badge+'</h2>'+
    '<div class="kv">SSID: <b>'+esc(it.ssid||'—')+'</b></div>'+
    '<div class="kv">IP: <b>'+esc(it.ip||'—')+'</b></div>'+
    '<div class="kv">MAC: <code>'+esc(it.mac)+'</code></div>'+
    '<div class="ctrl"></div><div class="netlist"></div>'+
    '<label>SSID</label><input class="ssid">'+
    '<label>Password</label><input class="pass" type="password">'+
    '<div class="row"><div><label>Addressing</label><select class="dhcp"><option value="1">DHCP</option><option value="0">Static</option></select></div>'+
    '<div style="flex:1"><label>Static IP (a.b.c.d/nn)</label><input class="addr" placeholder="for static only"></div></div>'+
    '<div class="row"><div style="flex:1"><label>Save as profile (optional)</label><input class="saveas"></div>'+
    '<button class="primary conn">Connect</button></div><div class="msg"></div>';
  var ctrl=el.querySelector('.ctrl');
  if(it.primary&&it.ap_capable){ var hs=document.createElement('button'); hs.textContent='Start hotspot';
    hs.onclick=function(){ act(el, api('POST','/api/network/hotspot',{iface:it.name})); }; ctrl.appendChild(hs); }
  var scan=document.createElement('button'); scan.textContent='Scan';
  scan.onclick=function(){ scan.textContent='Scanning…';
    api('POST','/api/network/scan',{iface:it.name}).then(function(r){return r.json();}).then(function(d){ scan.textContent='Scan';
      var list=el.querySelector('.netlist'); list.innerHTML='';
      (d.networks||[]).forEach(function(n){ var row=document.createElement('div'); row.textContent=n.ssid+'  ('+n.signal_dbm+' dBm)'+(n.secured?' 🔒':'');
        row.onclick=function(){ el.querySelector('.ssid').value=n.ssid; }; list.appendChild(row); });
      if(!(d.networks||[]).length) list.innerHTML='<div class="muted" style="padding:6px 9px">no networks found</div>';
    }).catch(function(){ scan.textContent='Scan'; }); };
  ctrl.appendChild(scan);
  el.querySelector('.conn').onclick=function(){
    var body={iface:it.name, ssid:el.querySelector('.ssid').value, password:el.querySelector('.pass').value, dhcp:el.querySelector('.dhcp').value==='1'};
    var addr=el.querySelector('.addr').value; if(!body.dhcp&&addr) body.address=addr;
    var sa=el.querySelector('.saveas').value; if(sa) body.save_as=sa;
    act(el, api('POST','/api/network/connect',body));
  };
  return el;
}
function profilesCard(s){
  var el=document.createElement('div'); el.className='card';
  var opts=s.interfaces.map(function(i){ return '<option>'+esc(i.name)+'</option>'; }).join('');
  var rows=(s.profiles||[]).map(function(p){
    return '<tr><td>'+esc(p)+'</td><td><button data-p="'+esc(p)+'" class="pc">Connect</button> <button data-p="'+esc(p)+'" class="pr">Remove</button></td></tr>'; }).join('')
    || '<tr><td colspan="2" class="muted">No saved networks</td></tr>';
  el.innerHTML='<h2>Saved networks</h2><div class="kv">Connect using <select class="pif">'+opts+'</select></div>'+
    '<table>'+rows+'</table>'+
    '<label>Add a network</label><div class="row"><input class="pn" placeholder="name" style="flex:1">'+
    '<input class="ps" placeholder="ssid" style="flex:1"><input class="pp" type="password" placeholder="password" style="flex:1">'+
    '<button class="primary pa">Add</button></div><div class="msg"></div>';
  el.querySelectorAll('.pc').forEach(function(b){ b.onclick=function(){ act(el, api('POST','/api/network/profile/connect',{iface:el.querySelector('.pif').value,name:b.dataset.p})); }; });
  el.querySelectorAll('.pr').forEach(function(b){ b.onclick=function(){ act(el, api('POST','/api/network/profile/remove',{name:b.dataset.p})); }; });
  el.querySelector('.pa').onclick=function(){ act(el, api('POST','/api/network/profile/add',{name:el.querySelector('.pn').value,ssid:el.querySelector('.ps').value,password:el.querySelector('.pp').value})); };
  return el;
}

// ---------------- Settings ----------------
function loadSettings(){
  getJSON('/api/status').then(function(s){
    document.getElementById('setgrid').innerHTML=
      '<div class="card"><h2>Hostname</h2><div class="muted">Reachable as <code>'+esc(s.hostname)+'.local</code></div>'+
        '<label>Hostname</label><input id="hn" value="'+esc(s.hostname)+'">'+
        '<div class="row" style="margin-top:8px"><button class="primary" onclick="saveHostname()">Save</button></div><div class="msg" id="hnmsg"></div></div>'+
      '<div class="card"><h2>Login</h2><label>Username</label><input id="au" placeholder="admin">'+
        '<label>New password</label><input id="ap" type="password"><div class="row" style="margin-top:8px"><button class="primary" onclick="savePassword()">Update</button></div><div class="msg" id="aumsg"></div></div>'+
      '<div class="card"><h2>Session</h2><div class="muted">Sign out of this device.</div><div class="row" style="margin-top:10px"><button class="ghost" onclick="logout()">Log out</button></div></div>';
  }).catch(function(){});
}
function fin(m,x){ m.className='msg '+(x.ok?'ok':'err'); m.textContent=x.ok?'Saved.':((x.d&&x.d.error)||'Failed'); }
function saveHostname(){ var m=document.getElementById('hnmsg'); m.className='msg'; m.textContent='Saving…';
  api('POST','/api/hostname',{name:document.getElementById('hn').value})
    .then(function(r){ return r.json().then(function(d){ return {ok:r.ok,d:d}; }); }).then(function(x){ fin(m,x); }); }
function savePassword(){ var m=document.getElementById('aumsg'); m.className='msg'; m.textContent='Saving…';
  var u=document.getElementById('au').value, p=document.getElementById('ap').value;
  api('POST','/api/account',{username:u||undefined,password:p})
    .then(function(r){ return r.json().then(function(d){ return {ok:r.ok,d:d}; }); }).then(function(x){ fin(m,x); }); }

// ---------------- poll loop ----------------
function poll(){
  getJSON('/api/status').then(function(s){
    document.getElementById('host').innerHTML='Device <b>'+esc(s.hostname)+'</b> · IP <b>'+esc(s.ip_address)+'</b> · Up '+fmtUptime(s.uptime);
    if(active==='cameras') renderCameras(s.cameras);
  }).catch(function(){});
  if(active==='system') loadSystem();
  else if(active==='logs') loadLogs();
}
poll();
setInterval(poll, 4000);
</script>
</body>
</html>
"#;
