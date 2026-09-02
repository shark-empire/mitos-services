//! Minimal, allocation-light logging for early boot.
//!
//! No `log`/`env_logger` dependency on purpose: pulling those in adds
//! compile time and a bit of binary size for functionality PID 1 doesn't
//! need. This writes straight to `/dev/kmsg` when it exists (so messages
//! show up in `dmesg` even before a syslog daemon is running) and falls
//! back to stdout/stderr otherwise.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Level {
    Error = 0,
    Warn = 1,
    Info = 2,
    Debug = 3,
}

impl Level {
    pub fn parse(s: &str) -> Option<Level> {
        match s.to_ascii_lowercase().as_str() {
            "error" => Some(Level::Error),
            "warn" | "warning" => Some(Level::Warn),
            "info" => Some(Level::Info),
            "debug" => Some(Level::Debug),
            _ => None,
        }
    }
}

static CURRENT_LEVEL: AtomicU8 = AtomicU8::new(Level::Info as u8);
static BOOT_CLOCK: OnceLock<Instant> = OnceLock::new();
static KMSG: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

/// Call once at startup, before anything else logs.
pub fn init() {
    BOOT_CLOCK.get_or_init(Instant::now);
    let kmsg = OpenOptions::new().write(true).open("/dev/kmsg").ok();
    let _ = KMSG.set(Mutex::new(kmsg));
}

pub fn set_level(level: Level) {
    CURRENT_LEVEL.store(level as u8, Ordering::Relaxed);
}

fn uptime_secs() -> f64 {
    BOOT_CLOCK
        .get()
        .map(|t| t.elapsed().as_secs_f64())
        .unwrap_or(0.0)
}

fn write_line(level: Level, tag: &str, msg: &str) {
    if (level as u8) > CURRENT_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    let line = format!("mitos-init [{tag:>4}] [{:>8.3}] {msg}\n", uptime_secs());

    // Prefer /dev/kmsg so the message survives in `dmesg` even before a
    // syslog daemon exists; fall back to stdout/stderr.
    let mut wrote_kmsg = false;
    if let Some(cell) = KMSG.get() {
        if let Ok(mut guard) = cell.lock() {
            if let Some(f) = guard.as_mut() {
                wrote_kmsg = f.write_all(line.as_bytes()).is_ok();
            }
        }
    }
    if !wrote_kmsg {
        if matches!(level, Level::Error | Level::Warn) {
            let _ = std::io::stderr().write_all(line.as_bytes());
        } else {
            let _ = std::io::stdout().write_all(line.as_bytes());
        }
    }
}

pub fn error(msg: &str) {
    write_line(Level::Error, "FAIL", msg);
}
pub fn warn(msg: &str) {
    write_line(Level::Warn, "WARN", msg);
}
pub fn info(msg: &str) {
    write_line(Level::Info, "OK", msg);
}
pub fn debug(msg: &str) {
    write_line(Level::Debug, "DBG", msg);
}
