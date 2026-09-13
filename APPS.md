# On-demand app launching - integration guide

This is the endpoints doc for `src/apps.rs` - how another MITOS project
(`mitos-shell`, `mitos-gui`, or anything else that needs to start a
user-facing application rather than a boot-time service) asks
mitos-services to launch one. See `INTEGRATION.md` for the
service-supervision side of this same control socket; this file only
covers the app-launch commands added on top of it.

## Why this exists

`supervisor.rs` starts a fixed, configured list of services at boot.
Launching an arbitrary application on request - "the user tapped this
icon" - is a different problem: no restart policy, no dependency
ordering, one launch per request, and (since the binary wasn't vetted
ahead of time the way a `.service` file's `ExecStart=` was) real
sandboxing by default. `apps.rs` is that second thing. See its module
doc comment for exactly what the sandbox does and does not cover before
depending on it for anything security-sensitive - in particular, the
seccomp filter has been reviewed against the documented kernel ABI but
not yet exercised on real hardware.

**This is a mechanism, not a policy.** Launching through this socket
always succeeds (modulo the binary existing and being executable) - there
is no rulebook, no risk classification, and no password prompt here.
MITOS's permission-model design calls that separate piece
`mitos-service` (singular); it doesn't exist yet, and this repo has no
dependency on it. Until it does, anything that can write to
`/run/mitos-services/control.sock` can launch anything. That socket is
mode `0600`, so today "anything that can write to it" means "root" -
treat that as the actual access boundary in the meantime, not the
absence of one.

## Talking to the socket

Same transport as every other command in `INTEGRATION.md`: connect to
`/run/mitos-services/control.sock`, write one line, read one line back,
disconnect.

### `LAUNCH <path> [args...]`

Launches `path` (must be an absolute path to an executable file) with
the given arguments, sandboxed as described in `apps.rs`'s module doc.

```
$ printf 'LAUNCH /usr/bin/gedit /home/user/notes.txt\n' | nc -U /run/mitos-services/control.sock
launched: app-6710a1b2-1
```

- Success: `launched: <app-id>\n`. The id is opaque - treat it as a
  string, not something to parse structure out of - and is what shows
  up in `APPS`'s output and in the log line mitos-services writes for
  this launch (`launched app '<id>' (<path>) as pid <pid>,
  sha256:<hash>`).
- Failure (bad path, exec permission denied, sandbox setup failed):
  `launch failed: <reason>\n`.
- The launched process's environment is **not** inherited from
  mitos-services - it gets a minimal, fixed environment
  (`MITOS_APP_ID`, a sane default `PATH`, and `TERM` if the caller's own
  environment had one). If your app needs something else from its
  environment (a `DISPLAY`/Wayland socket path, an XDG runtime dir, ...),
  set it as an argument or have the app discover it itself
  (`MITOS_APP_ID` is there so it can, for instance, look up its own
  assigned runtime directory once that convention exists) - there's no
  `Environment=`-equivalent parameter on this command today. If you need
  one, that's a small, additive change to `LAUNCH`'s argument parsing in
  `ipc.rs` - ask for it rather than working around its absence.

### `APPS`

Lists every currently-running launched app: id, pid, path, arguments,
and the SHA-256 of the binary that was launched.

```
$ printf 'APPS\n' | nc -U /run/mitos-services/control.sock
2 running app(s):
  app-6710a1b2-1 pid 4821: /usr/bin/gedit /home/user/notes.txt (sha256:...)
  app-6710a1b2-2 pid 4830: /usr/bin/firefox (sha256:...)
```

An app disappears from this list as soon as mitos-services reaps its
exit - there's no history kept of apps that already finished.

## What a caller should and shouldn't assume

- **Do** treat a `launched: <id>` response as "the process exists now",
  not "the process is doing anything useful yet" - same as any
  `fork`+`exec`, there's no readiness protocol for apps the way
  `notify.rs` gives services one.
- **Do** expect the sandbox described in `apps.rs` to already be
  applied by the time your app's `main()` runs - you don't need to (and
  can't, from outside) opt out of it per-launch today.
- **Don't** assume a launched app's pid is stable/reusable across a
  mitos-services restart - if mitos-services itself is restarted by
  mitos-init, every previously-launched app keeps running (they're not
  its children in any way that matters for that), but the `APPS`
  registry is in-memory only and starts empty again.
- **Don't** launch something you haven't already decided is safe to
  run. Restating the point above: this socket does not ask "should
  this be allowed" on your behalf.

## Future integration seam

`apps.rs::authorize()` is where a future `mitos-service` policy daemon
plugs in - today it's a no-op that always allows. When that daemon
exists, expect `LAUNCH` to start returning `launch failed: permission
denied` (or similar) for requests it declines, and expect that decision
to potentially involve a password prompt on a *different* channel
(the compositor-drawn prompt the permissions design describes) before
this socket's response comes back - i.e., `LAUNCH` may become slower and
occasionally interactive-on-someone-else's-screen once that exists. Not
a concern for any caller today; worth designing your own caller's
timeout expectations around if you're implementing one now anyway.
