//! Streaming subprocess control: spawn a child, pipe its stdout/stderr
//! line-by-line through a callback as [`Event`]s, and hand back a
//! [`ProcessHandle`] for cancellation (SIGTERM → SIGKILL).
//!
//! The environment is the caller's: this spawns what it is told to spawn, with
//! the `PATH` it is given. Finding a CLI a user installed — resolving a bare
//! name, locating the `node` it was installed under — is a different question,
//! and one only a caller driving such a CLI needs answered.
//!
//! Cancellation is the wrinkle: a run needs to be stoppable mid-stream
//! when the user closes the tab or hits "stop". `ProcessHandle::cancel()`
//! sends SIGTERM to the child's process group (SIGKILL fallback) on unix and
//! terminates its Job Object on Windows, then flips an atomic `cancelled` flag
//! the reader threads use to short-circuit. The tree, not just the process:
//! anything the child started inherited the pipe, so leaving it alive leaves
//! the stream open.

use crate::error::StreamError;
use serde::Serialize;
use std::io::{BufRead, BufReader, Read};
use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

/// Raw events emitted to the caller's callback during a streaming run.
/// JSON-tagged so axum SSE and Tauri Channel render identical payloads
/// on the wire. Harness-neutral: a process-backed adapter parses the
/// `Stdout` lines into a normalized event vocabulary (e.g. `agent-harness`'s `RunEvent`).
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
// New lifecycle events can be added without breaking downstream matches —
// consumers must carry a `_` arm. (Construction of existing variants is
// unaffected, so it's still ergonomic to build them.)
#[non_exhaustive]
pub enum Event {
    /// First event. Sent before the child has produced any output so the
    /// UI can show a "thinking…" state.
    Started { run_id: String },
    /// Raw stdout line. Process-backed CLIs emit one JSON object per line
    /// in their streaming mode. The caller parses.
    Stdout { run_id: String, line: String },
    /// Raw stderr line. Warnings + the occasional error.
    Stderr { run_id: String, line: String },
    /// Command / IO failure. Terminal — followed by `Exited`.
    Error { run_id: String, message: String },
    /// Process exited. Always sent exactly once at the end.
    Exited {
        run_id: String,
        exit_code: Option<i32>,
        /// True iff `cancel()` was called before exit.
        cancelled: bool,
    },
}

/// Handle to an in-flight streaming run. Caller stores it (e.g. in a
/// runId-keyed map) so a later `cancel()` can find it.
///
/// Dropping the handle does NOT cancel the run — the reader threads +
/// wait thread continue independently. Use `cancel()` explicitly when
/// the user closes the connection.
#[derive(Clone, Debug)]
pub struct ProcessHandle {
    inner: Arc<HandleInner>,
}

/// How many events `start` buffers before the reader threads wait.
///
/// Unbounded would mean a chatty child and a slow consumer growing memory
/// without limit. Bounded turns that into backpressure instead: enough that a
/// consumer doing ordinary work never feels it, small enough that a runaway
/// child cannot exhaust memory before anyone notices.
const EVENT_BUFFER: usize = 1024;

/// What the child's stdin is connected to.
///
/// Most CLIs get everything as arguments and want [`Closed`](Stdin::Closed): a
/// child that inherits a terminal's stdin can block forever waiting for input
/// nobody is typing. A child that *answers* — a JSON-RPC server over stdio —
/// needs [`Piped`](Stdin::Piped) and [`ProcessHandle::write_line`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stdin {
    #[default]
    Closed,
    Piped,
}

/// What happens to the child's stderr.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Stderr {
    /// Read it and deliver each line as [`Event::Stderr`].
    #[default]
    Streamed,
    /// Send it to the null device. The OS discards it, so a chatty child
    /// costs nothing and can never block on a full pipe — right for a server
    /// whose stderr is its own logging.
    Discarded,
}

/// What to spawn. Named fields rather than six positional arguments, so a call
/// site says which string is the program and which is the run id, and a new
/// knob is a field with a default instead of a break.
#[derive(Debug, Clone)]
pub struct Command {
    pub program: PathBuf,
    pub args: Vec<String>,
    /// Extra environment for the child, applied over the inherited one.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    /// The caller's correlation id, echoed on every [`Event`].
    pub run_id: String,
    pub stdin: Stdin,
    pub stderr: Stderr,
    /// Give up after this long. `None` (the default) waits indefinitely — the
    /// right answer for an agent run a user is watching and can stop, and the
    /// wrong one for anything unattended.
    pub timeout: Option<Duration>,
}

