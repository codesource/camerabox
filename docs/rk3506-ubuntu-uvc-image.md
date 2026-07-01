# Building an RK3506 Ubuntu image with UVC (USB camera) support

Stock Luckfox RK3506 images (including the community Ubuntu ones) ship a kernel
built **without** USB Video Class support — these minimal images strip the whole
V4L2 / media subsystem. The symptom: a USB webcam shows up in `lsusb` but there
is no `/dev/video0`, and `modprobe uvcvideo` fails with
`Module uvcvideo not found`. Since the driver was never compiled, nothing on the
running system can fix it — you have to **rebuild the kernel** with UVC enabled
and put it in your image.

This guide builds a full **Ubuntu (systemd)** image for the Luckfox Lyra Zero W
with a UVC-enabled kernel, so camera-box can then use USB cameras. Everything
else (Wi-Fi hotspot, dashboard) already works on the stock kernel — only USB
cameras need this.

> This is real, one-time build work on a Linux PC (an hour or few, plus disk).
> If you don't specifically need the Lyra, a Raspberry Pi Zero 2 W (the same
> ARMv7 target) runs UVC cameras out of the box with no kernel work.

## What you need

- An **x86-64 Linux build host** (Ubuntu 22.04 recommended).
- **~50 GB** free disk and a few hours.
- Build dependencies:

  ```sh
  sudo apt-get update && sudo apt-get install -y \
    git ssh make gcc g++ libssl-dev liblz4-tool expect expect-dev patchelf \
    chrpath gawk texinfo diffstat binfmt-support qemu-user-static live-build \
    bison flex fakeroot cmake gcc-multilib g++-multilib unzip \
    device-tree-compiler libncurses-dev bzip2 expat gpgv2 cpp-aarch64-linux-gnu \
    libgmp-dev libmpc-dev bc python-is-python3 curl file rsync bsdmainutils scons
  ```

## 1. Get the image builder / SDK

The community RK3506 **Ubuntu** image builder wraps the Luckfox SDK (kernel +
U-Boot) with an Ubuntu root filesystem:

```sh
git clone -b luckfox-bpi https://github.com/markbirss/rk3506-ubuntu
cd rk3506-ubuntu
```

Follow that repo's **README** to bootstrap (it fetches the Luckfox Lyra SDK and
the Ubuntu rootfs artifacts). Exact bootstrap/build script names change between
versions — the repo README is the source of truth. The steps below are the
Luckfox SDK build flow the builder drives; the one part that matters and is
stable across versions is the **kernel config change in step 3**.

> If you only wanted Buildroot you could use the Luckfox SDK directly
> (`luckfox-lyra-*.tar.gz` → `.repo/repo/repo sync -l`), but camera-box needs
> systemd, so build the **Ubuntu** image.

## 2. Select the board

```sh
./build.sh lunch
# choose "Luckfox Lyra Zero W", then SD_CARD
```

## 3. Enable UVC in the kernel — the essential change

Open the kernel menuconfig:

```sh
./build.sh kernel-config
```

In menuconfig: press `/`, search `USB_VIDEO_CLASS`, jump to it, and enable
**"USB Video Class (UVC)"** as a module (`M`). menuconfig auto-selects the
dependencies (Multimedia support, Media USB adapters, V4L2 core). Then `Save`
and exit.

The options that must end up set (verify in the resulting `.config`):

```text
CONFIG_MEDIA_SUPPORT=y
CONFIG_MEDIA_USB_SUPPORT=y
CONFIG_MEDIA_CAMERA_SUPPORT=y
CONFIG_VIDEO_DEV=y          # V4L2 core (videodev)
CONFIG_USB_VIDEO_CLASS=m    # the uvcvideo driver
```

To make the change permanent (survive a `make …_defconfig`), also add those
lines to the board's kernel defconfig under the SDK's kernel tree — for 32-bit
ARM that's `arch/arm/configs/<board>_defconfig` (grep the kernel dir for the
defconfig the build actually uses).

Keeping `USB_VIDEO_CLASS=m` (a module) is fine and preferred — it loads on
plug-in; to force it at boot, add `uvcvideo` to `/etc/modules-load.d/` in the
rootfs.

## 4. Build

```sh
./build.sh kernel      # builds the kernel + uvcvideo.ko
./build.sh rootfs      # Ubuntu rootfs (per the builder)
./build.sh firmware    # packages the flashable image
```

Outputs land in `rockdev/` (e.g. `boot.img`, `rootfs.img`, and a combined
`update.img`), plus whatever SD image the Ubuntu builder emits.

## 5. Flash

- If the builder produced a raw **`.img`** (dd-able) SD image, write it like any
  other: `sudo dd if=<image>.img of=/dev/sdX bs=4M status=progress conv=fsync`.
- Otherwise use the SDK's flashing path (`rkflash.sh` / `rkdeveloptool`, or
  `update.img` via RKDevTool on Windows).

## 6. Verify UVC, then provision camera-box

Boot the board, plug in the USB camera, and:

```sh
lsmod | grep uvc || sudo modprobe uvcvideo
ls -l /dev/video*                 # /dev/video0 should now exist
v4l2-ctl --list-devices           # (sudo apt install v4l-utils)
```

Once `/dev/video0` is there, provision camera-box normally — either on the
device (`curl … install.sh | sudo bash -s -- luckfox-lyra-zero-w`) or from your
PC against your freshly built image:

```sh
sudo bash prepare-sd.sh --image <your-built-image>.img
```

See [luckfox-lyra-zero-w.md](luckfox-lyra-zero-w.md) for the full provisioning
flow.

## Notes

- **Command names vary by SDK/builder version.** Trust the repo README for the
  exact `./build.sh …` invocations; the durable, portable part of this guide is
  the kernel config in step 3.
- If you'd rather not build this yourself, the low-effort alternatives are to
  **ask the image maintainer** to add `CONFIG_USB_VIDEO_CLASS=m` (a tiny
  defconfig change) or to use a board that ships UVC (the Pi Zero 2 W runs the
  same camera-box binary).
