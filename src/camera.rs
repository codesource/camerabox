//! Camera detection, registry, mode enumeration, and shared application state.
//!
//! Responsibilities:
//!   * Hold the app-wide [`AppState`] (config, hostname, camera registry).
//!   * Probe `/dev/video*` nodes with raw V4L2 ioctls to keep only USB video
//!     *capture* devices (the Pi's on-board ISP/codec and metadata-only nodes
//!     are ignored).
//!   * Enumerate each camera's supported resolutions + frame rates.
//!   * Detect plug/unplug via the kernel netlink uevent socket (no `libudev`).
//!   * Track *all* connected USB cameras; auto-enable the first `max_cameras`
//!     and let the user enable/disable each one. Enabled cameras get a stream
//!     port and a supervised `ustreamer` process (see [`crate::stream`]).
//!   * Persist per-camera enabled/resolution/fps choices across reboots.

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
use serde::Serialize;
use tokio::io::unix::AsyncFd;
use tokio::sync::{Mutex, Notify, RwLock};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::config::{CameraPersist, Config, PersistState};

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

/// Liveness of one ustreamer process. Updated by the supervisor task.
#[derive(Debug, Default)]
pub struct StreamRuntime {
    pub running: bool,
    pub pid: Option<u32>,
    pub restarts: u32,
}

/// Mutable capture settings, read by the supervisor on each (re)start.
#[derive(Debug, Clone)]
pub struct StreamSettings {
    pub resolution: String,
    pub fps: u32,
    /// Optional HTTP Basic Auth on this camera's MJPEG stream.
    pub user: Option<String>,
    pub password: Option<String>,
}

/// One supported capture mode: a resolution and the frame rates it offers.
#[derive(Debug, Clone, Serialize)]
pub struct CameraMode {
    pub width: u32,
    pub height: u32,
    pub fps: Vec<u32>,
}

/// A detected USB video capture device.
#[derive(Debug, Clone)]
pub struct CameraDevice {
    pub path: PathBuf,
    pub name: Option<String>,
    /// Stable id (V4L2 `bus_info`, e.g. `usb-3f980000.usb-1.2`); the
    /// persistence key. Falls back to the device path if `bus_info` is empty.
    pub id: String,
    /// True if the device can emit MJPEG directly (zero-copy passthrough).
    pub mjpeg: bool,
    pub modes: Vec<CameraMode>,
}

/// A connected camera and its desired/actual stream state.
pub struct ManagedCamera {
    pub device: CameraDevice,
    /// Desired state: should this camera be streaming?
    pub enabled: bool,
    /// Assigned stream port while streaming.
    pub port: Option<u16>,
    /// Sticky preferred port: kept (and persisted) while enabled so the camera
    /// reuses the same port across restarts; cleared when disabled.
    pub desired_port: Option<u16>,
    /// Present while a supervisor task is running for this camera.
    pub cancel: Option<CancellationToken>,
    /// Notify the supervisor to restart with updated settings.
    pub restart: Arc<Notify>,
    pub settings: Arc<Mutex<StreamSettings>>,
    pub runtime: Arc<Mutex<StreamRuntime>>,
}

impl ManagedCamera {
    fn is_streaming(&self) -> bool {
        self.cancel.is_some()
    }
}

/// Application-wide shared state.
pub struct AppState {
    pub config: Arc<Config>,
    pub started: Instant,
    pub state_path: PathBuf,
    /// All connected USB cameras, ordered by device path.
    pub cameras: RwLock<Vec<ManagedCamera>>,
    /// Persisted per-camera choices, mirrored to `state_path`.
    pub persist: Mutex<PersistState>,
    /// Credentials + sessions for the web UI.
    pub auth: crate::auth::Auth,
}

impl AppState {
    pub fn new(
        config: Config,
        persist: PersistState,
        state_path: PathBuf,
        auth: crate::auth::Auth,
    ) -> Self {
        Self {
            config: Arc::new(config),
            started: Instant::now(),
            state_path,
            cameras: RwLock::new(Vec::new()),
            persist: Mutex::new(persist),
            auth,
        }
    }
}

/// Errors from camera control operations.
#[derive(Debug)]
pub enum ControlError {
    NotFound,
    Unsupported,
}

// ---------------------------------------------------------------------------
// Manager entry point
// ---------------------------------------------------------------------------

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

fn scan_devices() -> io::Result<Vec<CameraDevice>> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/dev")?.flatten() {
        let fname = entry.file_name();
        let bytes = fname.as_bytes();
        if !is_video_node(bytes) {
            continue;
        }
        if let Some(cam) = evaluate_path(&entry.path(), bytes) {
            out.push(cam);
        }
    }
    out.sort_by_key(|c| video_index(&c.path));
    Ok(out)
}

