# The camera-box minimal image for the Luckfox Lyra Zero W

This is the **recommended** way to run camera-box on the Lyra Zero W: build our
own minimal OS image once, then deploy it to as many SD cards as you like.
The work is split across exactly two scripts:

| Step | Script | How often |
| --- | --- | --- |
| **Build** the image | [`scripts/build-minimal-image-luckfox-lyra-zero-w.sh`](../scripts/build-minimal-image-luckfox-lyra-zero-w.sh) | once (per kernel/rootfs change) |
| **Deploy** a card | [`scripts/prepare-sd.sh`](../scripts/prepare-sd.sh) | per SD card / per box |

## Why our own image

Every prebuilt image for this board ships the vendor 6.1 kernel with the whole
media/V4L2 subsystem compiled out (`CONFIG_MEDIA_SUPPORT` unset) — so a USB
camera shows in `lsusb` but **never gets `/dev/video0`**, and `modprobe
uvcvideo` can't fix a driver that was never compiled. On top of that, the
general-purpose rootfses carry netplan/networkd/cloud tooling that fights
camera-box for `wlan0`. The minimal image fixes both, permanently:

- **Kernel** — built from the **rk3506-ubuntu vendor SDK**
  ([markbirss/rk3506-ubuntu](https://github.com/markbirss/rk3506-ubuntu),
  branch `luckfox-bpi` — the same source the community Ubuntu image uses, so
  the AIC8800 Wi-Fi keeps working), with UVC enabled:
  `CONFIG_USB_VIDEO_CLASS=m` plus its V4L2 dependencies. The SDK build runs
  **inside Docker** — never natively (it needs python2, see below).
- **Rootfs** — a `debootstrap` Debian *minbase* containing **only** the
  camera-box requirements: `systemd`, `sshd`, `hostapd`, `dnsmasq`, `iw`,
  `wpa_supplicant`, `dhclient`, `avahi-daemon`, `rfkill`, `ustreamer`
  (plus tiny debug helpers: `usbutils`, `v4l-utils`, `htop`). No netplan, no
  NetworkManager, no cloud-init — nothing to steal `wlan0` from camera-box.
- **Boot chain** — the partition table comes from a known-good dd-able base
  image (e.g. `Luckfox_Lyra_Zero_W-2503_Ubuntu.img.bz2` from
  [platima/SBC-Images](https://github.com/platima/SBC-Images/tree/main/Luckfox/Lyra/Lyra%20Zero%20W)),
  but the boot chain is built fresh and written **in full**: idblock at
  sector 64 + u-boot in the `uboot` partition + kernel in the `boot`
  partition, all from the same SDK build. Important discovery: the community
  base image carries **no loader on the SD at all** (sector 64 is empty — it
  relied on a factory u-boot in the board's SPI flash), so it stops booting
  once the SPI is erased. The camera-box minimal image is **self-booting**,
  SPI erased or not. The AIC8800 firmware blobs come from the SDK (with a
  base-image harvest as fallback).

The built image is **generic**: no camera-box binary, no hotspot credentials.
Those are the deployment layer — `prepare-sd.sh` installs the binary and writes
the per-box configuration (SSID/password/IP, root password), exactly as it does
for any other image. Because the dependencies are already baked in, its slow
emulated `apt` step is skipped automatically (it detects the pre-installed
packages).

## 1. One-time build-host setup

An x86-64 Linux PC, ~50 GB free disk, **Docker**, and:

```sh
sudo apt install debootstrap qemu-user-static binfmt-support parted \
                 e2fsprogs rsync curl bzip2 xz-utils file
```

**The SDK** (for the kernel) is
[markbirss/rk3506-ubuntu](https://github.com/markbirss/rk3506-ubuntu), branch
`luckfox-bpi` — the repo *is* the full vendor SDK. Its build environment needs
Ubuntu 22.04 + **python2** + a global `python`→python2 symlink, so it must
**never run natively on a modern host**: all SDK commands run inside the
Docker image the SDK itself ships, and the build script enforces that
(it builds the Docker image automatically if missing; `--native` opts out).
Full details: [rk3506-ubuntu-uvc-image.md](rk3506-ubuntu-uvc-image.md).
One-time setup:

```sh
git clone --depth 1 -b luckfox-bpi https://github.com/markbirss/rk3506-ubuntu.git
cd rk3506-ubuntu
# select the rk3506 chip (verbatim from the SDK README):
( cd device/rockchip/.chips/rk3506 && \
  ln -s .chips/rk3506 ../../rk3506 && ln -s .chips/rk3506 ../../.chip )
```

That's the whole SDK setup. The build script does the rest itself: it builds
the SDK's Docker image if missing, selects the Lyra Zero W board
**non-interactively** (`./build.sh luckfox_lyra_zero-w_ubuntu_sdmmc_defconfig`
— no interactive `lunch`), registers the UVC kernel-config fragment, and
builds the kernel **and the external AIC8800 USB Wi-Fi driver** (with its
firmware, installed to the driver's default `/lib/firmware/aic8800DC`).
The Ubuntu-rootfs download from the SDK README is **not** needed — only the
kernel is built; the camera-box rootfs comes from `debootstrap`.

**Base image**: download a dd-able Ubuntu image for the Lyra Zero W from
[platima/SBC-Images](https://github.com/platima/SBC-Images/tree/main/Luckfox/Lyra/Lyra%20Zero%20W)
(e.g. `Luckfox_Lyra_Zero_W-2503_Ubuntu.img.bz2`). Its boot chain is reused;
nothing else from it survives except the Wi-Fi firmware blobs.

## 2. Build the image

```sh
sudo bash scripts/build-minimal-image-luckfox-lyra-zero-w.sh \
  --sdk ~/rk3506-ubuntu \
  --base-image ./Luckfox_Lyra_Zero_W-2503_Ubuntu.img.bz2
```

The script is interactive: it prompts for anything missing, **checks the free
disk space** in the chosen work/output folders against what the build needs,
and shows a build plan you confirm before the long part starts. Useful flags:

- `--out FILE.img` / `--work DIR` — where the image and scratch files go.
- `--skip-kernel` — reuse the SDK's existing kernel build (fast iteration on
  the rootfs).
- `--keep-base-kernel` — don't build a kernel at all: swap only the rootfs and
  keep the base image's stock kernel + modules. **No UVC** — this mode exists
  to validate the minimal rootfs (hotspot + dashboard) against the proven
  stock kernel before you trust your own kernel build.
- `--suite`, `--mirror` — Debian release (default `trixie`) and mirror.
- `--native` — run SDK commands on the host instead of Docker. Only for a
  dedicated Ubuntu 22.04 build box with python2 set up; **not recommended**.
- `--yes` — non-interactive (CI); fails instead of prompting.

First kernel build takes 20–60 min; the emulated debootstrap ~10–20 min.
Output: `camera-box-lyra-minimal.img`.

## 3. Deploy each SD card

```sh
sudo bash scripts/prepare-sd.sh \
  --image ./camera-box-lyra-minimal.img \
  --ssid CameraBox --pass CameraBox123 --ip 192.168.4.1/24 \
  --root-pass 'change-me'
```

`prepare-sd.sh` lets you pick the SD card, flashes the image, installs the
camera-box binary (downloads `camera-box-luckfox-lyra-zero-w` from the latest
release if you don't pass `--binary`), writes the hotspot config, presets the
root login, and enables the services. Different boxes = same image, different
`--ssid`/`--ip`.

## 4. First boot

1. On a **fresh board** whose SPI flash still holds the factory
   Buildroot system, erase the SPI once so the board boots from SD — see
   [luckfox-lyra-zero-w.md](luckfox-lyra-zero-w.md#troubleshooting). (An
   already-erased SPI is fine: this image carries its own complete boot
   chain.)
2. Insert the card, power on, wait ~30 s.
3. Connect to the **CameraBox** Wi-Fi → `http://192.168.4.1/`
   (admin / password), or `ssh root@192.168.4.1`.
4. Plug in a USB camera and check: `ls -l /dev/video*` → `/dev/video0`
   (the UVC driver is in this kernel; `v4l2-ctl --list-devices` to inspect).

## Troubleshooting

- **The build can't find `boot.img` / the defconfig.** SDK layouts vary a bit
  between versions; the script prints what it looked for — adjust the paths at
  the top of the corresponding phase.
- **`WARN: no aic8800 module in the kernel build output`.** In some SDK
  versions the Wi-Fi driver is an external module rather than in-tree. Without
  it there is no `wlan0` and no hotspot: find it in the SDK (e.g.
  `$SDK/external/*aic*`, or the Wi-Fi section of the board's `BoardConfig`)
  and make sure it gets built and installed.
- **No hotspot on the booted board.** `journalctl -u hostapd -b` over SSH or a
  USB-serial console; check `iw dev` shows `wlan0` and `dmesg | grep -i aic`
  shows the driver + firmware loading. The firmware blobs live in
  `/lib/firmware/aic8800*` (harvested from the base image at build time).
- **Flashed the bare image without `prepare-sd.sh`?** It boots inert but
  reachable: no hotspot, `ssh root@<dhcp-ip>` password `camerabox`. Run
  `prepare-sd.sh` against the card to finish provisioning.
- **Inspect a card that won't come up:**
  `sudo bash scripts/diagnose-sd.sh /dev/sdX3` (read-only).
