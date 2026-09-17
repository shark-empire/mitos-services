//! On-demand launching of user applications - as opposed to
//! `supervisor.rs`, which only ever starts the fixed, boot-time list of
//! *services* from `init.conf`/`services.d`. An app is launched once, on
//! request (over the control socket `mitosctl` uses - see `ipc.rs`'s
//! `LAUNCH` command), isn't restarted on exit, isn't ordered against
//! anything, and gets its own cgroup subtree
//! (`/sys/fs/cgroup/mitos-init/apps/<id>/`, kept separate from services'
//! own leaf cgroups directly under that root) so a runaway app can never
//! be confused for, or torn down alongside, a system service.
//!
//! This is the "launch every third-party app inside its own sandbox"
//! piece of MITOS's permission-model design - originally sketched as
//! part of mitos-init, moved here instead because the primitives it
//! needs (cgroups, privilege dropping, process reaping, a control
//! socket) already exist and are already exercised here for services;
//! see the crate README for why mitos-init itself stays as small as
//! possible instead of growing this directly.
//!
//! What's actually enforced (defense in depth, not a policy engine - see
//! "What this doesn't do" below) - see `sandbox.rs` and `seccomp.rs` for
//! the mechanics of each:
//! - A fresh mount, UTS, and IPC namespace per app, plus a private,
//!   size-capped tmpfs over `/tmp` inside it.
//! - `PR_SET_NO_NEW_PRIVS` and the entire capability bounding set
//!   dropped.
//! - A conservative seccomp-bpf syscall deny-list - **manually reviewed
//!   against the documented kernel ABI, not yet run on real hardware or
//!   a real kernel: see `seccomp.rs`'s module doc before depending on
//!   it**.
//! - A cgroup (reusing `cgroups.rs`'s primitives against a nested root)
//!   so the entire process tree the app spawns is trackable and
//!   killable as a unit, and can carry a `MemoryMax=`-style limit the
//!   same way a service's can.
//! - Identity: a generated app ID plus a SHA-256 hash of the exact
//!   binary launched, recorded in the registry (`mitosctl apps`) and
//!   returned to the caller.
//!
//! What's deliberately NOT here (flagged, not silently missing - see
//! `sandbox.rs`'s module doc for the fuller version):
//! - **PID namespace isolation.** Getting a launched app to land as PID
//!   1 of its own PID namespace needs an extra fork partway through
//!   `pre_exec` (`unshare(CLONE_NEWPID)` only affects a *future*
//!   `fork()` from the calling process, not the process that goes on to
//!   `exec()` right after it) - real, working complexity this project's
//!   own established caution around risky FFI (see `notify.rs`'s module
//!   doc) argues against shipping without a CI/hardware round-trip to
//!   check it against first. Process-tree teardown already works
//!   without it, via the cgroup above.
//! - **Any actual policy decision.** This module answers "how do I
//!   sandbox and launch a binary", never "should this app be allowed to
//!   run at all, or to do X once running" - there's no rulebook, no risk
//!   classification, no password prompt here. That's the separate
//!   policy daemon MITOS's permission design calls `mitos-service`
//!   (singular) - a different, not-yet-built project this repo
//!   deliberately has no dependency on. `authorize()` below is the seam
//!   for it: today it always allows; wiring it up later is a matter of
//!   having it make one IPC call there instead.

use crate::cgroups;
use crate::logging;
use crate::sandbox::Sandbox;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::os::unix::process::CommandExt;
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// Nested under the services' own already-delegated cgroup root (see
/// `cgroups::prepare_intermediate`'s doc comment) rather than a sibling
/// of it, so apps get real cgroup v2 memory-controller support without
/// mitos-init needing any change to delegate a second root.
const APPS_CGROUP_ROOT: &str = "/sys/fs/cgroup/mitos-init/apps";

pub struct AppInfo {
    pub id: String,
    pub path: String,
    pub args: Vec<String>,
    pub pid: i32,
    pub sha256: String,
}

#[derive(Default)]
struct Registry {
    by_pid: HashMap<i32, AppInfo>,
}

static REGISTRY: OnceLock<Mutex<Registry>> = OnceLock::new();
static NEXT_SEQ: AtomicU64 = AtomicU64::new(1);

fn registry() -> &'static Mutex<Registry> {
    REGISTRY.get_or_init(|| Mutex::new(Registry::default()))
}

