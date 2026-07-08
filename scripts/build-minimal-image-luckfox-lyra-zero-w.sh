#!/usr/bin/env bash
#
# build-minimal-image-luckfox-lyra-zero-w.sh — BUILD a minimal, dd-able OS image
# for the Luckfox Lyra Zero W (RK3506B) with everything camera-box needs baked
# in. It only BUILDS; deploying a card stays with scripts/prepare-sd.sh, so the
# flow is:
#
#   1. build the image ONCE:      sudo bash scripts/build-minimal-image-luckfox-lyra-zero-w.sh
#   2. deploy each SD card:       sudo bash scripts/prepare-sd.sh --image <built.img> ...
#
# prepare-sd.sh remains the single install/configuration layer (camera-box
# binary, hotspot SSID/password/IP, root password); this image is generic.
#
# Why this exists: the prebuilt images for this board ship the vendor 6.1
# kernel with the whole media/V4L2 subsystem compiled out (CONFIG_MEDIA_SUPPORT
# unset) — so USB cameras never get /dev/video0. And their general-purpose
# rootfses (netplan/networkd/cloud tooling) fight camera-box for wlan0. This
# builds both halves right — camera-box owns its own image:
#
#   KERNEL — built from the rk3506-ubuntu SDK (markbirss/rk3506-ubuntu, branch
#            luckfox-bpi — the vendor Luckfox/Rockchip tree the proven
#            community image uses, so AIC8800 Wi-Fi keeps working), with UVC
#            enabled (CONFIG_USB_VIDEO_CLASS=m + V4L2 deps). The SDK build
#            runs INSIDE DOCKER (the image its own rk3506-ubuntu.dockerfile
#            defines): the SDK needs Ubuntu 22.04 + python2 + a global
#            `python` -> python2 symlink, which must never touch your host.
#            Pass --native to run it on the host anyway (at your own risk).
#   ROOTFS — a debootstrap'd Debian *minbase* with ONLY the camera-box
#            requirements: systemd, sshd, hostapd, dnsmasq, iw, wpa_supplicant,
#            dhclient, avahi, rfkill, ustreamer (+ tiny debug helpers:
#            usbutils, v4l-utils). No netplan, no NetworkManager, no cloud-init.
#   BOOT   — the partition table comes from a known-good dd-able base image
#            (e.g. Luckfox_Lyra_Zero_W-2503_Ubuntu), but the boot chain is
#            built fresh and written in FULL: idblock at sector 64 + u-boot
#            in the 'uboot' partition + kernel in the 'boot' partition, all
#            from the same SDK build. The community base image itself carries
#            NO loader on the SD (it relied on a factory u-boot in SPI
#            flash!) — this image is SELF-BOOTING, SPI erased or not.
#
# Run on an x86-64 Linux PC, as root. It is INTERACTIVE: missing inputs are
# prompted for, and it checks free disk space in the chosen folders before
# starting. One-time SDK setup (see docs/rk3506-ubuntu-uvc-image.md):
#
#   git clone --depth 1 -b luckfox-bpi https://github.com/markbirss/rk3506-ubuntu.git
#   cd rk3506-ubuntu/device/rockchip/.chips/rk3506
#   ln -s .chips/rk3506 ../../rk3506 && ln -s .chips/rk3506 ../../.chip
#
# That's all: this script builds the SDK's docker image itself if missing and
# selects the Lyra Zero W board non-interactively ('./build.sh
# luckfox_lyra_zero-w_ubuntu_sdmmc_defconfig' — no './build.sh lunch' needed).
# The Ubuntu rootfs download from the SDK README is NOT needed either — only
# the kernel is built here; the rootfs comes from debootstrap.
#
#   sudo bash scripts/build-minimal-image-luckfox-lyra-zero-w.sh \
#       [--sdk ~/rk3506-ubuntu] \
#       [--base-image ./Luckfox_...img.bz2 | URL]  # local path OR http(s) URL \
#       [--suite trixie] [--mirror URL] [--out FILE.img] [--work DIR] \
#       [--skip-kernel]        # reuse the SDK's existing kernel build output
#       [--keep-base-kernel]   # don't touch the kernel: swap ONLY the rootfs
#                              # (keeps the base image's modules/firmware; no
#                              # UVC — for validating the minimal rootfs
#                              # against the proven stock kernel first)
#       [--native]             # run SDK commands on the host instead of docker
#                              # (needs Ubuntu 22.04 + python2 — NOT recommended)
#       [--yes]                # non-interactive: no prompts, fail if unsure
#
# The image boots inert-but-reachable if flashed as-is (ssh root/camerabox, no
# hotspot); prepare-sd.sh is what installs camera-box and arms the hotspot.
#
# Requires: debootstrap, an ARM user-mode qemu + binfmt registration, parted,
# e2fsprogs, rsync, curl, bzip2, xz-utils, file. On older Debian/Ubuntu the qemu
# piece is the single 'qemu-user-static' package; on Debian 13 / Ubuntu 24.10+
# that's a virtual package — install 'qemu-user qemu-user-binfmt' instead and
# register with 'sudo systemctl restart systemd-binfmt' (the arm interpreter must
# have the binfmt 'F' flag: grep F /proc/sys/fs/binfmt_misc/qemu-arm).
#   # older hosts:
#   sudo apt install debootstrap qemu-user-static binfmt-support parted \
#                    e2fsprogs rsync curl bzip2 xz-utils file
#   # Debian 13 / Ubuntu 24.10+:
#   sudo apt install debootstrap qemu-user qemu-user-binfmt parted \
#                    e2fsprogs rsync curl bzip2 xz-utils file
#   sudo systemctl restart systemd-binfmt
#
# First kernel build takes a while (20-60 min); debootstrap under qemu ~10-20
# min. Flashing + the one-time SPI erase are covered by prepare-sd.sh and
# docs/luckfox-lyra-zero-w.md.
#
# NOTE: written against the documented SDK/base-image layouts; not yet verified
# end-to-end on Lyra hardware. It fails loudly at each step it can't verify.
set -euo pipefail

# --- defaults ----------------------------------------------------------------
SDK=""
BASE_IMAGE=""
SUITE="trixie"
MIRROR="http://deb.debian.org/debian"
OUT=""
WORK=""
SKIP_KERNEL=""
KEEP_BASE_KERNEL=""
NATIVE=""
ASSUME_YES=""
DEFAULT_ROOT_PASS="camerabox"   # image fallback; prepare-sd.sh --root-pass overrides per card
DOCKER_IMG="lyra:rk3506-ubuntu-build"   # the SDK's own rk3506-ubuntu.dockerfile

# Everything the appliance needs — and nothing that manages the network.
PKGS_CORE="systemd systemd-sysv systemd-timesyncd udev dbus kmod
           openssh-server iproute2 hostapd dnsmasq iw wpasupplicant
           isc-dhcp-client avahi-daemon rfkill wireless-regdb ca-certificates"
