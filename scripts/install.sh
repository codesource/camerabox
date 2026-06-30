#!/usr/bin/env bash
#
# Install camera-box on a Raspberry Pi. Run ON THE PI as root, from the repo
# root (which must also contain the cross-built binary, or pass its path):
#
#   sudo bash scripts/install.sh [path-to-camera-box-binary]
#
# Defaults to ./camera-box if no path is given.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
BIN="${1:-$ROOT/camera-box}"

PREFIX=/usr/local/bin
CONF_DIR=/etc/camera-box
UNIT=/etc/systemd/system/camera-box.service

if [[ $EUID -ne 0 ]]; then
    echo "error: run as root (sudo bash scripts/install.sh ...)" >&2
    exit 1
fi

if [[ ! -f "$BIN" ]]; then
    echo "error: binary not found at: $BIN" >&2
    echo "       build it first (see README) and place it at the repo root," >&2
    echo "       or pass its path as the first argument." >&2
    exit 1
fi

# 1. Runtime dependency: ustreamer (does the actual MJPEG streaming).
if ! command -v ustreamer >/dev/null 2>&1; then
    echo ">> installing ustreamer..."
    apt-get update
    apt-get install -y ustreamer
fi

# 2. Install the daemon binary.
echo ">> installing binary -> $PREFIX/camera-box"
install -m 0755 "$BIN" "$PREFIX/camera-box"

# 3. Install config, but never clobber an existing one.
mkdir -p "$CONF_DIR"
if [[ -f "$CONF_DIR/config.toml" ]]; then
    echo ">> keeping existing $CONF_DIR/config.toml"
else
    echo ">> installing default config -> $CONF_DIR/config.toml"
    install -m 0644 "$ROOT/config.example.toml" "$CONF_DIR/config.toml"
fi

# 4. Install and enable the systemd service.
echo ">> installing systemd unit -> $UNIT"
install -m 0644 "$ROOT/systemd/camera-box.service" "$UNIT"
systemctl daemon-reload
systemctl enable --now camera-box.service

echo
echo ">> done. Status:"
systemctl --no-pager --full status camera-box.service || true
