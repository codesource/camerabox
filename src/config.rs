//! Configuration loading from `/etc/camera-box/config.toml`.
//!
//! Every field has a sane default (via `#[serde(default)]` + [`Default`]), so a
//! missing file or a partially-specified file both work without error.

use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Maximum number of cameras to manage (slots cam0..camN-1).
    pub max_cameras: usize,
    /// Port for the first camera; subsequent cameras use base + slot index.
    pub base_stream_port: u16,
    /// Port for the status web UI / API.
    pub web_port: u16,
    /// Fallback host for stream/UI URLs when a request has no `Host` header
    /// (normally the address the client connected to is used instead).
    pub device_ip: String,
    /// Path to the `ustreamer` binary.
    pub ustreamer_path: String,
    /// Requested capture resolution (e.g. `1280x720`).
    pub resolution: String,
    /// Requested frames per second.
    pub fps: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_cameras: 2,
            base_stream_port: 8080,
            web_port: 80,
            device_ip: "192.168.4.1".to_string(),
            ustreamer_path: "/usr/bin/ustreamer".to_string(),
            resolution: "1280x720".to_string(),
            fps: 30,
        }
    }
}

impl Config {
    /// Load configuration, falling back to defaults on any problem.
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(contents) => match toml::from_str::<Config>(&contents) {
                Ok(cfg) => {
                    info!(path = %path.display(), "loaded configuration");
                    cfg
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "invalid config; using defaults");
                    Config::default()
                }
            },
            Err(_) => {
                info!(path = %path.display(), "no config file found; using defaults");
                Config::default()
            }
        }
    }
}