# Small but invaluable on a headless camera box; trim if you want it tighter.
PKGS_DEBUG="usbutils v4l-utils iputils-ping htop"
# USB Wi-Fi dongle support for the second radio (wlan1): firmware blobs
# (Ralink/MediaTek in firmware-misc-nonfree; Realtek incl. rtw88's rtw8822c_fw in
# firmware-realtek — both need the 'non-free-firmware' component enabled in the
# rootfs sources.list below) plus usb-modeswitch, which flips Realtek CD-ROM-mode
# dongles (e.g. RTL8822CU, 0bda:1a2b) into WLAN mode (0bda:c812) on plug-in.
# The on-board AIC8800 uses none of these — its firmware is harvested separately.
PKGS_WIFI="firmware-misc-nonfree firmware-realtek usb-modeswitch usb-modeswitch-data"

die()  { echo "error: $*" >&2; exit 1; }
warn() { echo ">> WARN: $*" >&2; }
info() { echo ">> $*"; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing tool: $1 (see the header for the apt install line)"; }

# Reject the downloads that silently look like an image but aren't: a Git LFS
# pointer, an HTML error page, or a truncated file — and check the compression
# magic matches the extension. Diagnostics go to stderr so callers can capture
# a resolved path on stdout. die() aborts; wrap in a subshell for a soft check.
validate_base_image() {
    local f="$1" sz sig
    sz="$(stat -c%s "$f" 2>/dev/null || echo 0)"
    [[ "$sz" -ge 1048576 ]] || die "'$f' is only ${sz} bytes — too small to be a disk
     image (a Git LFS pointer or an error page?). For platima/SBC-Images use the
     github.com/.../raw/ URL, NOT raw.githubusercontent.com (which returns the pointer)."
    sig="$(head -c8 "$f" | od -An -tx1 | tr -d ' \n')"
    case "$f" in
        *.bz2) [[ "$sig" == 425a68* ]]       || die "'$f' is not a bzip2 file (magic '$sig') — re-download it." ;;
        *.xz)  [[ "$sig" == fd377a585a00* ]] || die "'$f' is not an xz file (magic '$sig') — re-download it." ;;
        *.gz)  [[ "$sig" == 1f8b* ]]         || die "'$f' is not a gzip file (magic '$sig') — re-download it." ;;
        *.img) : ;;  # raw image — nothing to check
        *)     warn "unrecognised base-image extension on '$f' — proceeding anyway" ;;
    esac
}

# Resolve --base-image to a local file: accept a path as-is, or download an
# http(s) URL into images/ (cached by basename; reused if already valid). Prints
# the resolved local path on stdout.
resolve_base_image() {
    local src="$1" name dest
    if [[ "$src" =~ ^https?:// ]]; then
        need curl
        name="$(basename "${src%%\?*}")"
        [[ "$name" == *.* ]] || die "cannot derive a filename from URL: $src"
        mkdir -p "$PWD/images"
        dest="$PWD/images/$name"
        if [[ -s "$dest" ]] && ( validate_base_image "$dest" ) >/dev/null 2>&1; then
            info "base image already present: $dest (delete it to re-fetch)" >&2
        else
            info "downloading base image -> $dest" >&2
            curl -fL --retry 3 --progress-bar -o "$dest" "$src" >&2 \
                || { rm -f "$dest"; die "download failed: $src"; }
        fi
        src="$dest"
    fi
    [[ -f "$src" ]] || die "base image not found: $src"
    validate_base_image "$src" >&2
    echo "$src"
}

# Fallback for SDK checkouts that lack the (normally shipped) rk3506-ubuntu
# .dockerfile: Ubuntu 22.04 + python2 + the SDK's build deps. CRUCIAL: create a
# UID-1000 user with passwordless sudo — the SDK runs as root then drops to the
# bind-mounted tree's owner (UID 1000) to compile via 'sudo -u #1000', so that
# user MUST exist or the kernel build dies with "unknown user #1000". python must
# point at python2 for the SDK's legacy scripts. No COPY/ADD (built via stdin).
write_sdk_dockerfile() {
    local out="$1"
    cat > "$out" <<'DOCKERFILE'
# Generated by camera-box build-minimal-image-luckfox-lyra-zero-w.sh as a
# fallback (the rk3506-ubuntu SDK normally ships its own rk3506-ubuntu.dockerfile).
FROM ubuntu:22.04
ENV DEBIAN_FRONTEND=noninteractive
RUN apt-get update && apt-get -y dist-upgrade && apt-get -y install \
        git ssh make gcc libssl-dev liblz4-tool expect expect-dev g++ patchelf \
        chrpath gawk texinfo diffstat binfmt-support qemu-user-static live-build \
        bison flex fakeroot cmake gcc-multilib g++-multilib unzip \
        device-tree-compiler ncurses-dev libgucharmap-2-90-dev bzip2 expat gpgv2 \
        cpp-aarch64-linux-gnu libgmp-dev libmpc-dev bc python-is-python3 python2 \
        rsync sudo bsdmainutils nano \
    && ln -sf /usr/bin/python2 /usr/bin/python \
    && rm -rf /var/lib/apt/lists/*
# The SDK drops to UID 1000 (the mounted tree's owner) to compile — it must exist.
RUN groupadd -g 1000 lyra \
    && useradd -u 1000 -g lyra -G sudo -m -s /bin/bash lyra \
    && sed -ri 's/^%sudo.*/%sudo ALL=(ALL:ALL) NOPASSWD: ALL/' /etc/sudoers \
    && echo 'lyra ALL=(ALL) NOPASSWD: ALL' >> /etc/sudoers
ENTRYPOINT ["/bin/bash", "-c"]
DOCKERFILE
}

usage() {
    awk 'NR>=2 && /^#/{sub(/^# ?/,"");print;next} NR>=2{exit}' "$0"
    exit "${1:-0}"
}

# --- args --------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --sdk)        SDK="${2:-}"; shift 2 ;;
        --base-image) BASE_IMAGE="${2:-}"; shift 2 ;;
        --suite)      SUITE="${2:-}"; shift 2 ;;
        --mirror)     MIRROR="${2:-}"; shift 2 ;;
        --out)        OUT="${2:-}"; shift 2 ;;
        --work)       WORK="${2:-}"; shift 2 ;;
        --skip-kernel)      SKIP_KERNEL=1; shift ;;
        --keep-base-kernel) KEEP_BASE_KERNEL=1; shift ;;
        --native)           NATIVE=1; shift ;;
        --yes|-y)     ASSUME_YES=1; shift ;;
        -h|--help) usage 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

