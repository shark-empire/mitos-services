# Changelog

All notable changes to mitos-services are documented here. Format
loosely follows [Keep a Changelog](https://keepachangelog.com/).

This project is pre-1.0 and has never been run - it was split out of
mitos-init in one migration, not built up incrementally with CI feedback
the way mitos-init's own history was. Treat this first version as
untested even by the standards the rest of MITOS is held to.

## [0.3.0] - Unreleased

### Added
- Targets (`src/targets.rs`): named service groups (`target=`/
  `X-Target=`, default `multi-user`) you can switch between at runtime
  with `mitosctl isolate <name>` / the `ISOLATE` control-socket command.
  `default_target=` in `init.conf` picks what boots. Narrower than real
  systemd's targets by design - see that module's doc comment.
- Timers (`src/timers.rs`): a `<name>.timer` file next to a
  `<name>.service` one runs that service once, on a relative schedule
  (`OnBootSec=`/`OnUnitActiveSec=`), instead of it needing to be part of
  a target's continuously-supervised set.
- On-demand sandboxed app launching (`src/apps.rs`, `src/sandbox.rs`,
  `src/seccomp.rs`, `APPS.md`): `LAUNCH <path> [args...]` and `APPS`
  over the control socket (`mitosctl launch`/`mitosctl apps`). Applies a
  fresh mount/UTS/IPC namespace, a private tmpfs `/tmp`,
  `PR_SET_NO_NEW_PRIVS` plus a fully-dropped capability bounding set,
  a conservative seccomp-bpf syscall deny-list, and a dedicated cgroup;
  records a generated app id and a SHA-256 hash of the launched binary
  as its identity. This is the "launch every third-party app in its own
  sandbox" piece of MITOS's permission-model design, moved here from an
  earlier mitos-init-centric sketch of it since the primitives it needs
  already exist and are already exercised here for services - see
  `apps.rs`'s module doc for the full rationale, and for what this
  deliberately doesn't cover (PID namespace isolation; any actual
  allow/deny policy - that stays with the not-yet-built `mitos-service`
  policy daemon).
- `Environment=`/`environment=` and `WorkingDirectory=`/`workdir=` for
  services, unblocking the `Environment=RUST_LOG=info` this repo's own
  `etc/mitos/services.d/mitos-settings.service` example already
  specified, silently, before this - it was parsed as an unrecognized
  key and dropped.
- `ServiceDef`/`RestartPolicy` now derive `Default`, so adding a field
  no longer means updating every test helper and `fallback_shell()` by
  hand across three files - the direct cause of how the
  `Environment=`/`WorkingDirectory=`/`target` fields above could be
  added as a five-line diff in `supervisor.rs`'s test module instead of
  a much larger one.

### Fixed
- `logging.rs` always tagged every line `mitos-init [...]`, copied
  verbatim from mitos-init's own copy of this file during the split and
  never updated - every mitos-services log line was misattributed to
  the wrong binary. Now says `mitos-services`.
- `defs_equal` (`supervisor.rs`, used by `reload_services` to decide
  what actually changed) didn't compare the fields added here
  (`environment`/`working_dir`/`target`) - a config reload that only
  changed a service's environment or working directory would have been
  silently treated as a no-op instead of restarting it.
- `cgroups.rs`'s per-service functions are now thin wrappers over new
  root-parameterized ones (`create_under`/`attach_under`/
  `kill_and_remove_under`/`prepare_intermediate`), which is what lets
  `apps.rs` get real cgroup v2 containment for launched apps, nested
  under the same already-delegated root mitos-init sets up for
  services, without mitos-init needing any change of its own. Existing
  service call sites are unchanged.

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
