# Changelog

All notable changes to mitos-services are documented here. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/).

This project is pre-1.0 and has never been run - it was split out of
mitos-init in one migration, not built up incrementally with CI feedback
the way mitos-init's own history was. Treat this first version as
untested even by the standards the rest of MITOS is held to.

## [0.2.0] - Unreleased

### Added
- `Before=`/`Requires=`/`Wants=` (`before=`/`requires=`/`wants=` inline):
  folded into an effective `After=` list (`supervisor::effective_after`)
  before `topological_order` runs. `Requires=` additionally skips a
  service (logged, not fatal to boot) if the service it names is
  configured but didn't end up running. Semantics are intentionally
  narrower than real systemd's - see `INTEGRATION.md` for why.
- Watchdog pings: `WatchdogSec=`/`watchdog_sec=` plus `WATCHDOG=1` over
  the existing per-service notify socket. A service that misses its
  deadline is killed and goes through the normal restart-policy path,
  the same as any other exit - closes the "hung but not exited" gap
  plain process supervision otherwise has no way to detect.

### Fixed
- `config::rescue_service()` removed - dead code after the mitos-init
  split (mitos-init's rescue mode bypasses this process's config
  entirely now, via `exec()`), which would have failed `-D warnings`.
- `ReadyState::forget()`: ready/watchdog state for a service name is now
  cleared on stop/restart/shutdown, so a stale record from a previous
  instance can't leak into a freshly (re)started one under the same
  name.

## [0.1.0] - Unreleased

Split out of mitos-init (see mitos-init's CHANGELOG entry for the same
version) to keep PID 1 minimal - see this README's "Why this is a
separate binary" section.

### Added
- Everything mitos-init's service-management code already did before
  the split: config/unit file parsing, cgroup v2 containment, the
  sd_notify-compatible readiness protocol, transactional reload with
  automatic rollback, dependency ordering (`after=`/`after_ready=`),
  and privilege dropping (`user=`/`group=`) - moved essentially as-is.
- `PR_SET_CHILD_SUBREAPER` on startup, so orphaned grandchildren of
  services reparent here instead of skipping past to mitos-init.
- `ipc.rs`: a Unix-socket control server (plain text protocol, not
  JSON), and `mitosctl` (`status`/`reload`/`ping`) as its client -
  the first way to talk to service management that isn't a raw signal.
- A FIFO-based handshake with mitos-init (`acknowledge_shutdown`) so
  mitos-init knows when it's actually safe to call `reboot(2)`, instead
  of guessing on a fixed timeout.
