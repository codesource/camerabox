//! System overview: CPU, memory, disk, temperature, model, time, uptime, and
//! per-interface byte counters (for a bandwidth monitor in the dashboard).
//!
//! Everything is read from `/proc`, `/sys`, and `statvfs` — no extra crates.

use std::ffi::CString;
use std::time::Duration;

use serde::Serialize;
use tokio::time::sleep;

#[derive(Debug, Serialize)]
pub struct SystemInfo {
    pub model: String,
    pub version: String,
    /// Device-local time, preformatted (e.g. `2026-06-30 16:40:12 CEST`).
    pub local_time: String,
    /// System uptime in seconds.
    pub uptime: u64,
    pub cpu_percent: f32,
    pub mem_total: u64,
    pub mem_used: u64,
    pub disk_total: u64,
    pub disk_used: u64,
    pub temperature_c: Option<f32>,
    /// Cumulative byte counters per interface; the UI derives rates from deltas.
    pub net: Vec<NetCounters>,
}

#[derive(Debug, Serialize)]
pub struct NetCounters {
    pub iface: String,
    pub rx_bytes: u64,
    pub tx_bytes: u64,
}

pub async fn info() -> SystemInfo {
    SystemInfo {
        model: model(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        local_time: local_time(),
        uptime: uptime_secs(),
        cpu_percent: cpu_percent().await,
        mem_total: mem().0,
        mem_used: mem().1,
        disk_total: disk().0,
        disk_used: disk().1,
        temperature_c: temperature(),
        net: net_counters(),
    }
}

fn model() -> String {
    std::fs::read_to_string("/proc/device-tree/model")
        .map(|s| s.trim_end_matches('\0').trim().to_string())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Unknown".to_string())
}

fn uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|s| s.split_whitespace().next().map(str::to_string))
        .and_then(|s| s.parse::<f64>().ok())
        .map(|v| v as u64)
        .unwrap_or(0)
}

fn temperature() -> Option<f32> {
    std::fs::read_to_string("/sys/class/thermal/thermal_zone0/temp")
        .ok()
        .and_then(|s| s.trim().parse::<f32>().ok())
        .map(|milli| milli / 1000.0)
}

/// Returns `(total_bytes, used_bytes)` for RAM.
fn mem() -> (u64, u64) {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let kb = |key: &str| -> u64 {
        text.lines()
            .find(|l| l.starts_with(key))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .map(|v| v * 1024)
            .unwrap_or(0)
    };
    let total = kb("MemTotal:");
    let available = kb("MemAvailable:");
    (total, total.saturating_sub(available))
}

/// Returns `(total_bytes, used_bytes)` for the root filesystem.
fn disk() -> (u64, u64) {
    let path = match CString::new("/") {
        Ok(p) => p,
        Err(_) => return (0, 0),
    };
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(path.as_ptr(), &mut st) } != 0 {
        return (0, 0);
    }
    let frsize = st.f_frsize as u64;
    let total = st.f_blocks as u64 * frsize;
    let avail = st.f_bavail as u64 * frsize;
    (total, total.saturating_sub(avail))
}

/// Overall CPU utilisation, sampled over a short interval.
async fn cpu_percent() -> f32 {
    let (idle1, total1) = cpu_times();
    sleep(Duration::from_millis(200)).await;
    let (idle2, total2) = cpu_times();
    let d_total = total2.saturating_sub(total1);
    let d_idle = idle2.saturating_sub(idle1);
    if d_total == 0 {
        0.0
    } else {
        ((1.0 - d_idle as f32 / d_total as f32) * 100.0).clamp(0.0, 100.0)
    }
}

/// `(idle, total)` jiffies from the aggregate `cpu` line of `/proc/stat`.
fn cpu_times() -> (u64, u64) {
    let line = std::fs::read_to_string("/proc/stat")
        .ok()
        .and_then(|s| s.lines().next().map(str::to_string))
        .unwrap_or_default();
    let vals: Vec<u64> = line
        .split_whitespace()
        .skip(1) // "cpu"
        .filter_map(|v| v.parse::<u64>().ok())
        .collect();
    // user, nice, system, idle, iowait, irq, softirq, steal, ...
    let idle = vals.get(3).copied().unwrap_or(0) + vals.get(4).copied().unwrap_or(0);
    let total: u64 = vals.iter().sum();
    (idle, total)
}

fn net_counters() -> Vec<NetCounters> {
    let mut out = Vec::new();
    if let Ok(entries) = std::fs::read_dir("/sys/class/net") {
        for e in entries.flatten() {
            let iface = e.file_name().to_string_lossy().into_owned();
            if iface == "lo" {
                continue;
            }
            let rd = |c: &str| -> u64 {
                std::fs::read_to_string(format!("/sys/class/net/{iface}/statistics/{c}"))
                    .ok()
                    .and_then(|s| s.trim().parse::<u64>().ok())
                    .unwrap_or(0)
            };
            out.push(NetCounters {
                rx_bytes: rd("rx_bytes"),
                tx_bytes: rd("tx_bytes"),
                iface,
            });
        }
    }
    out.sort_by(|a, b| a.iface.cmp(&b.iface));
    out
}

/// Device-local time as a preformatted string (uses the system timezone).
fn local_time() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return String::new();
    }
    let fmt = match CString::new("%Y-%m-%d %H:%M:%S %Z") {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = [0u8; 64];
    let n = unsafe {
        libc::strftime(
            buf.as_mut_ptr() as *mut libc::c_char,
            buf.len(),
            fmt.as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}
