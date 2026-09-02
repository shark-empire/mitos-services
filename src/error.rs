//! Centralized error type for mitos-init.
//!
//! PID 1 must never panic if it can possibly help it: an unhandled panic in
//! PID 1 kills the whole system (the kernel panics when init dies). Every
//! fallible operation in this crate returns `Result<T>` so `main` can decide
//! how to degrade gracefully instead of unwinding.

use std::fmt;

#[derive(Debug)]
pub enum InitError {
    Mount {
        target: String,
        source: nix::errno::Errno,
    },
    Io(std::io::Error),
    Signal(nix::errno::Errno),
    /// Catch-all for boot-sequencing failures (root switch, etc.) where the
    /// call site already has a precise, human-written message - avoids
    /// forcing every early-boot error through the "mount" or "I/O" framing.
    Boot(String),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InitError::Mount { target, source } => write!(f, "failed to mount {target}: {source}"),
            InitError::Io(e) => write!(f, "I/O error: {e}"),
            InitError::Signal(e) => write!(f, "signal setup failed: {e}"),
            InitError::Boot(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for InitError {}

impl From<std::io::Error> for InitError {
    fn from(e: std::io::Error) -> Self {
        InitError::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, InitError>;