[[ $EUID -eq 0 ]] || die "run as root (sudo ...)"
for t in debootstrap rsync losetup parted blkid mkfs.ext4 curl file bzip2 df stat; do need "$t"; done
# Need an ARM user-mode emulator reachable from the armhf chroot: either a static
# qemu-arm-static to copy in (older hosts), or qemu-arm registered in binfmt_misc
# with the 'F' (fix-binary) flag (Debian 13 / Ubuntu 24.10+). See the header.
[[ -x /usr/bin/qemu-arm-static ]] || grep -sq 'F' /proc/sys/fs/binfmt_misc/qemu-arm \
    || die "no ARM qemu available. Install 'qemu-user-static' (older) OR
     'qemu-user qemu-user-binfmt' + 'sudo systemctl restart systemd-binfmt'
     (newer), then verify: grep F /proc/sys/fs/binfmt_misc/qemu-arm"

# --- interactive: resolve the inputs -----------------------------------------
if [[ -z "$BASE_IMAGE" ]]; then
    [[ -z "$ASSUME_YES" ]] || die "--base-image is required with --yes"
    echo "A known-good dd-able base image is required — its bootloader/partition"
    echo "table are reused verbatim (e.g. Luckfox_Lyra_Zero_W-2503_Ubuntu.img.bz2"
    echo "from https://github.com/platima/SBC-Images, the one whose AP is proven)."
    echo "Accepts a local path OR an http(s) URL (downloaded into images/)."
    read -rp "Path or URL to the base image: " BASE_IMAGE
fi
# Accept a local file or an http(s) URL; a URL is fetched into images/ (cached)
# and validated (rejects Git LFS pointers / HTML / truncated downloads).
BASE_IMAGE="$(resolve_base_image "$BASE_IMAGE")"

if [[ -z "$KEEP_BASE_KERNEL" && -z "$SDK" ]]; then
    [[ -z "$ASSUME_YES" ]] || die "--sdk is required with --yes (or pass --keep-base-kernel)"
    echo
    echo "The UVC-enabled kernel is built from the rk3506-ubuntu SDK"
    echo "(markbirss/rk3506-ubuntu, branch luckfox-bpi — see the one-time setup"
    echo "in docs/rk3506-ubuntu-uvc-image.md). Leave empty to SKIP the kernel"
    echo "and only swap the rootfs — the image then keeps the stock kernel and"
    echo "has NO UVC."
    read -rp "Path to the rk3506-ubuntu SDK clone (empty = keep base kernel): " SDK
    [[ -n "$SDK" ]] || KEEP_BASE_KERNEL=1
fi
if [[ -z "$KEEP_BASE_KERNEL" ]]; then
    [[ -d "$SDK" ]] || die "SDK dir not found: $SDK"
    SDK="$(readlink -f "$SDK")"   # docker -v needs an absolute path
    [[ -e "$SDK/build.sh" ]] || die "$SDK/build.sh not found — this must be an rk3506-ubuntu SDK clone (git clone -b luckfox-bpi https://github.com/markbirss/rk3506-ubuntu.git)"
    [[ -e "$SDK/device/rockchip/.chip" ]] || die "SDK not bootstrapped: device/rockchip/.chip is missing. One-time setup:
    cd $SDK/device/rockchip/.chips/rk3506
    ln -s .chips/rk3506 ../../rk3506 && ln -s .chips/rk3506 ../../.chip
(board selection is done automatically by this script — no './build.sh lunch' needed)"
fi

# The SDK's own build environment: Ubuntu 22.04 + python2 + a global
# python->python2 symlink. NEVER install that on the host — run every SDK command
# inside the docker image built from rk3506-ubuntu.dockerfile (which the SDK
# README documents but does not ship, so we generate it via write_sdk_dockerfile).
# We pass the command via --entrypoint /bin/bash regardless of the image's own.
sdk_run() {
    if [[ -n "$NATIVE" ]]; then
        ( cd "$SDK" && bash -c "$*" )
    else
        docker run --rm -v "$SDK":/build -w /build \
            --entrypoint /bin/bash "$DOCKER_IMG" -c "$*"
    fi
}
if [[ -z "$KEEP_BASE_KERNEL" ]]; then
    if [[ -n "$NATIVE" ]]; then
        warn "--native: SDK commands run on THIS host — it needs Ubuntu 22.04,"
        warn "python2, and 'python' pointing at python2 (the SDK README's"
        warn "'ln -sf /usr/bin/python2 /usr/bin/python'). Docker is safer."
        command -v python2 >/dev/null 2>&1 || warn "python2 not found on this host — the SDK build will likely fail"
    else
        need docker
        # The image is usable only if it exists AND contains the build user
        # (UID 1000): the SDK runs as root then drops to that UID to compile
        # ('sudo -u #1000 make …'), so an image lacking it dies with
        # "unknown user #1000". Rebuild when missing or incomplete.
        if ! { docker image inspect "$DOCKER_IMG" >/dev/null 2>&1 \
               && docker run --rm --entrypoint /bin/bash "$DOCKER_IMG" \
                    -c 'getent passwd 1000 >/dev/null 2>&1'; }; then
            if docker image inspect "$DOCKER_IMG" >/dev/null 2>&1; then
                info "existing '$DOCKER_IMG' lacks build UID 1000 — rebuilding it"
                docker rmi -f "$DOCKER_IMG" >/dev/null 2>&1 || true
            fi
            if [[ ! -f "$SDK/rk3506-ubuntu.dockerfile" ]]; then
                info "SDK ships no rk3506-ubuntu.dockerfile — generating it (Ubuntu 22.04 + python2, per the SDK README)"
                write_sdk_dockerfile "$SDK/rk3506-ubuntu.dockerfile"
            fi
            info "building the SDK docker image '$DOCKER_IMG' (one-time, ~5-10 min)"
            # Pipe the dockerfile via stdin so the 2.7 GB SDK isn't sent as build
            # context (the image has no COPY/ADD, so it needs no file context).
            docker build --rm -t "$DOCKER_IMG" - < "$SDK/rk3506-ubuntu.dockerfile" \
                || die "docker image build failed"
        fi
    fi
fi

WORK="${WORK:-$PWD/build-lyra-minimal}"
OUT="${OUT:-$PWD/images/camera-box-lyra-minimal.img}"

# --- interactive: enough space in the selected folders? ----------------------
# The work dir holds the decompressed base copy + the debootstrap rootfs; the
# output is another full copy of the base image.
base_sz="$(stat -c%s "$BASE_IMAGE")"
case "$BASE_IMAGE" in
    *.img|*.raw) raw_est=$base_sz ;;
    *)           raw_est=$((base_sz * 4)) ;;   # rough decompressed estimate
esac
need_work=$((raw_est + 3 * 1024 * 1024 * 1024))   # base copy + rootfs + slack
need_out=$raw_est
mkdir -p "$WORK" "$(dirname "$OUT")"
free_b()  { df -B1 --output=avail "$1" 2>/dev/null | tail -1 | tr -d ' '; }
fs_of()   { df --output=source "$1" 2>/dev/null | tail -1; }
gb()      { echo "$(( ${1:-0} / 1024 / 1024 / 1024 ))G"; }

