//! Camera detection, slot assignment, and shared application state.
//!
//! Responsibilities:
//!   * Hold the app-wide [`AppState`] (config, hostname, camera slots).
//!   * Probe `/dev/video*` nodes with raw V4L2 ioctls to keep only real video
//!     *capture* devices (metadata-only nodes are ignored).
//!   * Detect plug/unplug at runtime by reading the kernel's netlink uevent
//!     socket directly — no `libudev` dependency, which keeps the binary pure
//!     Rust and trivial to cross-compile (notably for the ARMv6 Pi Zero W).
//!   * Assign cameras to stable slots (cam0, cam1, ...) and spawn/stop the
//!     per-camera `ustreamer` supervisor (see [`crate::stream`]).

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

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
        error!(error = %e, "uevent monitor loop terminated");
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

/// Enumerate existing `/dev/video*` nodes and keep the capture-capable ones,
/// ordered so the lowest-numbered node becomes cam0.
fn scan_devices() -> io::Result<Vec<CameraDevice>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/dev")?.flatten() {
        let fname = entry.file_name();
        let bytes = fname.as_bytes();
        if !is_video_node(bytes) {
            continue;
        }
        let path = entry.path();
        if let Some(cam) = evaluate_path(&path, bytes) {
            out.push(cam);
        }
    }
    out.sort_by_key(|c| video_index(&c.path));
    Ok(out)
}

fn is_video_node(name: &[u8]) -> bool {
    name.starts_with(b"video") && name.len() > 5 && name[5..].iter().all(u8::is_ascii_digit)
}

