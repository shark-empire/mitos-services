//! Transactional config reload.
//!
//! SIGUSR1 already reconciles the running service set against a
//! freshly-loaded config (`Supervisor::reload_services`) - this module
//! adds the transactional part: after a reload, watch the services it
//! actually touched for a short window. If one of them dies for good (a
//! critical exit, or its restart budget runs out) inside that window, the
//! reload is judged bad and gets automatically reverted to whatever
//! config was running before it. Same idea NixOS/ostree apply at the
//! OS-image layer - applied here at the service-supervision layer instead.
//!
//! The watch needs *something* to wake the main loop up while nothing is
//! otherwise happening, since the deadline itself has to be noticed even
//! if no child exits before it passes. `main.rs` handles that by polling
//! (`waitpid` with `WNOHANG` plus a short sleep) instead of blocking
//! indefinitely, but only while a watch is active - the rest of the time
//! the loop is back to a fully blocking, zero-poll wait.
//!
//! Reload/rollback both only ever touch whichever target (see
//! `targets.rs`) was active when the watch began - snapshotted into
//! `target` below - not every configured service across every target.
//! **Known gap:** if `mitosctl isolate` runs while a watch from an
//! *earlier* reload is still active, that watch's eventual rollback (if
//! it ends up triggering) reconciles back against its own snapshotted
//! target's service set, which un-does the isolate too. Narrow window
//! (the default watch is ten seconds) and a rare thing to do twice in a
//! row, but worth knowing about rather than silently surprising - a real
//! fix would need `check` to tell its caller "I rolled back" distinctly
//! from "confirmed good" so `main.rs` could resync `current_target`.

use crate::config::Config;
use crate::logging;
use crate::supervisor::Supervisor;
use crate::targets;
use std::time::{Duration, Instant};

/// How long after a reload a touched service's hard failure still counts
/// as "this reload broke it" rather than an unrelated later problem.
const WATCH_WINDOW: Duration = Duration::from_secs(10);

pub struct Watch {
    deadline: Instant,
    touched: Vec<String>,
    previous: Config,
    target: String,
}

impl Watch {
    fn active(&self) -> bool {
        Instant::now() < self.deadline
    }

    pub fn touched(&self, name: &str) -> bool {
        self.touched.iter().any(|n| n == name)
    }
}

/// Applies `new_cfg`'s services belonging to `target` against `sup` and
/// starts watching the result. `previous` is what gets restored (its own
/// `target`-filtered subset, not the whole thing) if this reload turns
/// out bad.
pub fn begin(sup: &mut Supervisor, new_cfg: &Config, previous: Config, target: &str) -> Watch {
    let subset = targets::services_in(&new_cfg.services, target);
    let touched = sup.reload_services(&subset);
    if touched.is_empty() {
        logging::info("reload: no service changes to apply");
    } else {
        logging::info(&format!(
            "reload: watching {} changed service(s) for {WATCH_WINDOW:?} before confirming",
            touched.len()
        ));
    }
    Watch {
        deadline: Instant::now() + WATCH_WINDOW,
        touched,
        previous,
        target: target.to_string(),
    }
}

/// Judges the current watch: pass `failed_name` when a service just died
/// for good (whether or not it's actually one of the watched ones -
/// that's checked here), or `None` for a periodic "has the window expired
/// yet?" check. Returns `None` once the watch is resolved (confirmed good,
/// or rolled back) - the caller drops it in that case; `Some(watch)` to
/// keep watching otherwise.
pub fn check(
    sup: &mut Supervisor,
    cfg: &mut Config,
    watch: Watch,
    failed_name: Option<&str>,
) -> Option<Watch> {
    if let Some(name) = failed_name {
        if watch.touched(name) {
            logging::error(&format!(
                "reload: '{name}' failed within the watch window, rolling back"
            ));
            let subset = targets::services_in(&watch.previous.services, &watch.target);
            sup.reload_services(&subset);
            logging::set_level(watch.previous.loglevel);
            if let Some(h) = &watch.previous.hostname {
                let _ = nix::unistd::sethostname(h);
            }
            logging::info("reload: rolled back to the previous config");
            *cfg = watch.previous;
            return None;
        }
    }

    if !watch.active() {
        logging::info("reload: no failures in the watch window, keeping the new config");
        return None;
    }

    Some(watch)
}
