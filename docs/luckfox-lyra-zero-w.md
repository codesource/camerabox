# Running camera-box on the Luckfox Lyra Zero W

The Luckfox Lyra Zero W (Rockchip **RK3506B**, triple-core Cortex-A7) is 32-bit
**ARMv7** hard-float — the same ISA as the 32-bit Raspberry Pi Zero 2 W. It runs
the exact same statically-linked binary camera-box ships (`camera-box-luckfox-lyra-zero-w`,
which is byte-identical to `camera-box-pi-zero-2w-armv7`). No special build.

**Two hard requirements:**

1. Use the board's **Ubuntu image** (it has `systemd`, which camera-box needs).
   The Buildroot/BusyBox image will *not* work.
2. It's a Rockchip board, so flashing isn't quite "just `dd` the official image".
   Use a community **`dd`-able Ubuntu image** (below).

There are two ways to set the card up. The **automated** route is one command;
the **manual** route is the same steps spelled out, for understanding or if the
script hits a snag on your image. Both are done entirely from your Linux PC, and
both leave a card that boots straight into the camera-box hotspot — no setup on
the device.

---

## 0. Get the Ubuntu image

Grab a `dd`-able **Ubuntu** image for the Lyra Zero W from the community
[platima/SBC-Images](https://github.com/platima/SBC-Images/tree/main/Luckfox/Lyra/Lyra%20Zero%20W)
repo (e.g. `Luckfox_Lyra_Zero_W-…_Ubuntu.img.bz2`). Note its download URL or save
the file — you'll pass it to the tools below.

- Default logins on that image: `root` / `root`, or `lyra` / `luckfox`.
- Layout: the root filesystem is **ext4 on partition 3**.

> Why not the official image? Rockchip's official images aren't a single raw
> `.img` — their flashing tool pre-partitions the card, so a plain `dd` doesn't
> reproduce them. The platima Ubuntu build *is* a raw image you can `dd`.

---

## Option A — automated (recommended)

One command flashes the image, then installs and configures everything. It runs
on your **Linux PC** (not on the Lyra), and it's interactive — so **download the
one script** and run it (no repo clone, and don't `curl | bash` it, or the SD
picker prompt has nowhere to read from):

```sh
# one-time: tools for the ARM chroot + partitioning
sudo apt install qemu-user-static binfmt-support parted e2fsprogs \
                 cloud-guest-utils gdisk curl

# grab just the script:
curl -fsSL https://raw.githubusercontent.com/codesource/camerabox/main/scripts/prepare-sd.sh -o prepare-sd.sh

# flash + provision in one go (it will let you PICK the SD card):
sudo bash prepare-sd.sh \
  --image https://github.com/platima/SBC-Images/raw/main/Luckfox/Lyra/Lyra%20Zero%20W/<image>.img.bz2
```

With no `--binary`, the script **lists the binaries in the latest release and
lets you pick one** (defaulting to the ARMv7 build the Lyra needs) — or pass your
own local build with `--binary ./camera-box`. `--image` also takes a **local
file** (`--image ./Luckfox_…_Ubuntu.img.bz2`) if you already downloaded it; skip
`--image` entirely to provision a card you flashed separately. Customise the
hotspot with `--ssid`, `--pass`, `--ip` (e.g. `--ip 192.168.5.1/24` for a second
box).

The script: lets you pick the SD card (and refuses your PC's system disk) →
flashes (if `--image`) → grows the rootfs → installs the dependencies into the
ARM rootfs via a `qemu-user-static` chroot → installs camera-box → pre-writes the
hotspot config → enables the services.

Then jump to [First boot](#first-boot).

---

## Option B — manual, step by step

The same thing by hand. Useful to understand it, or to adapt if your image
differs.

### 1. Flash the image

```sh
lsblk                                   # identify the SD card, e.g. /dev/sdX
bzip2 -dc Luckfox_…_Ubuntu.img.bz2 | sudo dd of=/dev/sdX bs=4M status=progress conv=fsync
sync
```

> ⚠️ Double-check `of=` is the SD card, not your disk.

### 2. Grow the root filesystem

The image ships nearly full; make room for the packages. A small image `dd`'d
onto a bigger card strands GPT's backup header mid-disk, so move it to the end
first (`sgdisk -e`), then grow:

```sh
sudo apt install cloud-guest-utils gdisk    # growpart + sgdisk
sudo sgdisk -e /dev/sdX                      # relocate GPT backup header to the end
sudo partprobe /dev/sdX
sudo growpart /dev/sdX 3                      # grow partition 3 to fill the card
sudo e2fsck -fy /dev/sdX3
sudo resize2fs /dev/sdX3
```

(For an SD reader that names partitions `mmcblk0p3`, use that instead of `sdX3`.)

### 3. Mount the rootfs and set up ARM emulation

```sh
sudo apt install qemu-user-static binfmt-support   # one-time
sudo mkdir -p /mnt/lyra
sudo mount /dev/sdX3 /mnt/lyra
sudo cp /usr/bin/qemu-arm-static /mnt/lyra/usr/bin/
sudo mount --bind /dev  /mnt/lyra/dev
sudo mount --bind /proc /mnt/lyra/proc
sudo mount --bind /sys  /mnt/lyra/sys
sudo cp /etc/resolv.conf /mnt/lyra/etc/resolv.conf
```

### 4. Install camera-box + dependencies

```sh
sudo install -D -m0755 ./camera-box-luckfox-lyra-zero-w /mnt/lyra/usr/local/bin/camera-box

sudo mkdir -p /mnt/lyra/tmp && sudo chmod 1777 /mnt/lyra/tmp
sudo chroot /mnt/lyra /bin/bash -e <<'EOF'
export DEBIAN_FRONTEND=noninteractive
# run apt as root — its unprivileged _apt sandbox user can't write /tmp under
# an emulated chroot, which breaks apt-get update (and then packages 404)
APT="apt-get -o APT::Sandbox::User=root -o Acquire::Languages=none"
$APT update
$APT install -y hostapd dnsmasq iw wpasupplicant isc-dhcp-client avahi-daemon rfkill
$APT install -y ustreamer || echo "ustreamer not in apt — build it (see Troubleshooting)"
EOF
```

### 5. Pre-write the hotspot config and service

So the AP is up on the very first boot. (This is exactly what camera-box
generates at runtime; writing it now means hostapd has a config before the
daemon starts.)

```sh
R=/mnt/lyra
sudo mkdir -p $R/etc/hostapd $R/etc/camera-box $R/etc/dnsmasq.d $R/var/lib/camera-box

sudo tee $R/etc/hostapd/hostapd.conf >/dev/null <<'EOF'
country_code=CH
interface=wlan0
driver=nl80211

ssid=CameraBox

hw_mode=g
channel=6

ieee80211n=1
wmm_enabled=1
auth_algs=1
ignore_broadcast_ssid=0

wpa=2
wpa_passphrase=CameraBox123
wpa_key_mgmt=WPA-PSK
rsn_pairwise=CCMP
EOF
echo 'DAEMON_CONF="/etc/hostapd/hostapd.conf"' | sudo tee $R/etc/default/hostapd >/dev/null

sudo tee $R/etc/dnsmasq.conf >/dev/null <<'EOF'
# Managed by camera-box — do not edit.
interface=wlan0
bind-dynamic
dhcp-range=192.168.4.100,192.168.4.200,255.255.255.0,24h
dhcp-option=option:router,192.168.4.1
dhcp-option=option:dns-server,192.168.4.1
domain-needed
bogus-priv
conf-dir=/etc/dnsmasq.d/,*.conf
EOF

sudo tee $R/etc/dnsmasq.d/camera-box.conf >/dev/null <<'EOF'
address=/camera-box/192.168.4.1
address=/camera-box.local/192.168.4.1
EOF

sudo tee $R/etc/systemd/system/camera-box-ip.service >/dev/null <<'EOF'
[Unit]
Description=Set static IP for the camera-box AP
Before=dnsmasq.service hostapd.service
After=sys-subsystem-net-devices-wlan0.device
Wants=sys-subsystem-net-devices-wlan0.device

[Service]
Type=oneshot
ExecStartPre=-/usr/sbin/rfkill unblock wifi
ExecStart=/sbin/ip addr flush dev wlan0
ExecStart=/sbin/ip addr add 192.168.4.1/24 dev wlan0
ExecStart=/sbin/ip link set wlan0 up
RemainAfterExit=yes

[Install]
WantedBy=multi-user.target
EOF

sudo tee $R/etc/systemd/system/camera-box.service >/dev/null <<'EOF'
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

sudo tee $R/etc/camera-box/config.toml >/dev/null <<'EOF'
base_stream_port = 8080
web_port         = 80
device_ip        = "192.168.4.1"
ustreamer_path   = "/usr/bin/ustreamer"
resolution       = "1280x720"
fps              = 30
EOF

sudo tee $R/var/lib/camera-box/network.toml >/dev/null <<'EOF'
ssid = "CameraBox"
password = "CameraBox123"
ip_cidr = "192.168.4.1/24"
EOF

# if the image uses NetworkManager, stop it managing wlan0
[ -d $R/etc/NetworkManager ] && sudo mkdir -p $R/etc/NetworkManager/conf.d && \
  printf '[keyfile]\nunmanaged-devices=interface-name:wlan0\n' | \
  sudo tee $R/etc/NetworkManager/conf.d/camera-box.conf >/dev/null
```

### 6. Enable the services and unmount

```sh
sudo systemctl --root=/mnt/lyra unmask hostapd
sudo systemctl --root=/mnt/lyra enable camera-box camera-box-ip hostapd dnsmasq

sudo rm -f /mnt/lyra/usr/bin/qemu-arm-static
sudo umount /mnt/lyra/dev /mnt/lyra/proc /mnt/lyra/sys /mnt/lyra
sync
```

---

## First boot

Insert the card, power on, and after ~30 s the board should broadcast the
**CameraBox** Wi-Fi. Connect to it and open:

```text
http://192.168.4.1/        # login: admin / password
```

Change the password (and the hotspot SSID/IP) under **Settings** / **Network**.
Plug in a USB webcam and enable it on the **Cameras** tab.

## Verify on the device

```sh
systemctl status camera-box hostapd dnsmasq   # all active
which ustreamer                               # installed
v4l2-ctl --list-devices                        # USB camera shows /dev/video0
iw dev                                         # wlan0 present, type AP
```

## Troubleshooting

- **No hotspot appears.** Check `journalctl -u hostapd -b`. The AIC8800 Wi-Fi
  driver is out-of-tree; if hostapd won't start in AP mode, confirm the driver
  is loaded (`iw dev`, `dmesg | grep -i aic`) and that the Luckfox image includes
  AP-mode support for it.
- **`ustreamer` not in apt.** Build it (small, V4L2-only):

  ```sh
  sudo apt install -y build-essential libjpeg-dev libevent-dev libbsd-dev
  git clone --depth=1 https://github.com/pikvm/ustreamer && cd ustreamer && make && sudo make install
  ```

  Set `ustreamer_path` in `/etc/camera-box/config.toml` if it lands somewhere
  other than `/usr/bin/ustreamer`.
- **`apt` in the chroot: `Couldn't create temporary file /tmp/apt.conf…` then
  `404 Not Found`.** apt dropped to its `_apt` sandbox user, which can't write
  `/tmp` under emulation, so `apt-get update` couldn't refresh and stale indexes
  404'd. Run apt as root: `apt-get -o APT::Sandbox::User=root update && …`
  (and `chmod 1777 /tmp` in the chroot). `prepare-sd.sh` already does this.
- **Camera not detected.** Confirm `v4l2-ctl --list-devices` shows a `usb-…`
  device; camera-box only manages USB capture devices.
- **Wi-Fi interface isn't `wlan0`.** camera-box uses the first wireless
  interface automatically, but the pre-written hostapd/dnsmasq/boot-IP files
  above assume `wlan0`; adjust them if your image names it differently.

> These steps mirror the Raspberry Pi setup, which is verified; the Lyra-specific
> parts (AIC8800 AP mode, `ustreamer` availability) should be confirmed on your
> board on first run.
