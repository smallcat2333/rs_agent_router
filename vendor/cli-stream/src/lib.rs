//! Streaming subprocess control: spawn a child, read its **stdout** and
//! **stderr** line-by-line, write to its **stdin**, and cancel it
//! (SIGTERM → SIGKILL on unix, `TerminateProcess` elsewhere). No console
//! window is flashed on Windows.
//!
//! It knows no CLI's output format and no agent's protocol — it moves lines.
//!
//! A child gets **pipes, never a terminal**, so `isatty` is false for it. That
//! is usually what you want — no colour codes, no progress bars — but a CLI
//! built around interactive prompts will refuse or hang, so run those in
//! whatever non-interactive mode they offer. [`needs_terminal`] recognises the
//! complaint when one slips through.
//!
//! ```no_run
//! use cli_stream::{Command, Event};
//!
//! # fn main() -> Result<(), cli_stream::StreamError> {
//! // stdout and stderr are always streamed.
//! let (handle, events) = Command::new("some-cli").args(["--verbose"]).start()?;
//! for event in events {
//!     match event {
//!         Event::Stdout { line, .. } => println!("out: {line}"),
//!         Event::Stderr { line, .. } => eprintln!("err: {line}"),
//!         _ => {}
//!     }
//! }
//! # let _ = handle;
//! # Ok(())
//! # }
//! ```
//!
//! Stdin is opt-in, because a child that inherits a terminal's stdin can block
//! forever waiting for input nobody is typing. Ask for it and answer the child
//! as it asks — the handle is yours for the whole run:
//!
//! ```no_run
//! use cli_stream::{Command, Event, Stdin};
//!
//! # fn main() -> Result<(), cli_stream::StreamError> {
//! let (handle, events) = Command::new("some-cli").stdin(Stdin::Piped).start()?;
//! for event in events {
//!     if let Event::Stdout { line, .. } = event {
//!         if line.contains("Password:") {
//!             handle.write_line("hunter2")?;
//!         }
//!     }
//! }
//! # Ok(())
//! # }
//! ```
//!
//! [`Command::stream`] takes a callback instead, for a caller forwarding onto a
//! sink rather than looping.
//!
//! [`InstallEvent`] is the sibling shape for streamed install/login output.
//!
//! A deliberate *leaf*: it depends on nothing of ours, so anything driving a
//! CLI can use it. Finding a CLI a user installed — resolving a bare name,
//! locating the `node` it was installed beside — is a different question, and
//! lives with the caller that needs it (`agent_harness::program_path`).

pub mod error;
pub mod install;
pub mod process;

pub use error::StreamError;
pub use install::InstallEvent;
pub use process::{hidden_command, needs_terminal, Command, Event, ProcessHandle, Stderr, Stdin};
