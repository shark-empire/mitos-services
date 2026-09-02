//! Signal handling for mitos-services.
//!
//! Unlike mitos-init (PID 1), this process doesn't receive
//! SIGTERM/SIGINT/SIGQUIT directly from the kernel or from
//! `reboot`/`poweroff`/`halt`/`shutdown` - those still target PID 1 as
//! before. mitos-init *relays* the same signals here once it decides a
//! shutdown is happening (see mitos-init's `main.rs`), and this process
//! reacts to them exactly the way it always did: stop services, then
//! (via `acknowledge_shutdown`) tell mitos-init it's safe to actually
//! reboot/power off/halt. SIGUSR1/SIGUSR2 (reload/status) are relayed
//! the same way, and can also be triggered via `ipc.rs`'s control
//! socket, which just sets the same atomic flags below - one code path
//! regardless of which way a request arrived.
//!
//! The handlers below only touch `AtomicBool`s — no allocation, no I/O —
//! which is about the only thing that's safe to do inside a signal
//! handler.
//!
//! Three shutdown-family signals map to three distinct end states,
//! matching the classic sysvinit convention mitos-init's own relay
//! preserves: SIGINT means reboot, SIGTERM means power off, SIGQUIT
//! means halt without powering off - though this process itself treats
//! all three identically (stop services, acknowledge) since which final
//! action mitos-init takes isn't this process's concern.

use crate::error::{InitError, Result};
use nix::sys::signal::{self, SaFlags, SigAction, SigHandler, SigSet, SigmaskHow, Signal};
use std::sync::atomic::AtomicBool;

/// SIGINT - matches the kernel's Ctrl-Alt-Del convention.
pub static REBOOT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// SIGTERM - general graceful shutdown.
pub static POWEROFF_REQUESTED: AtomicBool = AtomicBool::new(false);
/// SIGQUIT - halt without cutting power.
pub static HALT_REQUESTED: AtomicBool = AtomicBool::new(false);
/// SIGUSR1: reload `/etc/mitos/init.conf` (log level, hostname) without a reboot.
pub static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);
/// SIGUSR2: dump a one-line status summary of supervised services to the log.
pub static STATUS_DUMP_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(raw: i32) {
    use std::sync::atomic::Ordering::SeqCst;
    if raw == Signal::SIGINT as i32 {
        REBOOT_REQUESTED.store(true, SeqCst);
    } else if raw == Signal::SIGTERM as i32 {
        POWEROFF_REQUESTED.store(true, SeqCst);
    } else if raw == Signal::SIGQUIT as i32 {
        HALT_REQUESTED.store(true, SeqCst);
    } else if raw == Signal::SIGUSR1 as i32 {
        RELOAD_REQUESTED.store(true, SeqCst);
    } else if raw == Signal::SIGUSR2 as i32 {
        STATUS_DUMP_REQUESTED.store(true, SeqCst);
    }
    // SIGCHLD is intentionally left at its default disposition: the main
    // loop already reaps everything via a blocking waitpid(), so a custom
    // handler would only add signal-safety risk for no benefit.
}

/// Installs handlers for the signals PID 1 needs to care about.
///
/// Deliberately does *not* set `SA_RESTART`: we want a blocked `waitpid()`
/// in the main loop to return `EINTR` when one of these arrives, so the
/// loop promptly checks the flags above instead of waiting for the next
/// child to exit first.
pub fn install_handlers() -> Result<()> {
    let action = SigAction::new(
        SigHandler::Handler(on_signal),
        SaFlags::empty(),
        SigSet::empty(),
    );
    unsafe {
        signal::sigaction(Signal::SIGTERM, &action).map_err(InitError::Signal)?;
        signal::sigaction(Signal::SIGINT, &action).map_err(InitError::Signal)?;
        signal::sigaction(Signal::SIGQUIT, &action).map_err(InitError::Signal)?;
        signal::sigaction(Signal::SIGUSR1, &action).map_err(InitError::Signal)?;
        signal::sigaction(Signal::SIGUSR2, &action).map_err(InitError::Signal)?;
    }
    Ok(())
}

fn handled_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    set.add(Signal::SIGQUIT);
    set.add(Signal::SIGUSR1);
    set.add(Signal::SIGUSR2);
    set
}

/// Blocks the signals we handle on the *calling* thread only. A signal
/// delivered to a multi-threaded process goes to an arbitrary thread that
/// isn't blocking it - so once worker threads exist (e.g. the hotplug
/// listener), an unlucky delivery to one of them would flip our atomic
/// flags without ever unblocking the main thread's `waitpid()`, which is
/// the only place that acts on them. Call this on the main thread *before*
/// spawning any worker threads - they inherit the blocked mask at spawn
/// time - then call `unblock_handled` on the main thread only, right
/// before entering the event loop, so delivery is guaranteed to land
/// there.
pub fn block_handled() -> Result<()> {
    signal::pthread_sigmask(SigmaskHow::SIG_BLOCK, Some(&handled_set()), None)
        .map_err(InitError::Signal)
}

pub fn unblock_handled() -> Result<()> {
    signal::pthread_sigmask(SigmaskHow::SIG_UNBLOCK, Some(&handled_set()), None)
        .map_err(InitError::Signal)
}
