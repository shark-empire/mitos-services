//! A small, size-capped, in-memory log journal - `mitosctl logs
//! [filter]` / the `LOGS` control-socket command.
//!
//! This is *not* meant to replace `logging.rs`'s existing output (still
//! the primary log path - `/dev/kmsg` when available, else stdout/
//! stderr - and still what a real syslog/journald downstream would
//! collect from there). It's a bounded recent-history buffer so
//! `mitosctl logs` has something to show even when nothing else is
//! collecting mitos-services' own output, without ever growing
//! unbounded the way a plain append-only log file would - see
//! `MAX_LINES`.
//!
//! In-memory only, deliberately: a file-backed journal needs rotation,
//! error handling for a full or read-only filesystem, and (if it's
//! meant to survive a restart) a format stable enough to reopen - real
//! complexity for what this is meant to be, a live-debugging
//! convenience, not a durable audit trail. The tradeoff is that the
//! buffer resets when mitos-services itself restarts; a real audit log
//! belongs downstream (a syslog daemon reading `/dev/kmsg`, or
//! journald), not here.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

/// How many lines to keep. Bounded by count rather than bytes - simpler,
/// and this crate's log lines are all short, single-line, already-
/// bounded-ish text (see `logging.rs`), so a line cap is a reasonable
/// proxy for a byte cap without needing to measure anything. At a rough
/// ~100 bytes/line that's on the order of a couple hundred KB - a
/// deliberately small, fixed cost regardless of uptime.
const MAX_LINES: usize = 2000;

static JOURNAL: OnceLock<Mutex<VecDeque<String>>> = OnceLock::new();

fn journal() -> &'static Mutex<VecDeque<String>> {
    JOURNAL.get_or_init(|| Mutex::new(VecDeque::with_capacity(MAX_LINES)))
}

/// Appends one already-formatted log line (see `logging::write_line`) to
/// the journal, dropping the oldest line first if it's already at
/// `MAX_LINES`. Called from `logging.rs` for every line it writes -
/// nothing else should need to call this directly.
pub fn record(line: &str) {
    let Ok(mut j) = journal().lock() else {
        return;
    };
    if j.len() >= MAX_LINES {
        j.pop_front();
    }
    j.push_back(line.to_string());
}

/// Every currently-buffered line containing `filter` as a substring
/// (case-sensitive - service names and log tags are, so this is
/// consistent with how they're written), oldest first. An empty filter
/// returns every buffered line.
pub fn recent(filter: &str) -> Vec<String> {
    let Ok(j) = journal().lock() else {
        return Vec::new();
    };
    j.iter()
        .filter(|line| filter.is_empty() || line.contains(filter))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    // The journal is a single global (see JOURNAL above), so tests that
    // touch it can't run concurrently with each other without
    // interfering with each other's counts - this guards the whole
    // module's tests against that (Rust runs `#[test]` functions in
    // parallel by default).
    static TEST_LOCK: StdMutex<()> = StdMutex::new(());

    fn reset() {
        if let Ok(mut j) = journal().lock() {
            j.clear();
        }
    }

    #[test]
    fn records_and_recalls_lines() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        record("mitos-services [  OK] [   1.000] service 'web' started");
        record("mitos-services [WARN] [   2.000] service 'db' slow to start");
        let all = recent("");
        assert_eq!(all.len(), 2);
        let web_only = recent("'web'");
        assert_eq!(web_only.len(), 1);
        assert!(web_only[0].contains("started"));
    }

    #[test]
    fn drops_oldest_once_over_the_cap() {
        let _guard = TEST_LOCK.lock().unwrap();
        reset();
        for i in 0..MAX_LINES + 10 {
            record(&format!("line {i}"));
        }
        let all = recent("");
        assert_eq!(all.len(), MAX_LINES);
        // The first 10 lines should have been evicted; line 10 is the
        // oldest survivor.
        assert!(all[0].contains("line 10"));
        assert!(all.last().unwrap().contains(&format!("line {}", MAX_LINES + 9)));
    }
}
