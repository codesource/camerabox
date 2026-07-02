//! Native Wi-Fi / network management.
//!
//! Reimplements (in Rust) what the old `camera-box-wifi` bash script did, plus
//! multi-interface support. The built-in `wlan0` (primary) can run as an access
//! point (`hostapd` + `dnsmasq`) or as a client; additional Wi-Fi interfaces
//! (e.g. a USB dongle as `wlan1`) can be configured as clients with DHCP or a
//! static address.
//!
//! Everything is driven by invoking the system tools (`iw`, `ip`, `hostapd`,
//! `dnsmasq`, `wpa_supplicant`, `wpa_passphrase`, `dhclient`, `systemctl`) as
//! subprocesses — the daemon runs as root. Actions on the primary interface are
//! disruptive (switching AP↔client drops the hotspot), so the web layer gates
//! them behind Basic Auth.

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{info, warn};

use crate::config::ApConfig;

const PROFILE_DIR: &str = "/etc/camera-box/wifi";
const HOSTAPD_CONF: &str = "/etc/hostapd/hostapd.conf";
const DNSMASQ_CONF: &str = "/etc/dnsmasq.conf";
const CAMERA_BOX_IP_UNIT: &str = "/etc/systemd/system/camera-box-ip.service";

pub type NetResult<T> = Result<T, String>;

// ---------------------------------------------------------------------------
// Data model (serialised into the status API)
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct WifiInterface {
    pub name: String,
    /// First/built-in Wi-Fi interface — the AP-capable management interface.
    pub primary: bool,
    pub mac: String,
    pub up: bool,
    /// "ap", "client", "unknown".
    pub mode: String,
    /// AP SSID (in ap mode) or connected SSID (in client mode), if any.
    pub ssid: Option<String>,
    /// IPv4 address with prefix (e.g. `192.168.4.1/24`).
    pub ip: Option<String>,
    pub ap_capable: bool,
}

#[derive(Debug, Serialize)]
pub struct ScanResult {
    pub ssid: String,
    pub signal_dbm: i32,
    pub secured: bool,
}

#[derive(Debug, Serialize)]
pub struct NetworkStatus {
    pub interfaces: Vec<WifiInterface>,
    pub profiles: Vec<String>,
}

/// Parameters for connecting an interface to a network as a client.
#[derive(Debug, Deserialize)]
pub struct ConnectParams {
    pub iface: String,
    pub ssid: String,
    pub password: String,
    /// Optionally save the credentials as a named profile.
    #[serde(default)]
    pub save_as: Option<String>,
    /// DHCP (default) vs static addressing.
    #[serde(default = "default_true")]
    pub dhcp: bool,
    /// Static address `a.b.c.d/nn` (required when `dhcp` is false).
    #[serde(default)]
    pub address: Option<String>,
    #[serde(default)]
    pub gateway: Option<String>,
    #[serde(default)]
    pub dns: Option<String>,
}

fn default_true() -> bool {
    true
}

// ---------------------------------------------------------------------------
// Subprocess helpers
// ---------------------------------------------------------------------------

