//! In-memory ring buffer of recent log lines, for the web UI's Logs tab.
//!
//! A `tracing` layer mirrors every event into a bounded buffer (in addition to
//! the stdout/journald output), so the dashboard can show recent logs without
//! shelling out to `journalctl` or depending on the unit name.

use std::collections::VecDeque;
use std::ffi::CString;
use std::fmt::Write as _;
use std::sync::{Arc, Mutex, OnceLock};

use tracing::field::{Field, Visit};
use tracing::{Event, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

const CAPACITY: usize = 500;

type Buffer = Arc<Mutex<VecDeque<String>>>;
static BUFFER: OnceLock<Buffer> = OnceLock::new();

/// Create the shared buffer and return the tracing layer that fills it.
/// Call once, during tracing setup.
pub fn init() -> BufferLayer {
    let buf: Buffer = Arc::new(Mutex::new(VecDeque::with_capacity(CAPACITY)));
    let _ = BUFFER.set(buf.clone());
    BufferLayer { buf }
}

/// Snapshot the buffered log lines, oldest first.
pub fn snapshot() -> Vec<String> {
    BUFFER
        .get()
        .and_then(|b| b.lock().ok().map(|q| q.iter().cloned().collect()))
        .unwrap_or_default()
}

/// A `tracing` layer that appends formatted events to the ring buffer.
pub struct BufferLayer {
    buf: Buffer,
}

impl<S: Subscriber> Layer<S> for BufferLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = LineVisitor::default();
        event.record(&mut visitor);

        let line = format!(
            "{} {:>5} {}{}",
            now_hms(),
            meta.level(),
            visitor.message,
            visitor.fields
        );

        if let Ok(mut q) = self.buf.lock() {
            if q.len() >= CAPACITY {
                q.pop_front();
            }
            q.push_back(line);
        }
    }
}

#[derive(Default)]
struct LineVisitor {
    message: String,
    fields: String,
}

impl Visit for LineVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            let _ = write!(self.fields, " {}={}", field.name(), value);
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        if field.name() == "message" {
            self.message = format!("{value:?}");
        } else {
            let _ = write!(self.fields, " {}={:?}", field.name(), value);
        }
    }
}

/// Current local time as `HH:MM:SS` (best-effort).
fn now_hms() -> String {
    let now = unsafe { libc::time(std::ptr::null_mut()) };
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    if unsafe { libc::localtime_r(&now, &mut tm) }.is_null() {
        return String::new();
    }
    let fmt = match CString::new("%H:%M:%S") {
        Ok(f) => f,
        Err(_) => return String::new(),
    };
    let mut buf = [0u8; 16];
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