echo
echo "Build plan:"
echo "  base image : $BASE_IMAGE ($(gb "$base_sz"), ~$(gb "$raw_est") raw)"
if [[ -n "$KEEP_BASE_KERNEL" ]]; then
    echo "  kernel     : KEPT from the base image (no UVC!)"
else
    kenv="in docker ($DOCKER_IMG)"; [[ -n "$NATIVE" ]] && kenv="NATIVE on this host"
    echo "  kernel     : rk3506-ubuntu SDK at $SDK (+ UVC, $kenv)${SKIP_KERNEL:+ [reusing existing build]}"
fi
echo "  rootfs     : Debian $SUITE minbase from $MIRROR"
echo "  work dir   : $WORK (free $(gb "$(free_b "$WORK")"), need ~$(gb "$need_work"))"
echo "  output     : $OUT (free $(gb "$(free_b "$(dirname "$OUT")")"), need ~$(gb "$need_out"))"

space_ok=1
if [[ "$(fs_of "$WORK")" == "$(fs_of "$(dirname "$OUT")")" ]]; then
    [[ "$(free_b "$WORK")" -ge $((need_work + need_out)) ]] || space_ok=""
else
    [[ "$(free_b "$WORK")" -ge "$need_work" ]] || space_ok=""
    [[ "$(free_b "$(dirname "$OUT")")" -ge "$need_out" ]] || space_ok=""
fi
if [[ -z "$space_ok" ]]; then
    warn "NOT enough free space for the estimated need (pick another --work/--out, or free space)"
    [[ -z "$ASSUME_YES" ]] || die "aborting (--yes given)"
    read -rp "Continue anyway? [y/N] " ans
    [[ "$ans" =~ ^[yY] ]] || { echo "aborted."; exit 1; }
fi
if [[ -z "$ASSUME_YES" ]]; then
    read -rp "Type YES to build: " ans
    [[ "$ans" == "YES" ]] || { echo "aborted."; exit 1; }
fi

ROOTFS="$WORK/rootfs"
IMGMNT="$WORK/imgmnt"
BASEMNT="$WORK/basemnt"
mkdir -p "$ROOTFS" "$IMGMNT" "$BASEMNT"

LOOP=""
cleanup() {
    set +e
    umount "$ROOTFS/dev/pts" 2>/dev/null
    umount "$ROOTFS/dev" "$ROOTFS/proc" "$ROOTFS/sys" 2>/dev/null
    umount "$IMGMNT" "$BASEMNT" 2>/dev/null
    [[ -n "$LOOP" ]] && losetup -d "$LOOP" 2>/dev/null
}
trap cleanup EXIT

# =============================================================================
# Phase 1 — kernel with UVC, from the rk3506-ubuntu SDK (in docker)
# =============================================================================
BOOT_IMG=""
KDIR=""
KVER=""
if [[ -z "$KEEP_BASE_KERNEL" ]]; then
    # locate the kernel tree inside the SDK
    for d in "$SDK"/kernel-* "$SDK/kernel"; do
        [[ -f "$d/Makefile" && -d "$d/arch/arm/configs" ]] && { KDIR="$d"; break; }
    done
    [[ -n "$KDIR" ]] || die "couldn't find the kernel tree under $SDK (expected \$SDK/kernel*/Makefile)"
    info "kernel tree: $KDIR"

    BOARD_DEFCONFIG="luckfox_lyra_zero-w_ubuntu_sdmmc_defconfig"
    BOARD_CFG="$SDK/device/rockchip/.chip/$BOARD_DEFCONFIG"
    [[ -f "$BOARD_CFG" ]] || die "board defconfig not found: $BOARD_CFG (SDK layout changed?)"

    if [[ -z "$SKIP_KERNEL" ]]; then
        # -- enable UVC via a kernel config FRAGMENT (idempotent) -------------
        # Appending to the kernel defconfig does NOT survive: the SDK builds
        # with 'make <defconfig> <fragment>.config ...' and later fragments
        # override earlier values (rk3506-display.config touches the media
        # options). Registering our own fragment LAST is the SDK-native
        # mechanism and always wins. (Verified on kernel 6.1.99.)
        cat > "$KDIR/arch/arm/configs/rk3506-uvc.config" <<'EOF'
# camera-box: UVC (USB webcam) support
CONFIG_MEDIA_SUPPORT=y
CONFIG_MEDIA_USB_SUPPORT=y
CONFIG_MEDIA_CAMERA_SUPPORT=y
CONFIG_VIDEO_DEV=y
CONFIG_USB_VIDEO_CLASS=m
EOF
        if ! grep -q 'rk3506-uvc.config' "$BOARD_CFG"; then
            sed -i 's/^RK_KERNEL_CFG_FRAGMENTS="\(.*\)"$/RK_KERNEL_CFG_FRAGMENTS="\1 rk3506-uvc.config"/' "$BOARD_CFG"
            grep -q 'rk3506-uvc.config' "$BOARD_CFG" \
                || echo 'RK_KERNEL_CFG_FRAGMENTS="rk3506-uvc.config"' >> "$BOARD_CFG"
        fi
        info "UVC fragment registered in $BOARD_DEFCONFIG"

        # -- enable USB Wi-Fi dongle drivers via a second fragment ------------
        # camera-box already supports a secondary Wi-Fi interface (wlan1) as a
        # DHCP client (net.rs); the minimal image just lacks a USB Wi-Fi *device*
        # driver — mac80211/cfg80211 are already built. Enable the common
        # mainline USB-dongle drivers as modules (an unused one costs nothing) so
        # any supported dongle comes up as wlan1 without another rebuild. NB: the
        # RTL8188GU (RTL8710BU) has NO usable in-tree driver in 6.1 — use an
        # rt2800usb / mt7601u / mt76x2u / rtl8xxxu dongle instead.
        cat > "$KDIR/arch/arm/configs/rk3506-wifi.config" <<'EOF'
