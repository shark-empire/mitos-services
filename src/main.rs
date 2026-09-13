//! mitos-services — MITOS's service manager.
//!
//! Runs as a supervised child of mitos-init (PID 1), not PID 1 itself.
//! All the service-management complexity that used to live directly in
//! PID 1 - config/unit parsing, dependency ordering, cgroups, the
//! readiness protocol, transactional reload, IPC - lives here instead,
//! so a bug in any of it can only crash this process (which mitos-init
//! then restarts, with backoff), not kernel-panic the whole machine.
//! See INTEGRATION.md (mitos-init) for the split's full rationale.

mod apps;
mod cgroups;
mod config;
mod error;
mod ipc;
mod logging;
mod notify;
mod rollback;
mod sandbox;
mod seccomp;
mod signals;
mod supervisor;
mod targets;
mod timers;
mod units;
mod users;

use nix::sys::signal::{kill, Signal};
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use nix::unistd::Pid;
use std::io::Write;
use std::sync::atomic::Ordering;
use std::time::Duration;
use supervisor::{Outcome, Supervisor};

const CONFIG_PATH: &str = "/etc/mitos/init.conf";
const SHUTDOWN_ACK_FIFO: &str = "/run/mitos-init/shutdown-ack";

fn main() {
    logging::init();
    install_panic_hook();
    logging::info("mitos-services starting");

    become_subreaper();

    if !cgroups::available() {
        logging::warn(
            "cgroup delegation from mitos-init unavailable - falling back to plain pid-based supervision only",
        );
    }

    let mut cfg = config::load_or_default(CONFIG_PATH);
    cfg.services = config::merge_services(cfg.services, units::load_all());
    logging::set_level(cfg.loglevel);

    if let Some(hostname) = &cfg.hostname {
        if let Err(e) = nix::unistd::sethostname(hostname) {
            logging::warn(&format!("failed to set hostname to '{hostname}': {e}"));
        }
    }

    if let Err(e) = signals::install_handlers() {
        logging::error(&format!("failed to install signal handlers: {e}"));
    }
    // Same reasoning as mitos-init: block the relayed signals on this
    // thread before spawning any workers, so delivery can't land on one
    // of them instead of the thread actually waiting on it.
    if let Err(e) = signals::block_handled() {
        logging::warn(&format!(
            "failed to block signals ahead of worker threads: {e}"
        ));
    }

    ipc::spawn_listener();

    let current_target = cfg.default_target.clone();
    ipc::publish_targets(&current_target, &targets::list(&cfg.services));
    let timer_defs = timers::load_all();

    let mut sup = Supervisor::new();
    sup.spawn_all(&targets::services_in(&cfg.services, &current_target));

    if let Err(e) = signals::unblock_handled() {
        logging::error(&format!("failed to unblock signals: {e}"));
    }

    let grace = Duration::from_secs(cfg.shutdown_timeout_secs);
    run_event_loop(&mut sup, &mut cfg, current_target, timer_defs, grace);
}

/// So orphaned grandchildren of a service (not just its direct child)
/// reparent to *this* process instead of skipping past it to mitos-init
/// (PID 1) - without this, the process-tree isolation the cgroup split
/// is supposed to provide would be cosmetic only.
fn become_subreaper() {
    let ret = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1, 0, 0, 0) };
    if ret != 0 {
        logging::warn(&format!(
            "couldn't become a child subreaper: {} - orphaned grandchildren of services may \
             reparent to mitos-init instead of here",
            std::io::Error::last_os_error()
        ));
    }
}

