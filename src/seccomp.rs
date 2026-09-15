//! A minimal seccomp-bpf syscall filter: x86_64 only, deny-list only
//! (default action is *allow* - the filter only special-cases the
//! syscalls in `BLOCKED_SYSCALLS` below, returning `EPERM` for those and
//! falling through to `SECCOMP_RET_ALLOW` for everything else).
//!
//! This is deliberately not an allow-list. An allow-list is the
//! stronger guarantee, but it has to be right for *every* syscall an
//! arbitrary launched app might legitimately need (which varies by
//! language runtime, libc, and what the app links against) - getting
//! that wrong fails closed in the worst way, breaking apps in ways that
//! look like this project's bug, not theirs. A deny-list only has to be
//! right about the syscalls it names, which are also the ones easiest to
//! be confident about: whole-system operations (`mount`, `reboot`,
//! kernel module loading, `ptrace`, raw I/O port access, ...) that an
//! ordinary launched application has no legitimate reason to call at
//! all. This is a baseline, not a policy engine - see `apps.rs`'s module
//! doc for what layer actual per-app policy belongs in.
//!
//! **Verify before depending on this.** The struct layouts and syscall
//! numbers below are the stable, documented kernel ABI (`seccomp_data`,
//! `sock_filter`/`sock_fprog`, `arch/x86/entry/syscalls/syscall_64.tbl`)
//! and were checked against that ABI while writing this, not guessed -
//! but this project has never had a compiler or a real kernel in the
//! loop for any of it (see the crate `CHANGELOG.md`). Confirm a
//! sandboxed app actually starts, and that `strace`ing a blocked syscall
//! from inside one actually returns `EPERM`, on real hardware or a VM
//! before relying on this for anything that matters. If in doubt, the
//! rest of the sandbox (namespaces, capability bounding set, cgroup -
//! see `sandbox.rs`) does not depend on this module and is unaffected by
//! disabling it.

use std::os::raw::{c_int, c_ulong};

// --- Kernel ABI: struct seccomp_data (linux/seccomp.h) ---
// nr @0 (i32), arch @4 (u32), instruction_pointer @8 (u64), args @16..64.
// We only ever read nr/arch, but the struct is documented here so the
// two BPF_ABS offsets below (0 and 4) are traceable back to it.

// --- Kernel ABI: struct sock_filter / sock_fprog (linux/filter.h) ---
#[repr(C)]
struct SockFilter {
    code: u16,
    jt: u8,
    jf: u8,
    k: u32,
}

#[repr(C)]
struct SockFprog {
    len: u16,
    filter: *const SockFilter,
}

// BPF instruction classes/fields (linux/bpf_common.h) - only the ones
// this filter actually uses.
const BPF_LD: u16 = 0x00;
const BPF_JMP: u16 = 0x05;
const BPF_RET: u16 = 0x06;
const BPF_W: u16 = 0x00;
const BPF_ABS: u16 = 0x20;
const BPF_JEQ: u16 = 0x10;
const BPF_K: u16 = 0x00;

// seccomp mode/return values (linux/seccomp.h)
const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000; // Linux 4.14+
const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
const SECCOMP_RET_DATA_MASK: u32 = 0x0000_ffff;
const EPERM: u32 = 1;

// prctl(2) (linux/prctl.h)
const PR_SET_NO_NEW_PRIVS: c_int = 38;
const PR_SET_SECCOMP: c_int = 22;
#[allow(dead_code)]
const PR_CAPBSET_DROP: c_int = 24;
const SECCOMP_MODE_FILTER: c_ulong = 2;

// Offsets into struct seccomp_data.
const OFFSET_NR: u32 = 0;
const OFFSET_ARCH: u32 = 4;

// AUDIT_ARCH_X86_64 = EM_X86_64 (62) | __AUDIT_ARCH_64BIT (0x8000_0000)
// | __AUDIT_ARCH_LE (0x4000_0000) - <linux/audit.h>/<linux/elf-em.h>.
// This filter only ever installs on x86_64 (see `apply` below); the
// check is defense in depth against ever being called from a 32-bit
// compat syscall entry, which would otherwise let the *nr* comparisons
// below match against the wrong syscall table entirely.
const AUDIT_ARCH_X86_64: u32 = 0xC000_003E;

