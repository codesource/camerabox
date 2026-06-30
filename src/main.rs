//! camera-box: a small daemon for Raspberry Pi camera appliances.
//!
//! It detects USB UVC cameras, supervises one `ustreamer` process per camera
//! (the actual MJPEG streaming is delegated entirely to `ustreamer`), and
//! serves a status web UI + JSON API. See `README.md` for the big picture.

mod camera;
mod config;
mod logs;
mod net;
mod stream;
mod sys;
mod update;
mod web;

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use tokio::net::TcpListener;
use tracing::info;
use tracing_subscriber::EnvFilter;

use camera::AppState;
use config::{Config, PersistState};

/// Default location of the optional configuration file.
const CONFIG_PATH: &str = "/etc/camera-box/config.toml";
/// Where per-camera choices (enabled / resolution / fps) are persisted.
const STATE_PATH: &str = "/var/lib/camera-box/state.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_tracing();

    let config = Config::load(Path::new(CONFIG_PATH));
    info!(?config, "starting camera-box");

    let persist = PersistState::load(Path::new(STATE_PATH));
    let state = Arc::new(AppState::new(
        config,
        persist,
        std::path::PathBuf::from(STATE_PATH),
    ));

    // Background task: uevent hotplug detection + ustreamer supervision.
    tokio::spawn(camera::run(state.clone()));

    // Foreground task: the web UI / API.
    let addr = SocketAddr::from(([0, 0, 0, 0], state.config.web_port));
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding web server to {addr}"))?;
    info!(%addr, "web UI and API listening");

    axum::serve(listener, web::router(state))
        .await
        .context("web server terminated")?;

    Ok(())
}

/// Structured logging to stdout (for systemd/journald) plus an in-memory ring
/// buffer feeding the web UI's Logs tab. Honour `RUST_LOG`, else default `info`.
fn init_tracing() {
    use tracing_subscriber::prelude::*;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::registry()
        .with(filter)
        .with(tracing_subscriber::fmt::layer().with_target(false))
        .with(logs::init())
        .init();
}
