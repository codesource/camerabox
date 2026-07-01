# Building a ready-to-run Armbian image for the Luckfox Lyra Zero W

> **You probably don't need this.** Building a kernel is only necessary if the
> **prebuilt** Armbian image's kernel lacks UVC — and Armbian kernels usually
> include it. Try the fast path first:
>
> 1. Flash the prebuilt **Armbian minimal** image, erase the SPI flash, and
>    configure it with `prepare-sd.sh` (installs camera-box + deps + the hotspot,
>    and sets up a headless AP-only first boot). No build.
> 2. Boot it and run `sudo modprobe uvcvideo && ls /dev/video*`.
> 3. **If `/dev/video0` appears, you're done** — never build anything.
> 4. **Only if it doesn't** (kernel without UVC) do you need this guide.

If you got here because UVC is genuinely missing, this uses the
[Armbian build framework](https://github.com/armbian/build) to bake a custom
image that already contains a **UVC-enabled kernel**, **camera-box + its
dependencies**, and the **hotspot config** — so you flash it, erase the SPI
flash once, and it boots straight into the appliance. No `prepare-sd.sh`, no
qemu-chroot hacks, no on-device setup — and it enables UVC in the kernel, which
configuring a prebuilt image cannot do.

> One-time build on a Linux PC (Ubuntu 24.04 or Docker), ~50 GB disk, a couple of
> hours. After that you have a reproducible image you can reflash anytime.

## 1. Get the build framework

```sh
git clone --depth=1 https://github.com/armbian/build
cd build
```

Host: an **x86-64 machine running Ubuntu 24.04 (Noble)**, or **Docker** on any
Linux (the framework runs itself in a container), or **WSL2** on Windows. Needs
~8 GB RAM and ~50 GB disk. Run everything with `sudo`/root.

## 2. Drop in the camera-box binary

Put the ARMv7 binary where the build can see it (create the dirs if needed):

```sh
mkdir -p userpatches/overlay
cp /path/to/camera-box-luckfox-lyra-zero-w userpatches/overlay/camera-box
```

`userpatches/overlay/` is exposed as `/tmp/overlay/` inside the build chroot.

## 3. Bake in camera-box — `userpatches/customize-image.sh`

This script runs in the image's chroot near the end of the build (apt + systemd
work correctly there — none of the offline-provisioning quirks apply). It
installs the deps, the binary, the hotspot config, and enables the services:

```bash
#!/bin/bash
# userpatches/customize-image.sh   (args: $1=RELEASE $2=LINUXFAMILY $3=BOARD $4=DESKTOP)
set -euo pipefail

AP_SSID="CameraBox"
AP_PASS="CameraBox123"
AP_IP="192.168.4.1"

export DEBIAN_FRONTEND=noninteractive
apt-get -y update
apt-get -y install hostapd dnsmasq iw wpasupplicant isc-dhcp-client avahi-daemon rfkill
apt-get -y install ustreamer || echo "NOTE: ustreamer not in the repo — install/build on device"

install -D -m 0755 /tmp/overlay/camera-box /usr/local/bin/camera-box

mkdir -p /etc/hostapd /etc/camera-box /etc/dnsmasq.d /var/lib/camera-box

cat > /etc/hostapd/hostapd.conf <<EOF
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
echo 'DAEMON_CONF="/etc/hostapd/hostapd.conf"' > /etc/default/hostapd

cat > /etc/dnsmasq.conf <<EOF
# Managed by camera-box — do not edit.
interface=wlan0
bind-dynamic
dhcp-range=192.168.4.100,192.168.4.200,255.255.255.0,24h
dhcp-option=option:router,$AP_IP
dhcp-option=option:dns-server,$AP_IP
domain-needed
bogus-priv
conf-dir=/etc/dnsmasq.d/,*.conf
EOF

cat > /etc/dnsmasq.d/camera-box.conf <<EOF
address=/camera-box/$AP_IP
address=/camera-box.local/$AP_IP
EOF

cat > /etc/systemd/system/camera-box-ip.service <<EOF
[Unit]
Description=Set static IP for the camera-box AP
Before=dnsmasq.service hostapd.service
After=sys-subsystem-net-devices-wlan0.device
Wants=sys-subsystem-net-devices-wlan0.device
[Service]
Type=oneshot
ExecStartPre=-/usr/sbin/rfkill unblock wifi
ExecStart=/sbin/ip addr flush dev wlan0
ExecStart=/sbin/ip addr add $AP_IP/24 dev wlan0
ExecStart=/sbin/ip link set wlan0 up
RemainAfterExit=yes
[Install]
WantedBy=multi-user.target
EOF

cat > /etc/systemd/system/camera-box.service <<EOF
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
[Install]
WantedBy=multi-user.target
EOF

cat > /etc/camera-box/config.toml <<EOF
base_stream_port = 8080
web_port         = 80
device_ip        = "$AP_IP"
ustreamer_path   = "/usr/bin/ustreamer"
resolution       = "1280x720"
fps              = 30
EOF

cat > /var/lib/camera-box/network.toml <<EOF
ssid = "$AP_SSID"
password = "$AP_PASS"
ip_cidr = "$AP_IP/24"
EOF

# stop NetworkManager (if present) from managing the AP interface
if [ -d /etc/NetworkManager ]; then
  mkdir -p /etc/NetworkManager/conf.d
  printf '[keyfile]\nunmanaged-devices=interface-name:wlan0\n' > /etc/NetworkManager/conf.d/camera-box.conf
fi

# --- headless, AP-only first boot (IMPORTANT) --------------------------------
# The device must come up as a hotspot with no console/keyboard/uplink. Skip
# Armbian's interactive first-run account wizard (it otherwise waits for a
# console login on first boot), preset a root password so you can SSH in over
# the hotspot, and don't block boot waiting for an uplink that doesn't exist.
echo 'root:CameraBox123' | chpasswd
rm -f /root/.not_logged_in_yet
systemctl disable systemd-networkd-wait-online.service NetworkManager-wait-online.service 2>/dev/null || true

systemctl unmask hostapd 2>/dev/null || true
systemctl enable camera-box camera-box-ip hostapd dnsmasq
# make sure sshd is on so you can reach it over the hotspot
systemctl enable ssh 2>/dev/null || systemctl enable sshd 2>/dev/null || true
```

