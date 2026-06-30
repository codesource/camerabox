# camera-box

A small Rust daemon that turns a **Raspberry Pi Zero W / Zero 2 W** into a USB
camera appliance. It auto-detects USB UVC cameras and exposes each one as a
**low-latency MJPEG HTTP stream**, plus a status web UI and JSON API.

The actual video streaming is **not** done in Rust — it is delegated to
[`ustreamer`](https://github.com/pikvm/ustreamer). The daemon only:

- detects camera plug/unplug (kernel netlink uevents),
- assigns cameras to stable slots (`cam0`, `cam1`),
- starts/stops/restarts one `ustreamer` process per camera,
- serves the web UI, status API, and an update-endpoint placeholder.

## Target platform

- Raspberry Pi Zero W v1.1 (armv6) or Pi Zero 2 W (armv7/aarch64)
- Raspberry Pi OS Lite / Raspbian, **Linux only**
- Runs as a `systemd` service
- Wi-Fi AP provided externally by `hostapd` + `dnsmasq`
- Device IP expected at `192.168.4.1`

## How it works

```
USB camera  --uevent-->  camera.rs (detect + assign slot)
                              |
                              v
                       stream.rs (spawn & supervise)
                              |
                              v
                         ustreamer  --MJPEG-->  http://192.168.4.1:8080/stream
                                                http://192.168.4.1:8081/stream

web.rs / update.rs  --HTTP-->  http://192.168.4.1/        (web UI)
                               http://192.168.4.1/api/status
                               http://192.168.4.1/api/version
                               http://192.168.4.1/api/update   (501, placeholder)
```

- **Slot assignment is stable in memory.** The first valid camera gets `cam0`
  (port `8080`), the second gets `cam1` (port `8081`). Unplugging frees the
  slot; the next plugged-in camera takes the lowest free slot.
- **Only video *capture* devices are used.** Each `/dev/videoN` node is probed
  with `VIDIOC_QUERYCAP`; metadata-only nodes are ignored. MJPEG support is
  detected with `VIDIOC_ENUM_FMT` to prefer zero-copy passthrough.
- **Crashed streams restart automatically;** unplugged cameras stop cleanly.

## Source layout

| File             | Responsibility                                            |
|------------------|-----------------------------------------------------------|
| `src/main.rs`    | Startup: logging, config, spawn manager, run web server.  |
| `src/config.rs`  | Load `/etc/camera-box/config.toml` with safe defaults.    |
| `src/camera.rs`  | Shared state, V4L2 probing, uevent monitor, assignment.   |
| `src/stream.rs`  | Per-camera `ustreamer` supervisor (start/stop/restart).   |
| `src/web.rs`     | Web UI + `GET /api/status`.                               |
| `src/update.rs`  | `GET /api/version`, `POST /api/update` (placeholder).     |

## Prerequisites on the Pi

```sh
sudo apt update
sudo apt install ustreamer        # provides /usr/bin/ustreamer
```

(If `ustreamer` is not packaged on your image, build it from source and set
`ustreamer_path` in the config to wherever you installed it.)

## Install a prebuilt release (easiest)

Each tagged release ships prebuilt binaries, so the Pi needs no build tools —
just download the one for your board:

| Board                   | Asset                         |
|-------------------------|-------------------------------|
| Pi Zero W v1.1 (ARMv6)  | `camera-box-pi-zero-w-armv6`  |
| Pi Zero 2 W (32-bit OS) | `camera-box-pi-zero-2w-armv7` |
| Pi Zero 2 W (64-bit OS) | `camera-box-pi-zero-2w-arm64` |

```sh
# On the Pi (example: Pi Zero W v1.1)
sudo git clone https://github.com/codesource/camerabox.git /opt/camera-box
wget https://github.com/codesource/camerabox/releases/latest/download/camera-box-pi-zero-w-armv6 \
     -O /tmp/camera-box
sudo bash /opt/camera-box/scripts/install.sh /tmp/camera-box
```

Releases are produced by `.github/workflows/release.yml` on every `v*` tag.
To cut a release: `git tag v0.1.0 && git push origin v0.1.0`.

## Build from source

### Natively on a Pi

The daemon is **pure Rust** (no C library dependencies — camera hotplug is read
straight from the kernel netlink uevent socket), so cross-compiling needs no
custom toolchain image or sysroot.

```sh
cargo build --release
sudo install -m 0755 target/release/camera-box /usr/local/bin/camera-box
```

### Cross-compiling (recommended for the Zero)

Using [`cross`](https://github.com/cross-rs/cross) — the stock images work as-is:

```sh
# Pi Zero W v1.1 (armv6)
cross build --release --target arm-unknown-linux-gnueabihf

# Pi Zero 2 W (64-bit OS)
cross build --release --target aarch64-unknown-linux-gnu

# Pi Zero 2 W (32-bit OS) / armv7
cross build --release --target armv7-unknown-linux-gnueabihf
```

Copy the resulting `target/<triple>/release/camera-box` to the Pi at
`/usr/local/bin/camera-box`.

## Install

```sh
# Binary
sudo install -m 0755 camera-box /usr/local/bin/camera-box

# Config (optional — defaults are used if absent)
sudo mkdir -p /etc/camera-box
sudo cp config.example.toml /etc/camera-box/config.toml

# systemd service
sudo cp systemd/camera-box.service /etc/systemd/system/camera-box.service
sudo systemctl daemon-reload
sudo systemctl enable --now camera-box.service
```

## Usage

Once running, from a client connected to the Pi's hotspot:

- Web UI: <http://192.168.4.1/>
- Camera 0 stream: <http://192.168.4.1:8080/stream>
- Camera 1 stream: <http://192.168.4.1:8081/stream>

### Logs

```sh
journalctl -u camera-box -f
```

Set log verbosity with the `RUST_LOG` env var (e.g. `RUST_LOG=debug`).

## API

### `GET /api/status`

```json
{
  "hostname": "camera-box",
  "ip_address": "192.168.4.1",
  "uptime": 1234,
  "cameras": [
    {
      "slot": 0,
      "device_path": "/dev/video0",
      "name": "USB Camera",
      "stream_url": "http://192.168.4.1:8080/stream",
      "port": 8080,
      "running": true,
      "pid": 812
    }
  ]
}
```

### `GET /api/version`

```json
{ "name": "camera-box", "version": "0.1.0", "description": "..." }
```

### `POST /api/update`

Returns `501 Not Implemented` (placeholder for a future firmware updater).

## Configuration

See [`config.example.toml`](config.example.toml). All keys are optional;
defaults: `max_cameras=2`, `base_stream_port=8080`, `web_port=80`,
`device_ip="192.168.4.1"`, `ustreamer_path="/usr/bin/ustreamer"`,
`resolution="1280x720"`, `fps=30`.

## Notes & assumptions

- The appliance is assumed offline; clients reach it via the Pi hotspot.
- The daemon runs as `root` (port 80 + `/dev/video*` + netlink uevent access).
- Binding to two capture nodes from a single physical camera is uncommon for
  UVC webcams (the second node is typically metadata-only and ignored), but if
  a camera exposes two capture interfaces it may consume two slots.