impl Command {
    /// A run of `program`, in the current directory, with stdin closed.
    ///
    /// The program is the only thing a spawn cannot default, so it is the only
    /// argument. Everything else is a named method — three bare strings in a
    /// row read as "which one was the cwd again?".
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: std::env::current_dir().unwrap_or_default(),
            run_id: String::new(),
            stdin: Stdin::Closed,
            stderr: Stderr::Streamed,
            timeout: None,
        }
    }

    /// Where the child runs. Defaults to the current directory.
    #[must_use]
    pub fn cwd(mut self, cwd: impl Into<PathBuf>) -> Self {
        self.cwd = cwd.into();
        self
    }

    /// A correlation id echoed on every [`Event`], for a caller
    /// multiplexing several runs through one callback. Defaults to empty —
    /// with one run the handle already identifies it.
    #[must_use]
    pub fn run_id(mut self, run_id: impl Into<String>) -> Self {
        self.run_id = run_id.into();
        self
    }

    /// `.args(["--stdio"])` — anything string-like, borrowed or owned.
    #[must_use]
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args = args.into_iter().map(Into::into).collect();
        self
    }

    /// `.env([("RUST_LOG", "info")])` — applied over the inherited environment.
    #[must_use]
    pub fn env<I, K, V>(mut self, env: I) -> Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: Into<String>,
        V: Into<String>,
    {
        self.env = env.into_iter().map(|(k, v)| (k.into(), v.into())).collect();
        self
    }

    /// What the child's stdin is connected to. `stdout` and `stderr` are always
    /// piped — streaming them is what this crate is for — and arrive as
    /// [`Event::Stdout`] / [`Event::Stderr`].
    #[must_use]
    pub fn stdin(mut self, stdin: Stdin) -> Self {
        self.stdin = stdin;
        self
    }

    /// Whether the child's stderr is streamed or thrown away.
    #[must_use]
    pub fn stderr(mut self, stderr: Stderr) -> Self {
        self.stderr = stderr;
        self
    }

    /// Stop the child if it is still running after `timeout`, the same way
    /// [`ProcessHandle::cancel`] would — so it exits `cancelled: true` rather
    /// than hanging a caller that has nobody to press stop.
    #[must_use]
    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Run it, reading events off a channel.
    ///
    /// The channel closes on its own when the run ends: the forwarding closure
    /// is the only owner of the `Sender`, and it drops with the reader threads.
    pub fn start(self) -> Result<(ProcessHandle, std::sync::mpsc::Receiver<Event>), StreamError> {
        let (tx, rx) = std::sync::mpsc::sync_channel(EVENT_BUFFER);
        let handle = self.stream(move |event| {
            // Blocking here is the point: a full buffer stalls the reader
            // thread, which stops draining the child's pipe, which slows the
            // child. Memory stays bounded and no line is lost. A hung-up
            // receiver returns Err immediately rather than blocking, so a
            // caller that stopped reading does not wedge the run.
            let _ = tx.send(event);
        })?;
        Ok((handle, rx))
    }

    /// Run it, pushing each event to `callback` as it happens — for a caller
    /// forwarding straight onto a sink rather than looping.
    pub fn stream<F>(self, callback: F) -> Result<ProcessHandle, StreamError>
    where
        F: FnMut(Event) + Send + Sync + Clone + 'static,
    {
        spawn_streaming(self, callback)
    }
}


/// Windows has no signals and no process groups, so `TerminateProcess` on the
/// child leaves everything the child started running — holding the stdout
/// handle it inherited, which keeps the stream open and means no `Exited` ever
/// arrives. A Job Object is the OS's handle on "this program and everything it
/// starts": a process created by a process already in a job joins that job, so
/// assigning the direct child covers the tree beneath it.
///
/// Best-effort throughout. Every step can fail on a locked-down system, and a
/// cancel that ends only the direct child is what this crate did before — worse
/// than a tree kill, better than refusing to spawn.
#[cfg(windows)]
pub(crate) mod job {
    use std::os::windows::io::AsRawHandle;
    use std::process::Child;

    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::System::JobObjects::{
        AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject, TerminateJobObject,
        JobObjectExtendedLimitInformation, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
        JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    };

    /// An owned job handle. Closing it kills whatever is still inside, which is
    /// the backstop for a child that outlives the handle without being
    /// cancelled — the same orphan a crash would otherwise leave behind.
    ///
    /// `Debug` prints nothing useful about a raw handle, but `HandleInner`
    /// derives it, so the field needs one.
    pub(crate) struct Job(HANDLE);

    impl std::fmt::Debug for Job {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.write_str("Job(<handle>)")
        }
    }

    // SAFETY: a job handle is just a kernel handle; the Win32 calls that take
    // it are thread-safe, and nothing here holds interior state.
    unsafe impl Send for Job {}
    unsafe impl Sync for Job {}

    impl Job {
        /// Create a job whose members die when the last handle to it closes,
        /// and put `child` in it. `None` if the OS refuses any step, in which
        /// case cancelling falls back to ending the child alone.
        pub(crate) fn containing(child: &Child) -> Option<Self> {
            // SAFETY: a null name and null attributes are the documented way to
            // create an unnamed job; the return is checked for null.
            let handle = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if handle.is_null() {
                return None;
            }
            let job = Self(handle);

            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION =
                unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            // SAFETY: `limits` is a correctly-sized, fully-initialised struct of
            // the class named, and lives for the duration of the call.
            let set = unsafe {
                SetInformationJobObject(
                    job.0,
                    JobObjectExtendedLimitInformation,
                    std::ptr::addr_of!(limits).cast(),
                    std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
                )
            };
            if set == 0 {
                return None;
            }

            // SAFETY: the handle comes from a live `Child` this call does not
            // outlive, and the job handle is owned by `job`.
            let assigned =
                unsafe { AssignProcessToJobObject(job.0, child.as_raw_handle() as HANDLE) };
            (assigned != 0).then_some(job)
        }

        /// Kill every process in the job.
        pub(crate) fn terminate(&self) {
            // SAFETY: `self.0` is a live job handle owned by `self`.
            unsafe { TerminateJobObject(self.0, 1) };
        }
    }

    impl Drop for Job {
        fn drop(&mut self) {
            // SAFETY: owned handle, closed exactly once.
            unsafe { CloseHandle(self.0) };
        }
    }
}

#[derive(Debug)]
struct HandleInner {
    child: Mutex<Option<Child>>,
    /// The job the child was put in, so cancelling can end the tree. `None`
    /// when the OS refused, which degrades to ending the child alone.
    #[cfg(windows)]
    job: Option<job::Job>,
    /// The child's stdin, when it was piped. Taken from the `Child` at spawn so
    /// writing never has to lock the same mutex `cancel` uses.
    stdin: Mutex<Option<std::process::ChildStdin>>,
    cancelled: AtomicBool,
}