# camera-box: USB Wi-Fi dongle support (secondary client interface, wlan1)
CONFIG_WLAN=y
# Vendor gates: the base rk3506_luckfox_defconfig explicitly disables these
# (# CONFIG_WLAN_VENDOR_* is not set), which HIDES every driver beneath them —
# they MUST be re-enabled here or the driver symbols below are silently dropped.
CONFIG_WLAN_VENDOR_RALINK=y
CONFIG_WLAN_VENDOR_MEDIATEK=y
CONFIG_WLAN_VENDOR_REALTEK=y
# Ralink RT2870/RT3070/RT5370 (rt2x00) — cheap, reliable, 2.4GHz
CONFIG_RT2X00=m
CONFIG_RT2800USB=m
CONFIG_RT2800USB_RT33XX=y
CONFIG_RT2800USB_RT35XX=y
CONFIG_RT2800USB_RT3573=y
CONFIG_RT2800USB_RT53XX=y
CONFIG_RT2800USB_RT55XX=y
CONFIG_RT2800USB_UNKNOWN=y
# MediaTek MT7601U (2.4GHz) and MT7610U/MT7612U (dual-band 5GHz)
CONFIG_MT7601U=m
CONFIG_MT76x0U=m
CONFIG_MT76x2U=m
# Realtek RTL8188EU/8192EU/8188FU/8723BU (rtl8xxxu)
CONFIG_RTL8XXXU=m
CONFIG_RTL8XXXU_UNTESTED=y
# USB bus monitoring (usbmon) — lets 'usbtop' or a usbmon reader show per-device
# throughput. The camera and the Wi-Fi dongle share one USB 2.0 bus, so this is
# how you see whether they're contending for bandwidth.
CONFIG_USB_MON=m
EOF
        if ! grep -q 'rk3506-wifi.config' "$BOARD_CFG"; then
            sed -i 's/^RK_KERNEL_CFG_FRAGMENTS="\(.*\)"$/RK_KERNEL_CFG_FRAGMENTS="\1 rk3506-wifi.config"/' "$BOARD_CFG"
            grep -q 'rk3506-wifi.config' "$BOARD_CFG" \
                || echo 'RK_KERNEL_CFG_FRAGMENTS="rk3506-wifi.config"' >> "$BOARD_CFG"
        fi
        info "USB Wi-Fi fragment registered in $BOARD_DEFCONFIG"

        # Fix a vendor DTS bug: vdd_cpu declares itself as its own input
        # supply (vin-supply = <&vdd_cpu>), so the regulator fails to probe
        # (-EINVAL) and cpufreq/DVFS never comes up — the CPU stays stuck at
        # its boot frequency. A fixed regulator needs no vin-supply. NOTE:
        # the zero-w-sd board includes the *ultra* dtsi — patch both.
        sed -i '/vin-supply = <&vdd_cpu>;/d' \
            "$KDIR/arch/arm/boot/dts/rk3506-luckfox-lyra.dtsi" \
            "$KDIR/arch/arm/boot/dts/rk3506-luckfox-lyra-ultra.dtsi" 2>/dev/null || true
        # force the DTB to regenerate (its dep tracking misses dtsi edits)
        rm -f "$KDIR/arch/arm/boot/dts/rk3506b-luckfox-lyra-zero-w-sd.dtb"

        # select the board non-interactively — './build.sh <name>_defconfig'
        # replaces the interactive './build.sh lunch'. Both the select and the
        # build must run as root in the container (build.sh refuses otherwise
        # when the Ubuntu profile is active).
        info "selecting board: $BOARD_DEFCONFIG"
        sdk_run "./build.sh $BOARD_DEFCONFIG" || die "board select failed"
        if [[ -n "$NATIVE" ]]; then env_label="native"; else env_label="in docker"; fi
        info "building the kernel via the SDK ($env_label) — this is the long part"
        sdk_run './build.sh kernel' || die "SDK kernel build failed — check the SDK output above"
    else
        info "--skip-kernel: reusing the SDK's existing kernel build"
    fi

    grep -q '^CONFIG_USB_VIDEO_CLASS=m' "$KDIR/.config" \
        || die "CONFIG_USB_VIDEO_CLASS is NOT =m in $KDIR/.config after the build — the defconfig change didn't take; enable it via the SDK's kernel menuconfig and re-run with --skip-kernel"

    grep -qE '^CONFIG_(RT2800USB|MT7601U|MT76x2U|RTL8XXXU)=m' "$KDIR/.config" \
        || warn "no USB Wi-Fi dongle driver ended up =m in $KDIR/.config — a second-radio (wlan1) dongle won't work; check the rk3506-wifi.config fragment took"

    # the SDK packs the kernel+dtb into a Rockchip boot image — find it
    for c in "$SDK/output/image/boot.img" "$SDK/rockdev/boot.img" "$SDK/output/firmware/boot.img" \
             "$KDIR/boot.img" $(find "$SDK/output" "$SDK/rockdev" -maxdepth 3 -name 'boot.img' 2>/dev/null); do
        [[ -f "$c" && ( -z "$BOOT_IMG" || "$c" -nt "$BOOT_IMG" ) ]] && BOOT_IMG="$c"
    done
    [[ -n "$BOOT_IMG" ]] || die "no boot.img found under $SDK after the kernel build — check where your SDK version emits it and adjust this script"
    info "boot image: $BOOT_IMG"

    # stage the modules from inside the build environment (host has no
    # matching toolchain/python2); the staging dir lives in the bind-mounted
    # SDK so the host can copy from it afterwards.
    # Toolchain: prebuilts ships TWO — pick the Linux one (gnueabihf), NOT
    # the bare-metal arm-none-eabi.
    KREL="${KDIR##*/}"                       # e.g. kernel-6.1
    CROSS="$(find "$SDK/prebuilts" -path '*gnueabihf*/bin/arm-*-gcc' 2>/dev/null | head -1)"
    [[ -n "$CROSS" ]] || die "no ARM linux-gnueabihf cross toolchain under $SDK/prebuilts"
    CROSS_REL="${CROSS#"$SDK"/}"             # SDK-relative, valid in the container
    info "staging kernel modules (INSTALL_MOD_STRIP=1)"
    rm -rf "$SDK/.camerabox-modules"
    sdk_run "make -C $KREL ARCH=arm CROSS_COMPILE=\$PWD/${CROSS_REL%gcc} \
             INSTALL_MOD_PATH=\$PWD/.camerabox-modules INSTALL_MOD_STRIP=1 modules_install" \
        || die "modules_install failed"

    KVER="$(ls "$SDK/.camerabox-modules/lib/modules" 2>/dev/null | head -1)"
    [[ -n "$KVER" ]] || die "no modules staged under $SDK/.camerabox-modules"
    info "kernel release: $KVER"

    # build the AIC8800 (USB) Wi-Fi driver — an EXTERNAL module, not in-tree.
    # Its Makefile defaults are right for this board (USB=y, SDIO=n), but it
    # uses $(PWD) for M=, so it MUST be built with the driver dir as the
    # working directory — 'make -C' silently builds nothing.
    AIC_DIR="external/rkwifibt/drivers/aic8800/aic8800"
    [[ -d "$SDK/$AIC_DIR" ]] || die "aic8800 driver not found at \$SDK/$AIC_DIR"
    info "building the aic8800 Wi-Fi modules"
    sdk_run "SDKROOT=\$PWD; cd $AIC_DIR && \
             make KDIR=\$SDKROOT/$KREL ARCH=arm CROSS_COMPILE=\$SDKROOT/${CROSS_REL%gcc}" \
        || die "aic8800 build failed"
    mkdir -p "$SDK/.camerabox-modules/lib/modules/$KVER/updates"
    cp "$SDK/$AIC_DIR/aic_load_fw/aic_load_fw.ko" \
       "$SDK/$AIC_DIR/aic8800_fdrv/aic8800_fdrv.ko" \
       "$SDK/.camerabox-modules/lib/modules/$KVER/updates/" \
        || die "aic8800 .ko files missing after the build"
    info "aic8800_fdrv.ko + aic_load_fw.ko staged"

    # build the out-of-tree rtw88 (USB) driver. The in-tree rtw88 in this 6.1
    # kernel is PCIe/SDIO only (no usb.c / no 8822cu), so USB Realtek dongles —
    # notably RTL8822CU / 8821CU / 8811CU — need markbirss/rtw88, the SDK author's
    # USB-capable backport. Its modules are named rtw_* (distinct from the in-tree
    # rtw88_*) and are staged into updates/ so they win; firmware (rtw8822c_fw.bin)
    # comes from firmware-realtek. Pairs with usb-modeswitch in the rootfs, which
    # flips these dongles' CD-ROM mode (e.g. 0bda:1a2b) to WLAN mode (0bda:c812).
    RTW88_DIR="$SDK/.camerabox-rtw88"
    if [[ ! -e "$RTW88_DIR/Makefile" ]]; then
        info "fetching out-of-tree rtw88 (markbirss/rtw88) for USB Wi-Fi dongles"
        rm -rf "$RTW88_DIR"
        git clone --depth 1 https://github.com/markbirss/rtw88 "$RTW88_DIR" \
            || die "rtw88 clone failed (needs git + network on the host)"
    fi
    info "building the rtw88 USB Wi-Fi modules (all Realtek USB chips)"
    sdk_run "SDKROOT=\$PWD; cd .camerabox-rtw88 && \
             make KSRC=\$SDKROOT/$KREL KVER=$KVER ARCH=arm CROSS_COMPILE=\$SDKROOT/${CROSS_REL%gcc}" \
        || die "rtw88 build failed"
    find "$RTW88_DIR" -maxdepth 1 -name 'rtw_*.ko' \
        -exec cp {} "$SDK/.camerabox-modules/lib/modules/$KVER/updates/" \;
    find "$SDK/.camerabox-modules/lib/modules/$KVER/updates" -name 'rtw_8822cu.ko' | grep -q . \
        || die "rtw_8822cu.ko missing after the rtw88 build"
    info "rtw88 USB modules staged (rtw_8822cu + deps)"

    # build u-boot + idblock — the SD's own boot chain. The community base
    # image carries NO loader (sector 64 is empty; it relied on a factory
    # u-boot in SPI flash), so without this the image only boots on boards
    # whose SPI still holds a loader.
    UBOOT_IMG="$SDK/u-boot/uboot.img"
    IDBLOCK="$(ls "$SDK"/u-boot/*idblock*.img 2>/dev/null | head -1 || true)"
    if [[ ! -f "$UBOOT_IMG" || -z "$IDBLOCK" ]]; then
        info "building u-boot + idblock"
        sdk_run './build.sh uboot' || die "u-boot build failed"
        UBOOT_IMG="$SDK/u-boot/uboot.img"
        IDBLOCK="$(ls "$SDK"/u-boot/*idblock*.img 2>/dev/null | head -1 || true)"
    fi
    [[ -f "$UBOOT_IMG" && -n "$IDBLOCK" ]] || die "u-boot artifacts missing after the build (u-boot/uboot.img + *idblock*.img)"
    info "u-boot: $UBOOT_IMG, idblock: $IDBLOCK"
