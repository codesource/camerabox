//! Optional GPIO factory-reset button.
//!
//! When `reset_button = "chip:line"` is set in the config, this watches that
//! GPIO input; holding it for `reset_hold_secs` performs the same
//! [`crate::net::factory_reset`] as the dashboard button, then reboots. Wire a
//! momentary button between the GPIO and GND (default `reset_active_low = true`,
//! internal pull-up). A no-op if no button is configured.

use std::sync::Arc;
use std::time::Duration;

use gpiocdev::line::{Bias, Value};
use gpiocdev::Request;
use tracing::{error, info, warn};

use crate::config::Config;
use crate::net;

const POLL_MS: u64 = 200;

pub async fn watch(cfg: Arc<Config>) {
    let spec = cfg.reset_button.trim();
    if spec.is_empty() {
        return; // feature disabled
    }
    let (chip, line) = match parse(spec) {
        Some(x) => x,
        None => {
            error!(spec, "invalid reset_button (expected e.g. gpiochip0:17)");
            return;
        }
    };
    let dev = format!("/dev/{chip}");

    let mut b = Request::builder();
    b.on_chip(&dev)
        .with_consumer("camera-box-reset")
        .with_line(line)
        .as_input()
        .with_bias(Bias::PullUp);
    if cfg.reset_active_low {
        b.as_active_low();
    }
    let req = match b.request() {
        Ok(r) => r,
        Err(e) => {
            error!(error = %e, dev, line, "reset button: cannot open GPIO line");
            return;
        }
    };

    let need_ms = cfg.reset_hold_secs.max(1) * 1000;
    info!(dev, line, hold_secs = cfg.reset_hold_secs, "GPIO reset button armed");

    let mut held: u64 = 0;
    let mut tick = tokio::time::interval(Duration::from_millis(POLL_MS));
    loop {
        tick.tick().await;
        if matches!(req.value(line), Ok(Value::Active)) {
            held += POLL_MS;
            if held >= need_ms {
                warn!(secs = cfg.reset_hold_secs, "reset button held — factory reset");
                let _ = net::factory_reset().await;
                let _ = tokio::process::Command::new("systemctl")
                    .args(["reboot"])
                    .status()
                    .await;
                return;
            }
        } else {
            held = 0; // released — restart the hold timer
        }
    }
}

/// Parse `chip:line`, accepting `gpiochip0:17`, `0:17`, or `/dev/gpiochip0:17`.
fn parse(s: &str) -> Option<(String, u32)> {
    let (chip, line) = s.rsplit_once(':')?;
    let chip = chip.trim().trim_start_matches("/dev/");
    let chip = if chip.starts_with("gpiochip") {
        chip.to_string()
    } else {
        format!("gpiochip{}", chip.trim())
    };
    Some((chip, line.trim().parse().ok()?))
}
