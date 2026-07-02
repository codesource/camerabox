# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`camera-box` is a Rust/`tokio` daemon for a Raspberry Pi (Zero W / Zero 2 W)
USB-camera appliance with a **router-style web dashboard**. It detects USB UVC
cameras and exposes each as a low-latency MJPEG HTTP stream (via `ustreamer`),
and the dashboard also manages Wi-Fi (AP/client, scan, profiles), system info,
logs, hostname, and a login. Runs as a systemd service (as root) on Raspberry
Pi OS Lite. **Linux-only** (V4L2 ioctls, netlink uevents, and it drives `iw`/
`hostapd`/`wpa_supplicant`/`hostnamectl`) — it will not build or run on
Windows/macOS even though development may happen there.

Module map beyond camera/stream/web: `net.rs` (Wi-Fi + hostname/mDNS),
`sys.rs` (system overview + byte counters), `logs.rs` (log ring buffer),
`auth.rs` (hashed credentials + sessions). See the README's source-layout table.

## Core architectural constraint

**Do not implement video streaming in Rust.** All MJPEG capture/encoding is
delegated to the external `ustreamer` binary. The Rust daemon's job is strictly:
device detection, slot assignment, child-process supervision, status, API, UI.
Keep frame/socket-level work out of this codebase — if a feature seems to need
it, it almost certainly belongs as `ustreamer` flags in `stream.rs::build_command`.

## Commands

```sh
cargo build --release        # build
cargo run                    # run locally (needs root + Linux + cameras to do anything useful)
cargo clippy --all-targets   # lint
cargo fmt                    # format
```

Releases use static musl via `cross` (Docker): `arm-unknown-linux-musleabihf`
for Pi Zero W v1.1 (armv6), `aarch64-unknown-linux-musl` / `armv7-...` for the
Zero 2 W. The **Luckfox Lyra Zero W** (Rockchip RK3506B, triple Cortex-A7) is
ARMv7 hard-float — the release workflow ships the same `armv7` static binary
under a board-named asset. Its supported OS is the **camera-box minimal image**
(build once with `scripts/build-minimal-image-luckfox-lyra-zero-w.sh`, deploy
per card with `scripts/prepare-sd.sh` — docs/minimal-lyra-image.md); it must be
a **systemd** system, never Buildroot/BusyBox. **Armbian is not a supported
route** (its vendor kernel lacks UVC too, and its AIC8800 AP bring-up is broken
on this board). There are no tests yet; `cargo test` is a no-op.

**Dev workflow notes (this repo is developed on Windows):**

- Build with `cross build --release --target arm-unknown-linux-musleabihf` (needs
  Docker Desktop running). `rust-analyzer`/IDE diagnostics evaluate against the
  *Windows* target, so `libc::ioctl`, `OsStrExt`, `custom_flags`, etc. show as
  false "not found" errors — ignore them; the musl cross build is the truth.
- Deploy/test on the Pi (SSH alias `camera-box`): `scp` the binary, then run it
  as a transient unit — `systemctl stop camera-box; systemd-run --unit=camera-box-test
  --collect /tmp/camera-box-test` — and revert with `systemctl start camera-box`.
- `axum::Handler` requires `Send` futures: never put `.await` inside a `tracing`
  macro's field value (it holds a non-`Send` `Arguments` across the await) — bind
  the awaited value first.

## Architecture (the parts that span files)

Startup (`main.rs`) builds a single `Arc<AppState>` (defined in `camera.rs`) and
shares it between two halves:

1. **Camera manager** (`camera::run`, background task) — does an initial scan of
   `/dev/video*`, then watches the kernel netlink uevent socket directly (pure
   Rust, no `libudev`) via `AsyncFd` for `video4linux` add/remove events.
2. **Web server** (`axum`) — reads the same `AppState` to render the UI and API,
   and calls `camera::set_enabled` / `set_mode` for control actions.