fi

# =============================================================================
# Phase 2 — minimal Debian rootfs (debootstrap minbase, armhf, under qemu)
# =============================================================================
if [[ -f "$ROOTFS/etc/debian_version" ]]; then
    info "reusing existing debootstrap rootfs in $ROOTFS (delete it to rebuild)"
else
    info "debootstrap $SUITE minbase (armhf) -> $ROOTFS"
    debootstrap --arch=armhf --variant=minbase --foreign "$SUITE" "$ROOTFS" "$MIRROR"
    # Provide the ARM emulator for the second stage. Older hosts ship a static
    # /usr/bin/qemu-arm-static to copy in; newer ones (Debian 13 / Ubuntu 24.10+)
    # drop it and instead register qemu-arm with the binfmt_misc 'F' (fix-binary)
    # flag, so the kernel holds the interpreter fd open and it runs inside the
    # chroot with nothing copied in. Support both.
    if [[ -e /usr/bin/qemu-arm-static ]]; then
        cp /usr/bin/qemu-arm-static "$ROOTFS/usr/bin/"
    elif ! grep -sq 'F' /proc/sys/fs/binfmt_misc/qemu-arm; then
        die "no qemu-arm-static, and arm binfmt isn't registered with the F flag —
     install qemu-user + qemu-user-binfmt and run 'sudo systemctl restart
     systemd-binfmt', then re-run (check: grep F /proc/sys/fs/binfmt_misc/qemu-arm)"
    fi
    chroot "$ROOTFS" /debootstrap/debootstrap --second-stage
fi
[[ -e /usr/bin/qemu-arm-static ]] && cp -f /usr/bin/qemu-arm-static "$ROOTFS/usr/bin/" 2>/dev/null || true

mount --bind /dev  "$ROOTFS/dev"
mkdir -p "$ROOTFS/dev/pts"; mount -t devpts devpts "$ROOTFS/dev/pts" 2>/dev/null || true
mount --bind /proc "$ROOTFS/proc"
mount --bind /sys  "$ROOTFS/sys"
mkdir -p "$ROOTFS/tmp"; chmod 1777 "$ROOTFS/tmp"
printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\n' > "$ROOTFS/etc/resolv.conf"

# Enable the non-free-firmware component so USB Wi-Fi dongle firmware
# (firmware-misc-nonfree / firmware-realtek) is installable in the chroot below.
echo "deb $MIRROR $SUITE main non-free-firmware" > "$ROOTFS/etc/apt/sources.list"

info "installing the camera-box requirements (emulated — slow)"
# collapse the multi-line package lists to one line — an embedded newline
# would split the apt command inside the heredoc
PKGS_ALL="$(echo $PKGS_CORE $PKGS_DEBUG $PKGS_WIFI)"
chroot "$ROOTFS" /bin/bash -e <<CHROOT
export DEBIAN_FRONTEND=noninteractive
APT="apt-get -o APT::Sandbox::User=root -o Acquire::Languages=none \
     -o Dpkg::Options::=--force-confold -o Dpkg::Options::=--force-confdef"
\$APT update
\$APT install -y --no-install-recommends $PKGS_ALL
\$APT install -y --no-install-recommends ustreamer \
    || echo ">> NOTE: 'ustreamer' not in apt for $SUITE — build it on the device (see docs/luckfox-lyra-zero-w.md)"