Make it executable: `chmod +x userpatches/customize-image.sh`.

> **Why those headless lines matter.** Armbian's stock first boot runs an
> **interactive account-creation wizard** on the console and force-expires the
> root login (`/root/.not_logged_in_yet`). On a device with no
> keyboard/serial/HDMI, that wizard has nothing to talk to — the box can end up
> with SSH refusing your login even though the hotspot is up. Presetting the root
> password and removing that flag makes it boot fully and lets you SSH in over
> the AP (`ssh root@192.168.4.1`, password `CameraBox123` — change it after).
> The `unmanaged-devices` line keeps NetworkManager off `wlan0` so `hostapd` owns
> it. **Change `CameraBox123` to your own password before building.**

## 4. Enable UVC in the kernel

Open the kernel config for this board/branch, enable UVC, and save:

```sh
./compile.sh kernel-config BOARD=luckfox-lyra-zero-w BRANCH=vendor RELEASE=trixie
```

In menuconfig: press `/`, search `USB_VIDEO_CLASS`, enable **"USB Video Class
(UVC)"** as a module (`M`); it pulls in the media/V4L2 dependencies. Save & exit.
The options that must end up set:

```text
CONFIG_MEDIA_SUPPORT=y
CONFIG_MEDIA_USB_SUPPORT=y
CONFIG_MEDIA_CAMERA_SUPPORT=y
CONFIG_VIDEO_DEV=y
CONFIG_USB_VIDEO_CLASS=m
```

(If prompted, let it save the change so the `build` step below reuses it.)

## 5. Build the image

```sh
./compile.sh build \
  BOARD=luckfox-lyra-zero-w \
  BRANCH=vendor \
  RELEASE=trixie \
  BUILD_MINIMAL=yes \
  BUILD_DESKTOP=no
```

The finished image lands in `output/images/` (a `.img` you can `dd`).

> Verify the board/branch names against `config/boards/` in the build tree — the
> board file is `luckfox-lyra-zero-w.*` and the kernel is the **vendor** 6.1.x.
> Use `RELEASE=trixie` (Debian 13) for a small CLI base.

## 6. Flash, erase SPI, boot

```sh
sudo dd if=output/images/Armbian_*_Luckfox-lyra-zero-w_*.img of=/dev/sdX bs=4M status=progress conv=fsync
sync
```

Then **erase the board's SPI flash once** so it boots from the SD (see the
[SPI-erase note](luckfox-lyra-zero-w.md#troubleshooting)). Power on and give the
**first boot a minute or two** — Armbian expands the rootfs and may reboot once
before things settle. Then:

- the board hosts the **CameraBox** hotspot → connect to it and browse
  `http://192.168.4.1/` (admin/password), or `ssh root@192.168.4.1`
  (password from `customize-image.sh`),
- `sudo modprobe uvcvideo && ls -l /dev/video*` → your USB camera as `/dev/video0`.

Because the hotspot is your only way in, the two things to confirm are that the
**CameraBox network appears** (hostapd + the AIC8800 driver working) and that
**`uvcvideo` loads**. If the AP doesn't appear, you'll need a USB-serial console
to debug hostapd — worth having one on hand for the first boot.

## Notes

- The `customize-image.sh` config is exactly what camera-box generates at
  runtime; baking it in just means the hotspot is up on the very first boot.
- Change the hotspot per box by editing `AP_SSID` / `AP_PASS` / `AP_IP` at the
  top of the script (or several boxes → several builds).
- `ustreamer` is in Debian; if a given release lacks it, build it on the device
  (see [luckfox-lyra-zero-w.md](luckfox-lyra-zero-w.md#troubleshooting)).
- Exact Armbian option names evolve — the [User Configurations](https://docs.armbian.com/Developer-Guide_User-Configurations/)
  and [Build Preparation](https://docs.armbian.com/Developer-Guide_Build-Preparation/)
  docs are authoritative; the durable parts here are the customize-image.sh
  content and the kernel UVC options.