`AppState.cameras` is a `RwLock<Vec<ManagedCamera>>` listing **all** connected
USB cameras (ordered by `/dev/videoN`). Each `ManagedCamera` has a desired
`enabled` flag and, while streaming, a `port` + `CancellationToken`. `reconcile`
starts enabled-but-not-streaming cameras on the lowest free port at or above
`base_stream_port`. There is **no fixed concurrent-stream cap** — the practical
limit is the number of connected USB cameras, so enabling one always yields a
working stream (ports are sticky: a camera keeps its port until disabled).

Key behaviours:

- A **newly-seen** camera (no persisted state) starts **disabled**; the user
  enables it (and picks a resolution/fps) in the web UI. Previously-seen cameras
  resume their persisted `enabled`/`resolution`/`fps`.
- **Persistence**: per-camera choices are saved to `/var/lib/camera-box/state.toml`
  keyed by the V4L2 `bus_info` (the stable `device.id`), via `PersistState` in
  `config.rs`. `set_enabled`/`set_mode` mutate state, `reconcile`, then snapshot
  and save.
- `stream::spawn` runs a supervisor per streaming camera: it reads the shared
  `Arc<Mutex<StreamSettings>>` on each (re)start, writes liveness into
  `Arc<Mutex<StreamRuntime>>`, restarts on crash, restarts on `restart` `Notify`
  (settings changed), and stops cleanly on `CancellationToken`. Supervisors are
  the only writers of `StreamRuntime`; the web layer only reads.

### Device probing (`camera.rs`)

Detection uses **raw V4L2 ioctls via `libc`** (no v4l crate). `VIDIOC_QUERYCAP`
keeps only USB capture devices: requires `V4L2_CAP_VIDEO_CAPTURE` **and**
`bus_info` starting with `usb` (this excludes the Pi's on-board
`platform:bcm2835-isp`/`-codec` nodes and metadata-only nodes). `VIDIOC_ENUM_FMT`
picks the stream format (MJPEG preferred → passthrough). Frame size/interval
ioctls (`VIDIOC_ENUM_FRAMESIZES`, `VIDIOC_ENUM_FRAMEINTERVALS`) enumerate the
resolution/fps list shown in the UI.
The `#[repr(C)]` structs and ioctl constants must match the kernel ABI exactly —
verify struct sizes against the encoded ioctl numbers (comments give the
`_IOR`/`_IOWR` derivation). ioctl request codes are `u32` cast to `libc::Ioctl`
at the call site (it's `c_ulong` on glibc, `c_int` on musl).

### Web UI (`web.rs`)

The page is a static shell + vanilla JS that polls `/api/status` and updates the
DOM **in place** (no reload), skipping a `<select>` that's focused so it never
disrupts a selection. Control endpoints: `POST /api/cameras/:id/{enable,disable}`
and `/mode`. Optional Basic Auth (config `auth_user`/`auth_password`) wraps all
routes via a `from_fn` middleware; it does **not** cover the ustreamer streams.

### ustreamer invocation (`stream.rs`)

MJPEG-capable cameras get `--format=MJPEG --encoder=NOOP` (true passthrough,
lowest latency); others get `--encoder=CPU` (re-encode). Child stdio is
inherited so ustreamer logs land in journald next to ours.

## Conventions

- **Logging**: `tracing` to stdout (journald captures it). Always log camera
  connect/disconnect and child start/stop/restart events — these are the
  primary operational signal for a headless appliance.
- **Config** (`config.rs`): every field has a default via `#[serde(default)]` +
  `Default`. A missing or invalid `/etc/camera-box/config.toml` must never be
  fatal — fall back to defaults and log.
- **Modules are role-based and flat**: `main/config/camera/stream/web/update`.
  Keep new code in the matching module; `update.rs` is intentionally a
  placeholder structured so real update logic slots in behind the same routes.
- The daemon assumes it runs as root (port 80, `/dev/video*`, and binding the
  netlink uevent multicast group).
- **No C/system library dependencies** — hotplug is read from the kernel netlink
  uevent socket via raw `libc` calls, not the `udev` crate. This is deliberate:
  it keeps cross-compilation (esp. ARMv6 Pi Zero W) free of sysroot/`libudev`
  pain. Don't reintroduce `libudev`/`udev`-crate deps without good reason.