/// Run a command, returning stdout on success or an error string on failure.
async fn sh(program: &str, args: &[&str]) -> NetResult<String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .await
        .map_err(|e| format!("spawn {program}: {e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        Err(format!(
            "{program} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

/// Best-effort: run a command, ignoring any failure.
async fn sh_ok(program: &str, args: &[&str]) {
    if let Err(e) = sh(program, args).await {
        warn!(command = program, error = %e, "ignored command failure");
    }
}

fn read_sys(iface: &str, file: &str) -> String {
    std::fs::read_to_string(format!("/sys/class/net/{iface}/{file}"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Discovery & status
// ---------------------------------------------------------------------------

/// List wireless interfaces (those with a `wireless`/`phy80211` sysfs node).
pub fn wifi_interfaces() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            let base = format!("/sys/class/net/{name}");
            if Path::new(&format!("{base}/wireless")).exists()
                || Path::new(&format!("{base}/phy80211")).exists()
            {
                out.push(name);
            }
        }
    }
    out.sort();
    out
}

/// The primary (built-in, AP-capable) wireless interface, if any.
pub fn primary_wifi() -> Option<String> {
    wifi_interfaces().into_iter().next()
}

pub async fn status() -> NetworkStatus {
    let names = wifi_interfaces();
    let mut interfaces = Vec::with_capacity(names.len());
    for (i, name) in names.iter().enumerate() {
        let mut iface = interface_status(name).await;
        iface.primary = i == 0;
        interfaces.push(iface);
    }
    NetworkStatus {
        interfaces,
        profiles: list_profiles(),
    }
}

async fn interface_status(name: &str) -> WifiInterface {
    let mac = read_sys(name, "address");
    let operstate = read_sys(name, "operstate");
    let up = operstate == "up" || operstate == "dormant";

    let info = sh("iw", &["dev", name, "info"]).await.unwrap_or_default();
    let mode = if info.contains("type AP") {
        "ap"
    } else if info.contains("type managed") {
        "client"
    } else {
        "unknown"
    };

    let ssid = if mode == "ap" {
        info.lines()
            .find_map(|l| l.trim().strip_prefix("ssid ").map(str::to_string))
    } else {
        let link = sh("iw", &["dev", name, "link"]).await.unwrap_or_default();
        link.lines()
            .find_map(|l| l.trim().strip_prefix("SSID: ").map(str::to_string))
    };

    WifiInterface {
        ap_capable: ap_capable(name).await,
        name: name.to_string(),
        primary: false,
        mac,
        up,
        mode: mode.to_string(),
        ssid,
        ip: ipv4_of(name).await,
    }
}

async fn ipv4_of(iface: &str) -> Option<String> {
    let out = sh("ip", &["-o", "-4", "addr", "show", "dev", iface])
        .await
        .ok()?;
    // "3: wlan0    inet 192.168.204.97/24 brd ... scope global ..."
    let mut tokens = out.split_whitespace();
    while let Some(t) = tokens.next() {
        if t == "inet" {
            return tokens.next().map(str::to_string);
        }
    }
    None
}

/// Does the interface's phy support AP mode?
async fn ap_capable(iface: &str) -> bool {
    let phy = read_sys(iface, "phy80211/name");
    if phy.is_empty() {
        return false;
    }
    let info = sh("iw", &["phy", &phy, "info"]).await.unwrap_or_default();
    info.lines().any(|l| l.trim() == "* AP")
}

// ---------------------------------------------------------------------------
// Scanning
// ---------------------------------------------------------------------------

pub async fn scan(iface: &str) -> NetResult<Vec<ScanResult>> {
    validate_iface(iface)?;
    sh_ok("ip", &["link", "set", iface, "up"]).await;

    let raw = sh("iw", &["dev", iface, "scan"]).await?;
    let mut results: Vec<ScanResult> = Vec::new();
    let mut ssid: Option<String> = None;
    let mut signal: i32 = -100;
    let mut secured = false;

    let flush = |results: &mut Vec<ScanResult>, ssid: &mut Option<String>, signal: i32, secured: bool| {
        if let Some(name) = ssid.take() {
            if name.is_empty() {
                return;
            }
            // Keep the strongest entry per SSID.
            if let Some(existing) = results.iter_mut().find(|r| r.ssid == name) {
                if signal > existing.signal_dbm {
                    existing.signal_dbm = signal;
                    existing.secured = secured;
                }
            } else {
                results.push(ScanResult {
                    ssid: name,
                    signal_dbm: signal,
                    secured,
                });
            }
        }
    };

    for line in raw.lines() {
        let t = line.trim();
        if t.starts_with("BSS ") {
            flush(&mut results, &mut ssid, signal, secured);
            signal = -100;
            secured = false;
        } else if let Some(s) = t.strip_prefix("signal: ") {
            signal = s
                .split_whitespace()
                .next()
                .and_then(|v| v.parse::<f32>().ok())
                .map(|v| v as i32)
                .unwrap_or(-100);
        } else if let Some(s) = t.strip_prefix("SSID: ") {
            ssid = Some(s.to_string());
        } else if t.starts_with("RSN:") || t.starts_with("WPA:") {
            secured = true;
        }
    }
    flush(&mut results, &mut ssid, signal, secured);

    results.sort_by(|a, b| b.signal_dbm.cmp(&a.signal_dbm));
    Ok(results)
}

// ---------------------------------------------------------------------------
// Access-point mode (primary interface only)
// ---------------------------------------------------------------------------

pub async fn start_hotspot(iface: &str, ap: &ApConfig) -> NetResult<()> {
    validate_iface(iface)?;
    validate_ap(ap)?;
    info!(iface, ssid = %ap.ssid, ip = %ap.ip_cidr, "switching to hotspot (AP) mode");

    // Regenerate hostapd/dnsmasq/boot-IP config from the saved settings first.
    write_ap_files(iface, ap).await?;

    stop_client(iface).await;
    // Wi-Fi can be rfkill soft-blocked (especially after a reboot); unblock it
    // or `ip link set up` fails (exit 2) and the interface stays down.
    // Unblock ALL types, not just wifi: on combo chips (e.g. the Lyra's
    // AIC8800DC) the BLUETOOTH rfkill drives the chip's power GPIO — leaving
    // it blocked keeps the whole module unpowered and off the USB bus.
    sh_ok("rfkill", &["unblock", "all"]).await;
    sh_ok("ip", &["addr", "flush", "dev", iface]).await;
    sh_ok("ip", &["link", "set", iface, "down"]).await;
    sleep(Duration::from_secs(1)).await;
    sh_ok("ip", &["link", "set", iface, "up"]).await;

    // Assign the AP IP directly — don't depend on the external camera-box-ip
    // unit, which fails if the link was rfkill-blocked at boot.
    sh_ok("ip", &["addr", "add", &ap.ip_cidr, "dev", iface]).await;

    // hostapd ships masked on Raspberry Pi OS; make sure both can start.
    sh_ok("systemctl", &["unmask", "hostapd", "dnsmasq"]).await;
    // Restart dnsmasq AFTER the address is set, or it logs "DHCP packet
    // received on <iface> which has no address" and hands out no leases.
    sh("systemctl", &["restart", "dnsmasq"]).await?;
    sh("systemctl", &["restart", "hostapd"]).await?;
    info!(iface, "hotspot active");
    Ok(())
}

/// Save/apply new AP settings without switching mode: regenerate the config
/// files, and if this interface is currently serving the AP, re-assign the IP
/// and restart dnsmasq/hostapd so a changed SSID/password/IP takes effect now.
pub async fn apply_ap(iface: &str, ap: &ApConfig) -> NetResult<()> {
    validate_iface(iface)?;
    validate_ap(ap)?;
    write_ap_files(iface, ap).await?;
    if interface_status(iface).await.mode == "ap" {
        sh_ok("ip", &["addr", "flush", "dev", iface]).await;
        sh_ok("ip", &["addr", "add", &ap.ip_cidr, "dev", iface]).await;
        sh("systemctl", &["restart", "dnsmasq"]).await?;
        sh("systemctl", &["restart", "hostapd"]).await?;
        info!(iface, ssid = %ap.ssid, ip = %ap.ip_cidr, "applied new hotspot settings");
    }
    Ok(())
}

/// Regenerate the on-disk AP config files from the saved settings so a reboot
/// brings the hotspot up with the configured SSID/password/IP. Does **not**
/// restart any service — safe to call at startup without changing the live mode.
pub async fn sync_ap_config(iface: &str, ap: &ApConfig) -> NetResult<()> {
    write_ap_files(iface, ap).await
}

/// Write every file that defines the AP (hostapd, dnsmasq, boot-time IP unit,
/// and the hostname → AP-IP mapping) and reload systemd so the regenerated
/// `camera-box-ip.service` is picked up. Does not (re)start network services.
async fn write_ap_files(iface: &str, ap: &ApConfig) -> NetResult<()> {
    write_hostapd_conf(iface, ap)?;
    write_dnsmasq_conf(iface, ap)?;
    write_ap_hostname_mapping(&current_hostname(), ap_ip_of(&ap.ip_cidr))?;
    write_camera_box_ip_unit(iface, ap)?;
    sh_ok("systemctl", &["daemon-reload"]).await;
    Ok(())
}

/// Validate the SSID/passphrase (WPA2-PSK limits) and the AP IP/subnet.
pub fn validate_ap(ap: &ApConfig) -> NetResult<()> {
    let ssid_len = ap.ssid.len();
    if ssid_len == 0 || ssid_len > 32 {
        return Err("hotspot name (SSID) must be 1–32 characters".into());
    }
    if ap.ssid.contains(['\n', '\r']) || ap.password.contains(['\n', '\r']) {
        return Err("hotspot name/password must not contain line breaks".into());
    }
    let pw_len = ap.password.len();
    if !(8..=63).contains(&pw_len) {
        return Err("hotspot password must be 8–63 characters".into());
    }
    validate_ap_ip(&ap.ip_cidr)
}

/// The AP gateway address must be `a.b.c.d/24` with the host part in 1–254 and
/// outside the DHCP range (.100–.200), so the gateway can't collide with a lease.
fn validate_ap_ip(ip_cidr: &str) -> NetResult<()> {
    let (ip, prefix) = ip_cidr
        .split_once('/')
        .ok_or("hotspot IP must look like 192.168.4.1/24")?;
    if prefix != "24" {
        return Err("hotspot IP prefix must be /24".into());
    }
    let octets: Vec<&str> = ip.split('.').collect();
    if octets.len() != 4 {
        return Err("invalid hotspot IP".into());
    }
    let mut host = 0u16;
    for (i, o) in octets.iter().enumerate() {
        let n: u16 = o.parse().map_err(|_| "invalid hotspot IP")?;
        if n > 255 {
            return Err("invalid hotspot IP".into());
        }
        if i == 3 {
            host = n;
        }
    }
    if host == 0 || host >= 255 {
        return Err("hotspot IP host part must be 1–254".into());
    }
    if (100..=200).contains(&host) {
        return Err("hotspot IP must be outside .100–.200 (the DHCP range)".into());
    }
    Ok(())
}

/// The gateway address (the part before `/`).
fn ap_ip_of(ip_cidr: &str) -> &str {
    ip_cidr.split('/').next().unwrap_or("192.168.4.1")
}

/// dnsmasq `dhcp-range` for the AP subnet: `<net>.100,<net>.200,255.255.255.0,24h`.
fn dhcp_range_of(ip_cidr: &str) -> String {
    let ip = ap_ip_of(ip_cidr);
    let net3 = match ip.rsplit_once('.') {
        Some((net, _host)) => net,
        None => "192.168.4",
    };
    format!("{net3}.100,{net3}.200,255.255.255.0,24h")
}

/// Write `/etc/hostapd/hostapd.conf` from the saved AP settings. Fixed radio
/// parameters (band/channel/country) match the factory provisioning.
fn write_hostapd_conf(iface: &str, ap: &ApConfig) -> NetResult<()> {
    let body = format!(
        "country_code=CH\n\
         interface={iface}\n\
         driver=nl80211\n\
         \n\
         ssid={ssid}\n\
         \n\
         hw_mode=g\n\
         channel=6\n\
         \n\
         ieee80211n=1\n\
         wmm_enabled=1\n\
         auth_algs=1\n\
         ignore_broadcast_ssid=0\n\
         \n\
         wpa=2\n\
         wpa_passphrase={pass}\n\
         wpa_key_mgmt=WPA-PSK\n\
         rsn_pairwise=CCMP\n",
        iface = iface,
        ssid = ap.ssid,
        pass = ap.password,
    );
    write_secure(Path::new(HOSTAPD_CONF), &body)
}

/// Write `/etc/dnsmasq.conf` — camera-box owns it (dnsmasq exists only for the
/// AP here). The DHCP range and gateway/DNS options follow the configured AP IP,
/// and `conf-dir` keeps the `/etc/dnsmasq.d/` hostname mapping loading.
fn write_dnsmasq_conf(iface: &str, ap: &ApConfig) -> NetResult<()> {
    let ip = ap_ip_of(&ap.ip_cidr);
    let body = format!(
        "# Managed by camera-box — do not edit.\n\
         interface={iface}\n\
         bind-dynamic\n\
         dhcp-range={range}\n\
         dhcp-option=option:router,{ip}\n\
         dhcp-option=option:dns-server,{ip}\n\
         domain-needed\n\
         bogus-priv\n\
         conf-dir=/etc/dnsmasq.d/,*.conf\n",
        iface = iface,
        range = dhcp_range_of(&ap.ip_cidr),
        ip = ip,
    );
    std::fs::write(DNSMASQ_CONF, body).map_err(|e| e.to_string())
}

/// Rewrite the boot-time `camera-box-ip.service` so a reboot brings the primary
/// interface up on the configured AP IP (before dnsmasq/hostapd start).
fn write_camera_box_ip_unit(iface: &str, ap: &ApConfig) -> NetResult<()> {
    let body = format!(
        "[Unit]\n\
         Description=Set static IP for the camera-box AP\n\
         Before=dnsmasq.service hostapd.service\n\
         After=sys-subsystem-net-devices-{iface}.device\n\
         Wants=sys-subsystem-net-devices-{iface}.device\n\
         \n\
         [Service]\n\
         Type=oneshot\n\
         ExecStartPre=-/usr/sbin/rfkill unblock all\n\
         ExecStart=/sbin/ip addr flush dev {iface}\n\
         ExecStart=/sbin/ip addr add {cidr} dev {iface}\n\
         ExecStart=/sbin/ip link set {iface} up\n\
         RemainAfterExit=yes\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n",
        iface = iface,
        cidr = ap.ip_cidr,
    );
    std::fs::write(CAMERA_BOX_IP_UNIT, body).map_err(|e| e.to_string())
}

async fn stop_hotspot() {
    sh_ok("systemctl", &["stop", "hostapd"]).await;
    sh_ok("systemctl", &["stop", "dnsmasq"]).await;
    sh_ok("systemctl", &["stop", "camera-box-ip"]).await;
}

// ---------------------------------------------------------------------------
// Client mode (any interface)
// ---------------------------------------------------------------------------

pub async fn connect(params: &ConnectParams) -> NetResult<()> {
    validate_iface(&params.iface)?;
    if params.ssid.is_empty() {
        return Err("ssid is required".into());
    }
    if !params.dhcp && params.address.is_none() {
        return Err("static addressing requires an address".into());
    }

    // Build a wpa_supplicant config from the passphrase.
    let conf = wpa_passphrase(&params.ssid, &params.password).await?;
    let conf_path = format!("/run/camera-box-wpa-{}.conf", params.iface);
    write_secure(Path::new(&conf_path), &conf)?;

    if let Some(name) = &params.save_as {
        add_profile_conf(name, &conf)?;
    }

    connect_with_conf(
        params.iface.clone(),
        conf_path,
        params.dhcp,
        params.address.clone(),
        params.gateway.clone(),
        params.dns.clone(),
    )
    .await
}

pub async fn connect_profile(iface: &str, name: &str, dhcp: bool) -> NetResult<()> {
    validate_iface(iface)?;
    let conf_path = profile_path(name)?;
    if !conf_path.exists() {
        return Err(format!("profile not found: {name}"));
    }
    connect_with_conf(
        iface.to_string(),
        conf_path.to_string_lossy().into_owned(),
        dhcp,
        None,
        None,
        None,
    )
    .await
}

/// Bring `iface` up as a client using a wpa_supplicant config file. All
/// arguments are owned so the future stays `Send` across the long awaits
/// (required by `axum::Handler`).
async fn connect_with_conf(
    iface: String,
    conf_path: String,
    dhcp: bool,
    address: Option<String>,
    gateway: Option<String>,
    dns: Option<String>,
) -> NetResult<()> {
    let ifn: &str = &iface;
    let primary = wifi_interfaces().first().map(|p| p == &iface).unwrap_or(false);
    info!(iface = ifn, primary, dhcp, "switching to client mode");

    // The primary interface hosts the AP — tear it down before going client.
    if primary {
        stop_hotspot().await;
    }
    stop_client(ifn).await;

    sh_ok("ip", &["addr", "flush", "dev", ifn]).await;
    sh_ok("ip", &["link", "set", ifn, "down"]).await;
    sleep(Duration::from_secs(1)).await;
    sh_ok("ip", &["link", "set", ifn, "up"]).await;

    let pidfile = wpa_pidfile(ifn);
    sh(
        "wpa_supplicant",
        &["-B", "-i", ifn, "-c", conf_path.as_str(), "-P", pidfile.as_str()],
    )
    .await?;
    sleep(Duration::from_secs(3)).await;

    if dhcp {
        sh("dhclient", &[ifn]).await?;
    } else {
        let addr = address.as_deref().unwrap_or_default();
        sh("ip", &["addr", "add", addr, "dev", ifn]).await?;
        if let Some(gw) = &gateway {
            sh_ok("ip", &["route", "replace", "default", "via", gw.as_str(), "dev", ifn]).await;
        }
        if let Some(dns) = &dns {
            // Best-effort static resolver entry.
            let _ = std::fs::write("/etc/resolv.conf", format!("nameserver {dns}\n"));
        }
    }

    let ip = ipv4_of(ifn).await;
    info!(iface = ifn, ip = ip.as_deref().unwrap_or("?"), "client connected");
    Ok(())
}

/// Stop any client session on `iface` — including a `wpa_supplicant` started
/// by something other than this daemon (e.g. the old `camera-box-wifi` script
/// or the system), which would otherwise keep the interface in client mode and
/// stop `hostapd` from bringing up the AP.
async fn stop_client(iface: &str) {
    // First our own (tracked via pidfile), for a clean SIGTERM.
    let pidfile = wpa_pidfile(iface);
    if let Ok(contents) = std::fs::read_to_string(&pidfile) {
        if let Ok(pid) = contents.trim().parse::<i32>() {
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        }
        let _ = std::fs::remove_file(&pidfile);
    }
    // Then any other wpa_supplicant bound to this interface.
    sh_ok("pkill", &["-f", &format!("wpa_supplicant.*{iface}")]).await;
    sh_ok("dhclient", &["-r", iface]).await;
}

fn wpa_pidfile(iface: &str) -> String {
    format!("/run/camera-box-wpa-{iface}.pid")
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

pub fn list_profiles() -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir(PROFILE_DIR) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().into_owned();
            if let Some(stem) = name.strip_suffix(".conf") {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    out
}

pub async fn add_profile(name: &str, ssid: &str, password: &str) -> NetResult<()> {
    let conf = wpa_passphrase(ssid, password).await?;
    add_profile_conf(name, &conf)
}

fn add_profile_conf(name: &str, conf: &str) -> NetResult<()> {
    let path = profile_path(name)?;
    std::fs::create_dir_all(PROFILE_DIR).map_err(|e| e.to_string())?;
    let _ = std::fs::set_permissions(PROFILE_DIR, std::fs::Permissions::from_mode(0o700));
    write_secure(&path, conf)?;
    info!(profile = name, "saved Wi-Fi profile");
    Ok(())
}

pub fn remove_profile(name: &str) -> NetResult<()> {
    let path = profile_path(name)?;
    std::fs::remove_file(&path).map_err(|e| e.to_string())?;
    info!(profile = name, "removed Wi-Fi profile");
    Ok(())
}

fn profile_path(name: &str) -> NetResult<PathBuf> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err("invalid profile name (use letters, digits, '-' or '_')".into());
    }
    Ok(PathBuf::from(format!("{PROFILE_DIR}/{name}.conf")))
}

// ---------------------------------------------------------------------------
// Small utilities
// ---------------------------------------------------------------------------

async fn wpa_passphrase(ssid: &str, password: &str) -> NetResult<String> {
    if password.len() < 8 {
        return Err("Wi-Fi password must be at least 8 characters".into());
    }
    sh("wpa_passphrase", &[ssid, password]).await
}

fn write_secure(path: &Path, contents: &str) -> NetResult<()> {
    std::fs::write(path, contents).map_err(|e| e.to_string())?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    Ok(())
}

fn validate_iface(iface: &str) -> NetResult<()> {
    if wifi_interfaces().iter().any(|i| i == iface) {
        Ok(())
    } else {
        Err(format!("unknown wireless interface: {iface}"))
    }
}

// ---------------------------------------------------------------------------
// Hostname (+ make <hostname>.local resolvable for AP clients via dnsmasq)
// ---------------------------------------------------------------------------

const DNSMASQ_HOST_MAP: &str = "/etc/dnsmasq.d/camera-box.conf";

pub fn current_hostname() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "camera-box".to_string())
}

