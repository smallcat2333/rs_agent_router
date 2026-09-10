# cli-stream

A small, generic **streaming subprocess engine** for Rust: spawn a child
process, stream its stdout/stderr line-by-line through a callback, and cancel
it cleanly (SIGTERM → SIGKILL). Plus an optional `PATH` helper so a packaged
GUI app (a macOS `.app` launched from Finder, with a minimal environment) can
still find CLIs installed via nvm / Homebrew / asdf / volta.

No domain knowledge — just process streaming. Useful to anything that drives a
child CLI: a task runner, a build/test wrapper, a TUI, a desktop app shelling
out to tools.

```toml
cli-stream = "0.5.0"
```

```rust
use cli_stream::{spawn_streaming, ProcessEvent};

let handle = spawn_streaming(
    "git".into(),
    vec!["log".into(), "--oneline".into(), "-5".into()],
    vec![],                       // extra env
    std::env::current_dir()?,     // cwd
    "git-log".to_string(),        // a run id echoed on every event
    |event| match event {
        ProcessEvent::Stdout { line, .. } => println!("{line}"),
        ProcessEvent::Stderr { line, .. } => eprintln!("{line}"),
        ProcessEvent::Exited { exit_code, .. } => println!("done: {exit_code:?}"),
        _ => {}
    },
)?;
// handle.cancel()?;  // SIGTERM, then SIGKILL after a grace period
```

## What you get

- **`spawn_streaming(program, args, env, cwd, run_id, callback)`** → a
  `ProcessHandle`, streaming `ProcessEvent`s (Started / Stdout / Stderr / Error
  / Exited) from reader threads. Errors are a typed `StreamError` — `Spawn`
  carries the underlying `io::Error`, so "not on PATH" is distinguishable from
  "permission denied".
- **`ProcessHandle::cancel()`** — SIGTERM, then SIGKILL after a grace period.
  Polls `try_wait` (no blocking `wait` under a lock), so it terminates a
  *running* child promptly, not just on its next line of output.
- **Cancelling ends the tree.** A child that starts its own children and exits
  would otherwise leave them holding the stdout they inherited: the pipe never
  closes, so no `Exited` arrives and a caller waiting on the stream waits
  forever. The child leads its own process group on unix and is placed in a Job
  Object on Windows, so the signal or the terminate reaches everything it
  started.
- **`InstallEvent`** — a sibling event shape for streamed install/setup output,
  for hosts that run their own setup steps.

The environment is the caller's: this spawns what it is told to spawn, with the
`PATH` it is given. Finding a CLI a user installed — resolving a bare name,
locating the runtime it was installed under — is a different question, and
`agent-harness` answers it.

## License

MIT OR Apache-2.0.
