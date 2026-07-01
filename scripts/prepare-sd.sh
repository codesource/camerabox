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
#   sudo bash scripts/prepare-sd.sh [--binary ./camera-box | --no-binary] [--image FILE|URL] \
#                                   [--ssid NAME] [--pass SECRET] [--ip 192.168.4.1/24]
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
BINARY=""
IMAGE=""
NOBIN=""
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
        --image)  IMAGE="${2:-}";  shift 2 ;;
        --ssid)   AP_SSID="${2:-}"; shift 2 ;;
        --pass)   AP_PASS="${2:-}"; shift 2 ;;
        --ip)     AP_IP="${2:-}";   shift 2 ;;
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

# --- optional: flash a raw image first (local file OR http(s) URL) ----------
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

partprobe "$DEV" 2>/dev/null || true
command -v udevadm >/dev/null && udevadm settle 2>/dev/null || true
sleep 1
ROOTP="$(partof "$DEV" 3)"
[[ -b "$ROOTP" ]] || die "no partition 3 on $DEV — is it flashed with the Ubuntu image?"

# --- grow the rootfs to fill the card (image ships nearly full) --------------
if lsblk -lno NAME "$DEV" | tail -1 | grep -q "$(basename "$ROOTP")"; then
    info "growing rootfs $ROOTP to fill the card"
    # A small image dd'd onto a big card leaves GPT's backup header stranded
    # mid-disk, so parted can't grow the partition. Move it to the end first.
    if command -v sgdisk >/dev/null 2>&1; then
        sgdisk -e "$DEV" >/dev/null 2>&1 || true
        partprobe "$DEV" 2>/dev/null || true; sleep 1
    fi
    if command -v growpart >/dev/null 2>&1; then
        growpart "$DEV" 3 || warn "growpart failed (continuing)"
    elif parted -s "$DEV" resizepart 3 100%; then
        :
    else
        warn "could not grow partition 3 — install 'cloud-guest-utils' (growpart) or"
        warn "'gdisk' (sgdisk) and re-run, otherwise the rootfs stays small and apt"
        warn "may run out of space."
    fi
    partprobe "$DEV" 2>/dev/null || true; sleep 1
    e2fsck -fy "$ROOTP" || true
    resize2fs "$ROOTP" || warn "resize2fs failed (apt may run out of space)"
else
    warn "partition 3 is not the last partition — skipping auto-grow"
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
mount "$ROOTP" "$MNT"
[[ -d "$MNT/etc" && -d "$MNT/usr" ]] || die "partition 3 doesn't look like a Linux root filesystem"

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

ssid=$AP_SSID

hw_mode=g
channel=6

ieee80211n=1
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
ExecStartPre=-/usr/sbin/rfkill unblock wifi
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

# If the image uses NetworkManager, stop it fighting over wlan0.
if [[ -d "$MNT/etc/NetworkManager" ]]; then
    mkdir -p "$MNT/etc/NetworkManager/conf.d"
    printf '[keyfile]\nunmanaged-devices=interface-name:wlan0\n' \
        > "$MNT/etc/NetworkManager/conf.d/camera-box.conf"
fi

# --- install runtime dependencies inside the ARM rootfs (qemu chroot) -------
info "installing dependencies in the ARM rootfs (emulated — slow)"
cp /usr/bin/qemu-arm-static "$MNT/usr/bin/" 2>/dev/null || true
mkdir -p "$MNT/tmp" && chmod 1777 "$MNT/tmp"
mount --bind /dev "$MNT/dev"
mkdir -p "$MNT/dev/pts"; mount -t devpts devpts "$MNT/dev/pts" 2>/dev/null || true
mount --bind /proc "$MNT/proc"
mount --bind /sys "$MNT/sys"
cp /etc/resolv.conf "$MNT/etc/resolv.conf" 2>/dev/null || true
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
$APT install -y hostapd dnsmasq iw wpasupplicant isc-dhcp-client avahi-daemon rfkill
$APT install -y ustreamer || echo ">> NOTE: 'ustreamer' not in apt — build it on the device (see docs)"
CHROOT
umount "$MNT/dev/pts" 2>/dev/null || true
umount "$MNT/dev" "$MNT/proc" "$MNT/sys" 2>/dev/null || true
rm -f "$MNT/usr/bin/qemu-arm-static"
[[ "$deps_ok" == 1 ]] || warn "DEPENDENCY INSTALL FAILED — hostapd/dnsmasq/etc are NOT installed; the box won't host the hotspot until you fix apt (network?) and re-run this script."

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
echo "It should host the '$AP_SSID' Wi-Fi hotspot; connect and browse:"
echo "    http://$ip/    (login: admin / password)"
echo
echo "Verify on the device: 'systemctl status camera-box hostapd', a USB camera"
echo "as /dev/video0, and 'which ustreamer'."