fn is_video_node(name: &[u8]) -> bool {
    name.starts_with(b"video") && name.len() > 5 && name[5..].iter().all(u8::is_ascii_digit)
}

fn video_index(path: &Path) -> u32 {
    path.file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("video"))
        .and_then(|n| n.parse().ok())
        .unwrap_or(u32::MAX)
}

enum Action {
    Add(CameraDevice),
    Remove(PathBuf),
}

async fn monitor_loop(state: &Arc<AppState>) -> anyhow::Result<()> {
    let sock = open_uevent_socket().context("opening netlink uevent socket")?;
    let async_fd = AsyncFd::new(sock)?;
    info!("watching kernel uevents for camera hotplug");

    let mut buf = vec![0u8; 8192];
    loop {
        let mut guard = async_fd.readable().await?;

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
    // SAFETY: `fd` is a fresh, owned descriptor from socket().
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

fn parse_uevent(msg: &[u8]) -> Option<Action> {
    let mut action: Option<&[u8]> = None;
    let mut subsystem: Option<&[u8]> = None;
    let mut devname: Option<&[u8]> = None;

    for field in msg.split(|&b| b == 0) {
        let Some(eq) = field.iter().position(|&b| b == b'=') else {
            continue; // header line, no '='
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

fn dev_path(devname: &[u8]) -> PathBuf {
    let name = OsStr::from_bytes(devname);
    if devname.first() == Some(&b'/') {
        PathBuf::from(name)
    } else {
        Path::new("/dev").join(name)
    }
}

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
// Registry: add / remove / enable / disable / mode
// ---------------------------------------------------------------------------

async fn handle_add(state: &Arc<AppState>, device: CameraDevice) {
    let mut cams = state.cameras.write().await;
    if cams.iter().any(|c| c.device.path == device.path) {
        return; // duplicate notification
    }

    // Desired state: persisted choice if we've seen this camera, else
    // auto-enable up to max_cameras.
    let persisted = {
        let p = state.persist.lock().await;
        p.cameras.get(&device.id).cloned()
    };
    let (enabled, resolution, fps, user, password, desired_port) = match persisted {
        // Previously-seen camera: resume its saved settings, auth, and port.
        Some(p) => (
            p.enabled,
            p.resolution,
            p.fps,
            p.stream_user,
            p.stream_password,
            p.port,
        ),
        // New, unknown camera: start OFF. The user enables it (choosing a
        // resolution/fps) from the web UI. `initial_settings` only seeds the
        // pre-selected mode in the dropdown; it does not start a stream.
        None => {
            let (res, fps) = initial_settings(&device, &state.config);
            (false, res, fps, None, None, None)
        }
    };

    info!(
        path = %device.path.display(),
        id = %device.id,
        name = device.name.as_deref().unwrap_or("unknown"),
        mjpeg = device.mjpeg,
        modes = device.modes.len(),
        enabled,
        "USB camera connected"
    );

    cams.push(ManagedCamera {
        device,
        enabled,
        port: None,
        desired_port,
        cancel: None,
        restart: Arc::new(Notify::new()),
        settings: Arc::new(Mutex::new(StreamSettings { resolution, fps, user, password })),
        runtime: Arc::new(Mutex::new(StreamRuntime::default())),
    });
    cams.sort_by_key(|c| video_index(&c.device.path));

    reconcile(&state.config, &mut cams);
    let snapshot = snapshot_persist(&cams).await;
    drop(cams);
    persist_snapshot(state, snapshot).await;
}

async fn handle_remove(state: &Arc<AppState>, path: &Path) {
    let mut cams = state.cameras.write().await;
    let Some(pos) = cams.iter().position(|c| c.device.path == path) else {
        return;
    };
    let cam = cams.remove(pos);
    if let Some(token) = cam.cancel {
        info!(path = %path.display(), "USB camera disconnected; stopping stream");
        token.cancel();
    } else {
        info!(path = %path.display(), "USB camera disconnected");
    }
    // A freed port may let a waiting enabled camera start.
    let _ = reconcile(&state.config, &mut cams);
}

/// Enable or disable a camera by id, (re)starting/stopping its stream.
pub async fn set_enabled(
    state: &Arc<AppState>,
    id: &str,
    enabled: bool,
) -> Result<(), ControlError> {
    let snapshot;
    {
        let mut cams = state.cameras.write().await;
        let Some(cam) = cams.iter_mut().find(|c| c.device.id == id) else {
            return Err(ControlError::NotFound);
        };
        cam.enabled = enabled;
        if !enabled {
            if let Some(token) = cam.cancel.take() {
                token.cancel();
            }
            // Disabling frees the port (current + sticky reservation).
            cam.port = None;
            cam.desired_port = None;
            *cam.runtime.lock().await = StreamRuntime::default();
        }
        info!(id, enabled, "camera enable state changed");
        reconcile(&state.config, &mut cams);
        snapshot = snapshot_persist(&cams).await;
    }
    persist_snapshot(state, snapshot).await;
    Ok(())
}

/// Change a camera's resolution/fps; restarts the stream if running.
pub async fn set_mode(
    state: &Arc<AppState>,
    id: &str,
    resolution: &str,
    fps: u32,
) -> Result<(), ControlError> {
    let snapshot;
    {
        let mut cams = state.cameras.write().await;
        let Some(cam) = cams.iter_mut().find(|c| c.device.id == id) else {
            return Err(ControlError::NotFound);
        };
        let Some((w, h)) = parse_resolution(resolution) else {
            return Err(ControlError::Unsupported);
        };
        let supported = cam
            .device
            .modes
            .iter()
            .any(|m| m.width == w && m.height == h && m.fps.contains(&fps));
        // If the camera exposes no enumerable modes, accept any value.
        if !supported && !cam.device.modes.is_empty() {
            return Err(ControlError::Unsupported);
        }
        {
            let mut s = cam.settings.lock().await;
            s.resolution = format!("{w}x{h}");
            s.fps = fps;
        }
        if cam.is_streaming() {
            cam.restart.notify_one();
        }
        info!(id, resolution = %format!("{w}x{h}"), fps, "camera mode changed");
        reconcile(&state.config, &mut cams);
        snapshot = snapshot_persist(&cams).await;
    }
    persist_snapshot(state, snapshot).await;
    Ok(())
}

/// Set (or clear) per-camera HTTP Basic Auth on the MJPEG stream. An empty
/// username disables it. Restarts the stream if running.
pub async fn set_stream_auth(
    state: &Arc<AppState>,
    id: &str,
    user: Option<String>,
    password: Option<String>,
) -> Result<(), ControlError> {
    let snapshot;
    {
        let mut cams = state.cameras.write().await;
        let Some(cam) = cams.iter_mut().find(|c| c.device.id == id) else {
            return Err(ControlError::NotFound);
        };
        let user = user.filter(|s| !s.is_empty());
        let password = if user.is_some() {
            password.filter(|s| !s.is_empty())
        } else {
            None
        };
        let protected = user.is_some();
        {
            let mut s = cam.settings.lock().await;
            s.user = user;
            s.password = password;
        }
        if cam.is_streaming() {
            cam.restart.notify_one();
        }
        info!(id, protected, "camera stream auth changed");
        snapshot = snapshot_persist(&cams).await;
    }
    persist_snapshot(state, snapshot).await;
    Ok(())
}

/// Start enabled cameras that aren't streaming yet, up to the port pool size.
///
/// Two passes so ports are *sticky*: first reclaim each camera's remembered
/// (persisted) port when it's still free, then assign the lowest free port to
/// any remaining cameras.
fn reconcile(config: &Arc<Config>, cams: &mut [ManagedCamera]) {
    let mut used: Vec<u16> = cams.iter().filter_map(|c| c.port).collect();

    // Pass 1: honour the sticky port each enabled camera already owns.
    for cam in cams.iter_mut() {
        if cam.enabled && !cam.is_streaming() {
            if let Some(p) = cam.desired_port {
                if in_pool(config, p) && !used.contains(&p) {
                    start_stream(config, cam, p);
                    used.push(p);
                }
            }
        }
    }
    // Pass 2: give the lowest free port to anyone still waiting.
    for cam in cams.iter_mut() {
        if cam.enabled && !cam.is_streaming() {
            if let Some(port) = first_free_port(config, &used) {
                start_stream(config, cam, port);
                used.push(port);
            }
        }
    }
}

fn in_pool(config: &Config, port: u16) -> bool {
    port >= config.base_stream_port
        && port < config.base_stream_port + config.max_cameras as u16
}

fn first_free_port(config: &Arc<Config>, used: &[u16]) -> Option<u16> {
    (0..config.max_cameras as u16)
        .map(|i| config.base_stream_port + i)
        .find(|p| !used.contains(p))
}

/// Per-camera persistence snapshot. Async so it can read each settings lock.
type PersistRow = (
    String,
    bool,
    String,
    u32,
    Option<String>,
    Option<String>,
    Option<u16>,
);
async fn snapshot_persist(cams: &[ManagedCamera]) -> Vec<PersistRow> {
    let mut out = Vec::with_capacity(cams.len());
    for c in cams {
        let s = c.settings.lock().await;
        out.push((
            c.device.id.clone(),
            c.enabled,
            s.resolution.clone(),
            s.fps,
            s.user.clone(),
            s.password.clone(),
            c.desired_port,
        ));
    }
    out
}

fn start_stream(config: &Arc<Config>, cam: &mut ManagedCamera, port: u16) {
    let token = CancellationToken::new();
    cam.port = Some(port);
    cam.desired_port = Some(port); // remember it for next time
    cam.cancel = Some(token.clone());
    info!(path = %cam.device.path.display(), port, "starting stream");
    crate::stream::spawn(
        config.clone(),
        cam.device.clone(),
        port,
        cam.settings.clone(),
        cam.restart.clone(),
        cam.runtime.clone(),
        token,
    );
}

async fn persist_snapshot(state: &Arc<AppState>, snapshot: Vec<PersistRow>) {
    let mut p = state.persist.lock().await;
    for (id, enabled, resolution, fps, stream_user, stream_password, port) in snapshot {
        p.cameras.insert(
            id,
            CameraPersist {
                enabled,
                resolution,
                fps,
                stream_user,
                stream_password,
                port,
            },
        );
    }
    if let Err(e) = p.save(&state.state_path) {
        warn!(path = %state.state_path.display(), error = %e, "could not save state");
    }
}

fn parse_resolution(s: &str) -> Option<(u32, u32)> {
    let (w, h) = s.split_once('x')?;
    Some((w.trim().parse().ok()?, h.trim().parse().ok()?))
}

/// Pick sensible initial settings: the configured resolution/fps if the camera
/// supports it, else its first advertised mode, else the config values as-is.
fn initial_settings(device: &CameraDevice, config: &Config) -> (String, u32) {
    if let Some((cw, ch)) = parse_resolution(&config.resolution) {
        if device
            .modes
            .iter()
            .any(|m| m.width == cw && m.height == ch && m.fps.contains(&config.fps))
        {
            return (config.resolution.clone(), config.fps);
        }
    }
    if let Some(m) = device.modes.first() {
        let fps = m.fps.first().copied().unwrap_or(config.fps);
        return (format!("{}x{}", m.width, m.height), fps);
    }
    (config.resolution.clone(), config.fps)
}

// ---------------------------------------------------------------------------
// V4L2 capability + mode probing (raw ioctl, no extra crates)
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

#[allow(dead_code)]
#[repr(C)]
struct V4l2Frmsizeenum {
    index: u32,
    pixel_format: u32,
    typ: u32,
    // union: discrete {w,h} or stepwise {6 x u32}
    union_data: [u32; 6],
    reserved: [u32; 2],
}

#[allow(dead_code)]
#[repr(C)]
struct V4l2Frmivalenum {
    index: u32,
    pixel_format: u32,
    width: u32,
    height: u32,
    typ: u32,
    // union: discrete fract {num,den} or stepwise {3 x fract}
    union_data: [u32; 6],
    reserved: [u32; 2],
}

// ioctl request codes. Kept as `u32` and cast to `libc::Ioctl` at the call
// site: that alias is `c_ulong` on glibc but `c_int` on musl, and casting the
// 32-bit value works for both (the kernel only reads the low 32 bits).
const VIDIOC_QUERYCAP: u32 = 0x8068_5600; // _IOR ('V', 0, v4l2_capability)
const VIDIOC_ENUM_FMT: u32 = 0xc040_5602; // _IOWR('V', 2, v4l2_fmtdesc)
const VIDIOC_ENUM_FRAMESIZES: u32 = 0xc02c_564a; // _IOWR('V', 74, v4l2_frmsizeenum)
const VIDIOC_ENUM_FRAMEINTERVALS: u32 = 0xc034_564b; // _IOWR('V', 75, v4l2_frmivalenum)

const V4L2_CAP_VIDEO_CAPTURE: u32 = 0x0000_0001;
const V4L2_CAP_DEVICE_CAPS: u32 = 0x8000_0000;
const V4L2_BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const V4L2_FRMSIZE_TYPE_DISCRETE: u32 = 1;
const V4L2_FRMIVAL_TYPE_DISCRETE: u32 = 1;
/// fourcc 'M''J''P''G'
const V4L2_PIX_FMT_MJPEG: u32 = 0x4750_4a4d;

struct Probe {
    usable: bool,
    bus_info: String,
    mjpeg: bool,
    modes: Vec<CameraMode>,
}

fn evaluate_path(path: &Path, devname: &[u8]) -> Option<CameraDevice> {
    match inspect(path) {
        Ok(p) if p.usable => {
            let id = if p.bus_info.is_empty() {
                path.display().to_string()
            } else {
                p.bus_info
            };
            Some(CameraDevice {
                path: path.to_path_buf(),
                name: read_card_name(devname),
                id,
                mjpeg: p.mjpeg,
                modes: p.modes,
            })
        }
        Ok(_) => None, // non-USB (e.g. bcm2835 ISP/codec), metadata-only, etc.
        Err(e) => {
            warn!(path = %path.display(), error = %e, "could not probe video device");
            None
        }
    }
}

fn inspect(path: &Path) -> io::Result<Probe> {
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

    let caps = if cap.capabilities & V4L2_CAP_DEVICE_CAPS != 0 {
        cap.device_caps
    } else {
        cap.capabilities
    };
    let is_capture = caps & V4L2_CAP_VIDEO_CAPTURE != 0;
    // Only manage USB cameras (bus_info "usb-..."); exclude the Pi's on-board
    // blocks (bus_info "platform:bcm2835-isp" / "platform:bcm2835-codec").
    let is_usb = cap.bus_info.starts_with(b"usb");
    let usable = is_capture && is_usb;

    let bus_info = cstr_to_string(&cap.bus_info);
    let (mjpeg, modes) = if usable {
        match pick_format(fd) {
            Some(fmt) => (fmt == V4L2_PIX_FMT_MJPEG, enum_modes(fd, fmt)),
            None => (false, Vec::new()),
        }
    } else {
        (false, Vec::new())
    };

    Ok(Probe {
        usable,
        bus_info,
        mjpeg,
        modes,
    })
}

/// Choose the capture format to stream: MJPEG if offered (passthrough), else
/// the first advertised format.
fn pick_format(fd: RawFd) -> Option<u32> {
    let mut first = None;
    for index in 0..64u32 {
        let mut desc: V4l2Fmtdesc = unsafe { std::mem::zeroed() };
        desc.index = index;
        desc.typ = V4L2_BUF_TYPE_VIDEO_CAPTURE;
        let rc = unsafe { libc::ioctl(fd, VIDIOC_ENUM_FMT as libc::Ioctl, &mut desc as *mut _) };
        if rc < 0 {
            break;
        }
        if desc.pixelformat == V4L2_PIX_FMT_MJPEG {
            return Some(V4L2_PIX_FMT_MJPEG);
        }
        if first.is_none() {
            first = Some(desc.pixelformat);
        }
    }
    first
}

fn enum_modes(fd: RawFd, pixfmt: u32) -> Vec<CameraMode> {
    let mut modes = Vec::new();
    for index in 0..64u32 {
        let mut fs: V4l2Frmsizeenum = unsafe { std::mem::zeroed() };
        fs.index = index;
        fs.pixel_format = pixfmt;
        let rc = unsafe { libc::ioctl(fd, VIDIOC_ENUM_FRAMESIZES as libc::Ioctl, &mut fs as *mut _) };
        if rc < 0 {
            break;
        }
        if fs.typ != V4L2_FRMSIZE_TYPE_DISCRETE {
            break; // stepwise/continuous: not enumerated
        }
        let (w, h) = (fs.union_data[0], fs.union_data[1]);
        modes.push(CameraMode {
            width: w,
            height: h,
            fps: enum_frame_rates(fd, pixfmt, w, h),
        });
    }
    modes
}

fn enum_frame_rates(fd: RawFd, pixfmt: u32, width: u32, height: u32) -> Vec<u32> {
    let mut out = Vec::new();
    for index in 0..64u32 {
        let mut fi: V4l2Frmivalenum = unsafe { std::mem::zeroed() };
        fi.index = index;
        fi.pixel_format = pixfmt;
        fi.width = width;
        fi.height = height;
        let rc =
            unsafe { libc::ioctl(fd, VIDIOC_ENUM_FRAMEINTERVALS as libc::Ioctl, &mut fi as *mut _) };
        if rc < 0 {
            break;
        }
        if fi.typ != V4L2_FRMIVAL_TYPE_DISCRETE {
            break;
        }
        // interval = num/den seconds  ->  fps = den/num
        let (num, den) = (fi.union_data[0], fi.union_data[1]);
        if num > 0 {
            out.push(den / num);
        }
    }
    out.sort_unstable();
    out.dedup();
    out.reverse(); // highest fps first
    out
}

fn cstr_to_string(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}
