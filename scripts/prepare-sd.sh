#!/usr/bin/env bash
#
# prepare-sd.sh — provision a microSD card so a Luckfox Lyra Zero W (or any
# ARMv7 systemd board) boots with camera-box fully installed and hosting its
# Wi-Fi hotspot, with NO setup needed on the device.
#
# Run on a Linux PC, as root. The card must already be flashed with the board's
# Ubuntu (systemd) image — the script pre-seeds the existing root filesystem.
# It will interactively let you PICK the SD card. Optionally pass --image to
# flash a raw image first.
#
# This is the ONE deployment script. The recommended flow is the build/deploy
# split: build the purpose-built minimal image once with
# scripts/build-minimal-image-luckfox-lyra-zero-w.sh (UVC kernel + all
# dependencies baked in — the slow apt step is then skipped automatically),
# and deploy every card with this script. It also still works on the plain
# community Ubuntu image (hotspot/dashboard only; that kernel has no UVC).
#
#   sudo bash scripts/prepare-sd.sh [--binary ./camera-box | --no-binary] [--image FILE|URL] \
#                                   [--ssid NAME] [--pass SECRET] [--ip 192.168.4.1/24] \
#                                   [--root-pass SECRET] [--no-grow]
#
# With no --binary it lists the release binaries and lets you pick one (or 's'
# to skip). --no-binary provisions deps + config only, for a binary you add later.
#
# --image accepts a local file OR an http(s) URL, and .img / .img.xz / .img.bz2
# / .img.gz (it streams + decompresses on the fly). So the whole flow — flash
# the Ubuntu image, then provision — can be one command, e.g.:
#
#   sudo bash scripts/prepare-sd.sh \
#     --image https://github.com/platima/SBC-Images/raw/main/Luckfox/Lyra/Lyra%20Zero%20W/<image>.img.bz2 \
#     --binary ./camera-box-luckfox-lyra-zero-w
#
# Requires: qemu-user-static + binfmt-support (for the ARM chroot), parted,
# e2fsprogs, growpart/sgdisk (to grow the rootfs), and a modern systemctl
# (offline --root enable). On Debian/Ubuntu:
#   sudo apt install qemu-user-static binfmt-support parted e2fsprogs \
#                    cloud-guest-utils gdisk curl
#
# NOTE: developed against the Pi build; not yet verified on Lyra hardware.
set -euo pipefail

# --- defaults ---------------------------------------------------------------
AP_SSID="CameraBox"
AP_PASS="CameraBox123"
AP_IP="192.168.4.1/24"
ROOT_PASS="camerabox"   # headless: preset root login (SSH over the AP)
BINARY=""
IMAGE=""
NOBIN=""
GROW=1   # grow the root partition to fill the card (--no-grow to disable)
REPO="codesource/camerabox"
MNT=""

die()  { echo "error: $*" >&2; exit 1; }
warn() { echo ">> WARN: $*" >&2; }
info() { echo ">> $*"; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing tool: $1"; }

# Sanity-check an image source (local file or URL) BEFORE writing it to the
# card, so a Git LFS pointer / HTML error page / truncated or misnamed download
# fails loudly instead of silently corrupting the SD card.
validate_image() {
    local src="$1" sample; sample="$(mktemp)"
    if [[ "$src" =~ ^https?:// ]]; then
        curl -fsL "$src" 2>/dev/null | head -c 2048 > "$sample" || true
    else
        local sz; sz="$(stat -c%s "$src" 2>/dev/null || echo 0)"
        [[ "${sz:-0}" -ge 1048576 ]] || { rm -f "$sample"; die "'$src' is only ${sz} bytes — too small to be a disk image (probably a Git LFS pointer or an error page). Re-download it."; }
        head -c 2048 "$src" > "$sample" 2>/dev/null || { rm -f "$sample"; die "cannot read $src"; }
    fi
    [[ -s "$sample" ]] || { rm -f "$sample"; die "could not read/fetch $src"; }

    if head -c 128 "$sample" | grep -qa 'git-lfs.github.com'; then
        rm -f "$sample"; die "'$src' is a Git LFS *pointer*, not the image. Download from the github.com/.../raw/ link with 'curl -fL' — raw.githubusercontent.com returns the pointer."
    fi
    if head -c 128 "$sample" | grep -qai '<!doctype\|<html\|<?xml'; then
        rm -f "$sample"; die "'$src' looks like an HTML page, not an image — check the download URL."
    fi

    local sig; sig="$(od -An -tx1 -N6 "$sample" 2>/dev/null | tr -d ' \n')"
    rm -f "$sample"
    case "$src" in
        *.bz2) [[ "$sig" == 425a68* ]]       || die "'$src' is not a bzip2 file (magic '$sig') — re-download it (see README)." ;;
        *.gz)  [[ "$sig" == 1f8b* ]]         || die "'$src' is not a gzip file (magic '$sig') — re-download it." ;;
        *.xz)  [[ "$sig" == fd377a585a00* ]] || die "'$src' is not an xz file (magic '$sig') — re-download it." ;;
        *) : ;;  # raw .img/.raw: the size + text checks above are the guard
    esac
}