/// Change the system hostname and refresh mDNS (avahi) + AP DNS (dnsmasq).
/// `ap_ip` is the current hotspot gateway the name should resolve to.
pub async fn set_hostname(name: &str, ap_ip: &str) -> NetResult<()> {
    validate_hostname(name)?;
    sh("hostnamectl", &["set-hostname", name]).await?;
    update_etc_hosts(name)?;
    write_ap_hostname_mapping(name, ap_ip)?;
    // Re-advertise <name>.local over mDNS and reload AP DNS (no-op if stopped).
    sh_ok("systemctl", &["restart", "avahi-daemon"]).await;
    sh_ok("systemctl", &["restart", "dnsmasq"]).await;
    info!(hostname = name, "hostname changed");
    Ok(())
}

fn validate_hostname(name: &str) -> NetResult<()> {
    let ok = !name.is_empty()
        && name.len() <= 63
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if ok {
        Ok(())
    } else {
        Err("invalid hostname (letters, digits and '-', not starting/ending with '-')".into())
    }
}

/// Point the `127.0.1.1` line at the new hostname.
fn update_etc_hosts(name: &str) -> NetResult<()> {
    let content = std::fs::read_to_string("/etc/hosts").map_err(|e| e.to_string())?;
    let mut out = String::new();
    let mut replaced = false;
    for line in content.lines() {
        if line.trim_start().starts_with("127.0.1.1") {
            out.push_str(&format!("127.0.1.1\t{name}\n"));
            replaced = true;
        } else {
            out.push_str(line);
            out.push('\n');
        }
    }
    if !replaced {
        out.push_str(&format!("127.0.1.1\t{name}\n"));
    }
    std::fs::write("/etc/hosts", out).map_err(|e| e.to_string())
}

/// Make AP (dnsmasq) clients resolve `<name>` and `<name>.local` to the AP IP,
/// so the box is reachable by name in hotspot mode even without mDNS.
pub fn write_ap_hostname_mapping(name: &str, ap_ip: &str) -> NetResult<()> {
    let body = format!("address=/{name}/{ap_ip}\naddress=/{name}.local/{ap_ip}\n");
    std::fs::write(DNSMASQ_HOST_MAP, body).map_err(|e| e.to_string())
}
