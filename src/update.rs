//! Self-update: version info + in-place update from GitHub Releases.
//!
//!   * `GET  /api/version`      — the running build's name/version.
//!   * `GET  /api/update/check` — query the latest release; is an update available?
//!   * `POST /api/update`       — download the matching asset, verify its SHA-256
//!                                against the release's `SHA256SUMS`, swap the
//!                                binary in place, and restart the service.
//!
//! Trust model: the download is over HTTPS from this repo's releases and is
//! verified by SHA-256 (from `SHA256SUMS` in the same release). It is **not**
//! GPG-signed, so trust ultimately rests on GitHub / the repo account. The new
//! binary is also run once (`camera-box version`) before install, so a corrupt
//! or wrong-architecture download is rejected rather than bricking the box.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::{
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Serialize;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::process::Command;
use tracing::{error, info};

use crate::camera::AppState;

const REPO: &str = "codesource/camerabox";

pub fn router() -> Router<Arc<AppState>> {
    Router::new()
        .route("/api/version", get(version))
        .route("/api/update/check", get(check_update))
        .route("/api/update", post(apply_update))
}

#[derive(Serialize)]
struct VersionInfo {
    name: &'static str,
    version: &'static str,
    description: &'static str,
}

/// Report the running build's name/version (compile-time metadata; no network).
async fn version() -> Json<VersionInfo> {
    Json(VersionInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
        description: env!("CARGO_PKG_DESCRIPTION"),
    })
}

/// Query the latest release and report whether it's newer than what's running.
async fn check_update() -> Response {
    let current = env!("CARGO_PKG_VERSION");
    match latest_tag().await {
        Ok(tag) => {
            let latest = ver_num(&tag);
            Json(json!({
                "current": current,
                "latest": latest,
                "tag": tag,
                "update_available": version_gt(latest, current),
                "asset": detect_asset().await,
            }))
            .into_response()
        }
        Err(e) => (
            StatusCode::BAD_GATEWAY,
            Json(json!({ "current": current, "error": e })),
        )
            .into_response(),
    }
}

/// Perform the update, then schedule a restart so we can still return a response.
async fn apply_update(State(_state): State<Arc<AppState>>) -> Response {
    match run_update().await {
        Ok(tag) => {
            tokio::spawn(async {
                // give the HTTP response time to flush before systemd kills us
                tokio::time::sleep(Duration::from_secs(1)).await;
                let _ = Command::new("systemctl")
                    .args(["restart", "camera-box"])
                    .status()
                    .await;
            });
            Json(json!({
                "status": "ok",
                "restart": true,
                "message": format!("Updated to {tag}; restarting…"),
            }))
            .into_response()
        }
        Err(e) => {
            error!(error = %e, "self-update failed");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({ "error": e }))).into_response()
        }
    }
}

// ---------------------------------------------------------------------------

/// The release asset that matches this hardware (by kernel machine type). The
/// ARMv7 build serves both the Pi Zero 2 W (32-bit) and the Luckfox Lyra Zero W.
async fn detect_asset() -> Option<&'static str> {
    let m = sh_out("uname", &["-m"]).await.ok()?;
    Some(match m.trim() {
        "aarch64" | "arm64" => "camera-box-pi-zero-2w-arm64",
        "armv7l" => "camera-box-pi-zero-2w-armv7",
        "armv6l" => "camera-box-pi-zero-w-armv6",
        _ => return None,
    })
}

/// `tag_name` of the latest release (e.g. `v0.3.4`).
async fn latest_tag() -> Result<String, String> {
    let body = sh_out(
        "curl",
        &[
            "-fsSL",
            "-A",
            "camera-box",
            "--max-time",
            "20",
            &format!("https://api.github.com/repos/{REPO}/releases/latest"),
        ],
    )
    .await
    .map_err(|e| format!("could not reach GitHub (is the device online?): {e}"))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| format!("parsing release info: {e}"))?;
    v.get("tag_name")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| "no tag_name in the latest release".to_string())
}

