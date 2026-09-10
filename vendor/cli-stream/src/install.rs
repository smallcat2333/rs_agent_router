//! Streamed install / sign-in progress events.
//!
//! The neutral event vocabulary a subprocess install (or login) flow
//! reports through. Both transports (axum SSE, Tauri Channel) serialize
//! these identically — keep the field names stable, the TypeScript
//! front-end consumes them verbatim. The concrete install *scripts*
//! (e.g. bob's `install-bob.sh`) live in the per-harness crates; this is
//! just the shape they emit.

use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum InstallEvent {
    /// A `[…-INSTALL]`-prefixed marker line. Drives UI checkpoints
    /// without parsing prose.
    Step { text: String },
    /// Non-marker stdout. Curl progress, npm output, etc.
    Stdout { text: String },
    /// stderr line. Often warnings but sometimes the real error.
    Stderr { text: String },
    /// Terminal event. Always sent exactly once at the end.
    Done { exit_code: Option<i32>, ok: bool },
}
