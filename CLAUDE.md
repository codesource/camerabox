# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`camera-box` is a Rust/`tokio` daemon for a Raspberry Pi (Zero W / Zero 2 W)
USB-camera appliance. It detects USB UVC cameras and exposes each as a
low-latency MJPEG HTTP stream, plus a web UI and JSON API. It runs as a systemd
service on Raspberry Pi OS Lite. **Linux-only** (uses V4L2 ioctls + kernel
netlink uevents) — it will not build or run meaningfully on Windows/macOS even though
development may happen there.

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

Cross-compile for the Pi with `cross` (see README): `arm-unknown-linux-gnueabihf`
for Pi Zero W v1.1 (armv6), `aarch64-unknown-linux-gnu` for Pi Zero 2 W (64-bit).

There are no tests yet; `cargo test` is a no-op.

## Architecture (the parts that span files)

Startup (`main.rs`) builds a single `Arc<AppState>` (defined in `camera.rs`) and
shares it between two halves:

1. **Camera manager** (`camera::run`, background task) — does an initial scan of
   `/dev/video*`, then watches the kernel netlink uevent socket directly (pure
   Rust, no `libudev`) via `AsyncFd` for `video4linux` add/remove events. On add
   it probes the node and assigns it to a slot; on remove it frees the slot.
2. **Web server** (`axum`) — reads the same `AppState` to render the UI and API.

`AppState.slots` is a `RwLock<Vec<Option<Slot>>>` of fixed length `max_cameras`.
Index = slot number = port offset (`base_stream_port + index`). Slot assignment
is "first free slot"; this is the source of truth for cam0/cam1 stability.

Each occupied `Slot` owns a `CancellationToken` and an
`Arc<Mutex<StreamRuntime>>`. `stream::spawn` starts a supervisor task per camera
that (re)launches `ustreamer`, writes liveness (`running`/`pid`) into the shared
`StreamRuntime`, restarts on crash, and on token cancellation kills the child
cleanly. The web layer reads `StreamRuntime` for status — supervisors are the
only writers, the web layer only reads.

### Device probing (`camera.rs`)

Capture-capability and MJPEG detection use **raw V4L2 ioctls via `libc`** (no
v4l crate): `VIDIOC_QUERYCAP` filters out metadata-only nodes (keep only
`V4L2_CAP_VIDEO_CAPTURE`), `VIDIOC_ENUM_FMT` detects MJPEG so passthrough can be
preferred. The `#[repr(C)]` structs and ioctl constants must match the kernel
ABI exactly — if you touch them, verify struct sizes against the encoded ioctl
numbers (the comments give the `_IOR`/`_IOWR` derivation).

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
