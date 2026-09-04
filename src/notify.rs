//! sd_notify-compatible service readiness and watchdog protocol.
//!
//! Real systemd uses one shared datagram socket plus `SCM_CREDENTIALS`
//! ancillary messages to authenticate which process a given message
//! came from - correct, but `recvmsg`/cmsg parsing needs exact
//! alignment/padding that's genuinely easy to get subtly wrong without a
//! compiler to check it against (see how much of this project's riskier
//! FFI work has already needed a CI round-trip to fix).
//!
//! Since every service here already has a unique identity the supervisor
//! assigns it, we sidestep sender authentication entirely: each service
//! gets its *own* notify socket
//! (`/run/mitos-init/notify/<name>.sock`), so the socket a datagram
//! arrived on already says unambiguously which service it's from. The
//! wire format is unchanged - `NOTIFY_SOCKET` env var, `READY=1`/
//! `WATCHDOG=1` payloads - so real systemd-aware daemons calling
//! `sd_notify()` work against this without modification; only our half
//! of the implementation is simpler.

use crate::logging;
use std::collections::{HashMap, HashSet};
use std::ffi::CString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Instant;

const NOTIFY_DIR: &str = "/run/mitos-init/notify";

/// Ready/watchdog state shared between each per-service listener thread
/// and whatever wants to query it (`Supervisor::status_summary`,
/// `Supervisor::expired_watchdogs`).
#[derive(Default)]
pub struct ReadyState {
    ready: Mutex<HashSet<String>>,
    watchdog_pings: Mutex<HashMap<String, Instant>>,
}

impl ReadyState {
    pub fn is_ready(&self, name: &str) -> bool {
        self.ready.lock().map(|s| s.contains(name)).unwrap_or(false)
    }

    /// When `name` last sent `WATCHDOG=1`, if ever.
    pub fn last_ping(&self, name: &str) -> Option<Instant> {
        self.watchdog_pings.lock().ok()?.get(name).copied()
    }

    fn mark_ready(&self, name: &str) {
        if let Ok(mut s) = self.ready.lock() {
            s.insert(name.to_string());
        }
    }

    fn mark_ping(&self, name: &str) {
        if let Ok(mut p) = self.watchdog_pings.lock() {
            p.insert(name.to_string(), Instant::now());
        }
    }

    /// Drops any state for `name` - called when a service is stopped, so
    /// a stale ready/ping record from a previous instance doesn't leak
    /// into a freshly (re)started one under the same name.
    pub fn forget(&self, name: &str) {
        if let Ok(mut s) = self.ready.lock() {
            s.remove(name);
        }
        if let Ok(mut p) = self.watchdog_pings.lock() {
            p.remove(name);
        }
    }
}

/// Creates `name`'s notify socket and spawns a thread listening on it,
/// returning the path to set as `NOTIFY_SOCKET` in that service's
/// environment. `uid`/`gid` should be the identity the service will
/// actually run as (see `users.rs`) - the socket is chowned to match, so
/// a privilege-dropped service can still write to its own socket. Pass
/// `None` for either to leave that half of ownership as root (correct
/// when the service isn't dropping that half of its identity).
///
/// Best-effort: if the socket can't be created, the service just runs
/// without readiness/watchdog tracking - equivalent to how it behaves
/// against any init that doesn't support sd_notify at all, since a
/// well-behaved `sd_notify()` caller treats a missing/unset
/// `NOTIFY_SOCKET` as "notification isn't supported here" and carries on
/// regardless.
pub fn listen_for(name: &str, state: Arc<ReadyState>, uid: Option<u32>, gid: Option<u32>) -> Option<PathBuf> {
    if let Err(e) = std::fs::create_dir_all(NOTIFY_DIR) {
        logging::debug(&format!("couldn't create {NOTIFY_DIR}: {e}"));
        return None;
    }
    let _ = std::fs::set_permissions(NOTIFY_DIR, std::fs::Permissions::from_mode(0o700));

    let path = PathBuf::from(NOTIFY_DIR).join(format!("{name}.sock"));
    let _ = std::fs::remove_file(&path); // stale socket from a previous instance of this service

    let socket = match UnixDatagram::bind(&path) {
        Ok(s) => s,
        Err(e) => {
            logging::debug(&format!("couldn't bind notify socket for '{name}': {e}"));
            return None;
        }
    };
    // Since every service is given its own socket rather than one shared,
    // credential-authenticated socket (see the module doc comment),
    // filesystem permissions are what actually stop a *different* local
    // process from writing a spoofed READY=1/WATCHDOG=1 here. 0600 plus
    // chowning to the service's own uid/gid (when it's running as one,
    // via `user=`/`group=`) covers both "only this service can write
    // here" and "this service actually *can* write here" at once.
    let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    chown_path(&path, uid, gid);

    let thread_name = name.to_string();
    let ret_path = path.clone();
    thread::spawn(move || run(socket, &thread_name, state));
    Some(ret_path)
}

/// `uid`/`gid` of `None` leaves that half of ownership unchanged (passed
/// to `chown(2)` as -1, per POSIX - `u32::MAX` is that same bit pattern
/// reinterpreted as the unsigned `uid_t`/`gid_t` the call actually takes).
fn chown_path(path: &Path, uid: Option<u32>, gid: Option<u32>) {
    if uid.is_none() && gid.is_none() {
        return; // still root-owned, which is already correct for a root-run service
    }
    let Some(path_str) = path.to_str() else { return };
    let Ok(c_path) = CString::new(path_str) else { return };

    let raw_uid = uid.unwrap_or(u32::MAX);
    let raw_gid = gid.unwrap_or(u32::MAX);
    let ret = unsafe { libc::chown(c_path.as_ptr(), raw_uid, raw_gid) };
    if ret != 0 {
        logging::debug(&format!(
            "couldn't chown {}: {}",
            path.display(),
            io::Error::last_os_error()
        ));
    }
}

fn run(socket: UnixDatagram, name: &str, state: Arc<ReadyState>) {
    let mut buf = [0u8; 4096];
    loop {
        let n = match socket.recv(&mut buf) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return, // socket gone (service's cgroup/dir cleaned up) - stop listening
        };
        let Ok(text) = std::str::from_utf8(&buf[..n]) else {
            continue;
        };
        // Real sd_notify payloads can carry several `KEY=VALUE` lines
        // (STATUS=, MAINPID=, ...) - we act on READY=1 and WATCHDOG=1,
        // matching what `status_summary`/`expired_watchdogs` use.
        for line in text.lines() {
            match line {
                "READY=1" => {
                    state.mark_ready(name);
                    logging::debug(&format!("'{name}' reported READY=1"));
                }
                "WATCHDOG=1" => {
                    state.mark_ping(name);
                }
                _ => {}
            }
        }
    }
}
