//! OOM-score tuning (`OOMScoreAdjust=`) and memory-pressure (PSI)
//! observability - runs as its own background thread (`spawn_monitor`),
//! independent of the main event loop the same way `ipc::spawn_listener`
//! is, since periodic pressure checks have nothing to do with service
//! supervision or `waitpid`.
//!
//! **What this doesn't do (yet).** This only tunes which processes the
//! kernel's OOM killer reaches for *after* memory is already exhausted,
//! and logs when pressure is high - it never proactively stops or
//! throttles anything itself. A policy for "automatically stop which
//! services, in what order, under how much sustained pressure" is a
//! judgment call worth designing deliberately (and testing under real
//! memory pressure) rather than guessing at here alongside everything
//! else in this round - flagged rather than silently missing, same as
//! `apps.rs`'s deferred PID namespace isolation. `read_pressure`'s
//! result is deliberately public so that policy can be layered on later
//! without needing to touch this module again.

use crate::logging;
use std::fs;
use std::time::Duration;

/// How often the background thread checks `/proc/pressure/memory`.
pub const CHECK_INTERVAL: Duration = Duration::from_secs(30);

/// Above this `some avg10` (percentage of the last 10s where at least
/// one task was stalled on memory), the monitor logs a warning.
pub const WARN_THRESHOLD: f32 = 60.0;

/// Writes `score` to `/proc/<pid>/oom_score_adj` - `OOMScoreAdjust=`.
/// Valid range is -1000 (never kill first) to 1000 (kill first); Linux
/// clamps out-of-range values itself (see `proc_pid_oom_score_adj` in
/// proc(5)), so this doesn't validate the range either. Best-effort,
/// matching this crate's general style: a service that can't be
/// adjusted still runs, just without the requested protection.
pub fn set_score_adjust(pid: i32, score: i32) {
    let path = format!("/proc/{pid}/oom_score_adj");
    if fs::write(&path, score.to_string()).is_err() {
        logging::debug(&format!("couldn't set oom_score_adj for pid {pid}"));
    }
}

/// One reading of `/proc/pressure/memory` - see proc(5)'s "Pressure
/// Stall Information" section. `some_avg10` is the percentage of the
/// last 10 seconds where at least one task was stalled waiting on
/// memory; `full_avg10` is the stronger signal - the percentage where
/// *every* runnable task was stalled at once, i.e. the whole system made
/// zero progress.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pressure {
    pub some_avg10: f32,
    pub full_avg10: f32,
}

/// Reads and parses `/proc/pressure/memory`. `None` if PSI isn't
/// available (kernel built without `CONFIG_PSI`, no procfs, or the file
/// doesn't parse as expected) - callers should treat that the same as
/// "no pressure data available", not as an error worth logging on its
/// own on every check.
pub fn read_pressure() -> Option<Pressure> {
    let text = fs::read_to_string("/proc/pressure/memory").ok()?;
    parse_pressure(&text)
}

/// The parsing half of `read_pressure`, kept separate so it can be unit
/// tested against fixed input instead of the real (unmockable)
/// `/proc/pressure/memory` path.
fn parse_pressure(text: &str) -> Option<Pressure> {
    let mut some_avg10 = None;
    let mut full_avg10 = None;
    for line in text.lines() {
        let (label, rest) = line.split_once(' ')?;
        let avg10 = rest
            .split_whitespace()
            .find_map(|f| f.strip_prefix("avg10="))
            .and_then(|v| v.parse().ok());
        match label {
            "some" => some_avg10 = avg10,
            "full" => full_avg10 = avg10,
            _ => {}
        }
    }
    Some(Pressure {
        some_avg10: some_avg10?,
        full_avg10: full_avg10?,
    })
}

/// Starts the background monitor thread. Call once, at startup.
pub fn spawn_monitor() {
    std::thread::spawn(|| loop {
        std::thread::sleep(CHECK_INTERVAL);
        if let Some(p) = read_pressure() {
            if p.some_avg10 >= WARN_THRESHOLD {
                logging::warn(&format!(
                    "memory pressure high: some avg10={:.1}% full avg10={:.1}% - \
                     consider setting OOMScoreAdjust= on non-critical services",
                    p.some_avg10, p.full_avg10
                ));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(some10: &str, full10: &str) -> String {
        format!(
            "some avg10={some10} avg60=0.00 avg300=0.00 total=123\n\
             full avg10={full10} avg60=0.00 avg300=0.00 total=45\n"
        )
    }

    #[test]
    fn parses_a_well_formed_pressure_file() {
        let text = sample("12.34", "1.20");
        let p = parse_pressure(&text).unwrap();
        assert!((p.some_avg10 - 12.34).abs() < f32::EPSILON);
        assert!((p.full_avg10 - 1.20).abs() < f32::EPSILON);
    }

    #[test]
    fn malformed_content_yields_none() {
        assert!(parse_pressure("not the right format at all").is_none());
    }

    #[test]
    fn empty_content_yields_none() {
        assert!(parse_pressure("").is_none());
    }
}
