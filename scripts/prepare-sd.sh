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
#   sudo bash scripts/prepare-sd.sh [--binary ./camera-box] [--image ubuntu.img[.xz|.bz2|.gz]] \
#                                   [--ssid NAME] [--pass SECRET] [--ip 192.168.4.1/24]
#
# Requires: qemu-user-static + binfmt-support (for the ARM chroot), parted,
# e2fsprogs, and a modern systemctl (offline --root enable). On Debian/Ubuntu:
#   sudo apt install qemu-user-static binfmt-support parted e2fsprogs
#
# NOTE: developed against the Pi build; not yet verified on Lyra hardware.
set -euo pipefail

# --- defaults ---------------------------------------------------------------
AP_SSID="CameraBox"
AP_PASS="CameraBox123"
AP_IP="192.168.4.1/24"
BINARY=""
IMAGE=""
REPO="codesource/camerabox"
MNT=""

die()  { echo "error: $*" >&2; exit 1; }
warn() { echo ">> WARN: $*" >&2; }
info() { echo ">> $*"; }
need() { command -v "$1" >/dev/null 2>&1 || die "missing tool: $1"; }

usage() {
    awk 'NR>=2 && /^#/{sub(/^# ?/,"");print;next} NR>=2{exit}' "$0"
    exit "${1:-0}"
}

# --- args -------------------------------------------------------------------
while [[ $# -gt 0 ]]; do
    case "$1" in
        --binary) BINARY="${2:-}"; shift 2 ;;
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

# --- optional: flash a raw image first --------------------------------------
if [[ -n "$IMAGE" ]]; then
    [[ -f "$IMAGE" ]] || die "image not found: $IMAGE"
    info "flashing $IMAGE -> $DEV (this takes a while)"
    case "$IMAGE" in
        *.xz)  need xz;    xz -dc  "$IMAGE" | dd of="$DEV" bs=4M status=progress conv=fsync ;;
        *.bz2) need bzip2; bzip2 -dc "$IMAGE" | dd of="$DEV" bs=4M status=progress conv=fsync ;;
        *.gz)  need gzip;  gzip -dc "$IMAGE" | dd of="$DEV" bs=4M status=progress conv=fsync ;;
        *)     dd if="$IMAGE" of="$DEV" bs=4M status=progress conv=fsync ;;
    esac
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
    parted -s "$DEV" resizepart 3 100% || warn "resizepart failed (continuing)"
    partprobe "$DEV" 2>/dev/null || true; sleep 1
    e2fsck -fy "$ROOTP" || true
    resize2fs "$ROOTP" || warn "resize2fs failed (apt may run out of space)"
else
    warn "partition 3 is not the last partition — skipping auto-grow"
fi

# --- get the camera-box binary ----------------------------------------------
if [[ -z "$BINARY" ]]; then
    BINARY="$(mktemp)"
    info "downloading camera-box (armv7) from the latest release"
    ok=""
    for a in camera-box-luckfox-lyra-zero-w camera-box-pi-zero-2w-armv7; do
        if curl -fL "https://github.com/$REPO/releases/latest/download/$a" -o "$BINARY" 2>/dev/null; then ok=1; break; fi
    done
    [[ -n "$ok" ]] || die "could not download a binary — pass one with --binary"
fi
[[ -s "$BINARY" ]] || die "binary is empty: $BINARY"

# --- mount the rootfs (with cleanup on exit) --------------------------------
MNT="$(mktemp -d)"
cleanup() {
    set +e
    umount "$MNT/dev" "$MNT/proc" "$MNT/sys" 2>/dev/null
    umount "$MNT" 2>/dev/null
    rmdir "$MNT" 2>/dev/null
}
trap cleanup EXIT
mount "$ROOTP" "$MNT"
[[ -d "$MNT/etc" && -d "$MNT/usr" ]] || die "partition 3 doesn't look like a Linux root filesystem"

# --- install the binary -----------------------------------------------------
info "installing camera-box -> /usr/local/bin/camera-box"
install -D -m 0755 "$BINARY" "$MNT/usr/local/bin/camera-box"

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
mount --bind /dev "$MNT/dev"
mount --bind /proc "$MNT/proc"
mount --bind /sys "$MNT/sys"
cp /etc/resolv.conf "$MNT/etc/resolv.conf" 2>/dev/null || true
chroot "$MNT" /bin/bash -e <<'CHROOT' || warn "dependency install hit an error — check the log above"
export DEBIAN_FRONTEND=noninteractive
apt-get update
apt-get install -y hostapd dnsmasq iw wpasupplicant isc-dhcp-client avahi-daemon rfkill
apt-get install -y ustreamer || echo ">> WARN: 'ustreamer' not in apt — install or build it on the device"
CHROOT
umount "$MNT/dev" "$MNT/proc" "$MNT/sys" 2>/dev/null || true
rm -f "$MNT/usr/bin/qemu-arm-static"

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
info "done."
echo
echo "Insert the card into the Luckfox Lyra Zero W and power it on."
echo "It should host the '$AP_SSID' Wi-Fi hotspot; connect and browse:"
echo "    http://$ip/    (login: admin / password)"
echo
echo "Verify on the device: 'systemctl status camera-box hostapd', a USB camera"
echo "as /dev/video0, and 'which ustreamer'."
