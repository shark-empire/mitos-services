//! Process supervision (Phase 3): spawns configured services, reaps
//! whatever exits (tracked services and reparented orphans alike), and
//! restarts supervised services according to their restart policy.
//!
//! Beyond plain pid tracking, wired in here:
//! - Every service gets a cgroup (`cgroups.rs`) so teardown can reach
//!   grandchildren the tracked pid alone never could.
//! - Every service gets its own readiness/watchdog socket (`notify.rs`)
//!   so `status_summary` can report actual readiness, and
//!   `expired_watchdogs` can catch a hung-but-not-exited service.
//! - `reload_services` reconciles a running set against a new config
//!   (start/stop/restart only what changed) - the mechanism
//!   `rollback.rs`'s transactional reload is built on.
//! - `spawn_all` folds `before=`/`requires=`/`wants=` into an effective
//!   `after` list (`effective_after`), orders services by it
//!   (`topological_order`), enforces `requires=` (skip if the required
//!   service didn't end up running), and for `after_ready`, blocks
//!   briefly for the dependency's readiness before starting the
//!   dependent (`wait_for_ready`).
//! - Services can run as a specific `user`/`group` (`users.rs`) instead
//!   of inheriting mitos-init's own root privileges.

use crate::config::{RestartPolicy, ServiceDef};
use crate::logging;
use crate::notify::ReadyState;
use nix::sys::signal::{kill, Signal};
use nix::sys::wait::WaitStatus;
use nix::unistd::Pid;
use std::collections::{HashMap, HashSet};
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// If a service restarts more than this many times within `BACKOFF_WINDOW`,
/// supervision gives up on it rather than burning CPU in a restart storm.
const MAX_RESTARTS_IN_WINDOW: usize = 5;
const BACKOFF_WINDOW: Duration = Duration::from_secs(10);
/// How long `spawn_all` will wait for an `after_ready` dependency before
/// giving up and starting the dependent anyway.
const READY_WAIT_TIMEOUT: Duration = Duration::from_secs(15);

struct Supervised {
    def: ServiceDef,
    restart_times: Vec<Instant>,
    /// When this instance was spawned - `expired_watchdogs` falls back
    /// to this if the service hasn't sent its first `WATCHDOG=1` ping
    /// yet, so a freshly-started service isn't immediately treated as
    /// hung before it's had a chance to ping at all.
    started_at: Instant,
}

pub enum Outcome {
    /// Keep looping; nothing the caller needs to act on.
    Continue,
    /// A critical service exited (carries its name) — time to shut the
    /// system down, unless an active reload watch intercepts this first.
    Halt(String),
    /// A service exhausted its restart budget and won't be retried again
    /// (carries its name) - lets a reload watch tell whether this was one
    /// of the services *it* just touched.
    GaveUp(String),
}

pub struct Supervisor {
    services: HashMap<i32, Supervised>, // keyed by current pid
    ready_state: Arc<ReadyState>,
}

impl Supervisor {
    pub fn new() -> Self {
        Supervisor {
            services: HashMap::new(),
            ready_state: Arc::new(ReadyState::default()),
        }
    }

    /// Starts every service in `defs`, ordered so each comes after
    /// everything in its effective `after` list (`effective_after` folds
    /// in `before=`/`requires=`/`wants=`, then `topological_order` sorts
    /// by it), skipping any service whose `requires=` names a configured
    /// service that didn't end up running, and waiting briefly on any
    /// `after_ready` dependency's actual readiness before starting the
    /// dependent (`wait_for_ready`). Only used for the initial boot spawn
    /// - `reload_services` deliberately doesn't re-order, re-wait, or
    /// re-enforce `requires=` on an already-running system.
    pub fn spawn_all(&mut self, defs: &[ServiceDef]) {
        let ordered = topological_order(&effective_after(defs));
        let configured: HashSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
        let mut spawned: HashSet<String> = HashSet::new();

        for def in ordered {
            let hard_missing: Vec<String> = def
                .requires
                .iter()
                .filter(|r| configured.contains(r.as_str()) && !spawned.contains(r.as_str()))
                .cloned()
                .collect();
            if !hard_missing.is_empty() {
                logging::error(&format!(
                    "service '{}' requires {hard_missing:?} which didn't start - skipping",
                    def.name
                ));
                continue;
            }

            for dep in &def.after_ready {
                self.wait_for_ready(dep, READY_WAIT_TIMEOUT);
            }

            let name = def.name.clone();
            self.spawn_with_history(def, Vec::new());
            if self.services.values().any(|s| s.def.name == name) {
                spawned.insert(name);
            }
        }
    }

