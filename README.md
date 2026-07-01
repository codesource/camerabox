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
  Each stream can optionally require its own username/password.
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
- Also runs on the Luckfox Lyra Zero W (Rockchip RK3506B, ARMv7) on its
  **Ubuntu image** — see the install table below
- Raspberry Pi OS Lite / Raspbian (tested on trixie), **Linux only**
- Runs as a `systemd` service, as root
- Wi-Fi AP via `hostapd` + `dnsmasq`; default AP IP `192.168.4.1`

## Prerequisites on the Pi

Raspberry Pi OS Lite, plus these packages. **The installer below installs them
for you** — this list is for reference or a manual install:

```sh
sudo apt update
sudo apt install -y ustreamer hostapd dnsmasq iw wpasupplicant isc-dhcp-client
```

| Tool | Package | Used for |
| --- | --- | --- |
| `ustreamer` | `ustreamer` | the actual MJPEG streaming (**required**) |
| `hostapd` | `hostapd` | Wi-Fi hotspot (AP mode) |
| `dnsmasq` | `dnsmasq` | DHCP for hotspot clients |
| `iw`, `wpa_supplicant` | `iw`, `wpasupplicant` | Wi-Fi scanning + connecting as a client |
| `dhclient` | `isc-dhcp-client` | getting an IP as a Wi-Fi client |
| `hostnamectl`, `rfkill` | (base system) | set the hostname / unblock the radio |

## Install (recommended)

Every release ships prebuilt static binaries, so the Pi needs **no build tools
and no repo checkout**. Run one line on the Pi with your board's name:

```sh
curl -fsSL https://raw.githubusercontent.com/codesource/camerabox/main/scripts/install.sh \
  | sudo bash -s -- pi-zero-w-armv6
```

| Board | Use this name |
| --- | --- |
| Pi Zero W v1.1 (ARMv6) | `pi-zero-w-armv6` |
| Pi Zero 2 W (32-bit OS) | `pi-zero-2w-armv7` |
| Pi Zero 2 W (64-bit OS) | `pi-zero-2w-arm64` |
| Luckfox Lyra Zero W (RK3506B) | `luckfox-lyra-zero-w` |

> **Luckfox Lyra Zero W** (Rockchip RK3506B, triple Cortex-A7) is 32-bit ARMv7,
> so it runs the same static binary as the Pi Zero 2 W (32-bit). Use the
> **Ubuntu image** (it has `systemd`; the Buildroot/BusyBox image does not, which
> camera-box requires). On first run, confirm `ustreamer` installed, a USB
> camera shows up as `/dev/video0`, and the AIC8800 Wi-Fi starts the hotspot.

### Prepare an SD card from your PC (offline first boot)

To bake everything onto the card so the board boots ready as a hotspot — no
setup on the device — run [`scripts/prepare-sd.sh`](scripts/prepare-sd.sh) on
your Linux PC. It lets you **pick the SD card** interactively, optionally flashes
the Ubuntu image, grows the root filesystem, installs the dependencies into the
ARM rootfs (via a `qemu-user-static` chroot), installs camera-box, pre-writes the
hotspot config, and enables the services:

No repo clone needed — grab just the one script (with no `--binary` it lists the
binaries in the latest release and lets you pick one, defaulting to the ARMv7
build the Lyra needs):

```sh
sudo apt install qemu-user-static binfmt-support parted e2fsprogs \
                 cloud-guest-utils gdisk curl                        # one-time
curl -fsSL https://raw.githubusercontent.com/codesource/camerabox/main/scripts/prepare-sd.sh -o prepare-sd.sh

# flash the Ubuntu image AND provision, in one command (--image takes a URL or file):
sudo bash prepare-sd.sh \
  --image https://github.com/platima/SBC-Images/raw/main/Luckfox/Lyra/Lyra%20Zero%20W/<image>.img.bz2
# customise the hotspot: --ssid MyBox --pass MySecret123 --ip 192.168.5.1/24
# already flashed the card? drop --image and it just provisions.
# built the binary yourself? point at it with --binary ./camera-box
```

