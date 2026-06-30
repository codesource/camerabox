//! Camera detection, slot assignment, and shared application state.
//!
//! Responsibilities:
//!   * Hold the app-wide [`AppState`] (config, hostname, camera slots).
//!   * Probe `/dev/video*` nodes with raw V4L2 ioctls to keep only real video
//!     *capture* devices (metadata-only nodes are ignored).
//!   * Detect plug/unplug at runtime via a udev netlink monitor.
//!   * Assign cameras to stable slots (cam0, cam1, ...) and spawn/stop the
//!     per-camera `ustreamer` supervisor (see [`crate::stream`]).

use std::fs::OpenOptions;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};
use udev::{EventType, MonitorBuilder};

use crate::config::Config;

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Liveness of one ustreamer process. Updated by the supervisor task and read
/// by the web/status layer.
#[derive(Debug, Default)]
pub struct StreamRuntime {
    pub running: bool,
    pub pid: Option<u32>,
    pub restarts: u32,
}

/// A `/dev/videoN` node that supports video capture.
#[derive(Debug, Clone)]
pub struct CameraDevice {
    pub path: PathBuf,
    pub name: Option<String>,
    /// True if the device can emit MJPEG directly (enables zero-copy passthrough).
    pub mjpeg: bool,
}

/// An occupied camera slot.
pub struct Slot {
    pub device: CameraDevice,
    pub port: u16,
    /// Fires to tell the supervisor to stop ustreamer (camera unplugged).
    pub cancel: CancellationToken,
    /// Shared liveness, written by the supervisor.
    pub runtime: Arc<Mutex<StreamRuntime>>,
}

/// Application-wide shared state, handed to axum and the camera manager.
pub struct AppState {
    pub config: Arc<Config>,
    pub hostname: String,
    pub started: Instant,
    /// Fixed-length vector of length `max_cameras`; `None` == free slot.
    pub slots: RwLock<Vec<Option<Slot>>>,
}

impl AppState {
    pub fn new(config: Config) -> Self {
        let slots = (0..config.max_cameras).map(|_| None).collect();
        Self {
            hostname: read_hostname(),
            config: Arc::new(config),
            started: Instant::now(),
            slots: RwLock::new(slots),
        }
    }
}

fn read_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "camera-box".to_string())
}

// ---------------------------------------------------------------------------
// Manager entry point
// ---------------------------------------------------------------------------

/// Run the camera manager: initial scan, then watch for hotplug events forever.
pub async fn run(state: Arc<AppState>) {
    initial_scan(&state).await;
    if let Err(e) = monitor_loop(&state).await {
        error!(error = %e, "udev monitor loop terminated");
    }
}

async fn initial_scan(state: &Arc<AppState>) {
    match scan_devices() {
        Ok(devices) => {
            info!(found = devices.len(), "initial camera scan complete");
            for dev in devices {
                handle_add(state, dev).await;
            }
        }
        Err(e) => error!(error = %e, "initial camera scan failed"),
    }
}

/// Enumerate existing video4linux devices and keep the capture-capable ones.
fn scan_devices() -> io::Result<Vec<CameraDevice>> {
    let mut enumerator = udev::Enumerator::new()?;
    enumerator.match_subsystem("video4linux")?;

    let mut out = Vec::new();
    for dev in enumerator.scan_devices()? {
        if let Some(cam) = evaluate_device(&dev) {
            out.push(cam);
        }
    }
    Ok(out)
}

/// What a drained udev event maps to, once detached from the borrowed socket.
enum Action {
    Add(CameraDevice),
    Remove(PathBuf),
}

