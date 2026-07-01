#!/usr/bin/env bash
#
# camera-box installer — run ON THE PI as root. Installs the runtime
# dependencies, the prebuilt daemon, a systemd service, and starts it.
# No repo checkout is required; everything else is embedded or downloaded.
#
# Install straight from the latest release (pick your board):
#
#   curl -fsSL https://raw.githubusercontent.com/codesource/camerabox/main/scripts/install.sh \
#     | sudo bash -s -- pi-zero-w-armv6
#
#   boards: pi-zero-w-armv6 | pi-zero-2w-armv7 | pi-zero-2w-arm64
#
# Or install a binary you already have (e.g. a local build):
#
#   sudo bash install.sh /path/to/camera-box
#
set -euo pipefail

REPO=codesource/camerabox
PREFIX=/usr/local/bin
CONF_DIR=/etc/camera-box
UNIT=/etc/systemd/system/camera-box.service

if [[ $EUID -ne 0 ]]; then
    echo "error: run as root (sudo ...)" >&2
    exit 1
fi

usage() {
    echo "usage: install.sh <board|binary-path>" >&2
    echo "  board: pi-zero-w-armv6 | pi-zero-2w-armv7 | pi-zero-2w-arm64" >&2
    exit 1
}

ARG="${1:-}"
case "$ARG" in
    pi-zero-w-armv6 | pi-zero-2w-armv7 | pi-zero-2w-arm64)
        asset="camera-box-$ARG"
        BIN=/tmp/camera-box.download
        echo ">> downloading $asset from the latest release..."
        curl -fL "https://github.com/$REPO/releases/latest/download/$asset" -o "$BIN"
        ;;
    "")
        usage
        ;;
    *)
        BIN="$ARG" # treat anything else as a path to a local binary
        [[ -f "$BIN" ]] || { echo "error: not a known board nor a file: $ARG" >&2; usage; }
        ;;
esac

# 1. Runtime dependencies.
#    ustreamer                       — the actual MJPEG streaming
#    hostapd, dnsmasq                — Wi-Fi hotspot (AP mode + its DHCP)
#    iw, wpasupplicant, dhcp client  — Wi-Fi scanning + client mode
#    (hostnamectl, rfkill are part of the base system)
PKGS="ustreamer hostapd dnsmasq iw wpasupplicant isc-dhcp-client"
missing=""
for p in $PKGS; do dpkg -s "$p" >/dev/null 2>&1 || missing="$missing $p"; done
if [[ -n "$missing" ]]; then
    echo ">> installing dependencies:$missing"
    apt-get update
    apt-get install -y $missing
fi

# 2. Install the daemon binary.
echo ">> installing binary -> $PREFIX/camera-box"
install -m 0755 "$BIN" "$PREFIX/camera-box"

# 3. Default config, but never clobber an existing one.
mkdir -p "$CONF_DIR"
if [[ -f "$CONF_DIR/config.toml" ]]; then
    echo ">> keeping existing $CONF_DIR/config.toml"
else
    echo ">> writing default config -> $CONF_DIR/config.toml"
    cat >"$CONF_DIR/config.toml" <<'CONF'
# camera-box configuration. All keys are optional; the built-in defaults are
# shown below. Login credentials are NOT here — they default to admin/password
# and are changed from the web UI (or `camera-box reset-password`).
base_stream_port = 8080
web_port         = 80
device_ip        = "192.168.4.1"   # fallback for links only; usually auto-detected
ustreamer_path   = "/usr/bin/ustreamer"
resolution       = "1280x720"
fps              = 30
CONF
fi

# 4. Install and enable the systemd service.
echo ">> installing systemd unit -> $UNIT"
cat >"$UNIT" <<'UNITEOF'
[Unit]
Description=camera-box USB camera MJPEG appliance
Documentation=https://github.com/codesource/camerabox
# The Wi-Fi AP must be up first so clients can reach the streams.
After=network.target hostapd.service dnsmasq.service
Wants=hostapd.service dnsmasq.service

[Service]
Type=simple
ExecStart=/usr/local/bin/camera-box
Restart=on-failure
RestartSec=3
# Needs root for: binding port 80, opening /dev/video*, the netlink uevent
# multicast group, and driving the Wi-Fi tools.
User=root
StandardOutput=journal
StandardError=journal
NoNewPrivileges=true

[Install]
WantedBy=multi-user.target
UNITEOF

systemctl daemon-reload
systemctl enable --now camera-box.service

echo
echo ">> done. Dashboard: http://<device-ip>/  (login admin / password)"
systemctl --no-pager --full status camera-box.service || true
