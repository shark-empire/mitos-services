//! Unix-socket control server for `mitosctl`.
//!
//! A plain newline-delimited text protocol, not JSON - consistent with
//! this project's preference for hand-rolled, dependency-free parsing
//! over pulling in a serialization crate for a handful of simple
//! commands. One connection, one command, one response, then the
//! connection closes - not a persistent session protocol.
//!
//! Commands treat the socket as another way to deliver the same signals
//! `kill -USR1`/`-USR2 <mitos-services-pid>` already do: `RELOAD` and
//! `STATUS` just set the same atomic flags `signals.rs`'s handlers set,
//! so there's exactly one reload/status code path in the main event
//! loop regardless of which way it was triggered. `ISOLATE <target>`
//! (see `targets.rs`) is the same idea with an argument attached, so it
//! needs a small `Mutex<Option<String>>` instead of a bare flag.
//! `TARGETS` reads a small cache the main loop publishes on every
//! target-relevant change, the same way `STATUS` reads one the main loop
//! publishes on every `STATUS_DUMP_REQUESTED`. `LAUNCH <path> [args...]`
//! and `APPS` (see `apps.rs`) don't touch the main loop's state at all -
//! launching and listing on-demand apps needs no coordination with the
//! services the main loop supervises, so both run straight through to
//! `apps.rs` on this thread.
//!
//! Connections are handled one at a time, sequentially, on this
//! listener's own thread - not thread-per-connection. `mitosctl` usage
//! (connect, send one line, read one response, disconnect) is quick
//! enough that this is a reasonable simplification for a first version,
//! not a bottleneck worth the added complexity of a thread pool yet.

use crate::logging;
use crate::signals;
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::Ordering;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

const SOCKET_PATH: &str = "/run/mitos-services/control.sock";

/// (version, text) - version increments on every publish, so a waiting
/// STATUS request can tell "a fresh dump happened" apart from "the dump
/// hasn't run yet", even when the text itself happens to be unchanged.
static LAST_STATUS: OnceLock<Mutex<(u64, String)>> = OnceLock::new();

/// Called by the main loop whenever it produces a fresh status summary
/// (on SIGUSR2/STATUS_DUMP_REQUESTED), so an IPC STATUS request has real
/// data to hand back instead of just an acknowledgement.
pub fn publish_status(text: &str) {
    let cell = LAST_STATUS.get_or_init(|| Mutex::new((0, String::new())));
    if let Ok(mut guard) = cell.lock() {
        guard.0 += 1;
        guard.1 = text.to_string();
    }
}

fn snapshot() -> (u64, String) {
    LAST_STATUS
        .get_or_init(|| Mutex::new((0, String::new())))
        .lock()
        .map(|g| (g.0, g.1.clone()))
        .unwrap_or((0, String::new()))
}

/// (current target, every configured target) - published by the main
/// loop at boot and after every reload or completed isolate (see
/// `targets.rs`). `TARGETS` reads this directly rather than the
/// signal-and-wait pattern `STATUS` uses: there's no "please compute
/// this fresh" step, just whatever the main loop last knew.
static LAST_TARGETS: OnceLock<Mutex<(String, Vec<String>)>> = OnceLock::new();

pub fn publish_targets(current: &str, all: &[String]) {
    let cell = LAST_TARGETS.get_or_init(|| Mutex::new((String::new(), Vec::new())));
    if let Ok(mut guard) = cell.lock() {
        guard.0 = current.to_string();
        guard.1 = all.to_vec();
    }
}

fn targets_response() -> String {
    let cell = LAST_TARGETS.get_or_init(|| Mutex::new((String::new(), Vec::new())));
    let Ok(guard) = cell.lock() else {
        return "target list unavailable\n".to_string();
    };
    if guard.1.is_empty() {
        return "no targets published yet\n".to_string();
    }
    let mut lines = vec![format!("active: {}", guard.0)];
    for name in &guard.1 {
        let marker = if name == &guard.0 { "*" } else { " " };
        lines.push(format!("{marker} {name}"));
    }
    lines.push(String::new());
    lines.join("\n")
}