impl ProcessHandle {
    /// SIGTERM the process, then SIGKILL after 1.5s if it's still alive.
    /// The CLI is supposed to flush a final result on SIGTERM but we
    /// don't trust it to do so forever.
    ///
    /// Ends the **whole tree**, not just the process named. A child that
    /// starts its own children and exits would otherwise leave them holding
    /// the stdout they inherited: the pipe never closes, so no
    /// [`Event::Exited`] arrives and a caller waiting on the stream waits
    /// forever.
    ///
    /// The child leads its own process group on unix (set at spawn) and is put
    /// in a Job Object on Windows, so the signal or the terminate reaches
    /// everything it started. Both are best-effort — if the OS refuses, this
    /// falls back to ending the named process alone.
    pub fn cancel(&self) -> Result<(), StreamError> {
        self.inner.cancelled.store(true, Ordering::SeqCst);
        let mut guard = self
            .inner
            .child
            .lock()
            .map_err(|_| StreamError::CancelLockPoisoned)?;
        let Some(child) = guard.as_mut() else {
            // Already exited.
            return Ok(());
        };
        // Best-effort SIGTERM. On Unix, kill() sends SIGKILL by default;
        // we use libc::kill for SIGTERM, falling back to child.kill() if
        // the libc call fails. On Windows there's only TerminateProcess
        // via .kill().
        #[cfg(unix)]
        {
            let pid = child.id() as i32;
            // The *group*, not the process: the child leads its own group (set
            // at spawn), so a negative pid reaches everything it started. A
            // shell that backgrounds its work would otherwise survive as an
            // orphan holding the pipe, and the stream would never close.
            // SAFETY: `-pid` names the group this child leads; SIGTERM to a
            // group is well-defined, and a group that has already exited is a
            // harmless ESRCH.
            unsafe { libc::kill(-pid, libc::SIGTERM) };
            // Command the SIGKILL fallback inline to avoid holding the mutex
            // while sleeping.
            let inner = Arc::clone(&self.inner);
            thread::spawn(move || {
                thread::sleep(Duration::from_millis(1500));
                if let Ok(mut guard) = inner.child.lock()
                    && let Some(child) = guard.as_mut()
                {
                    // The group again, for the same reason.
                    // SAFETY: as above.
                    unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
                }
            });
        }
        #[cfg(windows)]
        {
            // The job ends the whole tree at once. Without one — the OS refused
            // to create or assign it — this is the old behaviour: the child
            // dies and anything it started does not.
            match &self.inner.job {
                Some(job) => job.terminate(),
                None => {
                    let _ = child.kill();
                }
            }
        }
        #[cfg(not(any(unix, windows)))]
        {
            let _ = child.kill();
        }
        Ok(())
    }

    /// Send one line to the child's stdin, newline-terminated and flushed.
    ///
    /// There is only one stream a caller can write to, so the name does not
    /// repeat it — [`Stdin::Piped`] on the command is where that was said.
    ///
    /// `Err` when the child was spawned with [`Stdin::Closed`] (the default), or
    /// when it has exited and the pipe is gone — both of which a caller
    /// expecting an answer needs to hear about rather than block on.
    pub fn write_line(&self, line: &str) -> Result<(), StreamError> {
        self.write(line.as_bytes())?;
        self.write(b"\n")
    }

    /// Send raw bytes to the child's stdin, flushed.
    ///
    /// [`write_line`](Self::write_line) covers newline-delimited protocols,
    /// which most CLIs and MCP's stdio transport use. This is for the ones that
    /// frame differently — LSP counts bytes in a `Content-Length` header, and a
    /// stray newline there is a protocol error.
    pub fn write(&self, bytes: &[u8]) -> Result<(), StreamError> {
        let mut guard = self.inner.stdin.lock().map_err(|_| StreamError::CancelLockPoisoned)?;
        let stdin = guard.as_mut().ok_or(StreamError::PipeNotCaptured { stream: "stdin" })?;
        use std::io::Write;
        stdin.write_all(bytes).and_then(|()| stdin.flush()).map_err(|source| StreamError::Write { source })
    }

    /// Close the child's stdin, so its next read is EOF.
    ///
    /// In a newline protocol EOF is itself a message: Claude Code's stream-json
    /// input reads it as "finish the current turn and exit", and a child that
    /// never sees it waits for the next line forever. Idempotent — a second
    /// close, or a close on a child spawned with [`Stdin::Closed`], changes
    /// nothing. A later [`write`](Self::write) reports the pipe as not captured.
    pub fn close_stdin(&self) -> Result<(), StreamError> {
        let mut guard = self.inner.stdin.lock().map_err(|_| StreamError::CancelLockPoisoned)?;
        guard.take();
        Ok(())
    }

    /// Whether `cancel()` was called. Tagged on the final `Exited` event.
    pub fn was_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::SeqCst)
    }

    /// The child's OS process id while it's alive, or `None` once it has been
    /// reaped (the `Child` is taken on exit). Lets an embedder record the pid
    /// so a child orphaned by a hard crash can be killed on the next launch.
    pub fn pid(&self) -> Option<u32> {
        self.inner
            .child
            .lock()
            .ok()
            .and_then(|guard| guard.as_ref().map(Child::id))
    }
}