\$APT clean
rm -rf /var/lib/apt/lists/*
CHROOT

# =============================================================================
# Phase 3 — generic OS base (NO hotspot/app config — that's prepare-sd.sh's job)
# =============================================================================
info "configuring the OS base (hostname, ssh, first-boot identity)"

echo "camera-box" > "$ROOTFS/etc/hostname"
cat > "$ROOTFS/etc/hosts" <<'EOF'
127.0.0.1	localhost
127.0.1.1	camera-box
EOF
# the bootloader's cmdline mounts / itself; fstab just remounts it sanely
cat > "$ROOTFS/etc/fstab" <<'EOF'
LABEL=rootfs  /  ext4  defaults,noatime  0  1
EOF
echo 'LANG=C.UTF-8' > "$ROOTFS/etc/default/locale"

# fallback root login if the image is flashed without prepare-sd.sh
# (prepare-sd.sh --root-pass overrides this per card)
chroot "$ROOTFS" /bin/bash -c "echo 'root:$DEFAULT_ROOT_PASS' | chpasswd"

# ssh: allow root+password over the AP; per-device host keys on first boot
mkdir -p "$ROOTFS/etc/ssh/sshd_config.d"
echo 'PermitRootLogin yes' > "$ROOTFS/etc/ssh/sshd_config.d/camera-box.conf"
rm -f "$ROOTFS"/etc/ssh/ssh_host_*
cat > "$ROOTFS/etc/systemd/system/ssh-hostkeys.service" <<'EOF'
[Unit]
Description=Generate SSH host keys on first boot
Before=ssh.service
ConditionPathExists=!/etc/ssh/ssh_host_ed25519_key

[Service]
Type=oneshot
ExecStart=/usr/bin/ssh-keygen -A
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

# unique machine-id per device (regenerated on first boot)
: > "$ROOTFS/etc/machine-id"
rm -f "$ROOTFS/var/lib/dbus/machine-id"

# default resolver placeholder; camera-box/dhclient overwrite it in client mode
printf 'nameserver 1.1.1.1\n' > "$ROOTFS/etc/resolv.conf"

# marker so prepare-sd.sh/diagnose-sd.sh recognise this image (headless setup)
cat > "$ROOTFS/etc/camerabox-minimal-release" <<EOF
IMAGE=camera-box-minimal-luckfox-lyra-zero-w
BUILD_DATE=$(date -u +%Y-%m-%dT%H:%M:%SZ)
SUITE=$SUITE
KERNEL=${KVER:-base-image-kernel}
EOF

# ssh on; hostapd/dnsmasq OFF until prepare-sd.sh writes their config and
# enables them — a bare image must boot clean, not with failing services
if ! systemctl --root="$ROOTFS" enable ssh ssh-hostkeys >/dev/null 2>&1; then
    w="$ROOTFS/etc/systemd/system/multi-user.target.wants"; mkdir -p "$w"
    ln -sf /etc/systemd/system/ssh-hostkeys.service "$w/ssh-hostkeys.service"
    for base in /lib/systemd/system /usr/lib/systemd/system; do
        [[ -f "$ROOTFS$base/ssh.service" ]] && { ln -sf "$base/ssh.service" "$w/ssh.service"; break; }
    done
fi
systemctl --root="$ROOTFS" disable hostapd dnsmasq >/dev/null 2>&1 \
    || rm -f "$ROOTFS/etc/systemd/system/multi-user.target.wants/"{hostapd,dnsmasq}.service

umount "$ROOTFS/dev/pts" 2>/dev/null || true
umount "$ROOTFS/dev" "$ROOTFS/proc" "$ROOTFS/sys" 2>/dev/null || true

# =============================================================================
# Phase 4 — kernel modules (ours) into the rootfs
# =============================================================================
if [[ -z "$KEEP_BASE_KERNEL" ]]; then
    info "installing the staged kernel modules into the rootfs"
    mkdir -p "$ROOTFS/lib"
    rm -rf "$ROOTFS/lib/modules/$KVER"
    cp -a "$SDK/.camerabox-modules/lib/modules" "$ROOTFS/lib/"
    depmod -b "$ROOTFS" "$KVER" 2>/dev/null || true

    find "$ROOTFS/lib/modules/$KVER" -name 'uvcvideo.ko*' | grep -q . \
        || die "uvcvideo.ko missing from the module install — UVC didn't build; check the kernel config"
    find "$ROOTFS/lib/modules/$KVER/updates" -name 'aic8800_fdrv.ko' | grep -q . \
        || die "aic8800_fdrv.ko missing — without it there is no wlan0 and no hotspot"
    info "uvcvideo.ko + aic8800 modules present"

    # Wi-Fi firmware: the SDK ships the blobs matching this driver, and the
    # driver's built-in default path is /lib/firmware/aic8800DC.
    if [[ -d "$SDK/external/rkwifibt/firmware/aicsemi/aic8800DC" ]]; then
        mkdir -p "$ROOTFS/lib/firmware"
        cp -a "$SDK/external/rkwifibt/firmware/aicsemi/aic8800DC" "$ROOTFS/lib/firmware/"
        info "aic8800DC firmware installed from the SDK"
    else
        warn "no aic8800DC firmware in the SDK — relying on the base-image harvest"
    fi
    # Load the whole USB + Wi-Fi + camera chain explicitly at boot: the USB
    # controller (dwc2) and PHY are modules in the vendor kernel, and OF
    # modalias autoload proved unreliable on the board (no USB bus at all
    # until these were force-loaded).
    cat > "$ROOTFS/etc/modules-load.d/camera-box.conf" <<'EOF'
phy-rockchip-inno-usb2
dwc2
aic_load_fw
aic8800_fdrv
uvcvideo
EOF
fi

# =============================================================================
# Phase 5 — assemble: proven boot chain + our boot.img + our rootfs
# =============================================================================
info "preparing the output image from the base image"
BASE_RAW="$WORK/base.img"
if [[ ! -f "$BASE_RAW" ]]; then
    case "$BASE_IMAGE" in
        *.bz2) need bzip2; bzip2 -dc "$BASE_IMAGE" > "$BASE_RAW" ;;
        *.xz)  need xz;    xz    -dc "$BASE_IMAGE" > "$BASE_RAW" ;;
        *.gz)  need gzip;  gzip  -dc "$BASE_IMAGE" > "$BASE_RAW" ;;
        *.img|*.raw)       cp "$BASE_IMAGE" "$BASE_RAW" ;;
        *) die "unrecognised base image extension: $BASE_IMAGE (want .img[.bz2|.gz|.xz])" ;;
    esac
fi
cp -f "$BASE_RAW" "$OUT"

LOOP="$(losetup --show -fP "$OUT")" || die "losetup failed"
info "image attached at $LOOP"
partprobe "$LOOP" 2>/dev/null || true; sleep 1

# find the rootfs partition: the ext4 one that contains /etc + /usr
ROOTP=""
for p in "$LOOP"p*; do
    [[ -b "$p" ]] || continue
    [[ "$(blkid -o value -s TYPE "$p" 2>/dev/null)" == ext4 ]] || continue
    if mount -o ro "$p" "$BASEMNT" 2>/dev/null; then
        [[ -d "$BASEMNT/etc" && -d "$BASEMNT/usr" ]] && ROOTP="$p"
        umount "$BASEMNT"
        [[ -n "$ROOTP" ]] && break
    fi
done
[[ -n "$ROOTP" ]] || { lsblk "$LOOP" >&2; die "no ext4 rootfs partition found in the base image"; }
info "base rootfs partition: $ROOTP"

# harvest what only the proven image can give us: the AIC8800 firmware blobs
# (and, with --keep-base-kernel, its modules — they must match its kernel)
mount -o ro "$ROOTP" "$BASEMNT"
info "harvesting AIC8800 firmware from the base image"
fw_found=""
for d in "$BASEMNT"/lib/firmware/*aic* "$BASEMNT"/usr/lib/firmware/*aic* \
         "$BASEMNT"/etc/firmware/*aic*; do
    [[ -e "$d" ]] || continue
    mkdir -p "$ROOTFS/lib/firmware"
    cp -a "$d" "$ROOTFS/lib/firmware/"
    fw_found=1
done
[[ -n "$fw_found" ]] || warn "no aic8800 firmware found in the base image — if Wi-Fi fails, locate the blobs (dmesg will name the missing file) and copy them to /lib/firmware"
if [[ -n "$KEEP_BASE_KERNEL" ]]; then
    info "--keep-base-kernel: copying the base image's kernel modules"
    cp -a "$BASEMNT/lib/modules" "$ROOTFS/lib/" 2>/dev/null \
        || warn "base image has no /lib/modules (kernel may be fully built-in)"
fi
BASE_UUID="$(blkid -o value -s UUID "$ROOTP")"
umount "$BASEMNT"

# replace the kernel: dd our boot.img over the base image's boot partition
if [[ -z "$KEEP_BASE_KERNEL" ]]; then
    BOOTP=""
    # prefer the GPT partition named "boot"
    for p in "$LOOP"p*; do
        [[ -b "$p" && "$p" != "$ROOTP" ]] || continue
        [[ "$(lsblk -no PARTLABEL "$p" 2>/dev/null)" == boot ]] && { BOOTP="$p"; break; }
    done
    # fallback: the partition whose current content shares our boot.img's magic
    if [[ -z "$BOOTP" ]]; then
        magic="$(od -An -tx1 -N4 "$BOOT_IMG" | tr -d ' \n')"
        for p in "$LOOP"p*; do
            [[ -b "$p" && "$p" != "$ROOTP" ]] || continue
            [[ "$(od -An -tx1 -N4 "$p" 2>/dev/null | tr -d ' \n')" == "$magic" ]] && { BOOTP="$p"; break; }
        done
    fi
    [[ -n "$BOOTP" ]] || { lsblk -o NAME,SIZE,PARTLABEL,FSTYPE "$LOOP" >&2; \
        die "couldn't identify the boot partition — check the layout above and adapt the script"; }
    bsz="$(blockdev --getsize64 "$BOOTP")"; isz="$(stat -c%s "$BOOT_IMG")"
    [[ "$isz" -le "$bsz" ]] || die "boot.img ($isz bytes) doesn't fit the boot partition ($bsz bytes)"
    info "writing our UVC-enabled boot.img -> $BOOTP"
    dd if="$BOOT_IMG" of="$BOOTP" bs=4M conv=fsync status=none

    # make the card SELF-BOOTING: idblock at sector 64 (raw, before the first
    # partition at sector 8192) + the matching u-boot FIT into the 'uboot'
    # partition. All three stages then come from the same SDK build.
    UBOOTP=""
    for p in "$LOOP"p*; do
        [[ -b "$p" && "$p" != "$ROOTP" && "$p" != "$BOOTP" ]] || continue
        [[ "$(lsblk -no PARTLABEL "$p" 2>/dev/null)" == uboot ]] && { UBOOTP="$p"; break; }
    done
    [[ -n "$UBOOTP" ]] || UBOOTP="${LOOP}p1"
    usz="$(blockdev --getsize64 "$UBOOTP")"; isz="$(stat -c%s "$UBOOT_IMG")"
    [[ "$isz" -le "$usz" ]] || die "uboot.img ($isz bytes) doesn't fit the uboot partition ($usz bytes)"
    idsz="$(stat -c%s "$IDBLOCK")"
    [[ "$idsz" -le $(( (8192 - 64) * 512 )) ]] || die "idblock ($idsz bytes) overlaps the first partition"
    info "writing idblock (sector 64) + uboot.img -> $UBOOTP (self-booting card)"
    dd if="$IDBLOCK" of="$LOOP" bs=512 seek=64 conv=notrunc,fsync status=none
    dd if="$UBOOT_IMG" of="$UBOOTP" bs=4M conv=fsync status=none
fi

# replace the rootfs: fresh ext4 (same UUID — the boot cmdline may reference
# it), then copy the minimal rootfs in
info "formatting the rootfs partition and copying the minimal rootfs"
mkfs.ext4 -Fq ${BASE_UUID:+-U "$BASE_UUID"} -L rootfs "$ROOTP"
mount "$ROOTP" "$IMGMNT"
rm -f "$ROOTFS/usr/bin/qemu-arm-static"
rsync -aHAX --numeric-ids "$ROOTFS"/ "$IMGMNT"/
sync
umount "$IMGMNT"
losetup -d "$LOOP"; LOOP=""

info "done."
echo
echo "Image: $OUT ($(du -h "$OUT" | cut -f1))"
echo "(compress for sharing: bzip2 -k9 $OUT)"
echo
echo "This image is generic — no camera-box, no hotspot yet. Deploy each SD"
echo "card with the ONE deployment script (it flashes, installs camera-box,"
echo "and writes the hotspot + root-password config; the apt step is skipped"
echo "because the dependencies are already in this image):"
echo
echo "  sudo bash scripts/prepare-sd.sh --image $OUT \\"
echo "      --ssid CameraBox --pass CameraBox123 --ip 192.168.4.1/24 --root-pass secret"
echo
echo "On a fresh board, erase the SPI flash once so it boots from SD"
echo "(docs/luckfox-lyra-zero-w.md#troubleshooting). After boot:"
if [[ -z "$KEEP_BASE_KERNEL" ]]; then
    echo "  - plug a USB camera:  ls -l /dev/video*   (uvcvideo is in this kernel)"
else
    echo "  NOTE: --keep-base-kernel — the stock kernel has NO UVC; this image is"
    echo "  for validating the minimal rootfs (hotspot + dashboard) only."
fi
