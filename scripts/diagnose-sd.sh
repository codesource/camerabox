#!/usr/bin/env bash
#
# diagnose-sd.sh — inspect a camera-box-provisioned SD rootfs and report why the
# hotspot / appliance might not come up. READ-ONLY; run on a Linux PC.
#
#   sudo bash scripts/diagnose-sd.sh /run/media/you/armbi_root   # a mounted rootfs
#   sudo bash scripts/diagnose-sd.sh /dev/sde1                   # or a partition (mounted ro)
#
# Paste the output back to Claude for analysis.
set -uo pipefail

ARG="${1:-}"
[[ -n "$ARG" ]] || { echo "usage: sudo bash diagnose-sd.sh <rootfs-mountpoint|partition>"; exit 1; }
[[ $EUID -eq 0 ]] || echo "(hint: run with sudo — /etc/shadow and some files need root to read)"

R=""; MNT=""
if [[ -b "$ARG" ]]; then
    MNT="$(mktemp -d)"
    mount -o ro "$ARG" "$MNT" 2>/dev/null || mount -o ro,noload "$ARG" "$MNT" 2>/dev/null \
        || { echo "error: cannot mount $ARG"; exit 1; }
    R="$MNT"
elif [[ -d "$ARG" ]]; then
    R="$ARG"
else
    echo "error: not a directory or block device: $ARG"; exit 1
fi
trap '[[ -n "$MNT" ]] && { umount "$MNT" 2>/dev/null; rmdir "$MNT" 2>/dev/null; }' EXIT

ok()   { echo "  [ OK ] $*"; }
bad()  { echo "  [FAIL] $*"; }
note() { echo "  [ .. ] $*"; }
hdr()  { echo; echo "== $* =="; }

hdr "OS"
[[ -f "$R/etc/os-release" ]] && grep -E '^(PRETTY_NAME|VERSION_CODENAME)=' "$R/etc/os-release" | sed 's/^/  /'
[[ -f "$R/etc/armbian-release" ]] && grep -E '^(BOARD|VERSION|BRANCH|LINUXFAMILY)=' "$R/etc/armbian-release" | sed 's/^/  /'

hdr "camera-box binary"
if [[ -x "$R/usr/local/bin/camera-box" ]]; then
    ok "present"; file "$R/usr/local/bin/camera-box" 2>/dev/null | sed 's/^/       /'
else bad "/usr/local/bin/camera-box missing"; fi

hdr "AP config files"
for f in etc/hostapd/hostapd.conf etc/default/hostapd etc/dnsmasq.conf \
         etc/systemd/system/camera-box.service etc/systemd/system/camera-box-ip.service; do
    [[ -f "$R/$f" ]] && ok "$f" || bad "$f MISSING"
done
[[ -f "$R/etc/hostapd/hostapd.conf" ]] && grep -E '^(interface|ssid|wpa_passphrase|channel|hw_mode|country_code)=' "$R/etc/hostapd/hostapd.conf" | sed 's/^/       /'
[[ -f "$R/etc/default/hostapd" ]] && grep -E '^DAEMON_CONF' "$R/etc/default/hostapd" | sed 's/^/       /'
[[ -f "$R/etc/dnsmasq.conf" ]] && grep -E '^(interface|dhcp-range|bind)' "$R/etc/dnsmasq.conf" | sed 's/^/       /'

hdr "required packages actually installed"
check() { local n="$1"; shift; for p in "$@"; do [[ -e "$R$p" ]] && { ok "$n ($p)"; return; }; done; bad "$n NOT installed"; }
check hostapd        /usr/sbin/hostapd
check dnsmasq        /usr/sbin/dnsmasq
check wpa_supplicant /usr/sbin/wpa_supplicant /sbin/wpa_supplicant
check iw             /usr/sbin/iw /sbin/iw
check ustreamer      /usr/bin/ustreamer /usr/local/bin/ustreamer
check rfkill         /usr/sbin/rfkill /usr/bin/rfkill

hdr "services enabled for boot"
W="$R/etc/systemd/system/multi-user.target.wants"
for s in camera-box camera-box-ip hostapd dnsmasq ssh; do
    [[ -L "$W/$s.service" ]] && ok "$s enabled" || note "$s NOT in multi-user.target.wants"
done
if [[ -L "$R/etc/systemd/system/hostapd.service" ]]; then
    t="$(readlink "$R/etc/systemd/system/hostapd.service")"
    [[ "$t" == /dev/null ]] && bad "hostapd is MASKED (-> /dev/null) — it will NOT start" \
                            || note "hostapd.service -> $t"
fi

hdr "headless / remote access"
[[ -e "$R/root/.not_logged_in_yet" ]] \
    && bad "Armbian first-run wizard still armed (/root/.not_logged_in_yet) — a console-less boot can block login" \
    || ok "first-run wizard flag cleared"
if [[ -r "$R/etc/shadow" ]]; then
    grep -q '^root:[$]' "$R/etc/shadow" && ok "root password is set" || bad "root has NO password (SSH login will fail)"
else note "can't read /etc/shadow (run with sudo)"; fi
[[ -f "$R/etc/NetworkManager/conf.d/camera-box.conf" ]] && ok "NetworkManager told to ignore wlan0" \
    || note "no NM unmanaged-wlan0 rule (fine if the image doesn't use NetworkManager)"

hdr "kernel modules (camera + wifi)"
km="$(ls -d "$R"/lib/modules/*/ 2>/dev/null | head -1)"
if [[ -n "$km" ]]; then
    note "kernel: $(basename "$km")"
    { find "$km" -iname 'uvcvideo*' 2>/dev/null | grep -q .; } \
        && ok "uvcvideo present (USB cameras can work)" \
        || bad "uvcvideo NOT built — USB cameras won't give /dev/video0 (needs a UVC kernel)"
    { find "$km" -iname '*aic8800*' 2>/dev/null | grep -q .; } \
        && ok "aic8800 wifi module present" \
        || note "no aic8800 module file (may be built into the kernel)"
else note "no /lib/modules on this partition"; fi

hdr "persisted logs (if any)"
if [[ -d "$R/var/log/journal" ]] && command -v journalctl >/dev/null 2>&1; then
    echo "  last boot, hostapd/wlan/aic lines:"
    journalctl --directory="$R/var/log/journal" -b -1 --no-pager 2>/dev/null \
        | grep -iE 'hostapd|wlan0|aic|dnsmasq|camera-box' | tail -30 | sed 's/^/       /' \
        || note "no matching journal entries"
else
    note "no persisted journal (Armbian usually logs to RAM) — for hostapd errors you"
    note "need the booted device: 'journalctl -u hostapd -b' over serial/SSH"
fi

echo
echo "Done. Paste this report back to Claude."