/// Command an arbitrary streaming child process — the generic engine behind
/// every process-backed harness (bob, Claude Code, Codex).
///
/// Pipes stdout/stderr line-by-line through `callback` using the raw
/// [`Event`] vocabulary (Started / Stdout / Stderr / Error /
/// Exited). `env` is applied over the inherited environment — secrets, or a
/// `PATH` the caller resolved. This crate does not augment `PATH` itself;
/// knowing where a user's toolchain lives belongs to whoever is driving the
/// CLI. Returns a [`ProcessHandle`] for cancellation.
///
/// `callback` is invoked from three threads (stdout reader, stderr
/// reader, exit watcher); the `Clone` bound lets us hand a copy to each.
/// `run_id` is opaque — the caller chooses it and uses it to correlate
/// events with the handle.
///
/// ```no_run
/// use cli_stream::{Command, Event};
///
/// # fn main() -> Result<(), cli_stream::StreamError> {
/// let handle = Command::new("echo").args(["hello"]).stream(|event| match event {
///     Event::Stdout { line, .. } => println!("{line}"),
///     Event::Exited { exit_code, .. } => eprintln!("exit {exit_code:?}"),
///     _ => {}
/// })?;
/// // `handle.cancel()` stops it early; dropping the handle does not.
/// let _ = handle;
/// # Ok(())
/// # }
/// ```
///
pub(crate) fn spawn_streaming<F>(spawn: Command, callback: F) -> Result<ProcessHandle, StreamError>
where
    F: FnMut(Event) + Send + Sync + Clone + 'static,
{
    let Command { program, args, env, cwd, run_id, stdin, stderr, timeout } = spawn;
    let mut command = hidden_command(&program);
    command
        .args(&args)
        .current_dir(&cwd)
        .stdin(match stdin {
            Stdin::Closed => Stdio::null(),
            Stdin::Piped => Stdio::piped(),
        })
        .stdout(Stdio::piped())
        .stderr(match stderr {
            Stderr::Streamed => Stdio::piped(),
            Stderr::Discarded => Stdio::null(),
        });
    for (key, value) in &env {
        command.env(key, value);
    }
    // Its own process group, so cancelling can signal the group and reach
    // whatever the child started. Without it a shell that backgrounds its work
    // leaves that work running, holding the stdout it inherited — and the
    // stream never closes.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|source| StreamError::Spawn {
        program: program.display().to_string(),
        source,
    })?;

    let stdout = child
        .stdout
        .take()
        .ok_or(StreamError::PipeNotCaptured { stream: "stdout" })?;
    // Absent by design when discarded — the OS is dropping it, so there is
    // nothing to read and no thread to spend on reading it.
    let stderr_pipe = child.stderr.take();

    // Taken now so `write_line` never contends with `cancel` for the child.
    let child_stdin = child.stdin.take();
    // Before anything else runs: a process the child starts joins its parent's
    // job automatically, so this covers the tree beneath it. The gap between
    // `spawn` returning and this line is the one moment a grandchild could
    // escape, which is why it is the next statement.
    #[cfg(windows)]
    let job = job::Job::containing(&child);

    let inner = Arc::new(HandleInner {
        child: Mutex::new(Some(child)),
        stdin: Mutex::new(child_stdin),
        cancelled: AtomicBool::new(false),
        #[cfg(windows)]
        job,
    });
    let handle = ProcessHandle {
        inner: Arc::clone(&inner),
    };

    // Emit Started immediately so the caller doesn't wait on the first
    // output line for a UI signal.
    let mut started_cb = callback.clone();
    started_cb(Event::Started {
        run_id: run_id.clone(),
    });

    // Reader threads. Each owns its own callback clone — the Clone bound
    // is the whole point.
    let stdout_cb = callback.clone();
    let stdout_run_id = run_id.clone();
    let stdout_handle = thread::spawn(move || {
        pump_lines(stdout, stdout_run_id, true, stdout_cb);
    });

    let stderr_handle = stderr_pipe.map(|pipe| {
        let stderr_cb = callback.clone();
        let stderr_run_id = run_id.clone();
        thread::spawn(move || pump_lines(pipe, stderr_run_id, false, stderr_cb))
    });

    // Exit watcher — emits the terminal Exited event with the cancellation
    // flag. It must NOT hold the child lock across a blocking `wait()`:
    // `cancel()` needs that same lock to signal the child, so a held lock
    // would block cancel until the process exited on its own (defeating it).
    // Instead poll `try_wait()`, locking only for each non-blocking check and
    // releasing between polls so `cancel()` can acquire the lock mid-run.
    let exit_inner = Arc::clone(&inner);
    let timeout_handle = handle.clone();
    let mut exit_cb = callback;
    let exit_run_id = run_id;
    thread::spawn(move || {
        let started = std::time::Instant::now();
        let wait_result = loop {
            {
                let mut guard = match exit_inner.child.lock() {
                    Ok(guard) => guard,
                    Err(_) => return, // poisoned — nothing safe to do
                };
                match guard.as_mut() {
                    Some(child) => match child.try_wait() {
                        Ok(Some(status)) => break Ok(status),
                        Ok(None) => {} // still running; poll again
                        Err(err) => break Err(err),
                    },
                    None => return, // already reaped
                }
            } // lock released before sleeping, so cancel() can acquire it
            // A run nobody is watching still has to end. Cancelling rather than
            // killing gives the child the same SIGTERM grace a user's stop
            // would, and the exit reports `cancelled` so the caller can tell
            // this apart from a child that finished on its own.
            if timeout.is_some_and(|limit| started.elapsed() >= limit) {
                let _ = timeout_handle.cancel();
            }
            thread::sleep(Duration::from_millis(50));
        };
        let _ = stdout_handle.join();
        if let Some(stderr_handle) = stderr_handle {
            let _ = stderr_handle.join();
        }
        let cancelled = exit_inner.cancelled.load(Ordering::SeqCst);

        match wait_result {
            Ok(status) => exit_cb(Event::Exited {
                run_id: exit_run_id.clone(),
                exit_code: status.code(),
                cancelled,
            }),
            Err(err) => exit_cb(Event::Error {
                run_id: exit_run_id.clone(),
                message: format!("wait failed: {err}"),
            }),
        }

        // Drop the child handle so subsequent cancel() calls
        // short-circuit cleanly.
        if let Ok(mut guard) = exit_inner.child.lock() {
            *guard = None;
        }
    });

    Ok(handle)
}