async fn run_update() -> Result<String, String> {
    let asset = detect_asset()
        .await
        .ok_or("unsupported architecture — no matching release asset")?;
    let tag = latest_tag().await?;
    let latest = ver_num(&tag);
    let current = env!("CARGO_PKG_VERSION");
    if !version_gt(latest, current) {
        return Err(format!("already up to date (running {current}, latest {latest})"));
    }

    info!(%tag, asset, "self-update: downloading");
    let base = format!("https://github.com/{REPO}/releases/download/{tag}");
    let dir = std::env::temp_dir();
    let bin = dir.join("camera-box.update");
    let sums = dir.join("camera-box.SHA256SUMS");

    sh_out(
        "curl",
        &["-fL", "-A", "camera-box", "--max-time", "300", "-o", path(&bin)?, &format!("{base}/{asset}")],
    )
    .await
    .map_err(|e| format!("downloading {asset}: {e}"))?;
    sh_out(
        "curl",
        &["-fsSL", "-A", "camera-box", "--max-time", "60", "-o", path(&sums)?, &format!("{base}/SHA256SUMS")],
    )
    .await
    .map_err(|e| format!("downloading SHA256SUMS: {e}"))?;

    verify_sha256(&bin, &sums, asset)?;
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod download: {e}"))?;

    // Self-check: the new binary must actually run on this CPU.
    let out = Command::new(&bin)
        .arg("version")
        .output()
        .await
        .map_err(|e| format!("the downloaded binary won't run: {e}"))?;
    if !out.status.success() {
        return Err("the downloaded binary failed its self-check (wrong architecture?)".into());
    }

    let target = std::env::current_exe().map_err(|e| format!("locating current binary: {e}"))?;
    install_atomic(&bin, &target)?;
    let _ = std::fs::remove_file(&bin);
    info!(%tag, "self-update: installed, restarting");
    Ok(tag)
}

/// Verify `file`'s SHA-256 against its entry in a `sha256sum`-format `sums` file.
fn verify_sha256(file: &Path, sums: &Path, asset: &str) -> Result<(), String> {
    let data = std::fs::read(file).map_err(|e| format!("reading download: {e}"))?;
    let mut hasher = Sha256::new();
    hasher.update(&data);
    let got: String = hasher.finalize().iter().map(|b| format!("{b:02x}")).collect();

    let text = std::fs::read_to_string(sums).map_err(|e| format!("reading SHA256SUMS: {e}"))?;
    let want = text
        .lines()
        .find_map(|l| {
            let mut it = l.split_whitespace();
            let hash = it.next()?;
            let name = it.next()?.trim_start_matches("./").trim_start_matches('*');
            (name == asset).then(|| hash.to_lowercase())
        })
        .ok_or_else(|| format!("no checksum for {asset} in SHA256SUMS"))?;

    if got == want {
        Ok(())
    } else {
        Err(format!("checksum mismatch for {asset} (got {got}, expected {want})"))
    }
}

/// Replace `target` with `src` atomically (stage in the same dir, then rename),
/// keeping a `.bak` of the old binary. A rename over a running executable is
/// safe on Linux — the running process keeps the old inode.
fn install_atomic(src: &Path, target: &Path) -> Result<(), String> {
    let dir = target.parent().ok_or("target has no parent directory")?;
    let _ = std::fs::copy(target, target.with_extension("bak")); // best-effort backup
    let staged = dir.join(".camera-box.new");
    std::fs::copy(src, &staged).map_err(|e| format!("staging new binary: {e}"))?;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("chmod staged: {e}"))?;
    std::fs::rename(&staged, target).map_err(|e| format!("installing (rename): {e}"))?;
    Ok(())
}

// ---- small helpers --------------------------------------------------------

fn path(p: &Path) -> Result<&str, String> {
    p.to_str().ok_or_else(|| "non-UTF8 temp path".to_string())
}

/// `X.Y.Z` from a `vX.Y.Z` tag.
fn ver_num(tag: &str) -> &str {
    tag.trim().trim_start_matches('v')
}

fn parse_ver(s: &str) -> (u64, u64, u64) {
    let mut it = s
        .split(['.', '-', '+'])
        .filter_map(|p| p.parse::<u64>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Is version `a` strictly newer than `b`?
fn version_gt(a: &str, b: &str) -> bool {
    parse_ver(a) > parse_ver(b)
}

/// Run a command, returning stdout on success or an error string on failure.
async fn sh_out(program: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
