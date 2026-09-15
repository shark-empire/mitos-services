//! Isolation applied to a launched app between `fork()` and `exec()` -
//! see `apps.rs`'s module doc for what this is part of and why it lives
//! here rather than in mitos-init. Every step here is meant to run from
//! inside `Command::pre_exec`, which the standard library restricts to
//! async-signal-safe-ish operations (see its docs: allocating memory or
//! acquiring a lock the parent might have held at `fork()` time can
//! deadlock the child). Concretely, that means:
//!
//! - Everything that can fail or needs to allocate (right now: just
//!   reading `/proc/sys/kernel/cap_last_cap`) happens in `prepare()`,
//!   called from the *parent*, before `fork()`. Its result is a small
//!   `Copy` struct with no heap allocations of its own, safe to move
//!   into the `pre_exec` closure.
//! - `apply()`, run inside the child, only makes raw syscalls
//!   (`libc::unshare`, `libc::mount`, `libc::prctl`) against data that's
//!   already fully built - no `String`/`Vec` allocation, no locks.
//!
//! What's applied, in order (least to most irreversible - see the
//! comment on why seccomp is last):
//! 1. A fresh mount, UTS, and IPC namespace (`unshare()`), immediately
//!    followed by marking the whole mount tree `MS_PRIVATE|MS_REC` - the
//!    standard "first thing after `unshare(CLONE_NEWNS)`" step; skipping
//!    it lets mount/unmount events leak back to the host namespace,
//!    which would make the new namespace mostly cosmetic.
//! 2. A private, size-capped tmpfs over `/tmp`.
//! 3. `PR_SET_NO_NEW_PRIVS` and the entire capability bounding set
//!    dropped - even a setuid/file-capability binary the app execs can't
//!    regain capabilities from that point on.
//! 4. The seccomp-bpf filter from `seccomp.rs` (**unverified beyond
//!    manual review - see that module's doc before depending on this
//!    step**).
//!
//! What's deliberately NOT here (see `apps.rs`'s module doc for the
//! fuller explanation): PID namespace isolation, and a PID/mount
//! namespace on any architecture but x86_64 (the seccomp step is
//! x86_64-only; steps 1-3 are architecture-independent and still apply
//! everywhere).

use std::ffi::c_void;
use std::os::raw::c_char;
use std::ptr;

// ============================================================================
// App Sandbox (Used by apps.rs)
// ============================================================================

/// Precomputed in the parent (see the module doc for why) - the highest
/// capability number this kernel knows about, so the bounding-set drop
/// loop in `apply` covers every capability that exists here rather than
/// a number hardcoded at compile time that would silently stop covering
/// new capabilities added by a newer kernel.
#[derive(Debug, Clone, Copy)]
pub struct Sandbox {
    cap_last_cap: u32,
}

const FALLBACK_LAST_CAP: u32 = 40;

pub fn prepare() -> Sandbox {
    let cap_last_cap = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(FALLBACK_LAST_CAP);
    Sandbox { cap_last_cap }
}

impl Sandbox {
    pub unsafe fn apply(&self) -> std::io::Result<()> {
        namespaces()?;
        private_tmp()?;
        drop_privileges(self.cap_last_cap)?;
        crate::seccomp::apply()?;
        Ok(())
    }
}

// ============================================================================
// Service Sandbox (Used by supervisor.rs)
// ============================================================================

/// Isolation options for system services (as opposed to on-demand apps
/// in `apps.rs`). A service can opt into any combination of these -
/// `supervisor.rs` reads them from the service's configuration and
/// only applies the non-empty ones.
#[derive(Debug, Clone, Copy, Default)]
pub struct ServiceSandbox {
    pub private_tmp: bool,
    pub protect_system: bool,
    pub no_new_privileges: bool,
}

impl ServiceSandbox {
    /// True if none of the sandboxing options were requested - lets
    /// `supervisor.rs` take the simpler/cheaper path of just using
    /// `Command::uid`/`gid` without a `pre_exec` closure.
    pub fn is_empty(&self) -> bool {
        !self.private_tmp && !self.protect_system && !self.no_new_privileges
    }

    /// Applies the requested isolation steps. Must be called from inside
    /// `Command::pre_exec` - see the module doc for why.
    ///
    /// # Safety
    /// Same restrictions as `Sandbox::apply` - makes raw syscalls in the
    /// single-threaded child before `exec()`.
    pub unsafe fn apply(&self) -> std::io::Result<()> {
        let needs_mounts = self.private_tmp || self.protect_system;

        if needs_mounts {
            // Only need a mount namespace for these (no UTS/IPC for services)
            if libc::unshare(libc::CLONE_NEWNS) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            // Make the new mounted tree private so changes don't propagate
            let ret = libc::mount(
                ptr::null::<c_char>(),
                c"/".as_ptr(), // Fixed
                ptr::null::<c_char>(),
                libc::MS_REC | libc::MS_PRIVATE,
                ptr::null::<c_void>(),
            );

            if ret != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }

        if self.private_tmp {
            private_tmp()?;
        }

        if self.protect_system {
            protect_system()?;
        }


        if self.no_new_privileges
            && libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0
        {
            return Err(std::io::Error::last_os_error());
        }


        Ok(())
    }
}

// ============================================================================
// Shared Primitives
// ============================================================================

unsafe fn namespaces() -> std::io::Result<()> {
    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    if libc::unshare(flags) != 0 {
        return Err(std::io::Error::last_os_error());
    }

    let ret = libc::mount(
        ptr::null::<c_char>(),
        c"/".as_ptr(), // Fixed
        ptr::null::<c_char>(),
        libc::MS_REC | libc::MS_PRIVATE,
        ptr::null::<c_void>(),
    );

    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

unsafe fn private_tmp() -> std::io::Result<()> {
    let ret = libc::mount(
        c"tmpfs".as_ptr(),
        c"/tmp".as_ptr(),
        c"tmpfs".as_ptr(),
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        c"size=64M,mode=1777".as_ptr() as *const c_void,
      );
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}


/// Implements `ProtectSystem=`-style isolation: bind-mounts `/usr`, `/boot`,
/// and `/etc` over themselves and remounts them read-only. Paths that
/// don't exist (or fail to bind-mount) are silently skipped rather than
/// aborting the service launch.
unsafe fn protect_system() -> std::io::Result<()> {
    // Cast to `&[u8]` so the array elements share the same type
    for path in [b"/usr\0" as &[u8], b"/boot\0" as &[u8], b"/etc\0" as &[u8]] {
        let path_ptr = path.as_ptr() as *const c_char;

        // First bind-mount the path onto itself.
        let bind_ret = libc::mount(
            path_ptr,
            path_ptr,
            ptr::null::<c_char>(),
            libc::MS_BIND | libc::MS_REC,
            ptr::null::<c_void>(),
        );
        if bind_ret != 0 {
            // Path probably doesn't exist; skip it.
            continue;
        }

        // Remount the bind-mount read-only.
        let _ = libc::mount(
            ptr::null::<c_char>(),
            path_ptr,
            ptr::null::<c_char>(),
            libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY | libc::MS_REC,
            ptr::null::<c_void>(),
        );
    }
    Ok(())
}

unsafe fn drop_privileges(cap_last_cap: u32) -> std::io::Result<()> {
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for cap in 0..=cap_last_cap {
        let _ = libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
    }
    Ok(())
}