fn pump_lines<R, F>(reader: R, run_id: String, is_stdout: bool, mut callback: F)
where
    R: Read,
    F: FnMut(Event),
{
    let mut buffered = BufReader::new(reader);
    let mut bytes = Vec::new();
    loop {
        bytes.clear();
        match buffered.read_until(b'\n', &mut bytes) {
            Ok(0) => return,
            Ok(_) => {
                strip_eol(&mut bytes);
                // Lossy on purpose. A child's stdout is a byte stream, and
                // agent CLIs share it with progress bars, ANSI art and paths
                // in whatever encoding the filesystem gave them. Decoding
                // strictly makes one undecodable byte end the transcript,
                // taking the result line with it.
                let text = String::from_utf8_lossy(&bytes).into_owned();
                let event = if is_stdout {
                    Event::Stdout {
                        run_id: run_id.clone(),
                        line: text,
                    }
                } else {
                    Event::Stderr {
                        run_id: run_id.clone(),
                        line: text,
                    }
                };
                callback(event);
            }
            Err(err) => {
                callback(Event::Error {
                    run_id: run_id.clone(),
                    message: format!("stream read failed: {err}"),
                });
                return;
            }
        }
    }
}

/// Drop one trailing line terminator, `\n` or `\r\n`.
fn strip_eol(bytes: &mut Vec<u8>) {
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
}

/// Compose a PATH for the spawned process that always includes the
/// directory containing the program — where `node`, `npm`, and friends
/// usually live in an nvm install. The user's existing PATH stays as a
/// fallback after our prepended directory.
/// A [`Command`] that never opens a console window on Windows.
///
/// A GUI host (a Tauri app, an IDE) spawning a console-subsystem CLI gets a
/// black console flashed on screen for every agent run and every `--version`
/// probe. `CREATE_NO_WINDOW` suppresses it. Use this in place of
/// `Command::new` for anything a desktop app spawns; it is a plain
/// `Command::new` on every other platform, so call sites stay `cfg`-free.
/// Whether a line from a child suggests it wanted a terminal and did not get
/// one.
///
/// Every child spawned here gets **pipes**, never a TTY, so `isatty` is false
/// and a CLI may change what it prints or refuse to run. Most of the time that
/// is welcome — no colour codes, no progress bars — but a CLI built around
/// interactive prompts fails, and the message it gives is easy to miss among
/// ordinary stderr.
///
/// Recognising it turns a confusing exit into a next step: run the CLI in
/// whatever non-interactive mode it has (`--yes`, `-p`, `exec`, …).
pub fn needs_terminal(line: &str) -> bool {
    const SIGNS: &[&str] = &[
        "not a tty",
        "not a terminal",
        "is not interactive",
        "input device is not a tty",
        "raw mode is not supported",
        "non-tty environment",
        "requires a tty",
    ];
    let lowered = line.to_lowercase();
    SIGNS.iter().any(|sign| lowered.contains(sign))
}