On boot the board hosts the hotspot; browse `http://192.168.4.1/` (admin /
password). For a Raspberry Pi the plain [Install](#install-recommended) one-liner
is simpler — this offline route is for boards (like the Lyra) you want fully
provisioned before first boot.

> Full walkthrough — flashing the Ubuntu image, the automated route, **and** the
> equivalent manual steps — is in
> [docs/luckfox-lyra-zero-w.md](docs/luckfox-lyra-zero-w.md). USB cameras need a
> UVC-enabled kernel, which the stock image lacks. The **recommended** way to get
> a ready-to-run image (UVC kernel + camera-box + hotspot baked in, boots as an
> AP) is to build a custom **Armbian** image —
> [docs/armbian-lyra-image.md](docs/armbian-lyra-image.md); the from-SDK Ubuntu
> route is in [docs/rk3506-ubuntu-uvc-image.md](docs/rk3506-ubuntu-uvc-image.md).

The installer downloads the right binary, installs the dependencies above, sets
up and starts the `systemd` service, and writes a default config (it never
overwrites an existing one). When it finishes, open the dashboard (see
[Usage](#usage)).

To update later, just run the same command again.

> Prefer to inspect first? Download
> [`scripts/install.sh`](scripts/install.sh), read it, then run
> `sudo bash install.sh pi-zero-w-armv6`.

## Usage

From a client connected to the Pi's hotspot (or on the same network):

- **Dashboard**: <http://192.168.4.1/> — log in with **admin / password** (change
  it under **Settings**).
- **Streams**: `http://192.168.4.1:8080/stream`, `…:8081/stream`, one port per
  enabled camera. For a stream you protected, use
  `http://user:pass@192.168.4.1:8080/stream` (works in VLC, ffmpeg, etc.).

The stream URLs and reported IP use whatever address you connected with, so
`http://camera-box.local/` works too where mDNS is available.

### Forgot the password?

```sh
sudo systemctl stop camera-box
sudo /usr/local/bin/camera-box reset-password                # back to admin / password
sudo /usr/local/bin/camera-box reset-password alice s3cret   # or set your own
sudo systemctl start camera-box
```

### Daemon logs

```sh
journalctl -u camera-box -f          # or the Logs tab in the UI
```

Set verbosity with `RUST_LOG` (e.g. `RUST_LOG=debug`).

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
  8080+) are served with only the optional per-camera stream password, if set.
- With a single `wlan0`, switching it from AP to client drops the hotspot — so
  you'd disconnect yourself if managing over that AP. A second (USB) Wi-Fi
  adapter avoids this: keep `wlan0` as the AP and use the dongle as the uplink.
- On the Pi Zero W, concurrent viewers/streams are limited by Wi-Fi bandwidth,
  not the software (`ustreamer` is multi-client); lower resolution/fps if needed.

## License

Released under the [MIT License](LICENSE) © 2026 Matthias Toscanelli.

---

## For developers

### How it works

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

### Build from source

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
Then install the binary with the same script, pointing it at your build:

```sh
sudo bash scripts/install.sh ./target/arm-unknown-linux-musleabihf/release/camera-box
```

Tagged releases (`git tag vX.Y.Z && git push origin vX.Y.Z`) are built and
published automatically by `.github/workflows/release.yml`.

### API

All routes require a session cookie except `GET /login` and `POST /api/login`.

| Method & path | Purpose |
| --- | --- |
| `POST /api/login` / `logout` | Start / end a session. |
| `POST /api/account` | Change username/password. |
| `GET /api/status` | Hostname, IP, uptime, and the camera list. |
| `POST /api/cameras/:id/{enable,disable,mode,auth}` | Control a camera (id = bus path). |
| `GET /api/system` | Model, CPU, RAM, disk, temperature, time, byte counters. |
| `GET /api/logs` | Recent log lines. |
| `GET /api/network` | Wi-Fi interfaces + saved profiles. |
| `POST /api/network/{scan,hotspot,connect}` | Scan / AP / client. |
| `POST /api/network/profile/{add,remove,connect}` | Manage Wi-Fi profiles. |
| `POST /api/hostname` | Set the hostname. |
| `GET /api/version`, `POST /api/update` | Version; update placeholder (`501`). |
