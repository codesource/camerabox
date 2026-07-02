# Building the RK3506 kernel with UVC (USB camera) support — the SDK, isolated in Docker

> **Automated alternative (recommended):** the camera-box **minimal image**
> script drives this exact SDK — including the Docker isolation below — and
> assembles the final image in one go: see
> [minimal-lyra-image.md](minimal-lyra-image.md). This page documents the SDK
> itself: the one-time setup, why it must run in Docker, the UVC kernel
> change, and (optionally) the SDK's own full-Ubuntu image build.

Stock Luckfox RK3506 images (including the community Ubuntu ones) ship a kernel
built **without** USB Video Class support — these minimal images strip the whole
V4L2 / media subsystem. The symptom: a USB webcam shows up in `lsusb` but there
is no `/dev/video0`, and `modprobe uvcvideo` fails with
`Module uvcvideo not found`. Since the driver was never compiled, nothing on the
running system can fix it — you have to **rebuild the kernel** with UVC enabled.

The kernel source is the
[markbirss/rk3506-ubuntu](https://github.com/markbirss/rk3506-ubuntu) repo,
branch **`luckfox-bpi`** — the repo **is** the full vendor SDK (top-level
`kernel-6.1/`, `u-boot/`, `device/rockchip/`, a prebuilt ARM toolchain under
`prebuilts/`, and the `build.sh` driver script).

## ⚠️ Do NOT set the SDK up natively on a modern Linux

The SDK's own README targets **Ubuntu 22.04** and requires **`python2`** plus:

```sh
sudo ln -sf /usr/bin/python2 /usr/bin/python   # from the SDK README — do NOT run this on your host
```

That symlink hijacks `python` for your whole system, and `python2` doesn't even
exist in the repositories of current distros (Debian 12+/Ubuntu 24.04+). So the
SDK build must be **isolated**. The repo ships its own Dockerfile
(`rk3506-ubuntu.dockerfile`: Ubuntu 22.04 + python2 + all build deps, with the
python symlink contained inside the image) — use that. Everything below runs
SDK commands inside that container; your host only needs `git` and `docker`.

## 1. One-time setup

### Get the SDK (~a few GB)

```sh
git clone -b luckfox-bpi https://github.com/markbirss/rk3506-ubuntu.git
cd rk3506-ubuntu

# select the rk3506 chip (from the SDK README, verbatim):
cd device/rockchip/.chips/rk3506
ln -s .chips/rk3506 ../../rk3506
ln -s .chips/rk3506 ../../.chip
cd ../../../../
```

### Build the container image (one-time)

```sh
docker build --rm -f rk3506-ubuntu.dockerfile -t lyra:rk3506-ubuntu-build .
```

The image's entrypoint is an interactive bash (shell-form `ENTRYPOINT`), so
one-off commands must go through `--entrypoint`. A helper you'll reuse:

```sh
# interactive shell in the build environment (SDK mounted at /build):
docker run --rm -it -v "$PWD":/build -w /build lyra:rk3506-ubuntu-build

# run one command in it:
sdk() { docker run --rm -it -v "$PWD":/build -w /build \
        --entrypoint /bin/bash lyra:rk3506-ubuntu-build -c "$*"; }
```

### Select the board (one-time, interactive)

```sh
sdk './build.sh lunch'     # choose the Luckfox Lyra Zero W board config
```

## 2. Enable UVC in the kernel — the essential change

Two equivalent ways; the **defconfig fragment** survives clean rebuilds and is
what the automated script does:

```sh
# find the board defconfig the build uses:
ls kernel-6.1/arch/arm/configs | grep -iE 'rk3506|lyra'

cat >> kernel-6.1/arch/arm/configs/<board>_defconfig <<'EOF'

# camera-box: UVC (USB webcam) support — do not remove
CONFIG_MEDIA_SUPPORT=y
CONFIG_MEDIA_USB_SUPPORT=y
CONFIG_MEDIA_CAMERA_SUPPORT=y
CONFIG_VIDEO_DEV=y
CONFIG_USB_VIDEO_CLASS=m
EOF
rm -f kernel-6.1/.config      # a stale .config would shadow the change
```

Or interactively (inside the container): `sdk './build.sh kernel-config'` /
menuconfig → `/` → search `USB_VIDEO_CLASS` → `M` → save. Either way, verify
after the build that `kernel-6.1/.config` contains `CONFIG_USB_VIDEO_CLASS=m`.

## 3. Build the kernel (all you need for camera-box)

```sh
sdk './build.sh kernel'
```

This produces the Rockchip **`boot.img`** (kernel + dtb packed; look in
`kernel-6.1/`, `output/`, or `rockdev/` depending on SDK version) and the
modules tree. To stage the modules for a rootfs (what the automated script
does):

```sh
sdk 'make -C kernel-6.1 ARCH=arm \
     CROSS_COMPILE=$PWD/prebuilts/gcc/linux-x86/arm/*/bin/arm-*- \
     INSTALL_MOD_PATH=$PWD/.camerabox-modules INSTALL_MOD_STRIP=1 modules_install'
```

**That's the hand-off point**: give the `boot.img` + modules to
[`scripts/build-minimal-image-luckfox-lyra-zero-w.sh`](../scripts/build-minimal-image-luckfox-lyra-zero-w.sh)
(it runs steps 2–3 itself when you point it at the SDK with `--sdk`), which
assembles the camera-box minimal image; then deploy cards with
`prepare-sd.sh`. You're done — the rest of this page is optional.

## 4. Optional: the SDK's own full-Ubuntu image

Only if you want the SDK's complete Ubuntu image instead of the camera-box
minimal rootfs. It additionally needs the Ubuntu rootfs tarball (note:
extracting it needs `7z` — `p7zip-full`, which the SDK README's dependency
list omits):

```sh
git clone https://github.com/markbirss/ubuntu_24.04.3.git
cd ubuntu_24.04.3
7z x ubuntu_24.04.3.7z.001
sha256sum ubuntu_24.04.3.tar.gz
mv ubuntu_24.04.3.tar.gz ../
cd .. && rm -rf ubuntu_24.04.3
mkdir -p ubuntu && mv ubuntu_24.04.3.tar.gz ubuntu/

sdk './build.sh'            # full build (u-boot + kernel + rootfs + firmware)
```

Outputs land under `output/`/`rockdev/` (`boot.img`, `rootfs.img`,
`update.img`). Flashing: `./rkflash.sh update` flashes over **USB** (MASKROM /
`rkdeveloptool`) — that path needs the board attached to the build host; for an
SD card, dd-able assembly from the parts is exactly what the minimal-image
script automates, so prefer that.

## Notes

- **Never install the SDK deps on the host.** Anything that fails outside the
  container (python2, old make/gcc assumptions) is expected — rerun it inside.
- SDK layouts drift between versions: `build.sh` sub-commands and the
  `boot.img` output path are the two things to re-check after pulling. The
  durable part of this guide is the **UVC config block in step 2**.
- If you'd rather not build at all, the low-effort alternative remains a board
  that ships UVC (a Pi Zero 2 W runs the same camera-box binary out of the
  box).