/// x86_64 syscall numbers with no legitimate use for an ordinary
/// launched application - whole-machine state (mount table, swap,
/// hostname, wall clock, kernel modules, kexec), raw hardware I/O port
/// access, process introspection/debugging (`ptrace`), and the
/// kernel keyring. Numbers are from
/// `arch/x86/entry/syscalls/syscall_64.tbl`. Deliberately NOT included:
/// `prctl`/`arch_prctl` (glibc's own startup and TLS setup depend on
/// these - blocking either breaks every dynamically-linked app
/// immediately), `personality` (legitimate uses exist - e.g. compat
/// layers toggling ASLR), `unshare`/`setns` (namespace operations an app
/// might legitimately use for its own sandboxing, e.g. a browser's own
/// renderer sandbox - this filter blocks system-wide danger, not
/// self-sandboxing).
const BLOCKED_SYSCALLS: &[i32] = &[
    101, // ptrace
    103, // syslog (kernel ring buffer - can leak other processes' data)
    154, // modify_ldt
    155, // pivot_root
    161, // chroot
    163, // acct
    164, // settimeofday
    165, // mount
    166, // umount2
    167, // swapon
    168, // swapoff
    169, // reboot
    170, // sethostname
    171, // setdomainname
    172, // iopl
    173, // ioperm
    175, // init_module
    176, // delete_module
    179, // quotactl
    246, // kexec_load
    248, // add_key
    249, // request_key
    250, // keyctl
    298, // perf_event_open
    313, // finit_module
    320, // kexec_file_load
    321, // bpf
];

fn stmt(code: u16, k: u32) -> SockFilter {
    SockFilter {
        code,
        jt: 0,
        jf: 0,
        k,
    }
}

fn jump(code: u16, k: u32, jt: u8, jf: u8) -> SockFilter {
    SockFilter { code, jt, jf, k }
}

/// Builds the BPF program described in the module doc. Pure data
/// construction - no syscalls, safe to call from anywhere (in
/// particular, from the parent process *before* `fork()`, so the
/// allocation it does never happens inside a freshly-forked child - see
/// `apps.rs::launch`).
fn build_program() -> Vec<SockFilter> {
    let mut prog = Vec::with_capacity(3 + BLOCKED_SYSCALLS.len() * 2 + 1);

    // Wrong architecture (e.g. a 32-bit compat syscall entry) -> kill
    // outright rather than let the nr checks below match the wrong table.
    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_ARCH));
    prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, AUDIT_ARCH_X86_64, 1, 0));
    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_KILL_PROCESS));

    prog.push(stmt(BPF_LD | BPF_W | BPF_ABS, OFFSET_NR));
    let errno_ret = SECCOMP_RET_ERRNO | (EPERM & SECCOMP_RET_DATA_MASK);
    for &nr in BLOCKED_SYSCALLS {
        // nr == this blocked syscall -> fall through (jt=0) to the ERRNO
        // return; otherwise skip it (jf=1) to reach the next check.
        prog.push(jump(BPF_JMP | BPF_JEQ | BPF_K, nr as u32, 0, 1));
        prog.push(stmt(BPF_RET | BPF_K, errno_ret));
    }

    prog.push(stmt(BPF_RET | BPF_K, SECCOMP_RET_ALLOW));
    prog
}

/// Applies `PR_SET_NO_NEW_PRIVS` (required before `PR_SET_SECCOMP` will
/// even be accepted - see `prctl(2)`) and then the filter from
/// `build_program`. Must be called from a single-threaded context after
/// the point of no return (i.e. from inside `Command::pre_exec` - see
/// `sandbox.rs::apply`), since a seccomp filter applies only to the
/// calling thread and its future children/execs, and once installed
/// cannot be removed, only further restricted.
///
/// # Safety
/// Calls raw `prctl(2)`. Building the filter itself does no I/O or
/// allocation beyond the one `Vec` this function briefly owns; the
/// pointer handed to the kernel is only read for the duration of the
/// syscall.
pub unsafe fn apply() -> std::io::Result<()> {
    let program = build_program();
    let fprog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };

    if libc::prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let ret = libc::prctl(
        PR_SET_SECCOMP,
        SECCOMP_MODE_FILTER,
        &fprog as *const SockFprog,
        0,
        0,
    );
    // `program` must outlive this call - it does, since `fprog` borrows
    // it and both are dropped together at the end of this function, well
    // after `prctl` has returned and the kernel has copied the filter.
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn program_is_well_formed() {
        let prog = build_program();
        // 3 (arch check) + 2 per blocked syscall + 1 (final allow)
        assert_eq!(prog.len(), 3 + BLOCKED_SYSCALLS.len() * 2 + 1);
        // Every program must end in a RET so execution can't fall off
        // the end into undefined instructions.
        assert_eq!(prog.last().unwrap().code, BPF_RET | BPF_K);
        assert_eq!(prog.last().unwrap().k, SECCOMP_RET_ALLOW);
    }

    #[test]
    fn fits_in_sock_fprog_len() {
        // sock_fprog.len is a u16 - this would silently truncate the
        // program if it ever grew past 65535 instructions. Nowhere close
        // today, but worth a canary as BLOCKED_SYSCALLS grows.
        assert!(build_program().len() < u16::MAX as usize);
    }

    #[test]
    fn no_duplicate_blocked_syscalls() {
        let mut sorted = BLOCKED_SYSCALLS.to_vec();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), BLOCKED_SYSCALLS.len());
    }
}
