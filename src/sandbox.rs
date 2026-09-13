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

/// Precomputed in the parent (see the module doc for why) - the highest
/// capability number this kernel knows about, so the bounding-set drop
/// loop in `apply` covers every capability that exists here rather than
/// a number hardcoded at compile time that would silently stop covering
/// new capabilities added by a newer kernel.
#[derive(Debug, Clone, Copy)]
pub struct Sandbox {
    cap_last_cap: u32,
}

/// Used when `/proc/sys/kernel/cap_last_cap` can't be read (e.g. no procfs
/// mounted) - `CAP_CHECKPOINT_RESTORE`, the highest capability defined as
/// of Linux 5.9. Kernels newer than that just get a bounding-set drop
/// that (harmlessly) also covers a few capability numbers the running
/// kernel might not have defined yet.
const FALLBACK_LAST_CAP: u32 = 40;

/// Reads the current capability count. Call from the parent, before
/// `fork()` - see the module doc.
pub fn prepare() -> Sandbox {
    let cap_last_cap = std::fs::read_to_string("/proc/sys/kernel/cap_last_cap")
        .ok()
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(FALLBACK_LAST_CAP);
    Sandbox { cap_last_cap }
}

impl Sandbox {
    /// Applies every isolation step described in the module doc. Must be
    /// called from inside `Command::pre_exec` (single-threaded child,
    /// after `fork()`, before `exec()`) - see the module doc for why.
    ///
    /// # Safety
    /// Makes raw `unshare(2)`/`mount(2)`/`prctl(2)` syscalls and, via
    /// `seccomp::apply`, installs a syscall filter. Only sound to call
    /// once, from the single thread that exists in a freshly-forked
    /// child, with no further allocation between this call and `exec()`.
    pub unsafe fn apply(&self) -> std::io::Result<()> {
        namespaces()?;
        private_tmp()?;
        drop_privileges(self.cap_last_cap)?;
        // Last: after this, any syscall not in seccomp.rs's allow set
        // fails - so nothing below this line, and nothing above it may
        // have relied on a syscall this filter blocks.
        crate::seccomp::apply()?;
        Ok(())
    }
}

unsafe fn namespaces() -> std::io::Result<()> {
    let flags = libc::CLONE_NEWNS | libc::CLONE_NEWUTS | libc::CLONE_NEWIPC;
    if libc::unshare(flags) != 0 {
        return Err(std::io::Error::last_os_error());
    }

    // Recursively make every mount in this (now-private-to-us) namespace
    // MS_PRIVATE, so nothing we mount or unmount from here propagates
    // back to the host, and nothing the host does propagates to us.
    let ret = libc::mount(
        ptr::null::<c_char>(),
        b"/\0".as_ptr() as *const c_char,
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
        b"tmpfs\0".as_ptr() as *const c_char,
        b"/tmp\0".as_ptr() as *const c_char,
        b"tmpfs\0".as_ptr() as *const c_char,
        libc::MS_NOSUID | libc::MS_NODEV | libc::MS_NOEXEC,
        b"size=64M,mode=1777\0".as_ptr() as *const c_void,
    );
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

unsafe fn drop_privileges(cap_last_cap: u32) -> std::io::Result<()> {
    if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    for cap in 0..=cap_last_cap {
        // Best-effort, matching this crate's general style elsewhere
        // (e.g. `notify.rs::chown_path`): a capability this kernel
        // doesn't define, or one we're not permitted to drop in this
        // context (e.g. already inside an unprivileged user namespace),
        // isn't a reason to abort the whole launch.
        let _ = libc::prctl(libc::PR_CAPBSET_DROP, cap as libc::c_ulong, 0, 0, 0);
    }
    Ok(())
}