/// The seam for MITOS's future policy daemon - see the module doc's
/// "What this doesn't do". Always allows; nothing in this repo overrides
/// it.
/// The seam for MITOS's policy daemon, `mitos-service` (see its own
/// repository) - asks it whether this app is allowed to launch at all,
/// under the fixed capability name `app_launch`.
///
/// Two different kinds of "no" here, treated differently on purpose:
/// - **mitos-service isn't reachable at all** (connection refused, no
///   such socket) - treated as `Ok`, allowing the launch. This is a
///   deliberate, temporary bootstrapping exception to the MITOS
///   permissions design's own "fail closed" rule: mitos-service is a
///   separate, independently-deployed component that doesn't exist on
///   every system yet, and failing every app launch closed by default
///   before it's even a normal part of a MITOS install would make this
///   whole feature non-functional out of the box. Revisit this once
///   mitos-service is a standard part of every deployment.
/// - **mitos-service responds with `DENY` or `ASK`** (i.e. it's running
///   and has an opinion, even an unresolved one) - treated as `Err`,
///   failing closed. The daemon being present and undecided is a real
///   policy signal, not an infrastructure gap, so the golden rule
///   applies here without exception.
fn authorize(path: &str, sha256: &str) -> Result<(), String> {
    use std::io::{Read, Write};
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    const SOCKET_PATH: &str = "/run/mitos-service/control.sock";
    // Short on purpose: ipc.rs's own listener handles one connection at
    // a time on a single thread (see its module doc), so this call
    // blocking is this crate's *entire* control socket blocking, not
    // just this one LAUNCH - a local Unix socket round trip to a
    // running daemon should be low milliseconds, so this is generous
    // for the happy path and still bounds the worst case (mitos-service
    // running but stuck) to something short.
    const TIMEOUT: Duration = Duration::from_millis(500);

    let mut stream = match UnixStream::connect(SOCKET_PATH) {
        Ok(s) => s,
        Err(_) => return Ok(()), // not deployed on this system yet - see above
    };
    let _ = stream.set_read_timeout(Some(TIMEOUT));
    let _ = stream.set_write_timeout(Some(TIMEOUT));

    if stream
        .write_all(format!("CHECK {sha256} app_launch\n").as_bytes())
        .is_err()
    {
        // Reachable a moment ago but broke mid-request - same
        // reasoning as an outright connection failure: don't block a
        // launch on this daemon's own hiccup.
        return Ok(());
    }
    let mut response = String::new();
    if stream.read_to_string(&mut response).is_err() {
        return Ok(());
    }

    let response = response.trim();
    if response == "ALLOW" {
        Ok(())
    } else if response == "DENY" || response.starts_with("ASK") {
        logging::info(&format!(
            "app launch for '{path}' blocked by mitos-service: {response}"
        ));
        Err(format!("not permitted by mitos-service ({response})"))
    } else {
        // An unrecognized response shouldn't be trusted either way -
        // treat it the same as DENY/ASK (fail closed), not as ALLOW.
        logging::warn(&format!(
            "mitos-service gave an unrecognized CHECK response '{response}', denying"
        ));
        Err(format!("unrecognized response from mitos-service: {response}"))
    }
}

/// SHA-256 of the file at `path`, hex-encoded. Hashing the exact bytes
/// about to be exec'd (rather than, say, a manifest) is what makes the
/// recorded identity trustworthy - it can't drift from what actually ran.
fn hash_file(path: &str) -> Result<String, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("couldn't read '{path}': {e}"))?;
    let digest = Sha256::digest(&bytes);
    Ok(digest.iter().map(|b| format!("{b:02x}")).collect())
}

fn next_id() -> String {
    let seq = NEXT_SEQ.fetch_add(1, Ordering::SeqCst);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("app-{now:x}-{seq:x}")
}

