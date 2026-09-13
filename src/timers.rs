//! Scheduled, periodic activation of a service - the `.timer`-equivalent
//! the crate README used to list under "what's deliberately not here
//! yet". Loads `/etc/mitos/services.d/<name>.timer`, paired by name with
//! a `<name>.service` unit (already loaded separately by `units.rs`);
//! when due, runs that service's `ExecStart=` once (its own
//! `User=`/`Group=`/`Environment=`/`WorkingDirectory=` still apply, same
//! as a normal spawn) and waits for it to exit before considering that
//! timer due again - not restarted, not added to `Supervisor`'s tracked
//! set, and not sandboxed the way `apps.rs`'s on-demand launches are
//! (a timer's paired service is configured, trusted system state the
//! same as any other unit file, not an arbitrary third-party app).
//!
//! Supported keys (`[Timer]` section):
//! - `OnBootSec=<seconds>` - first run, this many seconds after
//!   mitos-services started.
//! - `OnUnitActiveSec=<seconds>` - run again this many seconds after the
//!   *previous* run of the same timer finished (not started - a run
//!   that takes longer than the interval doesn't overlap with itself;
//!   see `tick`).
//!
//! Deliberately simplified vs real systemd's timers: no `OnCalendar=`
//! (wall-clock/date-based scheduling), and a timer with only
//! `OnBootSec=` set fires exactly once, not on every restart of
//! mitos-services (there's no persistent "did this already run" state
//! across restarts - matching this crate's general no-database-on-disk
//! design, same reasoning as `rollback.rs` only ever watching the
//! current run, never persisting history).

use crate::config::ServiceDef;
use crate::logging;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::Command;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const SERVICES_DIR: &str = "/etc/mitos/services.d";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimerDef {
    /// Matches the paired service's name (the shared file stem).
    pub name: String,
    pub on_boot: Option<Duration>,
    pub on_active: Option<Duration>,
}

/// Loads every `*.timer` file in `SERVICES_DIR`. A missing directory
/// isn't an error, matching `units::load_all`.
pub fn load_all() -> Vec<TimerDef> {
    let Ok(entries) = fs::read_dir(SERVICES_DIR) else {
        return Vec::new();
    };

    let mut paths: Vec<_> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().map(|e| e == "timer").unwrap_or(false))
        .collect();
    paths.sort();

    let mut timers = Vec::new();
    for path in paths {
        match fs::read_to_string(&path) {
            Ok(text) => match parse_timer(&path, &text) {
                Ok(t) => timers.push(t),
                Err(e) => logging::warn(&format!("skipping {}: {e}", path.display())),
            },
            Err(e) => logging::warn(&format!("couldn't read {}: {e}", path.display())),
        }
    }
    timers
}

fn parse_timer(path: &Path, text: &str) -> Result<TimerDef, String> {
    let name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .ok_or("unreadable filename")?
        .to_string();

    let mut section = String::new();
    let mut on_boot = None;
    let mut on_active = None;

    for raw_line in text.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            section = s.to_string();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let (key, value) = (key.trim(), value.trim());
        if section != "Timer" {
            continue;
        }
        match key {
            "OnBootSec" => on_boot = value.parse().ok().map(Duration::from_secs),
            "OnUnitActiveSec" => on_active = value.parse().ok().map(Duration::from_secs),
            _ => {}
        }
    }

    if on_boot.is_none() && on_active.is_none() {
        return Err("neither OnBootSec= nor OnUnitActiveSec= set".to_string());
    }
    Ok(TimerDef {
        name,
        on_boot,
        on_active,
    })
}

struct State {
    boot: Instant,
    last_finished: HashMap<String, Instant>,
    running: HashSet<String>,
    /// pid -> timer name, so `reap` can recognize a timer-triggered
    /// one-shot's exit and tell it apart from a service or app exit.
    pending: HashMap<i32, String>,
}

static STATE: OnceLock<Mutex<State>> = OnceLock::new();

fn state() -> &'static Mutex<State> {
    STATE.get_or_init(|| {
        Mutex::new(State {
            boot: Instant::now(),
            last_finished: HashMap::new(),
            running: HashSet::new(),
            pending: HashMap::new(),
        })
    })
}

fn is_due(t: &TimerDef, s: &State) -> bool {
    let now = Instant::now();
    match s.last_finished.get(&t.name) {
        None => match (t.on_boot, t.on_active) {
            (Some(d), _) => now >= s.boot + d,
            (None, Some(d)) => now >= s.boot + d, // OnUnitActiveSec-only: first run counts from boot
            (None, None) => false,                // unreachable - parse_timer rejects this
        },
        Some(last) => match t.on_active {
            Some(d) => now >= *last + d,
            None => false, // OnBootSec-only timer: already fired once, never repeats
        },
    }
}