/// Numeric index of a `/dev/videoN` path (for deterministic ordering).
fn video_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("video"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

/// What a parsed uevent maps to.
enum Action {
    Add(CameraDevice),
    Remove(PathBuf),
}

/// Watch the kernel netlink uevent socket for `video4linux` add/remove events.
async fn monitor_loop(state: &Arc<AppState>) -> anyhow::Result<()> {
    let sock = open_uevent_socket().context("opening netlink uevent socket")?;
    let async_fd = AsyncFd::new(sock)?;
    info!("watching kernel uevents for camera hotplug");

    let mut buf = vec![0u8; 8192];
    loop {
        let mut guard = async_fd.readable().await?;

        // Drain all datagrams currently queued, collecting owned actions before
        // doing any `.await` (handlers take async locks).
        let mut actions: Vec<Action> = Vec::new();
        loop {
            let n = unsafe {
                libc::recv(
                    async_fd.get_ref().as_raw_fd(),
                    buf.as_mut_ptr() as *mut libc::c_void,
                    buf.len(),
                    0,
                )
            };
            if n > 0 {
                if let Some(action) = parse_uevent(&buf[..n as usize]) {
                    actions.push(action);
                }
            } else if n == 0 {
                break;
            } else {
                let err = io::Error::last_os_error();
                match err.kind() {
                    io::ErrorKind::WouldBlock => break,
                    io::ErrorKind::Interrupted => continue,
                    _ => {
                        warn!(error = %err, "uevent recv error");
                        break;
                    }
                }
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

/// Open and bind a non-blocking `NETLINK_KOBJECT_UEVENT` socket subscribed to
/// the kernel uevent multicast group (group 1).
fn open_uevent_socket() -> io::Result<OwnedFd> {
    let fd = unsafe {
        libc::socket(
            libc::AF_NETLINK,
            libc::SOCK_RAW | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            libc::NETLINK_KOBJECT_UEVENT,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` is a fresh, owned descriptor returned by socket().
    let owned = unsafe { OwnedFd::from_raw_fd(fd) };

    let mut addr: libc::sockaddr_nl = unsafe { std::mem::zeroed() };
    addr.nl_family = libc::AF_NETLINK as u16;
    addr.nl_groups = 1; // group 1 = kernel-originated uevents

    let rc = unsafe {
        libc::bind(
            owned.as_raw_fd(),
            &addr as *const libc::sockaddr_nl as *const libc::sockaddr,
            std::mem::size_of::<libc::sockaddr_nl>() as libc::socklen_t,
        )
    };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(owned)
}

/// Parse a kernel uevent datagram (NUL-separated `KEY=value` pairs) into an
/// [`Action`], keeping only `video4linux` add/remove events.
fn parse_uevent(msg: &[u8]) -> Option<Action> {
    let mut action: Option<&[u8]> = None;
    let mut subsystem: Option<&[u8]> = None;
    let mut devname: Option<&[u8]> = None;

    for field in msg.split(|&b| b == 0) {
        // The first field is an `action@devpath` summary with no '='; skip it.
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue;
        };
        let (key, val) = (&field[..eq], &field[eq + 1..]);
        match key {
            b"ACTION" => action = Some(val),
            b"SUBSYSTEM" => subsystem = Some(val),
            b"DEVNAME" => devname = Some(val),
            _ => {}
        }
    }

    if subsystem != Some(b"video4linux") {
        return None;
    }
    let devname = devname?;
    let path = dev_path(devname);

    match action {
        Some(a) if a == b"add" => evaluate_path(&path, devname).map(Action::Add),
        Some(a) if a == b"remove" => Some(Action::Remove(path)),
        _ => None,
    }
}

/// Build the device-node path from a uevent `DEVNAME` (usually a bare basename
/// like `video0`, occasionally an absolute path).
fn dev_path(devname: &[u8]) -> PathBuf {
    let name = OsStr::from_bytes(devname);
    if devname.first() == Some(&b'/') {
        PathBuf::from(name)
    } else {
        Path::new("/dev").join(name)
    }
}

/// Read a device's friendly name from sysfs (`/sys/class/video4linux/<dev>/name`).
fn read_card_name(devname: &[u8]) -> Option<String> {
    let mut p = PathBuf::from("/sys/class/video4linux");
    p.push(OsStr::from_bytes(devname));
    p.push("name");
    std::fs::read_to_string(p)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
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
                info!(port = s.port, path = %path.display(), "camera disconnected; stopping stream");
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

// ioctl request codes. Kept as `u32` and cast to `libc::Ioctl` at the call
// site: that alias is `c_ulong` on glibc but `c_int` on musl, and casting the
// 32-bit value works for both (the kernel only reads the low 32 bits).
const VIDIOC_QUERYCAP: u32 = 0x8068_5600; // _IOR('V', 0, v4l2_capability)
const VIDIOC_ENUM_FMT: u32 = 0xc040_5602; // _IOWR('V', 2, v4l2_fmtdesc)

const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
/// fourcc 'M''J''P''G'
const V4L2_PIX_FMT_MJPEG: u32 = 0x4750_4a4d;

/// Turn a `/dev/videoN` path into a [`CameraDevice`] iff it is a real capture
/// device. Metadata-only nodes (no `V4L2_CAP_VIDEO_CAPTURE`) yield `None`.
fn evaluate_path(path: &Path, devname: &[u8]) -> Option<CameraDevice> {
    match inspect(path) {
        Ok((true, mjpeg)) => Some(CameraDevice {
            path: path.to_path_buf(),
            name: read_card_name(devname),
            mjpeg,
        }),
        Ok((false, _)) => None, // metadata-only or non-capture node
        Err(e) => {
            // Frequently a transient hotplug race, or a node we can't open.
            warn!(path = %path.display(), error = %e, "could not probe video device");
            None
        }
    }
}

/// Probe a video node. Returns `(supports_capture, supports_mjpeg)`.
fn inspect(path: &Path) -> io::Result<(bool, bool)> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)?;
    let fd = file.as_raw_fd();

    let mut cap: V4l2Capability = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(fd, VIDIOC_QUERYCAP as libc::Ioctl, &mut cap as *mut _) };
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
        let rc = unsafe { libc::ioctl(fd, VIDIOC_ENUM_FMT as libc::Ioctl, &mut desc as *mut _) };
        if rc < 0 {
            break; // no more formats
        }
        if desc.pixelformat == V4L2_PIX_FMT_MJPEG {
            return true;
        }
    }
    false
}