/// Watch the udev netlink socket for `video4linux` add/remove events.
async fn monitor_loop(state: &Arc<AppState>) -> anyhow::Result<()> {
    let socket = MonitorBuilder::new()?
        .match_subsystem("video4linux")?
        .listen()?;
    set_nonblocking(socket.as_raw_fd())?;

    let async_fd = AsyncFd::new(socket)?;
    info!("watching for camera hotplug events");

    loop {
        let mut guard = async_fd.readable().await?;

        // Drain all currently-queued events into owned actions first, so the
        // borrow on the socket is released before we do any `.await`.
        let mut actions: Vec<Action> = Vec::new();
        for event in guard.get_inner().iter() {
            match event.event_type() {
                EventType::Add => {
                    if let Some(cam) = evaluate_device(&event) {
                        actions.push(Action::Add(cam));
                    }
                }
                EventType::Remove => {
                    if let Some(node) = event.devnode() {
                        actions.push(Action::Remove(node.to_path_buf()));
                    }
                }
                _ => {}
            }
        }
        guard.clear_ready();

        for action in actions {
            match action {
                Action::Add(cam) => handle_add(state, cam).await,
                Action::Remove(path) => handle_remove(state, &path).await,
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Slot assignment
// ---------------------------------------------------------------------------

/// Assign a freshly detected camera to the first free slot and start its stream.
async fn handle_add(state: &Arc<AppState>, device: CameraDevice) {
    let mut slots = state.slots.write().await;

    // Ignore duplicate notifications for an already-assigned node.
    if slots.iter().flatten().any(|s| s.device.path == device.path) {
        return;
    }

    let Some(idx) = slots.iter().position(|s| s.is_none()) else {
        warn!(path = %device.path.display(), "no free camera slot; ignoring device");
        return;
    };

    let port = state.config.base_stream_port + idx as u16;
    let cancel = CancellationToken::new();
    let runtime = Arc::new(Mutex::new(StreamRuntime::default()));

    info!(
        slot = idx,
        path = %device.path.display(),
        name = device.name.as_deref().unwrap_or("unknown"),
        mjpeg = device.mjpeg,
        port,
        "camera connected"
    );

    crate::stream::spawn(
        state.config.clone(),
        device.clone(),
        port,
        runtime.clone(),
        cancel.clone(),
    );

    slots[idx] = Some(Slot {
        device,
        port,
        cancel,
        runtime,
    });
}

/// Free the slot whose device matches `path`, stopping its stream.
async fn handle_remove(state: &Arc<AppState>, path: &Path) {
    let mut slots = state.slots.write().await;
    for slot in slots.iter_mut() {
        if slot.as_ref().map(|s| s.device.path.as_path()) == Some(path) {
            if let Some(s) = slot.take() {
                info!(slot = s.port, path = %path.display(), "camera disconnected; stopping stream");
                s.cancel.cancel();
            }
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// V4L2 capability probing (raw ioctl, no extra crates)
// ---------------------------------------------------------------------------

#[allow(dead_code)]
#[repr(C)]
struct V4l2Capability {
    driver: [u8; 16],
    card: [u8; 32],
    bus_info: [u8; 32],
    version: u32,
    capabilities: u32,
    device_caps: u32,
    reserved: [u32; 3],
}

#[allow(dead_code)]
#[repr(C)]
struct V4l2Fmtdesc {
    index: u32,
    typ: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    reserved: [u32; 4],
}

// ioctl request codes (stable across Linux ABI; fit in a 32-bit c_ulong too).
const VIDIOC_QUERYCAP: libc::c_ulong = 0x8068_5600; // _IOR('V', 0, v4l2_capability)
const VIDIOC_ENUM_FMT: libc::c_ulong = 0xc040_5602; // _IOWR('V', 2, v4l2_fmtdesc)

const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
/// fourcc 'M''J''P''G'
const V4L2_PIX_FMT_MJPEG: u32 = 0x4750_4a4d;

/// Probe a video node. Returns `(supports_capture, supports_mjpeg)`.
fn inspect(path: &Path) -> io::Result<(bool, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let fd = file.as_raw_fd();

    let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, VIDIOC_QUERYCAP, &mut cap as *mut _) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }

    // Use the per-node `device_caps` when advertised; otherwise the global caps.
    let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };

    let is_capture = caps & V4L2_CAP_VIDEO_CAPTURE != 0;
    let mjpeg = is_capture && supports_mjpeg(fd);
    Ok((is_capture, mjpeg))
}

/// Enumerate capture formats and report whether MJPEG is offered.
fn supports_mjpeg(fd: RawFd) -> bool {
    for index in 0..64u32 {
        let mut desc: V4l2Fmtdesc = unsafe { std::mem::zeroed() };
        desc.index = index;
        desc.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        let rc = unsafe { libc::ioctl(fd, VIDIOC_ENUM_FMT, &mut desc as *mut _) };
        if rc < 0 {
            break; // no more formats
        }
        if desc.pixelformat == V4L2_PIX_FMT_MJPEG {
            return true;
        }
    }
    false
}

/// Convert a udev device into a [`CameraDevice`] iff it is a real capture
/// device. Metadata-only nodes (no `V4L2_CAP_VIDEO_CAPTURE`) yield `None`.
fn evaluate_device(dev: &udev::Device) -> Option<CameraDevice> {
    let path = dev.devnode()?.to_path_buf();
    match inspect(&path) {
        Ok((true, mjpeg)) => {
            let name = dev
                .attribute_value("name")
                .map(|s| s.to_string_lossy().trim().to_string())
                .filter(|s| !s.is_empty());
            Some(CameraDevice { path, name, mjpeg })
        }
        Ok((false, _)) => None, // metadata-only or non-capture node
        Err(e) => {
            // Frequently a transient hotplug race, or a node we can't open.
            warn!(path = %path.display(), error = %e, "could not probe video device");
            None
        }
    }
}

/// Put a raw fd into non-blocking mode (required for `AsyncFd`).
fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    unsafe {
        let flags = libc::fcntl(fd, libc::F_GETFL);
        if flags < 0 {
            return Err(io::Error::last_os_error());
        }
        if libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}
