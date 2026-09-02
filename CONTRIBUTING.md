# Contributing

## Before you start

This isn't PID 1 - a bug here crashes this process, not the kernel -
but it's still root-privileged, and mitos-init depends on it responding
correctly to shutdown signals within a bounded time (see
`acknowledge_shutdown` in `main.rs`) to actually be able to reboot/power
off the machine. A few habits worth keeping up:

- **Prefer `Result` over panicking**, same as mitos-init, even though the
  consequences of a panic here are less severe (this process restarts,
  the kernel doesn't go down with it). `install_panic_hook` makes an
  unexpected one visible in the log either way.
- **Always acknowledge shutdown, even on a bad path.** If you're touching
  the shutdown sequence, make sure `acknowledge_shutdown()` still gets
  called on every exit path - mitos-init blocks (bounded, but still)
  waiting for it, and a version that returns early without acking just
  means every reboot/poweroff takes an extra 20 seconds it didn't need
  to.
- **New parsing logic gets tests.** See the `#[cfg(test)]` modules in
  `config.rs`/`units.rs`/`cgroups.rs`/`users.rs` for the existing
  pattern. Logic that needs root or a real kernel (cgroups, the actual
  supervision loop) doesn't have automated tests yet; that's what VM
  testing is for.

## Workflow

1. `cargo fmt --all` before committing (or let
   `.github/workflows/format.yml` do it for you on push to `main`).
2. `cargo clippy --all-targets -- -D warnings` and `cargo test` should
   both be clean - CI (`.github/workflows/ci.yml`) runs both, along with
   `cargo check` and a `fmt --check`.
3. Test any change touching the shutdown path or supervision loop in a
   VM before considering it done, running under an actual mitos-init -
   see mitos-init's README for a QEMU example. This project doesn't have
   hardware-in-the-loop CI, so that step doesn't happen automatically.

## Where things live

See the README's "Layout" section for a one-line-per-file map, and
mitos-init's `ASSEMBLY.md` for how this repo relates to mitos-init and
mitos-gui, and the rest of MITOS. Anything that's specifically a PID-1
concern (mounting, the initramfs handoff, device permissions) belongs in
mitos-init instead, with its own CONTRIBUTING.md.