/// Set by the `ISOLATE` command, consumed once per main-loop iteration -
/// the same idea as `RELOAD_REQUESTED`/`STATUS_DUMP_REQUESTED` in
/// `signals.rs`, except this carries a target name, so a plain
/// `AtomicBool` isn't enough to hold it.
static PENDING_ISOLATE: OnceLock<Mutex<Option<String>>> = OnceLock::new();

fn request_isolate(name: &str) {
    let cell = PENDING_ISOLATE.get_or_init(|| Mutex::new(None));
    if let Ok(mut guard) = cell.lock() {
        *guard = Some(name.to_string());
    }
}

/// Returns and clears any `ISOLATE` request that arrived since the last
/// call - see `main.rs`'s event loop.
pub fn take_pending_isolate() -> Option<String> {
    let cell = PENDING_ISOLATE.get_or_init(|| Mutex::new(None));
    cell.lock().ok()?.take()
}

pub fn spawn_listener() {
    thread::spawn(|| {
        if let Err(e) = run() {
            logging::warn(&format!("IPC listener stopped: {e}"));
        }
    });
}

fn run() -> std::io::Result<()> {
    if let Some(parent) = std::path::Path::new(SOCKET_PATH).parent() {
        std::fs::create_dir_all(parent)?;
    }
    let _ = std::fs::remove_file(SOCKET_PATH); // stale socket from a previous run

    let listener = UnixListener::bind(SOCKET_PATH)?;
    let _ = std::fs::set_permissions(SOCKET_PATH, std::fs::Permissions::from_mode(0o600));
    logging::debug("IPC listener up");

    for conn in listener.incoming() {
        match conn {
            Ok(stream) => handle(stream),
            Err(e) => logging::debug(&format!("IPC accept error: {e}")),
        }
    }
    Ok(())
}

fn handle(stream: UnixStream) {
    let Ok(cloned) = stream.try_clone() else {
        return;
    };
    let mut reader = BufReader::new(cloned);
    let mut line = String::new();
    if reader.read_line(&mut line).is_err() {
        return;
    }
    let mut writer = stream;

    let trimmed = line.trim();
    let (cmd, rest) = trimmed.split_once(' ').unwrap_or((trimmed, ""));
    let rest = rest.trim();

    let response = match cmd {
        "STATUS" => {
            signals::STATUS_DUMP_REQUESTED.store(true, Ordering::SeqCst);
            wait_for_fresh_status(Duration::from_millis(500))
        }
        "RELOAD" => {
            signals::RELOAD_REQUESTED.store(true, Ordering::SeqCst);
            "reload requested - see the log for the outcome\n".to_string()
        }
        "PING" => "PONG\n".to_string(),
        "TARGETS" => targets_response(),
        "ISOLATE" if rest.is_empty() => "usage: ISOLATE <target-name>\n".to_string(),
        "ISOLATE" => {
            request_isolate(rest);
            format!("isolate to '{rest}' requested - see the log for the outcome\n")
        }
        "LAUNCH" if rest.is_empty() => "usage: LAUNCH <path> [args...]\n".to_string(),
        "LAUNCH" => {
            let mut parts = rest.split_whitespace();
            // `.next()` can't fail here - `rest.is_empty()` was already
            // ruled out by the guard above, so there's at least one token.
            let path = parts.next().unwrap_or_default();
            let args: Vec<String> = parts.map(str::to_string).collect();
            match crate::apps::launch(path, &args) {
                Ok(id) => format!("launched: {id}\n"),
                Err(e) => format!("launch failed: {e}\n"),
            }
        }
        "APPS" => format!("{}\n", crate::apps::list()),
        other => format!("unknown command '{other}'\n"),
    };

    let _ = writer.write_all(response.as_bytes());
}

/// `STATUS_DUMP_REQUESTED` is consumed by the main loop asynchronously,
/// so this polls briefly for a fresh `publish_status` call rather than
/// assuming one already happened by the time this function runs.
fn wait_for_fresh_status(timeout: Duration) -> String {
    let (before_version, _) = snapshot();
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        let (version, text) = snapshot();
        if version != before_version {
            return format!("{text}\n");
        }
        thread::sleep(Duration::from_millis(20));
    }
    "status request timed out\n".to_string()
}
