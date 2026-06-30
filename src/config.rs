//! Configuration (`/etc/camera-box/config.toml`) and persisted runtime state
//! (`/var/lib/camera-box/state.toml`).
//!
//! Config has sane defaults so a missing/partial file still works. Persisted
//! state remembers, per physical camera, whether it is enabled and at what
//! resolution/fps, so choices survive a reboot or replug.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use tracing::{info, warn};

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// Maximum number of cameras streaming at once (also the port pool size).
    pub max_cameras: usize,
    /// Port for the first stream; further streams use base + offset.
    pub base_stream_port: u16,
    /// Port for the status web UI / API.
    pub web_port: u16,
    /// Fallback host for stream/UI URLs when a request has no `Host` header
    /// (normally the address the client connected to is used instead).
    pub device_ip: String,
    /// Path to the `ustreamer` binary.
    pub ustreamer_path: String,
    /// Default capture resolution (e.g. `1280x720`).
    pub resolution: String,
    /// Default frames per second.
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

// ---------------------------------------------------------------------------
// Persisted per-camera state
// ---------------------------------------------------------------------------

/// What we remember about one physical camera between runs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CameraPersist {
    pub enabled: bool,
    pub resolution: String,
    pub fps: u32,
    /// Optional per-camera HTTP Basic Auth on the MJPEG stream.
    #[serde(default)]
    pub stream_user: Option<String>,
    #[serde(default)]
    pub stream_password: Option<String>,
}

/// On-disk state, keyed by a stable per-camera id (V4L2 `bus_info`).
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PersistState {
    #[serde(default)]
    pub cameras: BTreeMap<String, CameraPersist>,
}

impl PersistState {
    pub fn load(path: &Path) -> Self {
        match std::fs::read_to_string(path) {
            Ok(s) => match toml::from_str(&s) {
                Ok(state) => {
                    info!(path = %path.display(), "loaded persisted state");
                    state
                }
                Err(e) => {
                    warn!(path = %path.display(), error = %e, "invalid state file; ignoring");
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self, path: &Path) -> io::Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let body = toml::to_string_pretty(self)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        std::fs::write(path, body)
    }
}
