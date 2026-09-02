# Changelog

All notable changes to mitos-services are documented here. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/).

This project is pre-1.0 and has never been run - it was split out of
mitos-init in one migration, not built up incrementally with CI feedback
the way mitos-init's own history was. Treat this first version as
untested even by the standards the rest of MITOS is held to.

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
