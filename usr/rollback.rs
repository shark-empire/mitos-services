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

use crate::config::Config;
use crate::logging;
use crate::supervisor::Supervisor;
use std::time::{Duration, Instant};

/// How long after a reload a touched service's hard failure still counts
/// as "this reload broke it" rather than an unrelated later problem.
const WATCH_WINDOW: Duration = Duration::from_secs(10);

pub struct Watch {
    deadline: Instant,
    touched: Vec<String>,
    previous: Config,
}

impl Watch {
    fn active(&self) -> bool {
        Instant::now() < self.deadline
    }

    pub fn touched(&self, name: &str) -> bool {
        self.touched.iter().any(|n| n == name)
    }
}

/// Applies `new_cfg`'s services against `sup` and starts watching the
/// result. `previous` is what gets restored if this reload turns out bad.
pub fn begin(sup: &mut Supervisor, new_cfg: &Config, previous: Config) -> Watch {
    let touched = sup.reload_services(&new_cfg.services);
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
            sup.reload_services(&watch.previous.services);
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