/// Call once per main-loop iteration (cheap - just instant comparisons
/// unless something is actually due). Fires every due timer whose paired
/// service exists in `services` and isn't already mid-run.
pub fn tick(timers: &[TimerDef], services: &[ServiceDef]) {
    if timers.is_empty() {
        return;
    }
    let Ok(mut s) = state().lock() else { return };

    let due_names: Vec<String> = timers
        .iter()
        .filter(|t| !s.running.contains(&t.name) && is_due(t, &s))
        .map(|t| t.name.clone())
        .collect();

    for name in due_names {
        let Some(def) = services.iter().find(|d| d.name == name) else {
            logging::warn(&format!(
                "timer '{name}': no matching service definition, skipping"
            ));
            continue;
        };
        match spawn_oneshot(def) {
            Ok(pid) => {
                logging::info(&format!("timer '{name}': started pid {pid}"));
                s.running.insert(name.clone());
                s.pending.insert(pid, name);
            }
            Err(e) => logging::error(&format!("timer '{name}': failed to start: {e}")),
        }
    }
}

/// True whenever any timer is configured at all - even between runs, the
/// event loop still needs to poll to notice the *next* one becoming due
/// (there's nothing to `waitpid` on while idle between runs). `main.rs`
/// uses this the same way it already uses
/// `Supervisor::has_watchdog_services` to decide whether the event loop
/// needs to poll instead of blocking indefinitely in `waitpid`.
pub fn has_any(timers: &[TimerDef]) -> bool {
    !timers.is_empty()
}

fn spawn_oneshot(def: &ServiceDef) -> std::io::Result<i32> {
    let uid = def.user.as_deref().and_then(crate::users::resolve_uid);
    let gid = def.group.as_deref().and_then(crate::users::resolve_gid);

    let mut cmd = Command::new(&def.path);
    cmd.args(&def.args);
    for (key, value) in &def.environment {
        cmd.env(key, value);
    }
    if let Some(dir) = &def.working_dir {
        cmd.current_dir(dir);
    }
    if let Some(u) = uid {
        cmd.uid(u);
    }
    if let Some(g) = gid {
        cmd.gid(g);
    }

    let child = cmd.spawn()?;
    Ok(child.id() as i32)
}

/// Called from the main event loop for every exited pid, before falling
/// through to `apps::reap`/`Supervisor::handle_exit` - see `main.rs`.
/// Returns whether `pid` was a timer-triggered one-shot (and if so, has
/// already logged and updated scheduling state); `false` means the
/// caller should try the next layer instead.
pub fn reap(pid: i32) -> bool {
    let Ok(mut s) = state().lock() else {
        return false;
    };
    let Some(name) = s.pending.remove(&pid) else {
        return false;
    };
    logging::info(&format!("timer '{name}': run finished (pid {pid})"));
    s.running.remove(&name);
    s.last_finished.insert(name, Instant::now());
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_on_boot_and_on_active() {
        let text = "[Timer]\nOnBootSec=30\nOnUnitActiveSec=3600\n";
        let t = parse_timer(Path::new("logrotate.timer"), text).unwrap();
        assert_eq!(t.name, "logrotate");
        assert_eq!(t.on_boot, Some(Duration::from_secs(30)));
        assert_eq!(t.on_active, Some(Duration::from_secs(3600)));
    }

    #[test]
    fn rejects_a_timer_with_neither_key_set() {
        let text = "[Timer]\n# nothing here\n";
        assert!(parse_timer(Path::new("empty.timer"), text).is_err());
    }

    #[test]
    fn ignores_keys_outside_timer_section() {
        let text = "[Unit]\nOnBootSec=5\n\n[Timer]\nOnBootSec=30\n";
        let t = parse_timer(Path::new("x.timer"), text).unwrap();
        assert_eq!(t.on_boot, Some(Duration::from_secs(30)));
    }

    #[test]
    fn a_never_run_timer_with_on_boot_is_due_once_the_delay_has_passed() {
        let t = TimerDef {
            name: "x".into(),
            on_boot: Some(Duration::from_secs(0)),
            on_active: None,
        };
        let s = State {
            boot: Instant::now() - Duration::from_secs(1),
            last_finished: HashMap::new(),
            running: HashSet::new(),
            pending: HashMap::new(),
        };
        assert!(is_due(&t, &s));
    }

    #[test]
    fn an_on_boot_only_timer_never_repeats() {
        let t = TimerDef {
            name: "x".into(),
            on_boot: Some(Duration::from_secs(0)),
            on_active: None,
        };
        let mut last_finished = HashMap::new();
        last_finished.insert("x".to_string(), Instant::now() - Duration::from_secs(100));
        let s = State {
            boot: Instant::now() - Duration::from_secs(200),
            last_finished,
            running: HashSet::new(),
            pending: HashMap::new(),
        };
        assert!(!is_due(&t, &s));
    }

    #[test]
    fn an_on_active_timer_is_due_after_the_interval_since_last_finish() {
        let t = TimerDef {
            name: "x".into(),
            on_boot: None,
            on_active: Some(Duration::from_millis(10)),
        };
        let mut last_finished = HashMap::new();
        last_finished.insert("x".to_string(), Instant::now() - Duration::from_secs(1));
        let s = State {
            boot: Instant::now() - Duration::from_secs(2),
            last_finished,
            running: HashSet::new(),
            pending: HashMap::new(),
        };
        assert!(is_due(&t, &s));
    }
}
