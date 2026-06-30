//! ustreamer process supervision.
//!
//! One supervisor task per camera/port. It starts `ustreamer`, records its PID
//! into the shared [`StreamRuntime`], restarts it if it crashes, and shuts it
//! down cleanly when the [`CancellationToken`] fires (camera unplugged).
//!
//! NOTE: all video work is done by `ustreamer`. This module only manages the
//! child process lifecycle — it never touches frames or sockets itself.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Command;
use tokio::sync::Mutex;
use tokio::time::sleep;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::camera::{CameraDevice, StreamRuntime};
use crate::config::Config;

/// Delay before restarting a crashed stream.
const RESTART_BACKOFF: Duration = Duration::from_secs(2);
/// Delay before retrying when the binary could not even be spawned.
const SPAWN_BACKOFF: Duration = Duration::from_secs(3);

/// Spawn the supervisor task for one camera bound to `port`.
pub fn spawn(
    config: Arc<Config>,
    device: CameraDevice,
    port: u16,
    runtime: Arc<Mutex<StreamRuntime>>,
    cancel: CancellationToken,
) {
    tokio::spawn(supervise(config, device, port, runtime, cancel));
}

async fn supervise(
    config: Arc<Config>,
    device: CameraDevice,
    port: u16,
    runtime: Arc<Mutex<StreamRuntime>>,
    cancel: CancellationToken,
) {
    loop {
        if cancel.is_cancelled() {
            break;
        }

        match build_command(&config, &device, port).spawn() {
            Ok(mut child) => {
                let pid = child.id();
                set_running(&runtime, true, pid).await;
                info!(
                    path = %device.path.display(),
                    port,
                    pid = pid.unwrap_or(0),
                    "ustreamer started"
                );

                tokio::select! {
                    status = child.wait() => {
                        mark_exited(&runtime).await;
                        if cancel.is_cancelled() {
                            break;
                        }
                        warn!(
                            path = %device.path.display(),
                            port,
                            ?status,
                            "ustreamer exited unexpectedly; restarting"
                        );
                        // Back off, but bail immediately if cancelled meanwhile.
                        tokio::select! {
                            _ = sleep(RESTART_BACKOFF) => {}
                            _ = cancel.cancelled() => break,
                        }
                    }
                    _ = cancel.cancelled() => {
                        // Camera unplugged: stop the child cleanly.
                        let _ = child.start_kill();
                        let _ = child.wait().await;
                        set_running(&runtime, false, None).await;
                        info!(path = %device.path.display(), port, "ustreamer stopped (camera removed)");
                        break;
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
fn build_command(config: &Config, device: &CameraDevice, port: u16) -> Command {
    let mut cmd = Command::new(&config.ustreamer_path);
    cmd.arg(format!("--device={}", device.path.display()))
        .arg("--host=0.0.0.0")
        .arg(format!("--port={port}"))
        .arg(format!("--resolution={}", config.resolution))
        .arg(format!("--desired-fps={}", config.fps))
        // Skip identical frames to save bandwidth on the hotspot link.
        .arg("--drop-same-frames=30");

    if device.mjpeg {
        // Camera already emits MJPEG: NOOP forwards frames untouched — the
        // lowest-latency, lowest-CPU path (true passthrough).
        cmd.arg("--format=MJPEG").arg("--encoder=NOOP");
    } else {
        // Camera emits raw frames (e.g. YUYV): encode to JPEG on the CPU.
        cmd.arg("--encoder=CPU");
    }

    // Inherit stdio so ustreamer's own logs reach journald alongside ours.
    cmd.stdout(Stdio::inherit()).stderr(Stdio::inherit());
    // Safety net: if this task is dropped, don't leak the child.
    cmd.kill_on_drop(true);
    cmd
}
