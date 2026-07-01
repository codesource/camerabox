# camera-box

A small Rust daemon that turns a **Raspberry Pi Zero W / Zero 2 W** into a USB
camera appliance with a little **router-style web dashboard**. It auto-detects
USB UVC cameras and exposes each as a **low-latency MJPEG HTTP stream**, and lets
you manage cameras, Wi-Fi, and the device itself from one password-protected UI.

The actual video streaming is **not** done in Rust — it is delegated to
[`ustreamer`](https://github.com/pikvm/ustreamer). The daemon manages devices,
processes, networking, and the UI/API.

## Features

- **Cameras** — lists every connected USB camera; enable/disable each stream and
  pick its resolution/fps from the camera's advertised modes. Choices persist
  across reboots. MJPEG passthrough when supported; crashed streams auto-restart.
- **Network** — switch the built-in `wlan0` between **hotspot (AP)** and
  **client**, scan for networks, save Wi-Fi profiles, and configure an extra USB
  Wi-Fi adapter as a client (DHCP or static).
- **System** — model, version, local time, uptime, CPU, RAM, disk, SoC
  temperature, and a live per-interface bandwidth monitor.
- **Logs** — recent daemon logs in the browser.
- **Settings** — change the hostname (reachable as `<hostname>.local`, including
  in AP mode) and the login password.
- **Login** — form-based login with a session cookie. Default **admin /
  password**; change it in the UI, or reset from the CLI if forgotten.

## Target platform

- Raspberry Pi Zero W v1.1 (armv6) or Pi Zero 2 W (armv7/aarch64)
- Raspberry Pi OS Lite / Raspbian (tested on trixie), **Linux only**
- Runs as a `systemd` service, as root
- Wi-Fi AP via `hostapd` + `dnsmasq`; default AP IP `192.168.4.1`

## How it works

```text
USB camera --uevent--> camera.rs (detect, enumerate modes, assign port)
                            |
                            v
                     stream.rs (spawn & supervise one ustreamer per camera)
                            |
                            v
                       ustreamer --MJPEG--> http://<host>:8080/stream
                                            http://<host>:8081/stream

Browser --HTTP(login)--> web.rs : / (dashboard)  /api/status /api/system
                                   /api/network*  /api/logs   /api/hostname
                                   /api/login /api/logout /api/account
```

- **All USB cameras are listed.** A newly-seen camera starts **disabled**; you
  enable it and choose a resolution/fps in the UI. Each enabled camera takes the
  next free stream port (`8080 + n`); there is no fixed cap on how many stream
  at once.
- **Only USB video *capture* devices are used.** Nodes are probed with
  `VIDIOC_QUERYCAP` and must report `V4L2_CAP_VIDEO_CAPTURE` **and** a `usb-…`
  bus — so the Pi's on-board `bcm2835-isp`/`-codec` and metadata-only nodes are
  ignored. `VIDIOC_ENUM_FRAMESIZES`/`_FRAMEINTERVALS` enumerate the mode list.
- **No C dependencies** — camera hotplug is read straight from the kernel
  netlink uevent socket, so cross-compiling needs no sysroot.

## Source layout

| File             | Responsibility                                                  |
|------------------|----------------------------------------------------------------|
| `src/main.rs`    | Startup, CLI (`reset-password`), logging, run web server.      |
| `src/config.rs`  | `/etc/camera-box/config.toml` + persisted per-camera state.    |
| `src/camera.rs`  | Shared state, V4L2 probing/modes, uevent monitor, enable/mode. |
| `src/stream.rs`  | Per-camera `ustreamer` supervisor (start/stop/restart).        |
| `src/net.rs`     | Wi-Fi management (AP/client, scan, profiles), hostname/mDNS.    |
| `src/sys.rs`     | System overview + per-interface byte counters.                 |
| `src/logs.rs`    | In-memory log ring buffer for the Logs tab.                    |
| `src/auth.rs`    | Credentials (hashed) + sessions.                               |
| `src/web.rs`     | Login, dashboard, and the JSON/control API.                    |
| `src/update.rs`  | `GET /api/version`, `POST /api/update` (placeholder).          |

## Prerequisites on the Pi

```sh
sudo apt update
sudo apt install ustreamer        # provides /usr/bin/ustreamer
```

Wi-Fi management uses tools already present on Raspberry Pi OS (`iw`,
`wpa_supplicant`, `dhclient`, `hostapd`, `dnsmasq`, `hostnamectl`).

## Install a prebuilt release (easiest)

Each tagged release ships prebuilt static binaries — the Pi needs no build tools:

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

Releases are built by `.github/workflows/release.yml` on every `v*` tag
(`git tag vX.Y.Z && git push origin vX.Y.Z`).

## Build from source

The daemon is **pure Rust**, so cross-compiling needs no custom toolchain image.
Using [`cross`](https://github.com/cross-rs/cross) — static musl builds run on
any Pi OS version:

```sh
# Pi Zero W v1.1 (armv6)
cross build --release --target arm-unknown-linux-musleabihf

# Pi Zero 2 W (64-bit / 32-bit)
cross build --release --target aarch64-unknown-linux-musl
cross build --release --target armv7-unknown-linux-musleabihf
```

Or build natively on the Pi with `cargo build --release` (slow on the Zero W).
Copy the binary to `/usr/local/bin/camera-box`.

## Manual install

```sh
sudo install -D -m 0755 camera-box /usr/local/bin/camera-box
sudo mkdir -p /etc/camera-box && sudo cp config.example.toml /etc/camera-box/config.toml
sudo cp systemd/camera-box.service /etc/systemd/system/camera-box.service
sudo systemctl daemon-reload && sudo systemctl enable --now camera-box.service
```

## Usage

From a client connected to the Pi's hotspot (or on the same network):

- Dashboard: <http://192.168.4.1/> — log in with **admin / password** (change it
  under **Settings**).
- Streams: `http://192.168.4.1:8080/stream`, `…:8081/stream`.

The stream URLs and reported IP use whatever address you connected with, so
`http://camera-box.local/` works too where mDNS is available.

### Forgot the password?

```sh
sudo systemctl stop camera-box
sudo /usr/local/bin/camera-box reset-password           # back to admin / password
sudo /usr/local/bin/camera-box reset-password alice s3cret   # or set your own
sudo systemctl start camera-box
```

### Daemon logs

```sh
journalctl -u camera-box -f          # or the Logs tab in the UI
```

Set verbosity with `RUST_LOG` (e.g. `RUST_LOG=debug`).

## API

All routes require a session cookie except `GET /login` and `POST /api/login`.

| Method & path | Purpose |
|---|---|
| `POST /api/login` / `logout` | Start / end a session. |
| `POST /api/account` | Change username/password. |
| `GET /api/status` | Hostname, IP, uptime, and the camera list. |
| `POST /api/cameras/:id/{enable,disable,mode}` | Control a camera (id = bus path). |
| `GET /api/system` | Model, CPU, RAM, disk, temperature, time, byte counters. |
| `GET /api/logs` | Recent log lines. |
| `GET /api/network` | Wi-Fi interfaces + saved profiles. |
| `POST /api/network/{scan,hotspot,connect}` | Scan / AP / client. |
| `POST /api/network/profile/{add,remove,connect}` | Manage Wi-Fi profiles. |
| `POST /api/hostname` | Set the hostname. |
| `GET /api/version`, `POST /api/update` | Version; update placeholder (`501`). |

## Configuration

See [`config.example.toml`](config.example.toml) — all keys optional. Defaults:
`base_stream_port=8080`, `web_port=80`,
`device_ip="192.168.4.1"` (fallback only), `ustreamer_path="/usr/bin/ustreamer"`,
`resolution="1280x720"`, `fps=30`. Login credentials are **not** in this file —
they live (hashed) in `/var/lib/camera-box/auth.toml`; per-camera choices live in
`/var/lib/camera-box/state.toml`.

## Notes & limitations

- The daemon runs as `root` (port 80, `/dev/video*`, netlink, and the Wi-Fi
  tools it drives).
- The login covers the dashboard/API only; the `ustreamer` MJPEG streams (ports
  8080+) are served without authentication.
- With a single `wlan0`, switching it from AP to client drops the hotspot — so
  you'd disconnect yourself if managing over that AP. A second (USB) Wi-Fi
  adapter avoids this: keep `wlan0` as the AP and use the dongle as the uplink.
- On the Pi Zero W, concurrent viewers/streams are limited by Wi-Fi bandwidth,
  not the software (`ustreamer` is multi-client); lower resolution/fps if needed.