pub fn hidden_command(program: impl AsRef<std::ffi::OsStr>) -> std::process::Command {
    #[allow(unused_mut)]
    let mut command = std::process::Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // https://learn.microsoft.com/windows/win32/procthread/process-creation-flags
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

#[cfg(test)]
mod tests {
    use proptest::prelude::*;
    use super::*;

    /// Issue #35: a GUI host spawning a console-subsystem CLI flashed a console
    /// window on Windows for every run and every `--version` probe. The flag is
    /// Windows-only, so what is portable to assert is that the constructor is a
    /// drop-in for `Command::new` — it still runs, and still captures output.
    #[test]
    fn hidden_command_runs_like_a_plain_command() {
        let program = if cfg!(windows) { "cmd" } else { "echo" };
        let args: &[&str] = if cfg!(windows) {
            &["/C", "echo", "ok"]
        } else {
            &["ok"]
        };
        let out = hidden_command(program).args(args).output().unwrap();
        assert!(out.status.success());
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "ok");
    }

    use std::sync::Condvar;
    use std::time::Instant;

    type Done = Arc<(Mutex<bool>, Condvar)>;

    /// A thread-safe event collector that signals `done` on the terminal
    /// event. Returns the (cloneable) callback + the shared collections.
    fn collector() -> (
        impl FnMut(Event) + Send + Sync + Clone + 'static,
        Arc<Mutex<Vec<Event>>>,
        Done,
    ) {
        let events = Arc::new(Mutex::new(Vec::new()));
        let done: Done = Arc::new((Mutex::new(false), Condvar::new()));
        let cb = {
            let events = Arc::clone(&events);
            let done = Arc::clone(&done);
            move |ev: Event| {
                let terminal =
                    matches!(ev, Event::Exited { .. } | Event::Error { .. });
                events.lock().unwrap().push(ev);
                if terminal {
                    let (lock, cvar) = &*done;
                    *lock.lock().unwrap() = true;
                    cvar.notify_all();
                }
            }
        };
        (cb, events, done)
    }

    /// Block until the terminal event fires, or panic after `secs`.
    fn wait_done(done: &Done, secs: u64) {
        let (lock, cvar) = &**done;
        let mut finished = lock.lock().unwrap();
        let deadline = Instant::now() + Duration::from_secs(secs);
        while !*finished {
            let now = Instant::now();
            assert!(now < deadline, "process did not finish within {secs}s");
            let (guard, _) = cvar.wait_timeout(finished, deadline - now).unwrap();
            finished = guard;
        }
    }

    /// Command `program args`, block until it exits, return every event.
    fn run(program: &str, args: &[&str]) -> Vec<Event> {
        let (cb, events, done) = collector();
        let _handle = spawn_streaming(
            Command::new(program).run_id("t").args(args.iter().copied()),
            cb,
        )
        .expect("spawn");
        wait_done(&done, 10);
        let events = events.lock().unwrap();
        events.clone()
    }

    /// Emit `alpha` and `beta` on separate lines. `printf` is not a program on
    /// Windows, and the shell there does not read `%s\n` as a format — the
    /// child printed `alphabeta` and the engine faithfully reported the one
    /// line it was given.
    fn two_lines() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("cmd", vec!["/C", "echo alpha&echo beta"])
        } else {
            ("printf", vec!["%s\n", "alpha", "beta"])
        }
    }

    #[test]
    fn streams_stdout_lines_then_exits_zero() {
        let (program, args) = two_lines();
        let events = run(program, &args);
        // Started leads, Exited(0, not cancelled) closes.
        assert!(matches!(events.first(), Some(Event::Started { .. })));
        assert!(matches!(
            events.last(),
            Some(Event::Exited {
                exit_code: Some(0),
                cancelled: false,
                ..
            })
        ));
        // Lines arrive in order, one event each.
        let lines: Vec<&str> = events
            .iter()
            .filter_map(|e| match e {
                Event::Stdout { line, .. } => Some(line.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(lines, vec!["alpha", "beta"]);
    }

    #[test]
    fn nonzero_exit_code_is_reported() {
        let events = run("sh", &["-c", "exit 3"]);
        assert!(matches!(
            events.last(),
            Some(Event::Exited {
                exit_code: Some(3),
                cancelled: false,
                ..
            })
        ));
    }

    #[test]
    fn env_vars_are_passed_to_the_child() {
        // The `env` argument must reach the child's environment — exercise it
        // directly (the other lifecycle tests pass an empty env).
        let (cb, events, done) = collector();
        let _handle = spawn_streaming(
            Command::new("sh").run_id("t").args(vec![
                "-c".to_owned(),
                "printf '%s\\n' \"$CLI_STREAM_STUB\"".to_owned(),
            ]).env(vec![("CLI_STREAM_STUB".to_owned(), "from-env".to_owned())]),
            cb,
        )
        .expect("spawn");
        wait_done(&done, 10);
        let events = events.lock().unwrap();
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::Stdout { line, .. } if line == "from-env")),
            "child should observe the injected env var, got {events:?}"
        );
    }

    #[test]
    fn stderr_is_streamed_and_not_misrouted_to_stdout() {
        let events = run("sh", &["-c", "echo to-stderr 1>&2"]);
        assert!(events
            .iter()
            .any(|e| matches!(e, Event::Stderr { line, .. } if line == "to-stderr")));
        assert!(!events
            .iter()
            .any(|e| matches!(e, Event::Stdout { .. })));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Exited {
                exit_code: Some(0),
                ..
            }
        )));
    }

    /// A single process that runs for ~10s and holds no children.
    ///
    /// The distinction matters to what cancelling can promise. On unix `exec`
    /// makes the shell *become* `sleep`, so there is one process and SIGTERM
    /// reaches it. Windows has no `exec` and cancelling is `TerminateProcess`,
    /// which ends the process it names and not its descendants — so a shell
    /// wrapper there would leave the sleeper running, holding the pipe open,
    /// and no `Exited` would ever arrive. `ping` is the sleeper itself.
    fn long_sleeper() -> (&'static str, Vec<&'static str>) {
        if cfg!(windows) {
            ("ping", vec!["-n", "11", "127.0.0.1"])
        } else {
            ("sh", vec!["-c", "exec sleep 10"])
        }
    }

    /// The case that was silently broken: a child that starts its own child
    /// and exits, leaving the grandchild holding the stdout it inherited. Kill
    /// only the named process and that pipe stays open, so `Exited` never
    /// arrives and a caller waiting on the stream waits forever.
    ///
    /// Unix needs the signal to reach the process *group*; Windows needs a Job
    /// Object. Both are set up at spawn, so this asserts the same promise on
    /// either platform.
    #[cfg(unix)]
    #[test]
    fn cancel_reaches_a_child_the_child_started() {
        let (cb, events, done) = collector();
        // `sh` exits immediately; `sleep` inherits stdout and outlives it.
        let handle = spawn_streaming(
            Command::new("sh").run_id("t").args(["-c", "sleep 30 &"]),
            cb,
        )
        .expect("spawn");

        let canceller = handle.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(200));
            let _ = canceller.cancel();
        });

        // Without the group signal the grandchild holds the pipe and this
        // times out — which is exactly what it did before.
        wait_done(&done, 6);
        let events = events.lock().unwrap();
        assert!(
            matches!(events.last(), Some(Event::Exited { .. })),
            "the stream must close once the tree is gone, got {:?}",
            events.last()
        );
    }

    #[test]
    fn cancel_promptly_terminates_the_run_and_flags_it() {
        // A 10s sleeper we cancel ~immediately; a working engine must kill it
        // far sooner than 10s.
        let (cb, events, done) = collector();
        let (program, args) = long_sleeper();
        let handle =
            spawn_streaming(Command::new(program).run_id("t").args(args), cb).expect("spawn");

        // cancel() may block until the child is reaped, so fire it off-thread.
        let canceller = handle.clone();
        thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            let _ = canceller.cancel();
        });

        // Correct cancellation terminates the 10s sleep within a few seconds.
        wait_done(&done, 4);
        assert!(handle.was_cancelled());
        let events = events.lock().unwrap();
        assert!(
            matches!(
                events.last(),
                Some(Event::Exited {
                    cancelled: true,
                    ..
                })
            ),
            "expected Exited(cancelled=true), got {:?}",
            events.last()
        );
    }

    #[test]
    fn a_cli_asking_for_a_terminal_is_recognised_however_it_phrases_it() {
        // Children get pipes, never a TTY. When that is the problem, the CLI
        // says so on stderr and the run otherwise looks like an unexplained
        // failure — so the phrasings worth catching are the common ones.
        for complaint in [
            "Error: stdin is not a TTY",
            "the input device is not a TTY",
            "Raw mode is not supported on the current process.stdin",
            "Prompts cannot be rendered in a non-TTY environment",
            "this command requires a TTY",
            "warning: stdout is not a terminal",
        ] {
            assert!(needs_terminal(complaint), "missed: {complaint}");
        }

        // And ordinary noise is left alone — mislabelling it would bury the
        // real message under an explanation of the wrong problem.
        for ordinary in ["npm WARN deprecated foo@1.0.0", "compiling 12 files", "", "tty"] {
            assert!(!needs_terminal(ordinary), "false positive: {ordinary}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_stops_a_child_that_would_otherwise_run_forever() {
        // Unattended runs have nobody to press stop. The child must end, and
        // the exit has to say it was stopped rather than that it finished.
        let started = Instant::now();
        let (_handle, events) = Command::new("sleep")
            .run_id("hung")
            .args(["30"])
            .timeout(Duration::from_millis(200))
            .start()
            .expect("spawn");

        let exit = events
            .into_iter()
            .find_map(|e| match e {
                Event::Exited { cancelled, .. } => Some(cancelled),
                _ => None,
            })
            .expect("the run ends");
        assert!(exit, "a timed-out run reports as cancelled, not as a clean finish");
        assert!(started.elapsed() < Duration::from_secs(10), "and does not wait out the sleep");
    }

    #[cfg(unix)]
    #[test]
    fn a_run_inside_its_timeout_is_untouched() {
        let (_handle, events) = Command::new("echo")
            .run_id("quick")
            .args(["done"])
            .timeout(Duration::from_secs(30))
            .start()
            .expect("spawn");
        let seen: Vec<Event> = events.into_iter().collect();
        assert!(seen.iter().any(|e| matches!(e, Event::Stdout { line, .. } if line == "done")));
        assert!(
            seen.iter().any(|e| matches!(e, Event::Exited { cancelled: false, .. })),
            "finished on its own: {seen:?}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn discarded_stderr_never_reaches_the_caller() {
        // A server whose stderr is its own logging should cost nothing: the OS
        // drops it, so there is no pipe to fill and no thread reading it.
        let noisy = "echo out; echo noise 1>&2";
        let (_h, events) = Command::new("sh")
            .run_id("quiet")
            .args(["-c", noisy])
            .stderr(Stderr::Discarded)
            .start()
            .expect("spawn");
        let seen: Vec<Event> = events.into_iter().collect();
        assert!(seen.iter().any(|e| matches!(e, Event::Stdout { line, .. } if line == "out")));
        assert!(!seen.iter().any(|e| matches!(e, Event::Stderr { .. })), "got {seen:?}");

        // And streamed is still the default.
        let (_h, events) = Command::new("sh").run_id("loud").args(["-c", noisy]).start().expect("spawn");
        assert!(events.into_iter().any(|e| matches!(e, Event::Stderr { line, .. } if line == "noise")));
    }

    #[cfg(unix)]
    #[test]
    fn closing_stdin_is_how_a_child_that_reads_until_eof_gets_to_exit() {
        // `cat` runs until its input ends. Nothing else this handle offers makes
        // it stop short of a signal, so the close is the only clean exit.
        let (handle, events) =
            Command::new("cat").run_id("eof").stdin(Stdin::Piped).start().expect("spawn");
        handle.write_line("last words").expect("still open");
        handle.close_stdin().expect("close");
        handle.close_stdin().expect("closing twice is not an error");

        let deadline = Instant::now() + Duration::from_secs(5);
        let exit = loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("the child never exited after EOF");
            match events.recv_timeout(left) {
                Ok(Event::Exited { exit_code, cancelled, .. }) => break (exit_code, cancelled),
                Ok(_) => continue,
                Err(err) => panic!("no exit arrived: {err}"),
            }
        };
        assert_eq!(exit, (Some(0), false), "EOF ended it, not a signal");

        let err = handle.write_line("too late").unwrap_err();
        assert!(matches!(err, StreamError::PipeNotCaptured { stream: "stdin" }), "got {err}");
    }

    #[cfg(unix)]
    #[test]
    fn writing_needs_a_pipe_that_was_asked_for_and_a_child_still_listening() {
        // Both failures are ones a caller waiting on an answer has to hear
        // about: without them it blocks forever on a reply that is not coming.
        let quiet = Command::new("sleep").run_id("nostdin").args(["5"]).stream(|_| {}).expect("spawn");
        let err = quiet.write_line("anyone there?").unwrap_err();
        assert!(
            matches!(err, StreamError::PipeNotCaptured { stream: "stdin" }),
            "stdin was never piped, got {err}"
        );
        let _ = quiet.cancel();

        // `cat` echoes stdin, so it is listening until it is not.
        let (handle, events) =
            Command::new("cat").run_id("echoing").stdin(Stdin::Piped).start().expect("spawn");
        handle.write_line("hello").expect("a live child takes input");

        // Waited for with a deadline, not `events.iter()`. `cat` holds the
        // channel open for as long as it lives, so iterating blocks once the
        // queue drains — a version of this test that scanned for the line only
        // ever terminated *because* it was there, and hung on the failure it
        // exists to report.
        let deadline = Instant::now() + Duration::from_secs(5);
        let echoed = loop {
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("the child never echoed the line back");
            match events.recv_timeout(left) {
                Ok(Event::Stdout { line, .. }) => break line,
                Ok(_) => continue,
                Err(err) => panic!("nothing came back: {err}"),
            }
        };
        assert_eq!(echoed, "hello", "and reads it back");
        let _ = handle.cancel();
    }

    #[cfg(unix)]
    #[test]
    fn a_live_child_reports_a_pid_and_flips_when_cancelled() {
        // An embedder records the pid so a child a hard crash orphaned can be
        // reaped on the next launch, and reads `was_cancelled` to tell a run
        // the user stopped from one that finished. Both are answered by
        // forwarding, which is exactly the kind of code that silently returns
        // the wrong constant.
        let handle = spawn_streaming(
            Command::new("/bin/sleep").cwd(std::env::temp_dir()).run_id("pid").args(["30"]),
            |_| {},
        )
        .expect("sleep should spawn");

        let pid = handle.pid().expect("a live child has a pid");
        assert!(pid > 1, "a real OS pid, not a placeholder: {pid}");
        assert!(!handle.was_cancelled(), "nothing has stopped it yet");

        handle.cancel().expect("cancel");
        assert!(handle.was_cancelled(), "a stopped run says so");
    }

    #[test]
    fn spawning_a_missing_binary_is_err() {
        let result = spawn_streaming(
            Command::new("cli-stream-no-such-binary-zzz").run_id("t"),
            |_ev: Event| {},
        );
        // Typed: a `Spawn` error carrying the OS `NotFound` io::Error as its
        // source — the whole point of `StreamError` over a `String`. A caller
        // can branch on `ErrorKind` to tell "not installed" (NotFound) from
        // "permission denied", which a flattened string can't support.
        match result {
            Err(StreamError::Spawn { program, source }) => {
                assert!(program.contains("cli-stream-no-such-binary-zzz"));
                assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
            }
            other => panic!("expected StreamError::Spawn, got {other:?}"),
        }
    }

    fn pumped(bytes: &[u8]) -> Vec<Event> {
        let mut events = Vec::new();
        pump_lines(bytes, "t".to_owned(), true, |event| events.push(event));
        events
    }

    fn lines_of(events: &[Event]) -> Vec<String> {
        events
            .iter()
            .filter_map(|event| match event {
                Event::Stdout { line, .. } => Some(line.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn one_undecodable_byte_does_not_cost_us_the_rest_of_the_run() {
        // Agent CLIs write progress bars, ANSI art and the occasional raw byte
        // to the same pipe they write results to. A stream is a byte stream,
        // so the only safe reading is that a line we cannot decode is one
        // damaged line — not the end of the transcript.
        let mut bytes = b"first\n".to_vec();
        bytes.extend_from_slice(&[0xff, 0xfe]);
        bytes.extend_from_slice(b"\nlast\n");

        let lines = lines_of(&pumped(&bytes));

        assert_eq!(lines.first().map(String::as_str), Some("first"));
        assert_eq!(
            lines.last().map(String::as_str),
            Some("last"),
            "a line after the bad byte still arrives"
        );
        assert_eq!(lines.len(), 3, "the damaged line is kept, lossily");
    }

    /// Bytes shaped like a real child's stdout: mostly text, plenty of line
    /// terminators, and the high bytes that are never valid UTF-8 alone.
    /// Uniform `Vec<u8>` would hit `\n` once every 256 bytes and barely
    /// exercise the framing this is here to check.
    fn stream_bytes() -> impl Strategy<Value = Vec<u8>> {
        prop::collection::vec(
            prop_oneof![
                6 => 0x20u8..0x7f,
                3 => Just(b'\n'),
                1 => Just(b'\r'),
                2 => 0x80u8..=0xff,
            ],
            0..64,
        )
    }

    fn line_count(bytes: &[u8]) -> usize {
        if bytes.is_empty() {
            return 0;
        }
        let newlines = bytes.iter().filter(|byte| **byte == b'\n').count();
        newlines + usize::from(bytes.last() != Some(&b'\n'))
    }

    proptest! {
        /// Framing is a question about newlines, so it cannot depend on whether
        /// the bytes between them decode. This is the property the lossy fix is
        /// really about: strict decoding satisfied it only for valid UTF-8.
        #[test]
        fn every_line_the_child_wrote_is_one_the_caller_sees(bytes in stream_bytes()) {
            let events = pumped(&bytes);
            prop_assert_eq!(lines_of(&events).len(), line_count(&bytes));
            prop_assert!(
                !events.iter().any(|event| matches!(event, Event::Error { .. })),
                "no byte sequence is a read failure",
            );
        }

        /// A line never carries the delimiter that ended it. Only `\n`
        /// delimits: a bare `\r` is content — it is how a progress bar
        /// overwrites itself — and is stripped only as part of a `\r\n` pair.
        #[test]
        fn no_line_smuggles_its_delimiter(bytes in stream_bytes()) {
            for line in lines_of(&pumped(&bytes)) {
                prop_assert!(!line.contains('\n'), "got {line:?}");
            }
        }

        /// And for text, lossiness costs nothing: what the child wrote is
        /// exactly what the caller reads.
        #[test]
        fn text_arrives_unchanged(lines in prop::collection::vec("[^\r\n]{0,24}", 0..8)) {
            let written: String = lines.iter().map(|line| format!("{line}\n")).collect();
            prop_assert_eq!(lines_of(&pumped(written.as_bytes())), lines);
        }
    }
}
