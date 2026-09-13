# Security Policy

mitos-services runs as root (though not as PID 1 - see mitos-init's own
`SECURITY.md` for that boundary), and this project is pre-1.0 and has
never been run at all yet (see CHANGELOG.md) - please report anything
that looks like a security issue rather than opening a public issue for
it first.

## Reporting a vulnerability

Email <security@example.invalid> with a description of the issue and,
if possible, steps to reproduce it. (Replace this address with a real
contact before publishing this project.)

Please don't open a public GitHub issue for a suspected vulnerability
until there's been a chance to assess and, where needed, fix it first.

## Scope

Privilege escalation; anything that lets an unprivileged local process
spoof another service's `READY=1` (the per-service notify socket
permission/ownership model - `notify.rs`) or influence the control
socket (`ipc.rs`, restricted to `0600`); memory-safety issues in the
`unsafe` code in `main.rs` (`PR_SET_CHILD_SUBREAPER`) and, the larger
surface, `sandbox.rs`/`seccomp.rs` (namespace/mount/capability/seccomp
setup for a launched app - see `apps.rs`'s module doc, and note its
seccomp filter is explicitly flagged there as reviewed but not yet
exercised on real hardware); a launched app escaping the isolation
`sandbox.rs` is supposed to apply, or a service escaping the resource
limits or teardown guarantees its cgroup is supposed to provide.

Known, already-documented limitations that are *not* new reports:
config/unit files are trusted as-is with no permission or signature
checking - same trust model as `/etc` being root-owned on any mainstream
distro, called out explicitly in the README rather than left implicit.
Likewise, `LAUNCH` (see `APPS.md`) performs no authorization check at
all - anything that can reach the control socket can launch anything;
that's `APPS.md`'s documented scope, not a bug report, until the
separate `mitos-service` policy daemon it names exists. Also
already-documented: launched apps have no PID namespace isolation yet
(`apps.rs`'s module doc explains why that's deferred rather than
guessed at).

## Supported versions

Pre-1.0: only the latest commit on the default branch is supported.
There's no backport policy yet.
