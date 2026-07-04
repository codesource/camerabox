//! camera-box: a small daemon for Raspberry Pi camera appliances.
//!
//! It detects USB UVC cameras, supervises one `ustreamer` process per camera
//! (the actual MJPEG streaming is delegated entirely to `ustreamer`), and
//! serves a status web UI + JSON API. See `README.md` for the big picture.

mod auth;
mod button;
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
use config::{ApConfig, Config, PersistState};

/// Default location of the optional configuration file.
const CONFIG_PATH: &str = "/etc/camera-box/config.toml";
/// Where per-camera choices (enabled / resolution / fps) are persisted.
const STATE_PATH: &str = "/var/lib/camera-box/state.toml";
/// Where the web-UI credentials are stored.
const AUTH_PATH: &str = "/var/lib/camera-box/auth.toml";
/// Where the hotspot (AP) SSID/password/IP are persisted.
const AP_PATH: &str = "/var/lib/camera-box/network.toml";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args: Vec<String> = std::env::args().collect();

    // CLI: `camera-box version` prints the version and exits (also used by the
    // self-updater to confirm a freshly-downloaded binary runs on this CPU).
    if matches!(args.get(1).map(String::as_str), Some("version" | "--version" | "-V")) {
        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
        return Ok(());
    }

    // CLI: `camera-box reset-password [user] [pass]` resets the login and exits.
    if args.get(1).map(String::as_str) == Some("reset-password") {
        let user = args.get(2).cloned().unwrap_or_else(|| "admin".to_string());
        let pass = args.get(3).cloned().unwrap_or_else(|| "password".to_string());
        let auth = auth::Auth::load(std::path::PathBuf::from(AUTH_PATH));
        match auth.set_credentials(&user, &pass) {
            Ok(()) => println!("Password reset. Log in as '{user}'."),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // CLI: `camera-box factory-reset` wipes all saved state and reboots
    // (recovery path when you're locked out of the dashboard).
    if args.get(1).map(String::as_str) == Some("factory-reset") {
        if let Err(e) = net::factory_reset().await {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        println!("Factory reset complete. Rebooting…");
        let _ = tokio::process::Command::new("systemctl")
            .args(["reboot"])
            .status()
            .await;
        return Ok(());
    }

    init_tracing();

    let config = Config::load(Path::new(CONFIG_PATH));
    info!(?config, "starting camera-box");

    let persist = PersistState::load(Path::new(STATE_PATH));
    let auth = auth::Auth::load(std::path::PathBuf::from(AUTH_PATH));
    let ap = ApConfig::load(Path::new(AP_PATH));

    // Keep the on-disk AP config files (hostapd, dnsmasq, boot-time IP unit,
    // hostname mapping) in sync with the saved settings so a reboot brings the
    // hotspot up with the configured SSID/password/IP. This does not restart
    // anything, so it never disturbs the current network mode.
    if let Some(iface) = net::primary_wifi() {
        if let Err(e) = net::sync_ap_config(&iface, &ap).await {
            tracing::warn!(error = %e, "could not sync AP config files");
        }
    }

    let state = Arc::new(AppState::new(
        config,
        persist,
        std::path::PathBuf::from(STATE_PATH),
        auth,
        ap,
        std::path::PathBuf::from(AP_PATH),
    ));

    // Background task: uevent hotplug detection + ustreamer supervision.
    tokio::spawn(camera::run(state.clone()));

    // Background task: optional GPIO factory-reset button (no-op if unconfigured).
    tokio::spawn(button::watch(state.config.clone()));

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
