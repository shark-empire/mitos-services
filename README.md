# mitos-services

MITOS's service manager. Runs as a single supervised child of
[mitos-init](../mitos-init) (PID 1) - not PID 1 itself - and does
everything a full-featured service manager needs that PID 1 shouldn't:
parsing config and unit files, dependency ordering, per-service cgroups,
the `sd_notify`-compatible readiness protocol, transactional config
reload, and a control socket for `mitosctl`.

## Why this is a separate binary from mitos-init

mitos-init's release profile sets `panic = "abort"` - correct for PID 1,
since it means the *kernel* is what's left holding the bag if something
in early boot panics (`Kernel panic - not syncing: Attempted to kill
init!`). That's fine for a small, mostly `Result`-based boot sequence. It
stops being fine once you add dependency-graph cycle detection, unit file
parsing for arbitrary third-party services, and IPC - the surface area
for an edge-case bug goes up with every feature, and PID 1 is the one
place a bug can take the whole machine down with it.

Splitting this out means a bug here can only crash mitos-services -
which mitos-init then restarts, with the same crash-loop backoff shape
individual services already got, and falls back to a rescue shell if
that also keeps failing. The OS survives a bug in the complicated part.

## How it talks to mitos-init

- mitos-init spawns this binary (`/sbin/mitos-services`) after mounting
  the filesystem tree and preparing cgroup delegation, and supervises it
  as its one and only child.
- `reboot`/`poweroff`/`halt`/`shutdown` still signal PID 1 directly (a
  kernel/sysvinit convention, unchanged by this split). mitos-init relays
  the same signal here, waits for this process to stop every service and
  write an acknowledgement to a FIFO mitos-init created
  (`/run/mitos-init/shutdown-ack`), then performs the actual `reboot(2)`
  syscall itself - only mitos-init ever does that.
- `SIGUSR1`/`SIGUSR2` (reload/status) are relayed the same way, and
  `mitosctl` reaches this process directly over its own control socket
  instead.

## Building and running

```
cargo build --release
cargo test
```

Not meant to be run standalone in production - see mitos-init's
`ASSEMBLY.md` for how the pieces combine into a bootable MITOS. For
manual testing, `sudo ./target/release/mitos-services` works on its own
(it'll warn about missing cgroup delegation from mitos-init and fall back
to plain pid-based supervision, which is fine for testing).

## Config

Same format as before this split - `/etc/mitos/init.conf` and/or
`/etc/mitos/services.d/*.service`. See `init.conf.example`,
`services.d.example/`, and `INTEGRATION.md` (the reference for making
*any* project - not just ones inside MITOS - work well as a
mitos-services-supervised service).

## mitosctl

```
mitosctl status   # pid, ready/starting, critical or not, per service
mitosctl reload   # same as SIGUSR1, but doesn't require knowing the pid
mitosctl ping     # liveness check
```

Talks to `/run/mitos-services/control.sock` over a plain newline-
delimited text protocol - not JSON, matching this project's general
preference for hand-rolled parsing over pulling in a serialization crate
for a handful of simple commands.

## Layout

- `src/main.rs` - entry point: becomes a child subreaper
  (`PR_SET_CHILD_SUBREAPER`), loads config, spawns services, runs the
  event loop, acknowledges shutdown back to mitos-init
- `src/supervisor.rs` - service spawning, restart policy, dependency
  ordering (`after=`/`after_ready=`), privilege dropping
- `src/config.rs` - `/etc/mitos/init.conf` parsing
- `src/units.rs` - `/etc/mitos/services.d/*.service` unit file parsing
- `src/cgroups.rs` - per-service cgroup v2 (leaf cgroups only - mounting
  and controller delegation happen in mitos-init, before this process
  even exists)
- `src/notify.rs` - the `sd_notify`-compatible readiness protocol
- `src/rollback.rs` - transactional config reload
- `src/users.rs` - `/etc/passwd`/`/etc/group` lookups for `user=`/`group=`
- `src/ipc.rs` - the control socket `mitosctl` talks to
- `src/bin/mitosctl.rs` - the CLI client for that socket
- `src/signals.rs` - catches signals relayed from mitos-init
- `src/logging.rs` / `src/error.rs` - same shape as mitos-init's own
  (duplicated rather than shared, to keep both binaries independently
  buildable without a workspace-internal library crate)

## What's deliberately not here yet

Flagged rather than silently missing:

- **Targets** (`multi-user.target`, `graphical.target`, ...) - there's
  currently one flat service list, not named groups you can switch
  between (rescue mode is a mitos-init-level special case that bypasses
  this process entirely, not a target).
- **Timers** - no `.timer`-equivalent scheduled/periodic activation.

Already built, despite being listed as future work in an earlier version
of this file: dependency ordering beyond simple `After=` (`Before=`,
`Requires=`, `Wants=` - see `INTEGRATION.md` for the intentionally
narrower semantics vs real systemd) and watchdog pings (`WatchdogSec=`,
`WATCHDOG=1` over the same notify socket `READY=1` uses).

## License

Dual-licensed under [MIT](LICENSE-MIT) or [Apache License 2.0](LICENSE-APACHE).
`Cargo.toml`'s `authors` field is a placeholder - update it (and the
copyright line in both LICENSE files) with real attribution before
publishing.
