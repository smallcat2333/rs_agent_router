//! Typed errors for the streaming engine.

/// Why [`Command::stream`](crate::Command::stream) or
/// [`ProcessHandle::cancel`](crate::ProcessHandle::cancel) failed.
///
/// Carries the real underlying [`std::io::Error`] as a source (via
/// [`std::error::Error::source`]) rather than a pre-formatted string, so a
/// caller can downcast or inspect the OS error (e.g. distinguish
/// `NotFound` — the binary isn't on `PATH` — from `PermissionDenied`).
/// `#[non_exhaustive]` so adding a variant later isn't a breaking change.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StreamError {
    /// The child process could not be spawned: the binary isn't on `PATH`,
    /// isn't executable, or the OS refused. `source` is the spawn `io::Error`
    /// (commonly `NotFound`).
    #[error("failed to spawn {program}: {source}")]
    Spawn {
        /// The program that failed to launch (as passed to the engine).
        program: String,
        /// The OS error from `Command::spawn`.
        #[source]
        source: std::io::Error,
    },

    /// Writing to the child's stdin failed — most often because it has already
    /// exited, so the pipe is gone. A caller waiting on an answer needs to hear
    /// this rather than block forever.
    #[error("writing to the child's stdin failed: {source}")]
    Write {
        /// The OS error from the write or flush.
        #[source]
        source: std::io::Error,
    },

    /// The spawned child didn't expose a piped stdout/stderr. Shouldn't
    /// happen given the engine requests `Stdio::piped()`, but `Child`'s pipe
    /// accessors return `Option`, so the case is represented rather than
    /// `unwrap`ped.
    #[error("child {stream} pipe was not captured")]
    PipeNotCaptured {
        /// Which stream was missing — `"stdin"`, `"stdout"` or `"stderr"`.
        stream: &'static str,
    },

    /// Cancellation couldn't acquire the child lock because it was poisoned
    /// (a thread panicked while holding it). The process may still be
    /// running; the caller can retry or give up.
    #[error("cancel failed: the child lock was poisoned")]
    CancelLockPoisoned,
}
