# Integrating a project with mitos-services

This is the reference for making *your* program work well as a
mitos-services-supervised service - whether that's mitos-gui, something
else in the MITOS project, or an unrelated third-party daemon.

(mitos-init - a separate, minimal binary - boots the machine and
supervises mitos-services itself, but doesn't parse config or talk to
services directly; see mitos-init's own `INTEGRATION_SPLIT.md`
if you're curious about that boundary. For how mitos-init, mitos-services,
and mitos-gui specifically combine into a bootable MITOS - base rootfs,
build pipeline, the Stage-5 handoff - see mitos-init's `ASSEMBLY.md`
instead. This doc is the general service interface; that one's the
specific assembly of this project's pieces.)

## The short version

Write a unit file, drop it in `/etc/mitos/services.d/your-service.service`:

```ini
[Service]
ExecStart=/path/to/your/binary --flags
Restart=on-failure
```

That's a complete, valid integration. Everything below is what's
available beyond the minimum.

## What mitos-services gives every service

- **`$NOTIFY_SOCKET`** - a unix datagram socket path, set automatically
  in every service's environment. Write `READY=1\n` to it once you're
  actually ready to do work (not just "process started"), and a `STATUS`
  request via `mitosctl` (or the raw `SIGUSR2` signal) will show you as
  ready rather than just spawned. If you set `WatchdogSec=`, also write
  `WATCHDOG=1\n` at least that often once you're up - missing that
  deadline gets you killed and restarted per your `Restart=` policy, the
  same as if you'd exited outright, since a hung-but-not-exited process
  is otherwise invisible to supervision. Both are the same wire format as
  systemd's `sd_notify()` - if you already call that (the `sd-notify`
  crate, `libsystemd`, or a hand-rolled equivalent), it works against
  mitos-services unmodified; you don't need to detect which init/service
  manager you're running under.
- **A cgroup** at `/sys/fs/cgroup/mitos-init/<your-service-name>/` (yes,
  under mitos-init's path even though mitos-services manages it day to
  day - see mitos-init's `ASSEMBLY.md` for why) - not something you need
  to do anything with directly, but it's why stopping your service
  reliably reaches child processes you spawned too, even though only
  your one top-level pid was ever directly tracked.
- **`SIGTERM` on shutdown/stop**, with a grace period (`shutdown_timeout=`
  in `init.conf`, default 5s) before `SIGKILL`. Handle `SIGTERM` if you
  have cleanup to do; if you don't handle it, the default disposition
  (terminate) is still fine.

## Unit file reference

All keys are optional except `ExecStart=`.

| Key | Meaning |
|---|---|
| `ExecStart=` | Path plus space-separated args. No shell, no quoting rules beyond whitespace-splitting - if you need a shell, exec one explicitly. |
| `Restart=` | `no` (default) \| `always` \| `on-failure` |
| `X-Critical=true` | Exiting this service shuts the whole system down. Never restarted, regardless of `Restart=`. |
| `MemoryMax=` | e.g. `256M`, `1G`, or a bare byte count. Enforced via the service's cgroup. |
| `User=` / `Group=` | Run as this user/group (name or numeric id) instead of root. |
| `After=name,name` | Start after these services have been spawned - ordering only, checked once at boot. |
| `X-AfterReady=name,name` | Start only after these services report `READY=1`, up to a 15s timeout (then starts anyway, with a warning logged). Stronger than `After=` - use it when you'd actually break without the dependency up, not just prefer it up first. |
| `Before=name,name` | The inverse of `After=` - start these named services only after this one. Folded into their effective `After=` automatically. |
| `Requires=name,name` | Hard dependency: if a named, configured service didn't end up running, this one is skipped too (logged, not fatal to boot). Also implies ordering, same as `After=`. |
| `Wants=name,name` | Soft dependency: ordering only (same effect as `After=`), doesn't block this service if the named one fails or isn't configured. |
| `WatchdogSec=` | Seconds. Expect `WATCHDOG=1` at least this often once started - see above. Omit for no watchdog (most services). |

A note on `Requires=`/`Wants=` since real systemd users will have
expectations here: this project's semantics are intentionally narrower.
Real systemd's `Requires=`/`Wants=` primarily control what gets *pulled
into a transaction* when a unit is activated; mitos-services always
supervises a fixed, fully-configured list, so there's no activation
transaction for them to affect. Here they're reduced to what actually
matters in that simpler model: ordering, plus (for `Requires=` only)
skipping this service if the dependency didn't start.

`X-`-prefixed keys aren't real systemd syntax - they're this project's
own extensions, using the exact prefix the systemd unit file spec
reserves for vendor extensions real systemd would just ignore. Every
other key here is standard `[Service]` syntax, so a mitos-services unit
file is closer to portable than you might expect.

The same options exist as `init.conf` inline fields if you'd rather not
use a separate file per service: `path=`, `restart=`, `critical=`,
`mem_max=`, `user=`, `group=`, `after=`, `after_ready=`, `before=`,
`requires=`, `wants=`, `watchdog_sec=` - see
`init.conf.example`.

## Restart policy and crash loops

`Restart=always` and `Restart=on-failure` both back off automatically: a
service that restarts more than 5 times within 10 seconds stops being
retried, with a "giving up" message logged, rather than spinning forever.
If you're relying on restarts for resilience, make sure your actual
failure mode doesn't crash-loop faster than that window - fix the
underlying issue rather than working around the backoff.

## Testing your integration

1. Drop your unit file into `/etc/mitos/services.d/`.
2. `mitosctl reload` (or `kill -USR1 <mitos-services-pid>` - both do the
   same thing; the socket just doesn't require knowing the pid). Either
   way, mitos-services reconciles the running set against the new config
   and watches whatever changed for 10 seconds; if your service fails
   hard in that window, the whole reload reverts automatically rather
   than leaving things half-applied.
3. `mitosctl status` (or `kill -USR2 <pid>`) shows status - pid,
   ready/starting, critical or not - for every supervised service.
   `mitosctl` reads the live data back over the control socket;
   `SIGUSR2` only logs it, so `mitosctl` is the more useful of the two
   for scripting.

The control socket (`/run/mitos-services/control.sock`) is root-only
(`0600`); the raw-signal alternative needs root/`CAP_KILL` to signal
mitos-services' pid, same as signaling any other process you don't own.
Test in a VM before real hardware, same as any other change that touches
boot - see mitos-init's README, "Installing as your system's init", for
a QEMU example.