usage() {
    awk 'NR>=2 && /^#/{sub(/^# ?/,"");print;next} NR>=2{exit}' "$0"
    exit "${1:-0}"
}

# --- args -------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary) BINARY="${2:-}"; shift 2 ;;
        --no-binary) NOBIN=1; shift ;;
        --grow) GROW=1; shift ;;      # kept for back-compat (now the default)
        --no-grow) GROW=""; shift ;;
        --image)  IMAGE="${2:-}";  shift 2 ;;
        --ssid)   AP_SSID="${2:-}"; shift 2 ;;
        --pass)   AP_PASS="${2:-}"; shift 2 ;;
        --ip)     AP_IP="${2:-}";   shift 2 ;;
        --root-pass) ROOT_PASS="${2:-}"; shift 2 ;;
        -h|--help) usage 0 ;;
        *) die "unknown argument: $1 (see --help)" ;;
    esac
done

[[ $EUID -eq 0 ]] || die "run as root (sudo ...)"
for t in lsblk parted e2fsck resize2fs findmnt install curl; do need "$t"; done
[[ -x /usr/bin/qemu-arm-static ]] || die "install qemu-user-static + binfmt-support first"
[[ ${#AP_PASS} -ge 8 && ${#AP_PASS} -le 63 ]] || die "--pass must be 8-63 characters"
[[ "$AP_IP" == */24 ]] || die "--ip must be a.b.c.d/24 (e.g. 192.168.4.1/24)"

# --- pick the SD card -------------------------------------------------------
root_src="$(findmnt -no SOURCE / || true)"
root_disk="$(lsblk -no pkname "$root_src" 2>/dev/null | head -1 || true)"

mapfile -t DISKS < <(lsblk -dpno NAME,TYPE | awk '$2=="disk"{print $1}')
[[ ${#DISKS[@]} -gt 0 ]] || die "no disks found"

echo "Available disks:"
for i in "${!DISKS[@]}"; do
    d="${DISKS[$i]}"
    size="$(lsblk -dno SIZE "$d" 2>/dev/null | tr -d ' ')"
    model="$(lsblk -dno MODEL "$d" 2>/dev/null | sed 's/ *$//')"
    tran="$(lsblk -dno TRAN "$d" 2>/dev/null | tr -d ' ')"
    mark=""
    [[ "$d" == "/dev/${root_disk}" ]] && mark="  <-- THIS PC'S SYSTEM DISK, do NOT pick"
    printf "  [%d] %-14s %6s  %-20s %-4s%s\n" "$i" "$d" "${size:-?}" "${model:-?}" "${tran:-?}" "$mark"
done
read -rp "Select the SD card number: " sel
[[ "$sel" =~ ^[0-9]+$ && -n "${DISKS[$sel]:-}" ]] || die "invalid selection"
DEV="${DISKS[$sel]}"
[[ "$DEV" == "/dev/${root_disk}" ]] && die "refusing to touch this PC's system disk"

echo
echo "Selected: $DEV  ($(lsblk -dno SIZE "$DEV" | tr -d ' '), $(lsblk -dno MODEL "$DEV" | sed 's/ *$//'))"
[[ -n "$IMAGE" ]] && echo "Will FLASH $IMAGE to it first (erases the card), then provision."
read -rp "Type YES to continue: " ans
[[ "$ans" == "YES" ]] || { echo "aborted."; exit 1; }

# partition N of DEV (mmcblk0 -> p3, sdX -> 3)
partof() { local d="$1" n="$2"; [[ "$d" == *[0-9] ]] && echo "${d}p${n}" || echo "${d}${n}"; }

# Unmount every partition of the card the desktop may have auto-mounted.
unmount_all() {
    local p
    for p in $(lsblk -lnpo NAME,TYPE "$DEV" 2>/dev/null | awk '$2=="part"{print $1}'); do
        umount "$p" 2>/dev/null || true
    done
}

# --- optional: flash a raw image first (local file OR http(s) URL) ----------
# Unmount BEFORE writing: dd over a mounted filesystem lets its dirty page
# cache scribble on the fresh image afterwards, and a mounted partition also
# blocks the kernel from re-reading the new partition table.
unmount_all
if [[ -n "$IMAGE" ]]; then
    is_url=""; [[ "$IMAGE" =~ ^https?:// ]] && is_url=1
    [[ -n "$is_url" || -f "$IMAGE" ]] || die "image not found: $IMAGE"
    info "checking the image before writing..."
    validate_image "$IMAGE"
    # pick a decompressor from the file extension (stream, don't buffer to disk)
    case "$IMAGE" in
        *.xz)        need xz;    decomp=(xz -dc) ;;
        *.bz2)       need bzip2; decomp=(bzip2 -dc) ;;
        *.gz)        need gzip;  decomp=(gzip -dc) ;;
        *.img|*.raw) decomp=(cat) ;;
        *.rar|*.zip) die "extract $IMAGE first, then pass the resulting .img" ;;
        *) warn "unrecognised image extension — writing bytes as-is"; decomp=(cat) ;;
    esac
    info "flashing ${is_url:+and downloading }$IMAGE -> $DEV (this takes a while)"
    if [[ -n "$is_url" ]]; then
        curl -fL --retry 3 "$IMAGE" | "${decomp[@]}" | dd of="$DEV" bs=4M status=progress conv=fsync
    else
        "${decomp[@]}" "$IMAGE" | dd of="$DEV" bs=4M status=progress conv=fsync
    fi
    sync
fi

# Make sure the kernel has adopted the CURRENT on-disk partition table. If a
# partition was auto-mounted during the flash, partprobe fails silently and
# every later step then runs against the STALE table (e.g. the previous
# image's single huge partition) — wrong rootfs, wrong grow, corrupted card.
# Verify kernel view == on-disk table, retrying with unmounts; never continue
# on a mismatch.
reread_ok=""
for _i in 1 2 3 4 5; do
    unmount_all
    partprobe "$DEV" 2>/dev/null || blockdev --rereadpt "$DEV" 2>/dev/null || true
    command -v udevadm >/dev/null && udevadm settle 2>/dev/null || true
    sleep 1
    ondisk="$(partx -g "$DEV" 2>/dev/null | wc -l)"
    kernel="$(lsblk -lnpo NAME,TYPE "$DEV" 2>/dev/null | awk '$2=="part"' | wc -l)"
    [[ "$ondisk" -gt 0 && "$ondisk" -eq "$kernel" ]] && { reread_ok=1; break; }
done
[[ -n "$reread_ok" ]] || die "the kernel still sees a stale partition table on $DEV (kernel: $kernel partitions, on-disk: $ondisk). Something is holding the card busy — close file managers, unplug and replug the card reader, then re-run this script."

# Find the root filesystem partition. Layouts differ per image (the Luckfox
# Ubuntu image and our minimal image use partition 3; others differ), so probe
# each ext*/btrfs/f2fs partition for a Linux root instead of assuming a number.
find_rootfs() {
    local p type lbl tmp sz cand=()
    # collect all Linux-filesystem partitions on the card — a real rootfs is
    # at least 64M (small uboot/boot partitions can also carry an ext
    # signature and must never win)
    for p in $(lsblk -lnpo NAME,TYPE "$DEV" 2>/dev/null | awk '$2=="part"{print $1}'); do
        sz="$(lsblk -bno SIZE "$p" 2>/dev/null | head -1)"
        [[ "${sz:-0}" -ge 67108864 ]] || continue
        umount "$p" 2>/dev/null || true   # the desktop may auto-mount it
        type="$(blkid -o value -s TYPE "$p" 2>/dev/null || true)"
        [[ -n "$type" ]] || type="$(lsblk -no FSTYPE "$p" 2>/dev/null || true)"
        case "$type" in ext2|ext3|ext4|btrfs|f2fs) cand+=("$p") ;; esac
    done
    [[ ${#cand[@]} -gt 0 ]] || return 1
    # exactly one -> that's the rootfs (no fragile probe-mount needed)
    if [[ ${#cand[@]} -eq 1 ]]; then echo "${cand[0]}"; return 0; fi
    # several -> prefer a root-labelled one
    for p in "${cand[@]}"; do
        lbl="$(blkid -o value -s LABEL "$p" 2>/dev/null || true)"
        case "$lbl" in *[Rr]oot*) echo "$p"; return 0 ;; esac
    done
    # else probe for the one that actually contains /etc + /usr
    for p in "${cand[@]}"; do
        tmp="$(mktemp -d)"
        if mount -o ro "$p" "$tmp" 2>/dev/null || mount -o ro,noload "$p" "$tmp" 2>/dev/null; then
            if [[ -d "$tmp/etc" && -d "$tmp/usr" ]]; then
                umount "$tmp" 2>/dev/null; rmdir "$tmp" 2>/dev/null; echo "$p"; return 0
            fi
            umount "$tmp" 2>/dev/null
        fi
        rmdir "$tmp" 2>/dev/null
    done
    # last resort: the largest Linux-fs partition
    local best="" bestsz=0 sz
    for p in "${cand[@]}"; do
        sz="$(lsblk -bno SIZE "$p" 2>/dev/null | head -1)"
        [[ "${sz:-0}" -gt "$bestsz" ]] && { bestsz="$sz"; best="$p"; }
    done
    [[ -n "$best" ]] && { echo "$best"; return 0; }
    return 1
}
# Right after dd + partprobe the kernel/blkid can lag behind the new partition
# table (and picking the wrong partition then cascades) — retry a few times.
ROOTP=""
for _try in 1 2 3; do
    ROOTP="$(find_rootfs)" && [[ -n "$ROOTP" ]] && break
    sleep 2
    partprobe "$DEV" 2>/dev/null || true
    command -v udevadm >/dev/null && udevadm settle 2>/dev/null || true
done
if [[ -z "$ROOTP" ]]; then
    echo "Partitions on $DEV:" >&2
    lsblk -po NAME,SIZE,FSTYPE,LABEL "$DEV" >&2 || true
    die "couldn't find a Linux root filesystem on $DEV. If one of the partitions above is your rootfs, paste this output — its filesystem may be unrecognised or too dirty to probe."
fi
ROOTNUM="${ROOTP##*[!0-9]}"
umount "$ROOTP" 2>/dev/null || true   # the desktop may have auto-mounted it
info "root filesystem: $ROOTP"

# --- grow the root partition to fill the card (default; --no-grow disables) --
# The camera-box minimal image ships a fixed-size rootfs and nothing on the
# device auto-expands it, so growing here is the default. It always targets
# the partition that CONTAINS the root filesystem ($ROOTP, found above).
lastpart="$(lsblk -lnpo NAME,TYPE "$DEV" 2>/dev/null | awk '$2=="part"{n=$1} END{print n}')"
if [[ -z "$GROW" ]]; then
    info "not resizing the card (--no-grow)"
elif [[ "$ROOTP" != "$lastpart" ]]; then
    warn "root partition $ROOTP is not the last partition — cannot grow it, skipping"
else
    info "growing root partition $ROOTP to fill the card"
    if command -v sgdisk >/dev/null 2>&1; then
        # a small image dd'd onto a bigger card strands GPT's backup header
        # mid-disk; move it to the end first
        sgdisk -e "$DEV" >/dev/null 2>&1 || true
        partprobe "$DEV" 2>/dev/null || true; sleep 1
    fi
    if command -v growpart >/dev/null 2>&1; then
        growpart "$DEV" "$ROOTNUM" || warn "growpart failed (continuing)"
    elif parted -s "$DEV" resizepart "$ROOTNUM" 100%; then
        :
    else
        warn "could not grow $ROOTP — install 'cloud-guest-utils' (growpart) or 'gdisk'."
    fi
    partprobe "$DEV" 2>/dev/null || true; sleep 1
    umount "$ROOTP" 2>/dev/null || true   # the desktop may have re-auto-mounted it
    e2fsck -fy "$ROOTP" || true
    resize2fs "$ROOTP" || warn "resize2fs failed (apt may run out of space)"
fi

# --- get the camera-box binary ----------------------------------------------
# No --binary given: offer the binaries from the latest release and let the user
# pick one (defaulting to the ARMv7 build the Lyra needs).
pick_binary() {
    local names=() json
    if json="$(curl -fsSL "https://api.github.com/repos/$REPO/releases/latest" 2>/dev/null)"; then
        mapfile -t names < <(printf '%s\n' "$json" \
            | grep -oE '"name":[[:space:]]*"camera-box-[^"]*"' \
            | sed -E 's/.*"(camera-box-[^"]*)".*/\1/' \
            | grep -v 'SHA256')
    fi
    if [[ ${#names[@]} -eq 0 ]]; then
        warn "couldn't read the release assets — showing the standard list"
        names=(camera-box-luckfox-lyra-zero-w camera-box-pi-zero-2w-armv7 \
               camera-box-pi-zero-w-armv6 camera-box-pi-zero-2w-arm64)
    fi
    # default to the Lyra-appropriate build (luckfox / armv7)
    local def=0 i
    for i in "${!names[@]}"; do
        case "${names[$i]}" in *luckfox*|*armv7*) def=$i; break;; esac
    done
    echo "Which camera-box binary to install? (the Lyra Zero W needs armv7 / luckfox)"
    for i in "${!names[@]}"; do
        local tag=""; case "${names[$i]}" in *luckfox*|*armv7*) tag="  (recommended for the Lyra)";; esac
        printf "  [%d] %s%s\n" "$i" "${names[$i]}" "$tag"
    done
    echo "  [s] skip — provision deps + config only (add the binary later)"
    local sel
    read -rp "Select a number, or 's' to skip [default $def]: " sel
    sel="${sel:-$def}"
    case "$sel" in
        s|S|skip) NOBIN=1; info "skipping the binary — deps + config only"; return ;;
    esac
    [[ "$sel" =~ ^[0-9]+$ && -n "${names[$sel]:-}" ]] || die "invalid selection"
    local asset="${names[$sel]}"
    BINARY="$(mktemp)"
    info "downloading $asset from the latest release"
    curl -fL "https://github.com/$REPO/releases/latest/download/$asset" -o "$BINARY" \
        || die "download failed for $asset — pass a local build with --binary"
}

if [[ -z "$NOBIN" && -z "$BINARY" ]]; then
    pick_binary
fi
if [[ -z "$NOBIN" ]]; then
    [[ -s "$BINARY" ]] || die "binary '$BINARY' is missing or empty — pass a real camera-box build with --binary (or --no-binary to skip)."
fi

# --- mount the rootfs (with cleanup on exit) --------------------------------
MNT="$(mktemp -d)"
cleanup() {
    set +e
    umount "$MNT/dev/pts" 2>/dev/null
    umount "$MNT/dev" "$MNT/proc" "$MNT/sys" 2>/dev/null
    umount -R "$MNT/dev" 2>/dev/null
    umount "$MNT" 2>/dev/null
    rmdir "$MNT" 2>/dev/null
}
trap cleanup EXIT
umount "$ROOTP" 2>/dev/null || true   # in case the desktop re-auto-mounted it
mount "$ROOTP" "$MNT"
[[ -d "$MNT/etc" && -d "$MNT/usr" ]] || die "$ROOTP doesn't look like a Linux root filesystem"

# --- install the binary -----------------------------------------------------
if [[ -n "$NOBIN" ]]; then
    warn "no binary installed — place one at /usr/local/bin/camera-box on the device"
    warn "(e.g. the installer, or copy your build) before camera-box will run."
else
    info "installing camera-box -> /usr/local/bin/camera-box"
    install -D -m 0755 "$BINARY" "$MNT/usr/local/bin/camera-box"
fi

# --- write the AP + service config (matches what camera-box generates) ------
ip="${AP_IP%/*}"; net3="${ip%.*}"
host="$(tr -d '[:space:]' < "$MNT/etc/hostname" 2>/dev/null || true)"; host="${host:-camera-box}"
info "pre-writing hotspot config (SSID '$AP_SSID', $AP_IP)"

mkdir -p "$MNT/etc/hostapd" "$MNT/etc/camera-box" "$MNT/etc/dnsmasq.d" \
         "$MNT/var/lib/camera-box" "$MNT/etc/systemd/system"

cat > "$MNT/etc/hostapd/hostapd.conf" <<EOF
country_code=CH
interface=wlan0
driver=nl80211
ctrl_interface=/var/run/hostapd

ssid=$AP_SSID

hw_mode=g
channel=6

ieee80211n=1
ht_capab=[SHORT-GI-20]
wmm_enabled=1
auth_algs=1
ignore_broadcast_ssid=0

wpa=2
wpa_passphrase=$AP_PASS
wpa_key_mgmt=WPA-PSK
rsn_pairwise=CCMP
EOF
echo 'DAEMON_CONF="/etc/hostapd/hostapd.conf"' > "$MNT/etc/default/hostapd"

cat > "$MNT/etc/dnsmasq.conf" <<EOF
# Managed by camera-box — do not edit.
interface=wlan0
bind-dynamic
dhcp-range=$net3.100,$net3.200,255.255.255.0,24h
dhcp-option=option:router,$ip
dhcp-option=option:dns-server,$ip
domain-needed
bogus-priv
conf-dir=/etc/dnsmasq.d/,*.conf
EOF

cat > "$MNT/etc/dnsmasq.d/camera-box.conf" <<EOF
address=/$host/$ip
address=/$host.local/$ip
EOF

cat > "$MNT/etc/systemd/system/camera-box-ip.service" <<EOF
[Unit]
Description=Set static IP for the camera-box AP
Before=dnsmasq.service hostapd.service
After=sys-subsystem-net-devices-wlan0.device
Wants=sys-subsystem-net-devices-wlan0.device

[Service]
Type=oneshot
# 'all', not 'wifi': on combo chips (Lyra's AIC8800DC) the BLUETOOTH rfkill
# drives the chip's power GPIO — blocked BT = unpowered chip = no wlan0.
ExecStartPre=-/usr/sbin/rfkill unblock all
ExecStart=/sbin/ip addr flush dev wlan0
ExecStart=/sbin/ip addr add $AP_IP dev wlan0
ExecStart=/sbin/ip link set wlan0 up
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

cat > "$MNT/etc/systemd/system/camera-box.service" <<EOF
[Unit]
Description=camera-box USB camera MJPEG appliance
After=network.target hostapd.service dnsmasq.service
Wants=hostapd.service dnsmasq.service

[Service]
Type=simple
ExecStart=/usr/local/bin/camera-box
Restart=on-failure
RestartSec=3
User=root
StandardOutput=journal
StandardError=journal
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
EOF

if [[ ! -f "$MNT/etc/camera-box/config.toml" ]]; then
    cat > "$MNT/etc/camera-box/config.toml" <<EOF
base_stream_port = 8080
web_port         = 80
device_ip        = "$ip"
ustreamer_path   = "/usr/bin/ustreamer"
resolution       = "1280x720"
fps              = 30
EOF
fi

cat > "$MNT/var/lib/camera-box/network.toml" <<EOF
ssid = "$AP_SSID"
password = "$AP_PASS"
ip_cidr = "$AP_IP"
EOF

# The USB Wi-Fi enumerates late (module load -> chip power -> firmware
# download) — when wlan0 finally appears, (re)start the AP chain. This beats
# any boot-order race between hostapd and the driver.
mkdir -p "$MNT/etc/udev/rules.d"
cat > "$MNT/etc/udev/rules.d/90-camera-box-wlan0.rules" <<'EOF'
ACTION=="add", SUBSYSTEM=="net", KERNEL=="wlan0", TAG+="systemd", ENV{SYSTEMD_WANTS}+="camera-box-ip.service hostapd.service dnsmasq.service"
EOF

# ...and if hostapd loses a race anyway, retry instead of staying dead.
mkdir -p "$MNT/etc/systemd/system/hostapd.service.d"
printf '[Service]\nRestart=on-failure\nRestartSec=3\n' \
    > "$MNT/etc/systemd/system/hostapd.service.d/camera-box.conf"

# The AIC8800 driver enables chip power-saving by default — in AP mode that
# tanks latency/throughput (the radio naps between beacons). Keep it awake.
mkdir -p "$MNT/etc/modprobe.d"
echo 'options aic8800_fdrv ps_on=0' > "$MNT/etc/modprobe.d/camera-box-aic8800.conf"

# Preseed the saved rfkill state as UNBLOCKED. On combo chips (the Lyra's
# AIC8800DC) the bluetooth rfkill drives the chip's power GPIO; the driver
# registers it blocked, systemd-rfkill saves that on first boot and then
# restores "blocked" forever — the chip never powers onto the USB bus.
mkdir -p "$MNT/var/lib/systemd/rfkill"
echo 0 > "$MNT/var/lib/systemd/rfkill/platform-wireless-bluetooth:bluetooth"

# If the image uses NetworkManager, stop it fighting over wlan0.
if [[ -d "$MNT/etc/NetworkManager" ]]; then
    mkdir -p "$MNT/etc/NetworkManager/conf.d"
    printf '[keyfile]\nunmanaged-devices=interface-name:wlan0\n' \
        > "$MNT/etc/NetworkManager/conf.d/camera-box.conf"
fi

# Keep the kernel's interface name: systemd's default naming policy renames
# USB Wi-Fi to wlx<MAC> the moment it appears, and every wlan0-based config
# (hostapd, dnsmasq, camera-box-ip, the udev rule) then misses it.
mkdir -p "$MNT/etc/systemd/network"
cat > "$MNT/etc/systemd/network/50-camera-box-wlan0.link" <<'EOF'
[Match]
OriginalName=wlan*

[Link]
NamePolicy=kernel
EOF

# Same for systemd-networkd (netplan/networkd images): a "dhcp on all
# interfaces" netplan profile would claim wlan0 and strip the AP's static IP
# at boot. Harmless where networkd is disabled, essential where it isn't.
mkdir -p "$MNT/etc/systemd/network"
cat > "$MNT/etc/systemd/network/05-wlan0-unmanaged.network" <<'EOF'
# Managed by camera-box — keep systemd-networkd off the AP interface.
[Match]
Name=wlan0

[Link]
Unmanaged=yes
EOF

# Headless AP-only first boot. The hotspot is the only way in on a console-less
# device: preset a root login (SSH over the AP), enable ssh, and don't block
# boot waiting for an uplink that doesn't exist.
info "configuring a headless AP-only first boot (root pw: $ROOT_PASS)"
echo "root:$ROOT_PASS" | chpasswd --root "$MNT" 2>/dev/null \
    || chroot "$MNT" /bin/bash -c "echo 'root:$ROOT_PASS' | chpasswd" 2>/dev/null \
    || warn "could not preset the root password"
systemctl --root="$MNT" enable ssh >/dev/null 2>&1 \
    || systemctl --root="$MNT" enable sshd >/dev/null 2>&1 || true
systemctl --root="$MNT" disable systemd-networkd-wait-online.service \
    NetworkManager-wait-online.service >/dev/null 2>&1 || true

# --- install runtime dependencies inside the ARM rootfs (qemu chroot) -------
# Images built by build-minimal-image-luckfox-lyra-zero-w.sh already contain
# every dependency — detect that and skip the slow emulated apt step entirely.
deps_present=1
[[ -e "$MNT/usr/sbin/hostapd" ]] || deps_present=""
[[ -e "$MNT/usr/sbin/dnsmasq" ]] || deps_present=""
[[ -e "$MNT/usr/sbin/wpa_supplicant" || -e "$MNT/sbin/wpa_supplicant" ]] || deps_present=""
[[ -e "$MNT/usr/sbin/iw" || -e "$MNT/sbin/iw" ]] || deps_present=""
[[ -e "$MNT/usr/sbin/rfkill" || -e "$MNT/usr/bin/rfkill" ]] || deps_present=""
if [[ -n "$deps_present" ]]; then
    info "dependencies already present in the image — skipping the apt chroot step"
    deps_ok=1
else
info "installing dependencies in the ARM rootfs (emulated — slow)"
cp /usr/bin/qemu-arm-static "$MNT/usr/bin/" 2>/dev/null || true
mkdir -p "$MNT/tmp" && chmod 1777 "$MNT/tmp"
mount --bind /dev "$MNT/dev"
mkdir -p "$MNT/dev/pts"; mount -t devpts devpts "$MNT/dev/pts" 2>/dev/null || true
mount --bind /proc "$MNT/proc"
mount --bind /sys "$MNT/sys"
# DNS for the emulated chroot. Debian-based images point resolv.conf at the
# systemd-resolved stub (127.0.0.53) and use the 'resolve' NSS module, which
# can't work under qemu (resolved isn't running here) — apt then fails with
# getaddrinfo "Device or resource busy". Swap in a plain resolver + nsswitch
# for the duration and restore them afterwards.
resolv_bak=""; nss_bak=""
if [[ -e "$MNT/etc/resolv.conf" || -L "$MNT/etc/resolv.conf" ]]; then
    cp -a "$MNT/etc/resolv.conf" "$MNT/etc/resolv.conf.camerabox-bak" && resolv_bak=1
fi
rm -f "$MNT/etc/resolv.conf"
printf 'nameserver 1.1.1.1\nnameserver 8.8.8.8\n' > "$MNT/etc/resolv.conf"
if [[ -f "$MNT/etc/nsswitch.conf" ]] && grep -q '^hosts:.*resolve' "$MNT/etc/nsswitch.conf"; then
    cp -a "$MNT/etc/nsswitch.conf" "$MNT/etc/nsswitch.conf.camerabox-bak" && nss_bak=1
    sed -i 's/^hosts:.*/hosts: files dns/' "$MNT/etc/nsswitch.conf"
fi
deps_ok=1
chroot "$MNT" /bin/bash -e <<'CHROOT' || deps_ok=0
export DEBIAN_FRONTEND=noninteractive
# In an emulated chroot: run apt as root (its _apt sandbox user can't write
# /tmp, which breaks apt-key and then 404s on stale indexes); and keep our
# pre-written config files without the interactive dpkg conffile prompt
# (--force-conf* — there is no terminal to answer it).
APT="apt-get -o APT::Sandbox::User=root -o Acquire::Languages=none -o Dpkg::Options::=--force-confold -o Dpkg::Options::=--force-confdef"
# recover any half-configured packages left by an earlier interrupted run
dpkg --configure -a --force-confold --force-confdef 2>/dev/null || true
$APT update
$APT install -y hostapd dnsmasq iw wpasupplicant isc-dhcp-client avahi-daemon rfkill curl ca-certificates
$APT install -y ustreamer || echo ">> NOTE: 'ustreamer' not in apt — build it on the device (see docs)"
CHROOT
umount "$MNT/dev/pts" 2>/dev/null || true
umount "$MNT/dev" "$MNT/proc" "$MNT/sys" 2>/dev/null || true
rm -f "$MNT/usr/bin/qemu-arm-static"
# restore the image's original resolv.conf / nsswitch
[[ -n "$resolv_bak" ]] && mv -f "$MNT/etc/resolv.conf.camerabox-bak" "$MNT/etc/resolv.conf" || true
[[ -n "$nss_bak" ]] && mv -f "$MNT/etc/nsswitch.conf.camerabox-bak" "$MNT/etc/nsswitch.conf" || true
[[ "$deps_ok" == 1 ]] || warn "DEPENDENCY INSTALL FAILED — hostapd/dnsmasq/etc are NOT installed; the box won't host the hotspot until you fix apt (network?) and re-run this script."
fi

# --- enable the services for boot (offline, via the host systemctl) ---------
info "enabling services for first boot"
if systemctl --root="$MNT" unmask hostapd >/dev/null 2>&1 \
   && systemctl --root="$MNT" enable camera-box camera-box-ip hostapd dnsmasq >/dev/null 2>&1; then
    :
else
    warn "offline 'systemctl --root' unavailable — creating wants symlinks manually"
    rm -f "$MNT/etc/systemd/system/hostapd.service"   # remove the default mask, if any
    w="$MNT/etc/systemd/system/multi-user.target.wants"; mkdir -p "$w"
    ln -sf /etc/systemd/system/camera-box.service    "$w/camera-box.service"
    ln -sf /etc/systemd/system/camera-box-ip.service "$w/camera-box-ip.service"
    for u in hostapd dnsmasq; do
        for base in /lib/systemd/system /usr/lib/systemd/system; do
            [[ -f "$MNT$base/$u.service" ]] && { ln -sf "$base/$u.service" "$w/$u.service"; break; }
        done
    done
fi

sync
if [[ "${deps_ok:-1}" != 1 ]]; then
    info "FINISHED WITH ERRORS — dependencies did not install (see the WARN above)."
    echo "Fix networking / apt on this PC and re-run the script (skip --image; the"
    echo "card is already flashed) to complete the install before using the board."
    exit 1
fi
info "done."
echo
if [[ -n "$NOBIN" ]]; then
    echo "NOTE: no camera-box binary was installed. Dependencies + hotspot config"
    echo "are in place; add /usr/local/bin/camera-box on the device to finish."
    echo
fi
echo "Insert the card into the Luckfox Lyra Zero W and power it on."
echo "(On a Rockchip board you may need to erase the onboard SPI flash once so it"
echo " boots from the SD card — see docs/luckfox-lyra-zero-w.md.)"
echo "It should host the '$AP_SSID' Wi-Fi hotspot; connect, then:"
echo "    http://$ip/            dashboard (login: admin / password)"
echo "    ssh root@$ip     (password: $ROOT_PASS — change it)"
echo
echo "Then verify the camera works:"
echo "    sudo modprobe uvcvideo && ls -l /dev/video*"
echo "If /dev/video0 does NOT appear, the image's kernel lacks UVC — build our"
echo "own image with scripts/build-minimal-image-luckfox-lyra-zero-w.sh (see"
echo "docs/minimal-lyra-image.md) and re-run this script with --image on it."
