//! Per-service cgroup v2 management: one cgroup per supervised service,
//! under `/sys/fs/cgroup/mitos-init/<name>/`.
//!
//! Mounting cgroup2 and enabling the "memory" controller for this
//! subtree (`cgroup.subtree_control`) both happen in mitos-init, before
//! mitos-services even starts - see `mount::setup_service_cgroup_root`
//! there. That's a deliberate split: a cgroup only grants a controller
//! to its children once the controller is listed in the cgroup's own
//! subtree_control, and doing that from mitos-init (closer to the real
//! root cgroup) avoids mitos-services needing any special delegated
//! permission to reach up past its own parent directory. Everything
//! here just creates/manages *leaf* cgroups underneath that already-
//! prepared parent.
//!
//! This closes a real correctness gap plain PID-based supervision has:
//! `supervisor.rs` only ever tracks the *one* pid a service was spawned
//! as. If that process forks its own children, those children are
//! invisible to the supervisor - reparented to mitos-services (see
//! `main.rs`'s `PR_SET_CHILD_SUBREAPER` setup) as ordinary orphans,
//! untracked by any service, and critically: never torn down when that
//! service is stopped, since `kill()` on the one tracked pid doesn't
//! touch them. A cgroup lets us track and kill the *whole* tree
//! atomically via `cgroup.kill`, instead of just the pid we directly
//! spawned. It's also where `memory_limit` (see `config.rs`/`units.rs`)
//! gets enforced.
//!
//! Everything here is plain file I/O against cgroupfs (`mkdir`, `write`,
//! `rmdir`) - no ioctls, no hand-rolled structs, nothing that needs libc
//! FFI to get right.

use crate::logging;
use std::fs;
use std::path::PathBuf;

const CGROUP_ROOT: &str = "/sys/fs/cgroup/mitos-init";

/// True if mitos-init's `setup_service_cgroup_root` already ran and this
/// directory exists. Checked once at startup so mitos-services can log a
/// clear "running without cgroup support" warning up front, rather than
/// a stream of individual per-service debug lines that all boil down to
/// the same root cause.
pub fn available() -> bool {
    fs::metadata(CGROUP_ROOT).is_ok()
}

fn service_dir(name: &str) -> PathBuf {
    PathBuf::from(CGROUP_ROOT).join(name)
}

/// Creates (or reuses) the cgroup for `name` and applies `memory_limit`
/// (bytes) to it if given. Returns whether the cgroup is usable - callers
/// don't need to branch on this themselves, since `attach`/`kill_and_remove`
/// already no-op harmlessly when there's no cgroup to act on.
pub fn create_for(name: &str, memory_limit: Option<u64>) -> bool {
    let dir = service_dir(name);
    if let Err(e) = fs::create_dir_all(&dir) {
        logging::debug(&format!(
            "cgroup for '{name}': couldn't create {}: {e}",
            dir.display()
        ));
        return false;
    }
    if let Some(limit) = memory_limit {
        if fs::write(dir.join("memory.max"), limit.to_string()).is_err() {
            logging::debug(&format!("cgroup for '{name}': couldn't set memory.max"));
        }
    }
    true
}

/// Moves `pid` into `name`'s cgroup. Call this right after spawning -
/// there's no "spawn directly into a cgroup" primitive on Linux without a
/// helper like `systemd-run`, so moving a freshly-spawned process in
/// immediately afterward is the normal pattern.
pub fn attach(name: &str, pid: i32) {
    let path = service_dir(name).join("cgroup.procs");
    if let Err(e) = fs::write(&path, pid.to_string()) {
        logging::debug(&format!(
            "couldn't attach pid {pid} to cgroup '{name}': {e}"
        ));
    }
}

/// Kills every process in `name`'s cgroup - including grandchildren the
/// supervisor never directly tracked - then removes the (now-empty)
/// cgroup directory. This is the step plain `kill()` on a single tracked
/// pid can't do.
pub fn kill_and_remove(name: &str) {
    let dir = service_dir(name);
    if fs::write(dir.join("cgroup.kill"), "1").is_err() {
        return; // no cgroup for this service - not set up, or already gone
    }
    let _ = fs::remove_dir(&dir);
}

/// Parses a size like `256M`, `1G`, `512Ki`, or a bare byte count. Binary
/// (1024-based) multiples throughout, for predictability - not systemd's
/// stricter decimal-by-default `M=1000*1000` convention.
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim();
    let (num, mult): (&str, u64) =
        if let Some(n) = s.strip_suffix("Ki").or_else(|| s.strip_suffix('K')) {
            (n, 1024)
        } else if let Some(n) = s.strip_suffix("Mi").or_else(|| s.strip_suffix('M')) {
            (n, 1024 * 1024)
        } else if let Some(n) = s.strip_suffix("Gi").or_else(|| s.strip_suffix('G')) {
            (n, 1024 * 1024 * 1024)
        } else {
            (s, 1)
        };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bare_bytes() {
        assert_eq!(parse_size("1024"), Some(1024));
    }

    #[test]
    fn parses_binary_suffixes() {
        assert_eq!(parse_size("1K"), Some(1024));
        assert_eq!(parse_size("1Ki"), Some(1024));
        assert_eq!(parse_size("1M"), Some(1024 * 1024));
        assert_eq!(parse_size("1Mi"), Some(1024 * 1024));
        assert_eq!(parse_size("2G"), Some(2 * 1024 * 1024 * 1024));
    }

    #[test]
    fn trims_whitespace() {
        assert_eq!(parse_size("  256M  "), Some(256 * 1024 * 1024));
    }

    #[test]
    fn rejects_garbage() {
        assert_eq!(parse_size("not-a-size"), None);
        assert_eq!(parse_size(""), None);
    }
}