    /// Polls (bounded) for `name` to report `READY=1` before returning.
    fn wait_for_ready(&self, name: &str, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if self.ready_state.is_ready(name) {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        logging::warn(&format!(
            "timed out waiting for '{name}' to report ready, starting the dependent service anyway"
        ));
    }

    /// True if any currently-running service has a `watchdog_timeout`
    /// configured - the main loop uses this (alongside an active reload
    /// watch) to decide whether it needs to poll instead of blocking
    /// indefinitely in `waitpid`, since noticing a *missed* deadline
    /// needs periodic checking, not just reacting to child-exit events.
    pub fn has_watchdog_services(&self) -> bool {
        self.services
            .values()
            .any(|s| s.def.watchdog_timeout.is_some())
    }

    /// Checks every currently-running service with a `watchdog_timeout`
    /// configured, and returns the pids of any that have missed their
    /// deadline - a service that hasn't sent its *first* ping yet is
    /// judged against its spawn time instead, so it gets a fair chance
    /// before being killed for something it was never given time to do.
    /// Callers (the main event loop) are expected to `SIGKILL` the
    /// returned pids and let the normal reap-and-restart path in
    /// `handle_exit` take it from there - this function only detects,
    /// it doesn't act.
    pub fn expired_watchdogs(&self) -> Vec<i32> {
        let now = Instant::now();
        self.services
            .iter()
            .filter_map(|(&pid, sup)| {
                let timeout = sup.def.watchdog_timeout?;
                let last = self
                    .ready_state
                    .last_ping(&sup.def.name)
                    .unwrap_or(sup.started_at);
                if now.duration_since(last) > timeout {
                    Some(pid)
                } else {
                    None
                }
            })
            .collect()
    }

    fn spawn_with_history(&mut self, def: ServiceDef, restart_times: Vec<Instant>) {
        let uid = def.user.as_deref().and_then(crate::users::resolve_uid);
        if def.user.is_some() && uid.is_none() {
            logging::warn(&format!(
                "service '{}': couldn't resolve user '{}', running as root",
                def.name,
                def.user.as_deref().unwrap_or_default()
            ));
        }
        let gid = def.group.as_deref().and_then(crate::users::resolve_gid);
        if def.group.is_some() && gid.is_none() {
            logging::warn(&format!(
                "service '{}': couldn't resolve group '{}', running with the default group",
                def.name,
                def.group.as_deref().unwrap_or_default()
            ));
        }

        self.ready_state.forget(&def.name); // clear any stale state from a previous instance

        let mut cmd = Command::new(&def.path);
        cmd.args(&def.args);
        if let Some(sock) = crate::notify::listen_for(&def.name, self.ready_state.clone(), uid, gid)
        {
            cmd.env("NOTIFY_SOCKET", sock);
        }
        if let Some(u) = uid {
            cmd.uid(u);
        }
        if let Some(g) = gid {
            cmd.gid(g);
        }

        match cmd.spawn() {
            Ok(child) => {
                let pid = child.id() as i32;
                logging::info(&format!(
                    "started '{}' ({}) as pid {pid}",
                    def.name, def.path
                ));
                crate::cgroups::create_for(&def.name, def.memory_limit);
                crate::cgroups::attach(&def.name, pid);
                self.services.insert(
                    pid,
                    Supervised {
                        def,
                        restart_times,
                        started_at: Instant::now(),
                    },
                );
            }
            Err(e) => {
                logging::error(&format!(
                    "failed to start '{}' ({}): {e}",
                    def.name, def.path
                ));
                // A critical service that won't even start leaves the
                // machine unreachable — fall back to a rescue shell rather
                // than continue with no console at all.
                if def.critical {
                    if let Ok(child) = Command::new("/bin/sh").spawn() {
                        let pid = child.id() as i32;
                        logging::warn("fell back to /bin/sh");
                        let def = fallback_shell();
                        crate::cgroups::create_for(&def.name, None);
                        crate::cgroups::attach(&def.name, pid);
                        self.services.insert(
                            pid,
                            Supervised {
                                def,
                                restart_times: Vec::new(),
                                started_at: Instant::now(),
                            },
                        );
                    }
                }
            }
        }
    }

    /// Feeds one `waitpid` result into the supervisor.
    pub fn handle_exit(&mut self, status: WaitStatus) -> Outcome {
        let (pid, summary) = match status {
            WaitStatus::Exited(pid, code) => (pid.as_raw(), format!("exited with status {code}")),
            WaitStatus::Signaled(pid, sig, _) => {
                (pid.as_raw(), format!("killed by signal {sig:?}"))
            }
            _ => return Outcome::Continue, // Stopped/Continued/etc: not a real exit
        };

        let Some(sup) = self.services.remove(&pid) else {
            // Reaped an orphan we weren't tracking — nothing more to do.
            return Outcome::Continue;
        };

        logging::warn(&format!("service '{}' {summary}", sup.def.name));
        crate::cgroups::kill_and_remove(&sup.def.name); // sweep any grandchildren this exit left behind
        self.ready_state.forget(&sup.def.name);

        if sup.def.critical {
            return Outcome::Halt(sup.def.name.clone());
        }

        let should_restart = match sup.def.restart {
            RestartPolicy::Never => false,
            RestartPolicy::Always => true,
            RestartPolicy::OnFailure => !matches!(status, WaitStatus::Exited(_, 0)),
        };

        if should_restart {
            let mut restart_times = sup.restart_times;
            let now = Instant::now();
            restart_times.retain(|t| now.duration_since(*t) < BACKOFF_WINDOW);
            restart_times.push(now);

            if restart_times.len() > MAX_RESTARTS_IN_WINDOW {
                logging::error(&format!(
                    "service '{}' restarted {} times within {BACKOFF_WINDOW:?}, giving up",
                    sup.def.name,
                    restart_times.len()
                ));
                return Outcome::GaveUp(sup.def.name.clone());
            }
            self.spawn_with_history(sup.def.clone(), restart_times);
        }

        Outcome::Continue
    }

    /// Reconciles the running service set against `new_defs`: stops
    /// services no longer present, restarts ones whose definition
    /// changed, leaves unchanged ones running untouched, and starts
    /// brand-new ones. Returns the names of every service this call
    /// actually started or restarted - `rollback.rs` uses this to scope
    /// its failure watch to only the services a given reload is
    /// responsible for. Deliberately doesn't re-run `topological_order`,
    /// `wait_for_ready`, or `requires=` enforcement - re-establishing
    /// full startup ordering on every reload of an already-running
    /// system would be surprising.
    pub fn reload_services(&mut self, new_defs: &[ServiceDef]) -> Vec<String> {
        let mut touched = Vec::new();

        let removed_pids: Vec<i32> = self
            .services
            .iter()
            .filter(|(_, sup)| !new_defs.iter().any(|d| d.name == sup.def.name))
            .map(|(&pid, _)| pid)
            .collect();
        for pid in removed_pids {
            if let Some(sup) = self.services.remove(&pid) {
                logging::info(&format!(
                    "reload: stopping removed service '{}'",
                    sup.def.name
                ));
                stop_one(pid, &sup.def.name);
                self.ready_state.forget(&sup.def.name);
            }
        }

        for def in new_defs {
            // Collected as owned data so this borrow of self.services
            // doesn't overlap the mutation below.
            let found: Option<(i32, bool)> = self
                .services
                .iter()
                .find(|(_, sup)| sup.def.name == def.name)
                .map(|(&pid, sup)| (pid, defs_equal(&sup.def, def)));

            match found {
                Some((_, true)) => continue, // unchanged, leave it running
                Some((pid, false)) => {
                    logging::info(&format!(
                        "reload: restarting changed service '{}'",
                        def.name
                    ));
                    self.services.remove(&pid);
                    stop_one(pid, &def.name);
                    self.spawn_with_history(def.clone(), Vec::new());
                    touched.push(def.name.clone());
                }
                None => {
                    logging::info(&format!("reload: starting new service '{}'", def.name));
                    self.spawn_with_history(def.clone(), Vec::new());
                    touched.push(def.name.clone());
                }
            }
        }

        touched
    }

    /// Sends SIGTERM to every remaining supervised child, gives them
    /// `grace` to exit on their own, then SIGKILLs whatever's left, then
    /// unconditionally sweeps every service's cgroup - regardless of
    /// whether its tracked pid exited gracefully or had to be SIGKILLed -
    /// since that's the only way to also catch grandchildren a service
    /// forked off that plain pid-based signaling never reaches.
    pub fn shutdown_all(&mut self, grace: Duration) {
        let all_names: Vec<String> = self.services.values().map(|s| s.def.name.clone()).collect();

        for pid in self.services.keys() {
            let _ = kill(Pid::from_raw(*pid), Signal::SIGTERM);
        }

        let deadline = Instant::now() + grace;
        while Instant::now() < deadline && !self.services.is_empty() {
            match nix::sys::wait::waitpid(
                Pid::from_raw(-1),
                Some(nix::sys::wait::WaitPidFlag::WNOHANG),
            ) {
                Ok(WaitStatus::Exited(pid, _)) | Ok(WaitStatus::Signaled(pid, _, _)) => {
                    self.services.remove(&pid.as_raw());
                }
                Ok(WaitStatus::StillAlive) => std::thread::sleep(Duration::from_millis(50)),
                Ok(_) => {}
                Err(nix::errno::Errno::ECHILD) => break, // nothing left to wait for
                Err(_) => std::thread::sleep(Duration::from_millis(50)),
            }
        }

        for pid in self.services.keys() {
            logging::warn(&format!("pid {pid} didn't exit in time, sending SIGKILL"));
            let _ = kill(Pid::from_raw(*pid), Signal::SIGKILL);
        }

        for name in &all_names {
            crate::cgroups::kill_and_remove(name);
            self.ready_state.forget(name);
        }
    }

    pub fn status_summary(&self) -> String {
        if self.services.is_empty() {
            return "no supervised services running".to_string();
        }
        let mut lines = vec![format!("{} supervised service(s):", self.services.len())];
        for (pid, sup) in &self.services {
            let crit = if sup.def.critical { " [critical]" } else { "" };
            let ready = if self.ready_state.is_ready(&sup.def.name) {
                " ready"
            } else {
                " starting"
            };
            lines.push(format!(
                "  pid {pid}: {} ({}){crit}{ready}",
                sup.def.name, sup.def.path
            ));
        }
        lines.join("\n")
    }
}

/// Folds `before=`, `requires=`, and `wants=` into each service's
/// effective `after` list, so `topological_order` (which only knows
/// about `after`) sees the combined ordering constraint:
/// - `Before=X` on service A means A must come before X - injected into
///   X's effective `after` as A.
/// - `Requires=`/`Wants=` both imply ordering the same way an explicit
///   `after=` would; the difference between them (hard vs soft
///   dependency) is enforced separately, in `spawn_all`, not here.
fn effective_after(defs: &[ServiceDef]) -> Vec<ServiceDef> {
    let mut result: Vec<ServiceDef> = defs.to_vec();

    for def in result.iter_mut() {
        for dep in def.requires.iter().chain(def.wants.iter()) {
            if !def.after.contains(dep) {
                def.after.push(dep.clone());
            }
        }
    }

    let before_pairs: Vec<(String, String)> = defs
        .iter()
        .flat_map(|d| {
            d.before
                .iter()
                .map(move |target| (target.clone(), d.name.clone()))
        })
        .collect();
    for (target_name, source_name) in before_pairs {
        if let Some(target) = result.iter_mut().find(|d| d.name == target_name) {
            if !target.after.contains(&source_name) {
                target.after.push(source_name);
            }
        }
    }

    result
}

/// Orders `defs` so each service comes after everything in its `after`
/// list, using Kahn's algorithm. An unresolvable dependency (missing
/// service, or a cycle) doesn't stop boot: the remaining services are
/// just appended in their original order, with a warning, rather than
/// left out or the loop spinning forever.
fn topological_order(defs: &[ServiceDef]) -> Vec<ServiceDef> {
    let names: HashSet<&str> = defs.iter().map(|d| d.name.as_str()).collect();
    let mut remaining: Vec<&ServiceDef> = defs.iter().collect();
    let mut ordered: Vec<ServiceDef> = Vec::with_capacity(defs.len());
    let mut placed: HashSet<String> = HashSet::new();

    while !remaining.is_empty() {
        let ready_idx = remaining.iter().position(|d| {
            d.after
                .iter()
                .all(|dep| placed.contains(dep) || !names.contains(dep.as_str()))
        });

        match ready_idx {
            Some(i) => {
                let d = remaining.remove(i);
                placed.insert(d.name.clone());
                ordered.push(d.clone());
            }
            None => {
                logging::warn(&format!(
                    "dependency cycle among service(s) {:?}, starting in listed order",
                    remaining
                        .iter()
                        .map(|d| d.name.as_str())
                        .collect::<Vec<_>>()
                ));
                for d in remaining.drain(..) {
                    ordered.push(d.clone());
                }
            }
        }
    }

    ordered
}

/// Sends SIGTERM to a single pid, gives it a short grace period, SIGKILLs
/// it if it's still around, then sweeps its cgroup. Used by
/// `reload_services` for services being stopped or replaced outside the
/// full-shutdown path.
fn stop_one(pid: i32, name: &str) {
    let _ = kill(Pid::from_raw(pid), Signal::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if Instant::now() >= deadline {
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
            break;
        }
        match nix::sys::wait::waitpid(
            Pid::from_raw(pid),
            Some(nix::sys::wait::WaitPidFlag::WNOHANG),
        ) {
            Ok(WaitStatus::Exited(_, _)) | Ok(WaitStatus::Signaled(_, _, _)) => break,
            Ok(_) => std::thread::sleep(Duration::from_millis(50)),
            Err(_) => break, // already reaped elsewhere, or no such process
        }
    }
    crate::cgroups::kill_and_remove(name);
}

fn defs_equal(a: &ServiceDef, b: &ServiceDef) -> bool {
    a.path == b.path
        && a.args == b.args
        && a.critical == b.critical
        && a.restart == b.restart
        && a.memory_limit == b.memory_limit
        && a.after == b.after
        && a.after_ready == b.after_ready
        && a.before == b.before
        && a.requires == b.requires
        && a.wants == b.wants
        && a.user == b.user
        && a.group == b.group
        && a.watchdog_timeout == b.watchdog_timeout
}

fn fallback_shell() -> ServiceDef {
    ServiceDef {
        name: "shell".into(),
        path: "/bin/sh".into(),
        args: vec![],
        critical: true,
        restart: RestartPolicy::Never,
        memory_limit: None,
        after: vec![],
        after_ready: vec![],
        before: vec![],
        requires: vec![],
        wants: vec![],
        user: None,
        group: None,
        watchdog_timeout: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn svc(path: &str, mem: Option<u64>) -> ServiceDef {
        ServiceDef {
            name: "x".into(),
            path: path.into(),
            args: vec![],
            critical: false,
            restart: RestartPolicy::Never,
            memory_limit: mem,
            after: vec![],
            after_ready: vec![],
            before: vec![],
            requires: vec![],
            wants: vec![],
            user: None,
            group: None,
            watchdog_timeout: None,
        }
    }

    fn svc_named(name: &str, after: &[&str]) -> ServiceDef {
        ServiceDef {
            name: name.into(),
            path: "/bin/true".into(),
            args: vec![],
            critical: false,
            restart: RestartPolicy::Never,
            memory_limit: None,
            after: after.iter().map(|s| s.to_string()).collect(),
            after_ready: vec![],
            before: vec![],
            requires: vec![],
            wants: vec![],
            user: None,
            group: None,
            watchdog_timeout: None,
        }
    }

    #[test]
    fn identical_defs_are_equal() {
        assert!(defs_equal(&svc("/bin/a", None), &svc("/bin/a", None)));
    }

    #[test]
    fn a_changed_path_is_not_equal() {
        assert!(!defs_equal(&svc("/bin/a", None), &svc("/bin/b", None)));
    }

    #[test]
    fn a_changed_memory_limit_is_not_equal() {
        assert!(!defs_equal(
            &svc("/bin/a", None),
            &svc("/bin/a", Some(1024))
        ));
    }

    #[test]
    fn orders_by_after_dependency() {
        let a = svc_named("a", &[]);
        let b = svc_named("b", &["a"]);
        // Listed out of order on purpose - "a" should still come first.
        let ordered = topological_order(&[b, a]);
        let names: Vec<&str> = ordered.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn a_chain_orders_all_three() {
        let a = svc_named("a", &[]);
        let b = svc_named("b", &["a"]);
        let c = svc_named("c", &["b"]);
        let ordered = topological_order(&[c, a, b]);
        let names: Vec<&str> = ordered.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn an_unknown_dependency_is_treated_as_already_satisfied() {
        let a = svc_named("a", &["nonexistent"]);
        let ordered = topological_order(&[a]);
        assert_eq!(ordered.len(), 1);
    }

    #[test]
    fn breaks_cycles_without_hanging() {
        let a = svc_named("a", &["b"]);
        let b = svc_named("b", &["a"]);
        // The real assertion is that this returns at all.
        let ordered = topological_order(&[a, b]);
        assert_eq!(ordered.len(), 2);
    }

    #[test]
    fn before_folds_into_the_targets_after_list() {
        let mut a = svc_named("a", &[]);
        a.before = vec!["b".to_string()];
        let b = svc_named("b", &[]);

        let ordered = topological_order(&effective_after(&[b, a]));
        let names: Vec<&str> = ordered.iter().map(|d| d.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
    }

    #[test]
    fn requires_and_wants_both_imply_ordering() {
        let mut web = svc_named("web", &[]);
        web.requires = vec!["db".to_string()];
        let mut web2 = svc_named("web2", &[]);
        web2.wants = vec!["cache".to_string()];
        let db = svc_named("db", &[]);
        let cache = svc_named("cache", &[]);

        let ordered = topological_order(&effective_after(&[web, web2, db, cache]));
        let pos = |n: &str| ordered.iter().position(|d| d.name == n).unwrap();
        assert!(pos("db") < pos("web"));
        assert!(pos("cache") < pos("web2"));
    }
}
