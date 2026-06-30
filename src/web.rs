//! Web UI (the landing page) and the JSON status API.
//!
//! The firmware-update endpoints live in [`crate::update`] and are merged in
//! here so the whole HTTP surface is mounted from one router.

use std::sync::Arc;

use axum::{extract::State, response::Html, routing::get, Json, Router};
use serde::Serialize;

use crate::camera::AppState;
use crate::update;

/// Build the complete application router.
pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/status", get(status))
        .merge(update::router())
        .with_state(state)
}

// ---------------------------------------------------------------------------
// API model
// ---------------------------------------------------------------------------

#[derive(Serialize)]
pub struct StatusResponse {
    pub hostname: String,
    pub ip_address: String,
    /// Daemon uptime in seconds.
    pub uptime: u64,
    pub cameras: Vec<CameraStatus>,
}

#[derive(Serialize)]
pub struct CameraStatus {
    pub slot: usize,
    pub device_path: String,
    pub name: Option<String>,
    pub stream_url: String,
    pub port: u16,
    pub running: bool,
    pub pid: Option<u32>,
}

/// Snapshot the shared state into a serialisable status response.
async fn build_status(state: &AppState) -> StatusResponse {
    let slots = state.slots.read().await;
    let mut cameras = Vec::new();
    for (idx, slot) in slots.iter().enumerate() {
        if let Some(s) = slot {
            let rt = s.runtime.lock().await;
            cameras.push(CameraStatus {
                slot: idx,
                device_path: s.device.path.display().to_string(),
                name: s.device.name.clone(),
                stream_url: format!("http://{}:{}/stream", state.config.device_ip, s.port),
                port: s.port,
                running: rt.running,
                pid: rt.pid,
            });
        }
    }

    StatusResponse {
        hostname: state.hostname.clone(),
        ip_address: state.config.device_ip.clone(),
        uptime: state.started.elapsed().as_secs(),
        cameras,
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn status(State(state): State<Arc<AppState>>) -> Json<StatusResponse> {
    Json(build_status(&state).await)
}

async fn index(State(state): State<Arc<AppState>>) -> Html<String> {
    Html(render_index(&build_status(&state).await))
}

// ---------------------------------------------------------------------------
// Minimal server-rendered HTML (no frontend framework, auto-refreshing)
// ---------------------------------------------------------------------------

fn render_index(s: &StatusResponse) -> String {
    // Build the table rows first.
    let mut rows = String::new();
    if s.cameras.is_empty() {
        rows.push_str("<tr><td colspan=\"6\" class=\"empty\">No cameras connected</td></tr>");
    } else {
        for c in &s.cameras {
            let badge = if c.running {
                "<span class=\"ok\">running</span>"
            } else {
                "<span class=\"down\">stopped</span>"
            };
            let pid = c.pid.map(|p| p.to_string()).unwrap_or_else(|| "-".to_string());
            let name = c.name.clone().unwrap_or_else(|| "Unknown".to_string());
            rows.push_str(&format!(
                "<tr><td>cam{slot}</td><td>{name}</td><td><code>{path}</code></td>\
                 <td><a href=\"{url}\">{url}</a></td><td>{badge}</td><td>{pid}</td></tr>",
                slot = c.slot,
                name = html_escape(&name),
                path = html_escape(&c.device_path),
                url = html_escape(&c.stream_url),
                badge = badge,
                pid = pid,
            ));
        }
    }

    // Static head/style as a raw string (avoids brace-escaping the CSS).
    let mut page = String::from(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<meta http-equiv="refresh" content="5">
<title>camera-box</title>
<style>
body{font-family:system-ui,Segoe UI,sans-serif;margin:0;background:#0f1115;color:#e6e6e6}
header{background:#171a21;padding:16px 20px;border-bottom:1px solid #2a2f3a}
h1{margin:0;font-size:18px;letter-spacing:.5px}
.meta{color:#9aa4b2;font-size:13px;margin-top:6px}
main{padding:20px}
table{border-collapse:collapse;width:100%;font-size:14px}
th,td{text-align:left;padding:9px 10px;border-bottom:1px solid #2a2f3a}
th{color:#9aa4b2;font-weight:600}
code{background:#1c2029;padding:2px 6px;border-radius:4px}
a{color:#5cb3ff;text-decoration:none}
a:hover{text-decoration:underline}
.ok{color:#46d369;font-weight:600}
.down{color:#ff5c5c;font-weight:600}
.empty{color:#9aa4b2;text-align:center;padding:26px}
footer{padding:12px 20px;color:#6b7280;font-size:12px;border-top:1px solid #2a2f3a}
</style>
</head>
<body>
<header>
<h1>camera-box</h1>
"#,
    );

    page.push_str(&format!(
        "<div class=\"meta\">Device <b>{host}</b> &middot; IP <b>{ip}</b> &middot; \
         Uptime {uptime} &middot; {count} camera(s)</div>\n",
        host = html_escape(&s.hostname),
        ip = html_escape(&s.ip_address),
        uptime = format_uptime(s.uptime),
        count = s.cameras.len(),
    ));

    page.push_str(
        "</header>\n<main>\n<table>\n\
         <thead><tr><th>Slot</th><th>Name</th><th>Device</th>\
         <th>Stream URL</th><th>Stream</th><th>PID</th></tr></thead>\n<tbody>\n",
    );
    page.push_str(&rows);
    page.push_str(
        "</tbody>\n</table>\n</main>\n\
         <footer>Auto-refreshes every 5s &middot; \
         <a href=\"/api/status\">/api/status</a> &middot; \
         <a href=\"/api/version\">/api/version</a></footer>\n</body>\n</html>",
    );

    page
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn format_uptime(secs: u64) -> String {
    let days = secs / 86_400;
    let hours = (secs % 86_400) / 3_600;
    let mins = (secs % 3_600) / 60;
    let s = secs % 60;
    if days > 0 {
        format!("{days}d {hours}h {mins}m")
    } else if hours > 0 {
        format!("{hours}h {mins}m {s}s")
    } else {
        format!("{mins}m {s}s")
    }
}
