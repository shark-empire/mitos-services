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
small amount of `unsafe` code (`PR_SET_CHILD_SUBREAPER` in `main.rs`);
and anything that lets a service escape the resource limits or teardown
guarantees its cgroup is supposed to provide.

Known, already-documented limitations that are *not* new reports:
config/unit files are trusted as-is with no permission or signature
checking - same trust model as `/etc` being root-owned on any mainstream
distro, called out explicitly in the README rather than left implicit.

## Supported versions

Pre-1.0: only the latest commit on the default branch is supported.
There's no backport policy yet.