/// Launches `path` with `args`, sandboxed as described in the module
/// doc. Returns the new app's id (which `mitosctl apps`/the `APPS` IPC
/// command look up later), or an error string suitable for sending back
/// over the control socket as-is.
pub fn launch(path: &str, args: &[String]) -> Result<String, String> {
    let sha256 = hash_file(path)?;
    authorize(path, &sha256)?;
    let id = next_id();

    // Best-effort, same as every other cgroup operation in this crate -
    // an app still launches (just without cgroup-backed teardown/limits)
    // if this isn't available, matching how services degrade when
    // `cgroups::available()` is false.
    cgroups::prepare_intermediate(APPS_CGROUP_ROOT);

    // Built in this thread (the IPC listener thread - see `ipc.rs`), not
    // inside `pre_exec`, so the SHA-256 read above and this precompute
    // are the only allocation this launch does before `fork()` - see
    // `sandbox.rs`'s module doc for why that split matters.
    let sandbox = crate::sandbox::prepare();

    let mut cmd = Command::new(path);
    cmd.args(args);
    cmd.env_clear();
    cmd.env("MITOS_APP_ID", &id);
    cmd.env(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    if let Ok(term) = std::env::var("TERM") {
        cmd.env("TERM", term);
    }
    apply_sandbox_on_exec(&mut cmd, sandbox);

    match cmd.spawn() {
        Ok(child) => {
            let pid = child.id() as i32;
            cgroups::create_under(APPS_CGROUP_ROOT, &id, None);
            cgroups::attach_under(APPS_CGROUP_ROOT, &id, pid);
            logging::info(&format!(
                "launched app '{id}' ({path}) as pid {pid}, sha256:{sha256}"
            ));
            if let Ok(mut reg) = registry().lock() {
                reg.by_pid.insert(
                    pid,
                    AppInfo {
                        id: id.clone(),
                        path: path.to_string(),
                        args: args.to_vec(),
                        pid,
                        sha256,
                    },
                );
            }
            // std::mem::forget the child handle rather than dropping it:
            // dropping a std::process::Child does NOT reap it (unlike
            // some other languages' process handles) - it's already
            // reaped through the normal waitpid(-1, ...) path in
            // main.rs's event loop, same as every service. Explicit here
            // because getting this wrong (assuming Drop reaps) is an easy
            // mistake to carry over from other languages.
            drop(child);
            Ok(id)
        }
        Err(e) => Err(format!("couldn't launch '{path}': {e}")),
    }
}

/// # Safety note
/// `pre_exec`'s closure runs after `fork()` in the child, before `exec()`
/// - see `sandbox.rs`'s module doc for what is and isn't safe to do
///   there. Everything the closure touches (`sandbox`) was already fully
///   built in the parent before this call.
fn apply_sandbox_on_exec(cmd: &mut Command, sandbox: Sandbox) {
    unsafe {
        cmd.pre_exec(move || sandbox.apply());
    }
}

// NOTE for reviewers: the closure above calls `Sandbox::apply`, itself
// `unsafe fn`; that call is soundly reached through the `unsafe { }`
// block wrapping `pre_exec` for the same reason `main.rs::become_subreaper`
// needs no separate inner block around its own single `libc::prctl` call
// - a closure literal with no intervening item boundary is part of the
// same unsafe scope as the block it's written in. If a future edit adds
// a second unsafe call at a different nesting level, re-check this.

/// Called from the main event loop for every exited pid, before falling
/// through to `Supervisor::handle_exit` - see `main.rs`. Returns whether
/// `pid` was a tracked app (and if so, has already logged and cleaned it
/// up); `false` means the caller should treat the exit as a normal
/// service/orphan exit instead.
pub fn reap(pid: i32) -> bool {
    let Ok(mut reg) = registry().lock() else {
        return false;
    };
    let Some(info) = reg.by_pid.remove(&pid) else {
        return false;
    };
    drop(reg);
    logging::info(&format!("app '{}' ({}) exited", info.id, info.path));
    cgroups::kill_and_remove_under(APPS_CGROUP_ROOT, &info.id);
    true
}

/// Human-readable listing for `mitosctl apps` / the `APPS` IPC command.
pub fn list() -> String {
    let Ok(reg) = registry().lock() else {
        return "app registry unavailable".to_string();
    };
    if reg.by_pid.is_empty() {
        return "no running apps".to_string();
    }
    let mut lines = vec![format!("{} running app(s):", reg.by_pid.len())];
    for info in reg.by_pid.values() {
        lines.push(format!(
            "  {} pid {}: {} {} (sha256:{})",
            info.id,
            info.pid,
            info.path,
            info.args.join(" "),
            info.sha256
        ));
    }
    lines.join("\n")
}
