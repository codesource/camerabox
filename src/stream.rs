//! ustreamer process supervision.
//!
//! One supervisor task per *streaming* camera. It starts `ustreamer` with the
//! camera's current [`StreamSettings`], records its PID into the shared
//! [`StreamRuntime`], restarts it if it crashes or when settings change
//! (`restart` notify), and shuts it down cleanly when the [`CancellationToken`]
//! fires (camera disabled or unplugged).
//!
//! All video work is done by `ustreamer`; this module only manages the child
//! process lifecycle.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::{Mutex, Notify};
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::camera::{CameraDevice, StreamRuntime, StreamSettings};
use crate::config::Config;

const RESTART_BACKOFF: Duration = Duration::from_secs(2);
const SPAWN_BACKOFF: Duration = Duration::from_secs(3);

/// Spawn the supervisor task for one camera bound to `port`.
#[allow(clippy::too_many_arguments)]
pub fn spawn(
    config: Arc<Config>,
    device: CameraDevice,
    port: u16,
    settings: Arc<Mutex<StreamSettings>>,
    restart: Arc<Notify>,
    runtime: Arc<Mutex<StreamRuntime>>,
    cancel: CancellationToken,
) {
    tokio::spawn(supervise(
        config, device, port, settings, restart, runtime, cancel,
    ));
}

#[allow(clippy::too_many_arguments)]
async fn supervise(
    config: Arc<Config>,
    device: CameraDevice,
    port: u16,
    settings: Arc<Mutex<StreamSettings>>,
    restart: Arc<Notify>,
    runtime: Arc<Mutex<StreamRuntime>>,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            break;
        }

        let current = settings.lock().await.clone();
        match build_command(&config, &device, port, &current).spawn() {
            Ok(mut child) => {
                let pid = child.id();
                set_running(&runtime, true, pid).await;
                info!(
                    path = %device.path.display(),
                    port,
                    resolution = %current.resolution,
                    fps = current.fps,
                    pid = pid.unwrap_or(0),
                    "ustreamer started"
                );

                tokio::select! {
                    status = child.wait() => {
                        mark_exited(&runtime).await;
                        if cancel.is_cancelled() {
                            break;
                        }
                        warn!(path = %device.path.display(), port, ?status, "ustreamer exited; restarting");
                        tokio::select! {
                            _ = sleep(RESTART_BACKOFF) => {}
                            _ = cancel.cancelled() => break,
                        }
                    }
                    _ = cancel.cancelled() => {
                        stop_child(&mut child, &runtime).await;
                        info!(path = %device.path.display(), port, "ustreamer stopped");
                        break;
                    }
                    _ = restart.notified() => {
                        info!(path = %device.path.display(), port, "restarting ustreamer with new settings");
                        stop_child(&mut child, &runtime).await;
                        // loop: respawn with freshly-read settings
                    }
                }
            }
            Err(e) => {
                set_running(&runtime, false, None).await;
                error!(
                    path = %device.path.display(),
                    ustreamer = %config.ustreamer_path,
                    error = %e,
                    "failed to spawn ustreamer (is it installed at this path?)"
                );
                tokio::select! {
                    _ = sleep(SPAWN_BACKOFF) => {}
                    _ = cancel.cancelled() => break,
                }
            }
        }
    }
}

async fn stop_child(child: &mut tokio::process::Child, runtime: &Arc<Mutex<StreamRuntime>>) {
    let _ = child.start_kill();
    let _ = child.wait().await;
    set_running(runtime, false, None).await;
}

async fn set_running(runtime: &Arc<Mutex<StreamRuntime>>, running: bool, pid: Option<u32>) {
    let mut r = runtime.lock().await;
    r.running = running;
    r.pid = pid;
}

async fn mark_exited(runtime: &Arc<Mutex<StreamRuntime>>) {
    let mut r = runtime.lock().await;
    r.running = false;
    r.pid = None;
    r.restarts += 1;
}

/// Build the `ustreamer` command with low-latency MJPEG settings.
fn build_command(config: &Config, device: &CameraDevice, port: u16, settings: &StreamSettings) -> Command {
    let mut cmd = Command::new(&config.ustreamer_path);
    cmd.arg(format!("--device={}", device.path.display()))
        .arg("--host=0.0.0.0")
        .arg(format!("--port={port}"))
        .arg(format!("--resolution={}", settings.resolution))
        .arg(format!("--desired-fps={}", settings.fps));
    // NB: deliberately *not* using `--drop-same-frames`. It suppresses frames
    // identical to the previous one, which makes a static scene collapse to
    // ~1-2 fps (and looks like a broken stream) even though capture is at the
    // full rate. We favour a smooth, constant fps over the bandwidth saving.

    if device.mjpeg {
        // Camera already emits MJPEG: NOOP forwards frames untouched — the
        // lowest-latency, lowest-CPU path (true passthrough).
        cmd.arg("--format=MJPEG").arg("--encoder=NOOP");
    } else {
        // Camera emits raw frames (e.g. YUYV): encode to JPEG on the CPU.
        cmd.arg("--encoder=CPU");
    }

    // Optional per-camera HTTP Basic Auth on the stream.
    if let Some(user) = settings.user.as_deref().filter(|u| !u.is_empty()) {
        cmd.arg(format!("--user={user}"));
        cmd.arg(format!("--passwd={}", settings.password.as_deref().unwrap_or("")));
    }

    // Inherit stdio so ustreamer's own logs reach journald alongside ours.
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    cmd.kill_on_drop(true);
    cmd
}