/// Mirrors mitos-init's own event loop shape (see its `main.rs`) almost
/// exactly - the difference is what happens at the ends: a shutdown
/// signal here means "stop services and tell mitos-init it's safe to
/// proceed" (`acknowledge_shutdown`), not "call reboot(2)" - only
/// mitos-init does that.
fn run_event_loop(
    sup: &mut Supervisor,
    cfg: &mut config::Config,
    mut current_target: String,
    mut timer_defs: Vec<timers::TimerDef>,
    shutdown_grace: Duration,
) {
    let mut watch: Option<rollback::Watch> = None;

    loop {
        // Evaluated unconditionally (not with `||` short-circuiting) so
        // every flag actually gets cleared, regardless of which one(s)
        // are set.
        let reboot = signals::REBOOT_REQUESTED.swap(false, Ordering::SeqCst);
        let poweroff = signals::POWEROFF_REQUESTED.swap(false, Ordering::SeqCst);
        let halt = signals::HALT_REQUESTED.swap(false, Ordering::SeqCst);
        if reboot || poweroff || halt {
            logging::info("shutdown relayed from mitos-init - stopping services");
            sup.shutdown_all(shutdown_grace);
            acknowledge_shutdown();
            return;
        }

        if signals::RELOAD_REQUESTED.swap(false, Ordering::SeqCst) {
            logging::info("reloading config");
            let mut new_cfg = config::load_or_default(CONFIG_PATH);
            new_cfg.services = config::merge_services(new_cfg.services, units::load_all());
            logging::set_level(new_cfg.loglevel);
            if let Some(h) = &new_cfg.hostname {
                if let Err(e) = nix::unistd::sethostname(h) {
                    logging::warn(&format!("failed to apply reloaded hostname: {e}"));
                }
            }
            timer_defs = timers::load_all();
            let previous = cfg.clone();
            watch = Some(rollback::begin(sup, &new_cfg, previous, &current_target));
            *cfg = new_cfg;
            ipc::publish_targets(&current_target, &targets::list(&cfg.services));
        }

        if let Some(name) = ipc::take_pending_isolate() {
            let available = targets::list(&cfg.services);
            if !available.iter().any(|t| t == &name) {
                logging::warn(&format!(
                    "isolate: no services configured for target '{name}', ignoring"
                ));
            } else {
                logging::info(&format!("isolating to target '{name}'"));
                let subset = targets::services_in(&cfg.services, &name);
                sup.reload_services(&subset);
                current_target = name;
                ipc::publish_targets(&current_target, &available);
            }
        }

        if signals::STATUS_DUMP_REQUESTED.swap(false, Ordering::SeqCst) {
            let summary = format!("active target: {current_target}\n{}", sup.status_summary());
            logging::info(&summary);
            ipc::publish_status(&summary);
        }

        timers::tick(&timer_defs, &cfg.services);

        // Poll instead of blocking whenever there's something that needs
        // periodic (not just event-driven) checking: a reload watch's
        // deadline, a service's watchdog deadline, or any timer at all
        // (even an idle one still needs noticing once it becomes due -
        // see `timers::has_any`).
        let polling =
            watch.is_some() || sup.has_watchdog_services() || timers::has_any(&timer_defs);

        let wait_result = if polling {
            waitpid(Pid::from_raw(-1), Some(WaitPidFlag::WNOHANG))
        } else {
            waitpid(Pid::from_raw(-1), None)
        };

        for pid in sup.expired_watchdogs() {
            logging::error(&format!(
                "pid {pid} missed its watchdog deadline, killing it"
            ));
            let _ = kill(Pid::from_raw(pid), Signal::SIGKILL);
        }

        match wait_result {
            Ok(status) => {
                let mut failed_name: Option<String> = None;

                if matches!(status, WaitStatus::StillAlive) {
                    if polling {
                        std::thread::sleep(Duration::from_millis(100));
                    }
                } else {
                    // A timer's one-shot run or an on-demand app - see
                    // `timers.rs`/`apps.rs` - is never one of
                    // `Supervisor`'s tracked services, so both get first
                    // refusal at recognizing this exit before it falls
                    // through to `handle_exit` as an ordinary service
                    // (or genuinely-unknown-orphan) exit.
                    let exited_pid = match status {
                        WaitStatus::Exited(p, _) | WaitStatus::Signaled(p, _, _) => {
                            Some(p.as_raw())
                        }
                        _ => None,
                    };
                    let already_handled = exited_pid
                        .map(|pid| timers::reap(pid) || apps::reap(pid))
                        .unwrap_or(false);

                    if !already_handled {
                        match sup.handle_exit(status) {
                            Outcome::Halt(name) => {
                                let touched = watch.as_ref().is_some_and(|w| w.touched(&name));
                                if touched {
                                    failed_name = Some(name);
                                } else {
                                    logging::info(
                                        "critical service exited, stopping mitos-services",
                                    );
                                    sup.shutdown_all(shutdown_grace);
                                    acknowledge_shutdown();
                                    return;
                                }
                            }
                            Outcome::GaveUp(name) => failed_name = Some(name),
                            Outcome::Continue => {}
                        }
                    }
                }

                if let Some(w) = watch.take() {
                    watch = rollback::check(sup, cfg, w, failed_name.as_deref());
                }
            }
            Err(nix::errno::Errno::EINTR) => continue,
            Err(nix::errno::Errno::ECHILD) => {
                logging::warn("no children left to wait for");
                std::thread::sleep(Duration::from_secs(1));
            }
            Err(e) => logging::error(&format!("waitpid failed: {e}")),
        }
    }
}

/// Writes to the shutdown-ack FIFO mitos-init created, telling it it's
/// safe to actually reboot/power off/halt now that services are
/// stopped. Best-effort: if mitos-init couldn't create the FIFO (or this
/// isn't running under mitos-init at all - e.g. manual testing),
/// mitos-init just proceeds on its own fixed timeout instead of waiting
/// forever.
fn acknowledge_shutdown() {
    match std::fs::OpenOptions::new()
        .write(true)
        .open(SHUTDOWN_ACK_FIFO)
    {
        Ok(mut f) => {
            let _ = f.write_all(b"\n");
        }
        Err(e) => logging::debug(&format!(
            "couldn't open {SHUTDOWN_ACK_FIFO} to acknowledge shutdown: {e}"
        )),
    }
}

/// Same rationale as mitos-init's own hook: makes an unexpected panic
/// visible in the log instead of a possibly-unread stderr write. Unlike
/// mitos-init, a panic here doesn't kernel-panic the machine - that's
/// the entire point of this process existing separately - but it's
/// still worth knowing about.
fn install_panic_hook() {
    std::panic::set_hook(Box::new(|info| {
        logging::error(&format!("PANIC: {info}"));
    }));
}
