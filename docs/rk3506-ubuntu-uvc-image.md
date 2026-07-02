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

### Get the SDK (~8 GB)

```sh
git clone --depth 1 -b luckfox-bpi https://github.com/markbirss/rk3506-ubuntu.git
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

### Select the board (non-interactive — no `lunch` needed)

`build.sh` accepts a board defconfig name directly; for the Lyra Zero W on SD:

```sh
sdk './build.sh luckfox_lyra_zero-w_ubuntu_sdmmc_defconfig'
```

That board config declares everything relevant:
`RK_KERNEL_CFG="rk3506_luckfox_defconfig"`,
`RK_KERNEL_CFG_FRAGMENTS="rk3506-display.config"`,
`RK_KERNEL_DTS_NAME="rk3506b-luckfox-lyra-zero-w-sd"`,
`RK_WIFIBT_CHIP="AIC8800DC"`.

> `build.sh` must run **as root inside the container** when the Ubuntu profile
> is selected (it refuses otherwise) — so no `--user` on `docker run`.

## 2. Enable UVC in the kernel — the essential change

⚠️ **Appending to the kernel defconfig does NOT work** in this SDK. The build
runs `make <defconfig> <fragment>.config …` and merges fragments **after** the
defconfig — `rk3506-display.config` touches the media options and silently
overrides an appended `USB_VIDEO_CLASS`. (Verified: the appended block yielded
`# CONFIG_MEDIA_USB_SUPPORT is not set`.)

The SDK-native way is to register **your own fragment, last** — later
fragments always win:

```sh
cat > kernel-6.1/arch/arm/configs/rk3506-uvc.config <<'EOF'
# camera-box: UVC (USB webcam) support
CONFIG_MEDIA_SUPPORT=y
CONFIG_MEDIA_USB_SUPPORT=y
CONFIG_MEDIA_CAMERA_SUPPORT=y
CONFIG_VIDEO_DEV=y
CONFIG_USB_VIDEO_CLASS=m
EOF

# register it after the existing fragments in the board defconfig:
sed -i 's/^RK_KERNEL_CFG_FRAGMENTS="rk3506-display.config"$/RK_KERNEL_CFG_FRAGMENTS="rk3506-display.config rk3506-uvc.config"/' \
    device/rockchip/.chips/rk3506/luckfox_lyra_zero-w_ubuntu_sdmmc_defconfig

# re-run the board select so the new fragment list is picked up:
sdk './build.sh luckfox_lyra_zero-w_ubuntu_sdmmc_defconfig'
```

After the build, verify `kernel-6.1/.config` contains
`CONFIG_USB_VIDEO_CLASS=m` **and** `CONFIG_MEDIA_USB_SUPPORT=y`.

## 3. Build the kernel + Wi-Fi driver (all you need for camera-box)

```sh
sdk './build.sh kernel'
```

Produces the Rockchip **`boot.img`** at `kernel-6.1/boot.img` (a zboot/FIT
image, also linked from `output/firmware/boot.img`) and compiles the modules
(`uvcvideo.ko` included). Stage the modules — use the **linux-gnueabihf**
toolchain from `prebuilts` (there is also a bare-metal `arm-none-eabi` one;
wrong for this):

```sh
sdk 'CROSS=$PWD/prebuilts/gcc/linux-x86/arm/gcc-arm-10.3-2021.07-x86_64-arm-none-linux-gnueabihf/bin/arm-none-linux-gnueabihf-
     make -C kernel-6.1 ARCH=arm CROSS_COMPILE=$CROSS \
          INSTALL_MOD_PATH=$PWD/.camerabox-modules INSTALL_MOD_STRIP=1 modules_install'
```

**The AIC8800DC Wi-Fi driver is an external module** (not in-tree):
`external/rkwifibt/drivers/aic8800/aic8800` (defaults already right for this
board: `CONFIG_USB_SUPPORT=y`, SDIO off). Its Makefile uses `$(PWD)` for `M=`,
so it **must be built from inside its directory** — `make -C` silently builds
nothing:

```sh
sdk 'CROSS=$PWD/prebuilts/gcc/linux-x86/arm/gcc-arm-10.3-2021.07-x86_64-arm-none-linux-gnueabihf/bin/arm-none-linux-gnueabihf-
     SDKROOT=$PWD; cd external/rkwifibt/drivers/aic8800/aic8800 &&
     make KDIR=$SDKROOT/kernel-6.1 ARCH=arm CROSS_COMPILE=$CROSS'
```

That yields `aic_load_fw/aic_load_fw.ko` and `aic8800_fdrv/aic8800_fdrv.ko`
(install them under `updates/` in the modules tree). The matching firmware
ships in the SDK at `external/rkwifibt/firmware/aicsemi/aic8800DC/` and the
driver's built-in default path is **`/lib/firmware/aic8800DC`**.

**That's the hand-off point**: the
[`scripts/build-minimal-image-luckfox-lyra-zero-w.sh`](../scripts/build-minimal-image-luckfox-lyra-zero-w.sh)
script does ALL of sections 1–3 itself when pointed at the SDK with `--sdk`,
then assembles the camera-box minimal image; deploy cards with `prepare-sd.sh`.
You're done — the rest of this page is optional.

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
