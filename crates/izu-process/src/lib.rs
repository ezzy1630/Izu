#![forbid(unsafe_code)]
//! Cooperative owned Unix process groups with bounded nonblocking capture.
#![cfg(unix)]

use rustix::{
    fs::{OFlags, fcntl_getfl, fcntl_setfl},
    process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid},
};
use std::{
    ffi::{OsStr, OsString},
    fmt,
    io::{self, Read, Write},
    net::{Ipv4Addr, TcpListener, TcpStream},
    os::{
        fd::AsFd,
        unix::{
            ffi::{OsStrExt, OsStringExt},
            fs::PermissionsExt,
            net::{SocketAddr, UnixListener, UnixStream},
            process::{CommandExt, ExitStatusExt},
        },
    },
    path::PathBuf,
    process::{Child, ChildStderr, ChildStdin, ChildStdout, Command, ExitStatus, Stdio},
    thread,
    time::{Duration, Instant},
};

const MAGIC: &[u8] = b"IZUP\x01";
const MAX_TICKET: usize = 1024 * 1024;
const MAX_STATUS: usize = 256;
const STEP: usize = 16 * 1024;
const TICK: Duration = Duration::from_millis(1);

#[derive(Clone, Debug)]
pub struct WorkerLauncher {
    pub executable: PathBuf,
    pub prefix_args: Vec<OsString>,
}
/// Exact raw command data, before std::Command substitutes invalid NUL strings
/// or hides environment and other launch attributes. Only these fields cross
/// the worker boundary; stdin and process ownership are controlled by the runner.
#[derive(Clone)]
pub struct CommandSpec {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub directory: Option<PathBuf>,
    pub environment: Vec<(OsString, Option<OsString>)>,
}
impl CommandSpec {
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            directory: None,
            environment: Vec::new(),
        }
    }
    pub fn arg(&mut self, arg: impl Into<OsString>) -> &mut Self {
        self.args.push(arg.into());
        self
    }
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }
    pub fn current_dir(&mut self, path: impl Into<PathBuf>) -> &mut Self {
        self.directory = Some(path.into());
        self
    }
    pub fn env(&mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> &mut Self {
        let key = key.into();
        self.environment.retain(|(k, _)| *k != key);
        self.environment.push((key, Some(value.into())));
        self
    }
    pub fn env_remove(&mut self, key: impl Into<OsString>) -> &mut Self {
        let key = key.into();
        self.environment.retain(|(k, _)| *k != key);
        self.environment.push((key, None));
        self
    }
}
impl fmt::Debug for CommandSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CommandSpec")
            .field("argument_count", &self.args.len())
            .field("environment_count", &self.environment.len())
            .field("directory_set", &self.directory.is_some())
            .finish_non_exhaustive()
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BaseEnvironment {
    Inherit,
    Clear,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OutputPolicy {
    Terminate,
    TruncateDrain,
}
#[derive(Clone, Debug)]
pub struct RunOptions {
    pub stdin_limit: usize,
    pub stdout_limit: usize,
    pub stderr_limit: usize,
    pub timeout: Duration,
    pub cleanup_timeout: Duration,
    pub output_policy: OutputPolicy,
    pub base_environment: BaseEnvironment,
}
impl Default for RunOptions {
    fn default() -> Self {
        Self {
            stdin_limit: 16 * 1024 * 1024,
            stdout_limit: 16 * 1024 * 1024,
            stderr_limit: 64 * 1024,
            timeout: Duration::from_secs(30),
            cleanup_timeout: Duration::from_secs(2),
            output_policy: OutputPolicy::Terminate,
            base_environment: BaseEnvironment::Clear,
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Protocol,
    Stdin,
    Stdout,
    Stderr,
    Observe,
    TargetSpawn,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Termination {
    Completed,
    Cancelled,
    TimedOut,
    OutputLimit(Phase),
    Io { phase: Phase, errno: Option<i32> },
    Protocol,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupSignal {
    Sent,
    AlreadyAbsent,
    Uncertain(Option<i32>),
    OwnershipLost,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GroupQuiescence {
    AbsentObserved,
    Present,
    Uncertain(Option<i32>),
    NotProbed,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CleanupReport {
    pub group_signal: GroupSignal,
    pub group_quiescence: GroupQuiescence,
    pub leader_reaped: bool,
    pub leader_status: Option<ExitStatus>,
}
impl CleanupReport {
    /// Owned leader-reap and observed group-absence evidence, never containment.
    /// Descendants that change group/session/UID are outside this statement.
    pub fn cooperative_stop_succeeded(&self) -> bool {
        self.group_quiescence == GroupQuiescence::AbsentObserved
            && self.group_signal != GroupSignal::OwnershipLost
            && self.leader_reaped
    }
}
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Capture {
    pub bytes: Vec<u8>,
    pub dropped: u64,
    pub eof: bool,
}
#[derive(Debug)]
pub struct Outcome {
    pub exit: Option<ExitStatus>,
    pub stdout: Capture,
    pub stderr: Capture,
    pub input_written: usize,
    pub termination: Termination,
    pub cleanup: CleanupReport,
    /// Worker-observed target spawn-start to exit. Includes polling latency.
    pub command_elapsed: Option<Duration>,
    /// Full launcher, target, cleanup and final-drain elapsed time.
    pub elapsed: Duration,
    pub cleanup_elapsed: Duration,
    pub worker_executable: PathBuf,
}
#[derive(Debug)]
pub enum RunError {
    Cancelled,
    TimedOut,
    InputLimit,
    Configuration,
    TicketLimit,
    Allocation,
    WorkerExecutable(io::Error),
    Spawn(io::Error),
    Setup(io::Error),
}
impl fmt::Display for RunError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Cancelled => f.write_str("process cancelled before launch"),
            Self::TimedOut => f.write_str("process deadline exceeded before launch"),
            Self::InputLimit => f.write_str("process input limit exceeded"),
            Self::Configuration => f.write_str("invalid process configuration"),
            Self::TicketLimit => f.write_str("process ticket limit exceeded"),
            Self::Allocation => f.write_str("process allocation failed"),
            Self::WorkerExecutable(e) => write!(f, "process worker executable unavailable: {e}"),
            Self::Spawn(e) => write!(f, "process worker spawn failed: {e}"),
            Self::Setup(e) => write!(f, "process setup failed: {e}"),
        }
    }
}
impl std::error::Error for RunError {}

static OWNED_SPAWN: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The only public process-signalling owner. It cannot be made from a PID or an
/// externally reaped Child. All signals occur while its private child is unreaped.
pub struct OwnedProcess {
    child: Child,
    pid: Pid,
    cleanup: Option<CleanupReport>,
}
impl OwnedProcess {
    /// Wait for owned-spawn admission using the caller's absolute deadline and
    /// cancellation signal. Checks apply before and after admission; the OS
    /// spawn call itself retains std::Command's blocking syscall behavior.
    pub fn spawn_until(
        command: &mut Command,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Self, RunError> {
        let spawning = acquire_spawn(deadline, cancelled)?;
        Self::spawn_locked(command, spawning).map_err(RunError::Spawn)
    }

    fn spawn_locked(
        command: &mut Command,
        spawning: std::sync::MutexGuard<'_, ()>,
    ) -> io::Result<Self> {
        // Another owned spawn must not inherit a transient pipe endpoint.
        // The guard covers pipe setup/spawn only, never execution or cleanup.
        command.process_group(0);
        let mut child = command.spawn()?;
        drop(spawning);
        let pid = i32::try_from(child.id())
            .ok()
            .and_then(Pid::from_raw)
            .filter(|p| *p != Pid::INIT);
        let Some(pid) = pid else {
            let _ = child.kill(); // The OS returned an invalid Unix identity; no group signalling.
            return Err(io::Error::other("invalid owned Unix child identity"));
        };
        Ok(Self {
            child,
            pid,
            cleanup: None,
        })
    }
    pub fn id(&self) -> u32 {
        self.child.id()
    }
    pub fn take_stdin(&mut self) -> Option<ChildStdin> {
        self.child.stdin.take()
    }
    pub fn take_stdout(&mut self) -> Option<ChildStdout> {
        self.child.stdout.take()
    }
    pub fn take_stderr(&mut self) -> Option<ChildStderr> {
        self.child.stderr.take()
    }
    /// Nonreaping observation; does not surrender the leader identity.
    pub fn observe_exit(&self) -> io::Result<bool> {
        if self.cleanup.as_ref().is_some_and(|r| r.leader_reaped) {
            return Ok(true);
        }
        waitid(
            WaitId::Pid(self.pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(io::Error::from)
    }
    /// Idempotent. Kill the owned group before any reaping, then bound reap time.
    /// No TERM grace is used: untrusted descendants may ignore TERM.
    pub fn stop(&mut self, timeout: Duration) -> CleanupReport {
        if let Some(report) = &self.cleanup {
            return report.clone();
        }
        let deadline = Instant::now().checked_add(timeout);
        let ownership = self.observe_exit();
        let group_signal = match ownership {
            Err(e) if e.raw_os_error() == Some(rustix::io::Errno::CHILD.raw_os_error()) => {
                GroupSignal::OwnershipLost
            }
            Err(e) => GroupSignal::Uncertain(e.raw_os_error()),
            Ok(_) => match kill_process_group(self.pid, Signal::KILL) {
                Ok(()) => GroupSignal::Sent,
                Err(rustix::io::Errno::SRCH) => GroupSignal::AlreadyAbsent,
                Err(e) => GroupSignal::Uncertain(Some(e.raw_os_error())),
            },
        };
        let mut report = CleanupReport {
            group_signal,
            group_quiescence: GroupQuiescence::NotProbed,
            leader_reaped: false,
            leader_status: None,
        };
        if group_signal != GroupSignal::OwnershipLost {
            loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        report.leader_reaped = true;
                        report.leader_status = Some(status);
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => break,
                }
                if deadline.is_none_or(|d| Instant::now() >= d) {
                    break;
                }
                thread::sleep(TICK.min(timeout));
            }
        }
        if report.leader_reaped {
            loop {
                // Signal zero is a harmless existence/permission probe. Never
                // send a real signal after reaping. If this numeric group ID
                // was reused, a present group only makes proof conservative.
                report.group_quiescence = match rustix::process::test_kill_process_group(self.pid) {
                    Err(rustix::io::Errno::SRCH) => GroupQuiescence::AbsentObserved,
                    Ok(()) => GroupQuiescence::Present,
                    Err(e) => GroupQuiescence::Uncertain(Some(e.raw_os_error())),
                };
                if report.group_quiescence == GroupQuiescence::AbsentObserved
                    || deadline.is_none_or(|d| Instant::now() >= d)
                {
                    break;
                }
                thread::sleep(TICK);
            }
        }
        self.cleanup = Some(report.clone());
        report
    }
}
fn acquire_spawn(
    deadline: Instant,
    cancelled: &dyn Fn() -> bool,
) -> Result<std::sync::MutexGuard<'static, ()>, RunError> {
    loop {
        if cancelled() {
            return Err(RunError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(RunError::TimedOut);
        }
        match OWNED_SPAWN.try_lock() {
            Ok(spawning) => {
                if cancelled() {
                    return Err(RunError::Cancelled);
                }
                if Instant::now() >= deadline {
                    return Err(RunError::TimedOut);
                }
                return Ok(spawning);
            }
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(RunError::Spawn(io::Error::other(
                    "owned spawn lock poisoned",
                )));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                thread::sleep(TICK.min(deadline.saturating_duration_since(Instant::now())));
            }
        }
    }
}

impl Drop for OwnedProcess {
    fn drop(&mut self) {
        // Diagnostics can block on a full inherited stderr pipe. Drop has no
        // result channel; callers requiring evidence must explicitly call stop.
        let _ = self.stop(Duration::ZERO);
    }
}
/// Set only the supplied pipe/socket descriptor nonblocking; no raw descriptor API.
pub fn set_nonblocking(fd: &impl AsFd) -> io::Result<()> {
    let flags = fcntl_getfl(fd).map_err(io::Error::from)?;
    fcntl_setfl(fd, flags | OFlags::NONBLOCK).map_err(io::Error::from)
}

pub fn run(
    command: &CommandSpec,
    input: &[u8],
    options: &RunOptions,
    worker: &WorkerLauncher,
    cancelled: &dyn Fn() -> bool,
) -> Result<Outcome, RunError> {
    run_interactive(command, input, options, worker, cancelled, &mut |_, _| true)
}
/// Caller predicate must do bounded work. It sees retained bytes only. After all
/// input is written, stdin closes once the predicate returns true.
pub fn run_interactive(
    command: &CommandSpec,
    input: &[u8],
    options: &RunOptions,
    worker: &WorkerLauncher,
    cancelled: &dyn Fn() -> bool,
    close_stdin: &mut dyn FnMut(&[u8], &[u8]) -> bool,
) -> Result<Outcome, RunError> {
    let start = Instant::now();
    if cancelled() {
        return Err(RunError::Cancelled);
    }
    if input.len() > options.stdin_limit {
        return Err(RunError::InputLimit);
    }
    let deadline = start
        .checked_add(options.timeout)
        .ok_or(RunError::Configuration)?;
    if options.timeout.is_zero() || options.cleanup_timeout > Duration::from_secs(30) {
        return Err(RunError::Configuration);
    }
    let ticket = encode_command(command, options.base_environment)?;
    let worker_executable = worker
        .executable
        .canonicalize()
        .map_err(RunError::WorkerExecutable)?;
    if !worker_executable.is_file() {
        return Err(RunError::Configuration);
    }
    let spawning = acquire_spawn(deadline, cancelled)?;
    let (dir, listener, endpoint, nonce) = private_channel().map_err(RunError::Setup)?;
    let mut launch = Command::new(&worker_executable);
    launch
        .args(&worker.prefix_args)
        .arg("--process-channel")
        .arg(&endpoint)
        .env_clear()
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if cancelled() {
        return Err(RunError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(RunError::TimedOut);
    }
    let mut process = OwnedProcess::spawn_locked(&mut launch, spawning).map_err(RunError::Spawn)?;
    drop(launch);
    let mut stdin = process.take_stdin();
    let mut stdout = process
        .take_stdout()
        .map(|p| std::fs::File::from(std::os::fd::OwnedFd::from(p)));
    let mut stderr = process
        .take_stderr()
        .map(|p| std::fs::File::from(std::os::fd::OwnedFd::from(p)));
    let mut out = Capture::default();
    let mut err = Capture::default();
    let mut termination = Termination::Completed;
    let mut exit = None;
    let mut command_elapsed = None;
    let mut channel: Option<Channel> = None;
    let mut received = Vec::new();
    let mut ticket_sent = 0;
    let mut input_written = 0;
    let mut handshaken = false;
    let mut bootstrap_written = 0;
    let mut candidates = AuthCandidates::default();
    let setup = stdin
        .as_ref()
        .map(set_nonblocking)
        .transpose()
        .and_then(|_| stdout.as_ref().map(set_nonblocking).transpose())
        .and_then(|_| stderr.as_ref().map(set_nonblocking).transpose());
    let streams_nonblocking = setup.is_ok();
    if let Err(e) = setup {
        termination = Termination::Io {
            phase: Phase::Protocol,
            errno: e.raw_os_error(),
        };
    }
    let mut active = termination == Termination::Completed;
    while active {
        let progress_before = (
            input_written,
            bootstrap_written,
            ticket_sent,
            out.bytes.len(),
            out.dropped,
            err.bytes.len(),
            err.dropped,
            received.len(),
            handshaken,
            channel.is_some(),
        );
        if cancelled() {
            termination = Termination::Cancelled;
            break;
        }
        if Instant::now() >= deadline {
            termination = Termination::TimedOut;
            break;
        }
        if let Some(nonce) = &nonce
            && bootstrap_written < nonce.len()
            && let Some(pipe) = &mut stdin
        {
            match pipe.write(&nonce[bootstrap_written..]) {
                Ok(0) => {
                    termination = Termination::Protocol;
                    break;
                }
                Ok(n) => bootstrap_written += n,
                Err(e) if transient(&e) => {}
                Err(e) => {
                    termination = Termination::Io {
                        phase: Phase::Stdin,
                        errno: e.raw_os_error(),
                    };
                    break;
                }
            }
        }
        if channel.is_none() {
            let accepted = match &listener {
                ChannelListener::Unix(listener) => {
                    listener.accept().map(|(stream, _)| Channel::Unix(stream))
                }
                ChannelListener::Tcp(listener) => {
                    listener.accept().map(|(stream, _)| Channel::Tcp(stream))
                }
            };
            match accepted {
                Ok(stream) => {
                    if let Err(e) = stream.set_nonblocking(true) {
                        termination = Termination::Io {
                            phase: Phase::Protocol,
                            errno: e.raw_os_error(),
                        };
                        break;
                    }
                    if nonce.is_some() {
                        candidates.push(stream);
                    } else {
                        channel = Some(stream);
                    }
                }
                Err(e) if transient(&e) => {}
                Err(e) => {
                    termination = Termination::Io {
                        phase: Phase::Protocol,
                        errno: e.raw_os_error(),
                    };
                    break;
                }
            }
            if let Some(nonce) = &nonce {
                channel = candidates.poll(nonce, process.id());
                if channel.is_some() {
                    handshaken = true;
                }
            }
        }
        if let Some(stream) = &mut channel {
            let mut bytes = [0u8; MAX_STATUS];
            match stream.read(&mut bytes) {
                Ok(0) => {
                    termination = Termination::Protocol;
                    break;
                }
                Ok(n) => {
                    if received
                        .len()
                        .checked_add(n)
                        .is_none_or(|len| len > MAX_STATUS)
                    {
                        termination = Termination::Protocol;
                        break;
                    }
                    if received.try_reserve(n).is_err() {
                        termination = Termination::Io {
                            phase: Phase::Protocol,
                            errno: None,
                        };
                        break;
                    }
                    received.extend_from_slice(&bytes[..n]);
                }
                Err(e) if transient(&e) => {}
                Err(e) => {
                    termination = Termination::Io {
                        phase: Phase::Protocol,
                        errno: e.raw_os_error(),
                    };
                    break;
                }
            }
            if !handshaken && received.len() >= MAGIC.len() + 4 {
                let pid = u32::from_le_bytes([received[5], received[6], received[7], received[8]]);
                if &received[..MAGIC.len()] != MAGIC || pid != process.id() {
                    termination = Termination::Protocol;
                    break;
                }
                received.drain(..9);
                handshaken = true;
            }
            if handshaken && ticket_sent < ticket.len() {
                match stream
                    .write(&ticket[ticket_sent..ticket.len().min(ticket_sent.saturating_add(STEP))])
                {
                    Ok(0) => {
                        termination = Termination::Protocol;
                        break;
                    }
                    Ok(n) => ticket_sent += n,
                    Err(e) if transient(&e) => {}
                    Err(e) => {
                        termination = Termination::Io {
                            phase: Phase::Protocol,
                            errno: e.raw_os_error(),
                        };
                        break;
                    }
                }
            }
            if handshaken && !received.is_empty() {
                match received[0] {
                    b'S' => {
                        received.drain(..1);
                    }
                    b'E' if received.len() >= 13 => {
                        let raw = i32::from_le_bytes([
                            received[1],
                            received[2],
                            received[3],
                            received[4],
                        ]);
                        let nanos = u64::from_le_bytes([
                            received[5],
                            received[6],
                            received[7],
                            received[8],
                            received[9],
                            received[10],
                            received[11],
                            received[12],
                        ]);
                        exit = Some(ExitStatus::from_raw(raw));
                        command_elapsed = Some(Duration::from_nanos(nanos));
                        break;
                    }
                    b'F' if received.len() >= 5 => {
                        let raw = i32::from_le_bytes([
                            received[1],
                            received[2],
                            received[3],
                            received[4],
                        ]);
                        termination = Termination::Io {
                            phase: Phase::TargetSpawn,
                            errno: if raw == 0 { None } else { Some(raw) },
                        };
                        break;
                    }
                    b'E' | b'F' => {}
                    _ => {
                        termination = Termination::Protocol;
                        break;
                    }
                }
            }
        }
        if ticket_sent == ticket.len()
            && handshaken
            && let Some(pipe) = &mut stdin
        {
            if input_written < input.len() {
                match pipe.write(
                    &input[input_written..input.len().min(input_written.saturating_add(STEP))],
                ) {
                    Ok(0) => {
                        stdin = None;
                    }
                    Ok(n) => input_written += n,
                    Err(e) if transient(&e) => {}
                    Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                        stdin = None;
                    }
                    Err(e) => {
                        termination = Termination::Io {
                            phase: Phase::Stdin,
                            errno: e.raw_os_error(),
                        };
                        break;
                    }
                }
            }
            if input_written == input.len() && close_stdin(&out.bytes, &err.bytes) {
                stdin = None;
            }
        }
        for (pipe, capture, limit, phase) in [
            (&mut stdout, &mut out, options.stdout_limit, Phase::Stdout),
            (&mut stderr, &mut err, options.stderr_limit, Phase::Stderr),
        ] {
            match drain(pipe, capture, limit) {
                Ok(exceeded) if exceeded && options.output_policy == OutputPolicy::Terminate => {
                    termination = Termination::OutputLimit(phase);
                    active = false;
                    break;
                }
                Ok(_) => {}
                Err(e) => {
                    termination = Termination::Io {
                        phase,
                        errno: e.raw_os_error(),
                    };
                    active = false;
                    break;
                }
            }
        }
        match process.observe_exit() {
            Ok(true) => {
                termination = Termination::Protocol;
                break;
            }
            Ok(false) => {}
            Err(e) => {
                termination = Termination::Io {
                    phase: Phase::Observe,
                    errno: e.raw_os_error(),
                };
                break;
            }
        }
        let progress_after = (
            input_written,
            bootstrap_written,
            ticket_sent,
            out.bytes.len(),
            out.dropped,
            err.bytes.len(),
            err.dropped,
            received.len(),
            handshaken,
            channel.is_some(),
        );
        // Ready nonblocking pipes should drain at their available rate. Sleep
        // only when every channel would block; each iteration still checks
        // cancellation/deadline and gives all three streams one bounded step.
        if active && progress_before == progress_after {
            thread::sleep(TICK.min(deadline.saturating_duration_since(Instant::now())));
        }
    }
    drop(candidates); // Close partial unauthenticated peers at cancellation/deadline.
    drop(listener); // Close queued peers too; no admission continues during cleanup.
    drop(stdin); // Close writer before signalling; never wait on blocked writer.
    let cleanup_started = Instant::now();
    let cleanup = process.stop(options.cleanup_timeout);
    let drain_deadline = cleanup_started
        .checked_add(options.cleanup_timeout)
        .unwrap_or(cleanup_started);
    // A failed lease setup cannot safely enter any output read loop.
    if streams_nonblocking {
        drain_after_stop(
            (&mut stdout, &mut out),
            (&mut stderr, &mut err),
            options,
            &mut termination,
            drain_deadline,
        );
    }
    if options.output_policy == OutputPolicy::Terminate && termination == Termination::Completed {
        if out.dropped > 0 {
            termination = Termination::OutputLimit(Phase::Stdout);
        } else if err.dropped > 0 {
            termination = Termination::OutputLimit(Phase::Stderr);
        }
    }
    // Any escaped writer remains outside group control. Closing our descriptors
    // bounds return; eof=false records that complete output was not observed.
    drop(stdout);
    drop(stderr);
    drop(channel);
    drop(dir);
    Ok(Outcome {
        exit,
        stdout: out,
        stderr: err,
        input_written,
        termination,
        cleanup,
        command_elapsed,
        elapsed: start.elapsed(),
        cleanup_elapsed: cleanup_started.elapsed(),
        worker_executable,
    })
}
fn drain_after_stop<T: Read>(
    output: (&mut Option<T>, &mut Capture),
    errors: (&mut Option<T>, &mut Capture),
    options: &RunOptions,
    termination: &mut Termination,
    drain_deadline: Instant,
) {
    let (stdout, out) = output;
    let (stderr, err) = errors;
    let mut final_probe = false;
    loop {
        let mut progress = false;
        for (pipe, capture, limit, phase) in [
            (&mut *stdout, &mut *out, options.stdout_limit, Phase::Stdout),
            (&mut *stderr, &mut *err, options.stderr_limit, Phase::Stderr),
        ] {
            let previous = (capture.bytes.len(), capture.dropped);
            match drain(pipe, capture, limit) {
                Ok(_) => {}
                Err(e) => {
                    if *termination == Termination::Completed {
                        *termination = Termination::Io {
                            phase,
                            errno: e.raw_os_error(),
                        };
                    }
                    *pipe = None;
                }
            }
            progress |= previous != (capture.bytes.len(), capture.dropped);
        }
        if stdout.is_none() && stderr.is_none() {
            break;
        }
        if Instant::now() >= drain_deadline {
            // A final positive read can consume every pending byte without
            // observing EOF. Permit one more nonblocking step per stream,
            // capped at STEP bytes, with no sleep or new drain deadline.
            // Open/escaped writers and larger remaining output still retain
            // eof=false; byte completeness never substitutes for EOF.
            if final_probe || !progress {
                break;
            }
            final_probe = true;
            continue;
        }
        if !progress {
            thread::sleep(TICK);
        }
    }
}

fn transient(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
    )
}
const NONCE_LEN: usize = 32;
const AUTH_LEN: usize = NONCE_LEN + 9;
const MAX_AUTH_CANDIDATES: usize = 8;

enum Channel {
    Unix(UnixStream),
    Tcp(TcpStream),
}
impl Channel {
    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.set_nonblocking(nonblocking),
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
        }
    }
}
impl Read for Channel {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.read(bytes),
            Self::Tcp(stream) => stream.read(bytes),
        }
    }
}
impl Write for Channel {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self {
            Self::Unix(stream) => stream.write(bytes),
            Self::Tcp(stream) => stream.write(bytes),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Unix(stream) => stream.flush(),
            Self::Tcp(stream) => stream.flush(),
        }
    }
}
enum ChannelListener {
    Unix(UnixListener),
    Tcp(TcpListener),
}
struct AuthCandidate {
    channel: Channel,
    bytes: [u8; AUTH_LEN],
    received: usize,
}
#[derive(Default)]
struct AuthCandidates(std::collections::VecDeque<AuthCandidate>);
impl AuthCandidates {
    fn push(&mut self, channel: Channel) {
        // A partial unauthenticated connection cannot monopolize the listener.
        // Both descriptor ownership and per-cycle reads have a fixed bound.
        if self.0.len() == MAX_AUTH_CANDIDATES {
            self.0.pop_front();
        }
        self.0.push_back(AuthCandidate {
            channel,
            bytes: [0; AUTH_LEN],
            received: 0,
        });
    }
    fn poll(&mut self, nonce: &[u8; NONCE_LEN], pid: u32) -> Option<Channel> {
        let mut expected = [0; AUTH_LEN];
        expected[..NONCE_LEN].copy_from_slice(nonce);
        expected[NONCE_LEN..NONCE_LEN + MAGIC.len()].copy_from_slice(MAGIC);
        expected[NONCE_LEN + MAGIC.len()..].copy_from_slice(&pid.to_le_bytes());
        for _ in 0..self.0.len() {
            let Some(mut candidate) = self.0.pop_front() else {
                break;
            };
            match candidate
                .channel
                .read(&mut candidate.bytes[candidate.received..])
            {
                Ok(0) => continue,
                Ok(n) => candidate.received += n,
                Err(e) if transient(&e) => {}
                Err(_) => continue,
            }
            if candidate.received == AUTH_LEN {
                // Refuse only complete authentication records: a prefix refusal
                // would disclose which guessed nonce bytes match.
                let difference = candidate
                    .bytes
                    .iter()
                    .zip(expected.iter())
                    .fold(0u8, |difference, (actual, expected)| {
                        difference | (actual ^ expected)
                    });
                if difference != 0 {
                    continue;
                }
                self.0.clear();
                return Some(candidate.channel);
            }
            self.0.push_back(candidate);
        }
        None
    }
}
fn private_channel() -> io::Result<(
    tempfile::TempDir,
    ChannelListener,
    OsString,
    Option<[u8; NONCE_LEN]>,
)> {
    let dir = tempfile::Builder::new()
        .prefix("ep")
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let path = dir.path().join("c");
    // Generated paths contain no NUL. Address construction refusal means the
    // pathname cannot fit; bind/permission failures never select the fallback.
    if SocketAddr::from_pathname(&path).is_err() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let mut nonce = [0; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|error| io::Error::other(error.to_string()))?;
        let endpoint = OsString::from(format!("tcp:{}", listener.local_addr()?.port()));
        return Ok((dir, ChannelListener::Tcp(listener), endpoint, Some(nonce)));
    }
    let listener = UnixListener::bind(&path)?;
    listener.set_nonblocking(true)?;
    Ok((
        dir,
        ChannelListener::Unix(listener),
        path.into_os_string(),
        None,
    ))
}

fn drain<T: Read>(pipe: &mut Option<T>, capture: &mut Capture, limit: usize) -> io::Result<bool> {
    let Some(pipe_ref) = pipe else {
        return Ok(false);
    };
    let mut block = [0u8; STEP];
    match pipe_ref.read(&mut block) {
        Ok(0) => {
            capture.eof = true;
            *pipe = None;
            Ok(false)
        }
        Ok(n) => {
            let keep = n.min(limit.saturating_sub(capture.bytes.len()));
            capture
                .bytes
                .try_reserve(keep)
                .map_err(|_| io::Error::other("process capture allocation failed"))?;
            capture.bytes.extend_from_slice(&block[..keep]);
            capture.dropped = capture
                .dropped
                .checked_add((n - keep) as u64)
                .ok_or_else(|| io::Error::other("process output count overflow"))?;
            Ok(keep < n)
        }
        Err(e) if transient(&e) => Ok(false),
        Err(e) => Err(e),
    }
}

fn push_bytes(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), RunError> {
    let needed = out
        .len()
        .checked_add(bytes.len())
        .ok_or(RunError::TicketLimit)?;
    if needed > MAX_TICKET {
        return Err(RunError::TicketLimit);
    }
    out.try_reserve(bytes.len())
        .map_err(|_| RunError::Allocation)?;
    out.extend_from_slice(bytes);
    Ok(())
}
fn field(out: &mut Vec<u8>, bytes: &[u8]) -> Result<(), RunError> {
    if out
        .len()
        .checked_add(4)
        .and_then(|n| n.checked_add(bytes.len()))
        .is_none_or(|n| n > MAX_TICKET)
    {
        return Err(RunError::TicketLimit);
    }
    if bytes.contains(&0) {
        return Err(RunError::Configuration);
    }
    let len = u32::try_from(bytes.len()).map_err(|_| RunError::TicketLimit)?;
    push_bytes(out, &len.to_le_bytes())?;
    push_bytes(out, bytes)
}
fn encode_command(
    command: &CommandSpec,
    environment: BaseEnvironment,
) -> Result<Vec<u8>, RunError> {
    if command.program.is_empty() {
        return Err(RunError::Configuration);
    }
    if command.args.len() > 4096 || command.environment.len() > 4096 {
        return Err(RunError::TicketLimit);
    }
    let mut keys = std::collections::HashSet::new();
    keys.try_reserve(command.environment.len())
        .map_err(|_| RunError::Allocation)?;
    for (key, value) in &command.environment {
        if key.len() > MAX_TICKET || value.as_ref().is_some_and(|v| v.len() > MAX_TICKET) {
            return Err(RunError::TicketLimit);
        }
        if key.is_empty() || key.as_bytes().contains(&b'=') || !keys.insert(key.as_os_str()) {
            return Err(RunError::Configuration);
        }
    }
    let mut body = Vec::new();
    push_bytes(&mut body, MAGIC)?;
    field(&mut body, command.program.as_bytes())?;
    let args = command.args.iter();
    if args.len() > 4096 {
        return Err(RunError::TicketLimit);
    }
    push_bytes(
        &mut body,
        &u32::try_from(args.len())
            .map_err(|_| RunError::TicketLimit)?
            .to_le_bytes(),
    )?;
    for arg in args {
        field(&mut body, arg.as_bytes())?;
    }
    match command.directory.as_deref() {
        Some(path) => {
            push_bytes(&mut body, &[1])?;
            field(&mut body, path.as_os_str().as_bytes())?;
        }
        None => push_bytes(&mut body, &[0])?,
    }
    // Expand inherited environment once. Clear is explicit because Command does
    // not expose whether env_clear was called; adapters declare that policy.
    if command.environment.len() > 4096 {
        return Err(RunError::TicketLimit);
    }
    let count_at = body.len();
    push_bytes(&mut body, &[0; 4])?;
    let mut count = 0u32;
    if environment == BaseEnvironment::Inherit {
        for (key, value) in std::env::vars_os() {
            if keys.contains(key.as_os_str()) {
                continue;
            }
            count += 1;
            if count > 4096 {
                return Err(RunError::TicketLimit);
            }
            field(&mut body, key.as_bytes())?;
            field(&mut body, value.as_bytes())?;
        }
    }
    for (key, value) in &command.environment {
        if let Some(value) = value {
            count += 1;
            if count > 4096 {
                return Err(RunError::TicketLimit);
            }
            field(&mut body, key.as_bytes())?;
            field(&mut body, value.as_bytes())?;
        }
    }
    body[count_at..count_at + 4].copy_from_slice(&count.to_le_bytes());
    let mut out = Vec::new();
    push_bytes(
        &mut out,
        &u32::try_from(body.len())
            .map_err(|_| RunError::TicketLimit)?
            .to_le_bytes(),
    )?;
    push_bytes(&mut out, &body)?;
    Ok(out)
}

#[derive(Debug)]
pub struct WorkerError {
    phase: &'static str,
}
impl fmt::Display for WorkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "process worker {} failed", self.phase)
    }
}
impl std::error::Error for WorkerError {}
fn worker_error(phase: &'static str) -> WorkerError {
    WorkerError { phase }
}
/// Entry point for the hidden facade mode or bundled standalone helper. Worker
/// parses only the final internal channel flag; command argv never passes shell.
pub fn worker_main() -> Result<(), WorkerError> {
    let mut args = std::env::args_os();
    let path = args.next_back().ok_or(worker_error("arguments"))?;
    if args.next_back().as_deref() != Some(OsStr::new("--process-channel")) {
        return Err(worker_error("arguments"));
    }
    if rustix::process::getpgid(None).map_err(|_| worker_error("group identity"))?
        != rustix::process::getpid()
    {
        return Err(worker_error("owned group required"));
    }
    let (mut channel, nonce) =
        if let Some(port) = path.to_str().and_then(|value| value.strip_prefix("tcp:")) {
            let port = port
                .parse::<u16>()
                .ok()
                .filter(|port| *port != 0)
                .ok_or(worker_error("endpoint"))?;
            let mut nonce = [0; NONCE_LEN];
            // Unbuffered exact read consumes only bootstrap bytes; the target
            // inherits this same stdin pipe with the original input remaining.
            std::fs::File::from(
                rustix::io::fcntl_dupfd_cloexec(io::stdin(), 3)
                    .map_err(|_| worker_error("bootstrap descriptor"))?,
            )
            .read_exact(&mut nonce)
            .map_err(|_| worker_error("bootstrap"))?;
            let stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port))
                .map_err(|_| worker_error("channel"))?;
            (Channel::Tcp(stream), Some(nonce))
        } else {
            (
                Channel::Unix(UnixStream::connect(&path).map_err(|_| worker_error("channel"))?),
                None,
            )
        };
    let mut handshake = Vec::new();
    if let Some(nonce) = nonce {
        handshake.extend_from_slice(&nonce);
    }
    handshake.extend_from_slice(MAGIC);
    handshake.extend_from_slice(&std::process::id().to_le_bytes());
    channel
        .write_all(&handshake)
        .map_err(|_| worker_error("handshake"))?;
    let mut len = [0u8; 4];
    channel
        .read_exact(&mut len)
        .map_err(|_| worker_error("ticket"))?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_TICKET {
        return Err(worker_error("ticket limit"));
    }
    let mut ticket = Vec::new();
    ticket
        .try_reserve_exact(len)
        .map_err(|_| worker_error("allocation"))?;
    ticket.resize(len, 0);
    channel
        .read_exact(&mut ticket)
        .map_err(|_| worker_error("ticket"))?;
    let mut command = decode_command(&ticket)?;
    channel
        .set_nonblocking(true)
        .map_err(|_| worker_error("channel mode"))?;
    let start = Instant::now();
    // A disconnected parent or unexpected worker failure must also terminate
    // the cooperative group. This lease exists only after verifying that this
    // process is its group leader and before any target is launched.
    let _lease = WorkerGroupLease;
    match command.spawn() {
        Ok(mut child) => {
            write_status(&mut channel, b"S")?;
            loop {
                match child.try_wait() {
                    Ok(Some(status)) => {
                        let mut message = Vec::new();
                        message.push(b'E');
                        message.extend_from_slice(&status.into_raw().to_le_bytes());
                        message.extend_from_slice(
                            &u64::try_from(start.elapsed().as_nanos())
                                .unwrap_or(u64::MAX)
                                .to_le_bytes(),
                        );
                        write_status(&mut channel, &message)?;
                        break;
                    }
                    Ok(None) => {}
                    Err(_) => return Err(worker_error("target wait")),
                }
                check_parent(&mut channel)?;
                thread::sleep(TICK);
            }
        }
        Err(e) => {
            let mut message = vec![b'F'];
            message.extend_from_slice(&e.raw_os_error().unwrap_or(0).to_le_bytes());
            write_status(&mut channel, &message)?;
        }
    }
    // The live worker retains the group identity until the parent kills it.
    loop {
        check_parent(&mut channel)?;
        thread::sleep(TICK);
    }
}
struct WorkerGroupLease;
impl Drop for WorkerGroupLease {
    fn drop(&mut self) {
        let _ = rustix::process::kill_current_process_group(Signal::KILL);
    }
}
fn check_parent(channel: &mut Channel) -> Result<(), WorkerError> {
    let mut byte = [0u8; 1];
    match channel.read(&mut byte) {
        Ok(0) => {
            let _ = rustix::process::kill_current_process_group(Signal::KILL);
            Err(worker_error("parent disconnected"))
        }
        Ok(_) => Err(worker_error("unexpected control")),
        Err(e) if transient(&e) => Ok(()),
        Err(_) => Err(worker_error("control")),
    }
}
fn write_status(channel: &mut Channel, bytes: &[u8]) -> Result<(), WorkerError> {
    let deadline = Instant::now()
        .checked_add(Duration::from_secs(1))
        .ok_or(worker_error("deadline"))?;
    let mut offset = 0;
    while offset < bytes.len() {
        match channel.write(&bytes[offset..]) {
            Ok(0) => return Err(worker_error("status")),
            Ok(n) => offset += n,
            Err(e) if transient(&e) => {
                if Instant::now() >= deadline {
                    return Err(worker_error("status deadline"));
                }
                thread::sleep(TICK);
            }
            Err(_) => return Err(worker_error("status")),
        }
    }
    Ok(())
}
struct Decoder<'a> {
    bytes: &'a [u8],
    at: usize,
}
impl<'a> Decoder<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], WorkerError> {
        let end = self
            .at
            .checked_add(n)
            .ok_or(worker_error("ticket arithmetic"))?;
        let value = self
            .bytes
            .get(self.at..end)
            .ok_or(worker_error("ticket shape"))?;
        self.at = end;
        Ok(value)
    }
    fn count(&mut self) -> Result<usize, WorkerError> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }
    fn os(&mut self) -> Result<OsString, WorkerError> {
        let n = self.count()?;
        let bytes = self.take(n)?;
        if bytes.contains(&0) {
            return Err(worker_error("ticket NUL"));
        }
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(n)
            .map_err(|_| worker_error("allocation"))?;
        owned.extend_from_slice(bytes);
        Ok(OsString::from_vec(owned))
    }
}
fn decode_command(bytes: &[u8]) -> Result<Command, WorkerError> {
    let mut d = Decoder { bytes, at: 0 };
    if d.take(5)? != MAGIC {
        return Err(worker_error("version"));
    }
    let program = d.os()?;
    if program.is_empty() {
        return Err(worker_error("program"));
    }
    let mut command = Command::new(program);
    let count = d.count()?;
    if count > 4096 {
        return Err(worker_error("argument count"));
    }
    for _ in 0..count {
        command.arg(d.os()?);
    }
    match d.take(1)?[0] {
        0 => {}
        1 => {
            command.current_dir(d.os()?);
        }
        _ => return Err(worker_error("directory")),
    }
    command.env_clear();
    let count = d.count()?;
    if count > 4096 {
        return Err(worker_error("environment count"));
    }
    for _ in 0..count {
        let key = d.os()?;
        let value = d.os()?;
        if key.is_empty() || key.as_bytes().contains(&b'=') {
            return Err(worker_error("environment key"));
        }
        command.env(key, value);
    }
    if d.at != bytes.len() {
        return Err(worker_error("ticket trailing bytes"));
    }
    command
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn contended_spawn_observes_the_same_deadline_and_cancellation_before_launch() {
        let _held = OWNED_SPAWN.lock().expect("test spawn gate");
        let mut command = Command::new("/does/not/exist");
        let deadline = Instant::now() + Duration::from_millis(10);
        assert!(matches!(
            OwnedProcess::spawn_until(&mut command, deadline, &|| false),
            Err(RunError::TimedOut)
        ));
        let calls = std::cell::Cell::new(0usize);
        let cancelled = || {
            calls.set(calls.get() + 1);
            calls.get() >= 3
        };
        assert!(matches!(
            OwnedProcess::spawn_until(
                &mut command,
                Instant::now() + Duration::from_secs(1),
                &cancelled
            ),
            Err(RunError::Cancelled)
        ));
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn short_run_deadline_expires_at_spawn_gate_without_launching_worker() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("worker-launched");
        let worker = WorkerLauncher {
            executable: PathBuf::from("/bin/sh"),
            prefix_args: vec![
                "-c".into(),
                "printf launched > \"$0\"".into(),
                marker.clone().into_os_string(),
            ],
        };
        let held = OWNED_SPAWN.lock().unwrap();
        for millis in [25, 30] {
            let options = RunOptions {
                timeout: Duration::from_millis(millis),
                ..RunOptions::default()
            };
            let outcome = run(
                &CommandSpec::new("/bin/true"),
                b"",
                &options,
                &worker,
                &|| false,
            );
            assert!(matches!(outcome, Err(RunError::TimedOut)));
            assert!(
                !marker.exists(),
                "worker must not launch after admission expiry"
            );
        }
        assert!(matches!(
            run(
                &CommandSpec::new("/bin/true"),
                b"",
                &RunOptions::default(),
                &worker,
                &|| true
            ),
            Err(RunError::Cancelled)
        ));
        assert!(!marker.exists());
        drop(held);
        // The same executable really runs when admission is available. Its
        // deliberate lack of the worker protocol cannot claim target success.
        let outcome = run(
            &CommandSpec::new("/bin/true"),
            b"",
            &RunOptions::default(),
            &worker,
            &|| false,
        )
        .unwrap();
        assert!(marker.exists());
        assert_ne!(outcome.termination, Termination::Completed);
        assert!(outcome.cleanup.cooperative_stop_succeeded());
    }

    #[test]
    fn loopback_authentication_bounds_partial_peers_and_retains_protocol_bytes() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("private listener");
        let address = listener.local_addr().expect("endpoint");
        let mut candidates = AuthCandidates::default();
        let mut peers = Vec::new();
        for _ in 0..MAX_AUTH_CANDIDATES + 1 {
            let peer = TcpStream::connect(address).expect("partial peer");
            peer.set_read_timeout(Some(Duration::from_secs(1)))
                .expect("peer read bound");
            let (server, _) = listener.accept().expect("accepted peer");
            server.set_nonblocking(true).expect("server nonblocking");
            candidates.push(Channel::Tcp(server));
            peers.push(peer);
            assert!(candidates.0.len() <= MAX_AUTH_CANDIDATES);
        }
        let mut byte = [0];
        assert_eq!(peers[0].read(&mut byte).expect("evicted peer closed"), 0);
        let nonce = [42; NONCE_LEN];
        assert!(candidates.poll(&nonce, 123).is_none());
        // Wrong nonce and wrong PID cannot select a stream or receive a ticket.
        for wrong_pid in [false, true] {
            let mut peer = TcpStream::connect(address).expect("wrong peer");
            peer.set_read_timeout(Some(Duration::from_secs(1)))
                .expect("wrong peer read bound");
            let (server, _) = listener.accept().expect("wrong accepted");
            server.set_nonblocking(true).expect("wrong nonblocking");
            candidates.push(Channel::Tcp(server));
            let mut auth = nonce.to_vec();
            if !wrong_pid {
                auth[0] ^= 1;
            }
            auth.extend_from_slice(MAGIC);
            auth.extend_from_slice(&(if wrong_pid { 124u32 } else { 123 }).to_le_bytes());
            peer.write_all(&auth[..1]).expect("partial wrong auth");
            let deadline = Instant::now() + Duration::from_secs(1);
            let count = candidates.0.len();
            while candidates.0.back().expect("partial candidate").received == 0 {
                assert!(
                    Instant::now() < deadline,
                    "partial auth received under bound"
                );
                assert!(candidates.poll(&nonce, 123).is_none());
                thread::yield_now();
            }
            assert_eq!(
                candidates.0.len(),
                count,
                "partial auth cannot disclose a matching prefix"
            );
            peer.write_all(&auth[1..]).expect("remaining wrong auth");
            while candidates.0.len() == count {
                assert!(Instant::now() < deadline, "wrong auth rejected under bound");
                assert!(candidates.poll(&nonce, 123).is_none());
                thread::yield_now();
            }
            assert_eq!(
                peer.read(&mut byte)
                    .expect("wrong peer closed without ticket"),
                0
            );
        }
        let mut peer = TcpStream::connect(address).expect("worker peer");
        let (server, _) = listener.accept().expect("worker accepted");
        server.set_nonblocking(true).expect("worker nonblocking");
        candidates.push(Channel::Tcp(server));
        peer.write_all(&nonce[..1]).expect("partial auth");
        assert!(candidates.poll(&nonce, 123).is_none());
        let mut auth = nonce[1..].to_vec();
        auth.extend_from_slice(MAGIC);
        auth.extend_from_slice(&123u32.to_le_bytes());
        auth.extend_from_slice(b"S");
        peer.write_all(&auth).expect("complete auth plus protocol");
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut selected = loop {
            if let Some(channel) = candidates.poll(&nonce, 123) {
                break channel;
            }
            assert!(
                Instant::now() < deadline,
                "authentication completes under bound"
            );
            thread::yield_now();
        };
        assert!(candidates.0.is_empty());
        assert_eq!(selected.read(&mut byte).expect("protocol byte remains"), 1);
        assert_eq!(byte, *b"S");
    }

    #[test]
    fn final_drain_observes_eof_after_cleanup_used_its_budget() {
        let bytes = b"complete acknowledgement\n";
        let mut child = Command::new("/usr/bin/printf")
            .arg("complete acknowledgement\n")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("owned finite child");
        assert!(child.wait().expect("reap writer").success());
        let read = std::fs::File::from(std::os::fd::OwnedFd::from(
            child.stdout.take().expect("stdout"),
        ));
        set_nonblocking(&read).expect("nonblocking stdout");
        let mut stdout = Some(read);
        let read = std::fs::File::from(std::os::fd::OwnedFd::from(
            child.stderr.take().expect("stderr"),
        ));
        set_nonblocking(&read).expect("nonblocking stderr");
        let mut stderr = Some(read);
        let mut out = Capture::default();
        let mut err = Capture::default();
        let mut termination = Termination::Completed;
        drain_after_stop(
            (&mut stdout, &mut out),
            (&mut stderr, &mut err),
            &RunOptions::default(),
            &mut termination,
            Instant::now(),
        );
        assert_eq!(out.bytes, bytes);
        assert!(out.eof, "captured all bytes but missed stdout EOF");
        assert!(err.eof);
        assert_eq!(termination, Termination::Completed);
    }

    #[test]
    fn final_drain_keeps_live_writers_incomplete_and_honors_capture_caps() {
        let (output, mut writer) = UnixStream::pair().expect("output stream");
        let (errors, _error_writer) = UnixStream::pair().expect("error stream");
        output.set_nonblocking(true).expect("nonblocking output");
        errors.set_nonblocking(true).expect("nonblocking errors");
        let bytes = b"complete acknowledgement\n";
        writer.write_all(bytes).expect("pending output");
        let mut stdout = Some(output);
        let mut stderr = Some(errors);
        let mut out = Capture::default();
        let mut err = Capture::default();
        let mut termination = Termination::Completed;
        let options = RunOptions {
            stdout_limit: 8,
            ..RunOptions::default()
        };
        let start = Instant::now();
        drain_after_stop(
            (&mut stdout, &mut out),
            (&mut stderr, &mut err),
            &options,
            &mut termination,
            start,
        );
        assert!(start.elapsed() < Duration::from_secs(1), "no drain grace");
        assert_eq!(out.bytes, &bytes[..8]);
        assert_eq!(out.dropped, (bytes.len() - 8) as u64);
        assert!(!out.eof && !err.eof, "writers remain open");
        assert!(stdout.is_some() && stderr.is_some());
        assert_eq!(termination, Termination::Completed);
    }

    #[test]
    fn final_drain_probe_has_a_fixed_read_bound_after_deadline() {
        let mut stdout = Some(io::Cursor::new(vec![b'x'; 3 * STEP]));
        let mut stderr = Some(io::Cursor::new(Vec::new()));
        let mut out = Capture::default();
        let mut err = Capture::default();
        let mut termination = Termination::Completed;
        drain_after_stop(
            (&mut stdout, &mut out),
            (&mut stderr, &mut err),
            &RunOptions::default(),
            &mut termination,
            Instant::now(),
        );
        assert_eq!(out.bytes.len(), 2 * STEP);
        assert!(!out.eof, "unread bytes require refusal");
        assert!(err.eof);
    }

    #[test]
    fn signal_and_leader_reap_cannot_claim_group_quiescence() {
        let mut report = CleanupReport {
            group_signal: GroupSignal::Sent,
            group_quiescence: GroupQuiescence::NotProbed,
            leader_reaped: true,
            leader_status: Some(ExitStatus::from_raw(0)),
        };
        for state in [
            GroupQuiescence::NotProbed,
            GroupQuiescence::Present,
            GroupQuiescence::Uncertain(Some(rustix::io::Errno::PERM.raw_os_error())),
        ] {
            report.group_quiescence = state;
            assert!(!report.cooperative_stop_succeeded());
        }
        report.group_quiescence = GroupQuiescence::AbsentObserved;
        assert!(report.cooperative_stop_succeeded());
        report.group_signal = GroupSignal::Uncertain(Some(rustix::io::Errno::PERM.raw_os_error()));
        assert!(report.cooperative_stop_succeeded());
        report.group_signal = GroupSignal::OwnershipLost;
        assert!(!report.cooperative_stop_succeeded());
    }
    #[test]
    fn command_channel_directory_is_private_at_creation() {
        let (dir, _listener, _, _) = private_channel().unwrap();
        assert!(dir.path().starts_with(std::env::temp_dir()));
        assert_eq!(
            std::fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[test]
    fn malformed_ticket_version_nul_trailing_and_count_are_rejected() {
        let mut command = CommandSpec::new("/bin/true");
        let good = encode_command(&command, BaseEnvironment::Clear).unwrap();
        let mut bad = good[4..].to_vec();
        bad[4] = 2;
        assert!(decode_command(&bad).is_err());
        let mut bad = good[4..].to_vec();
        bad.push(0);
        assert!(decode_command(&bad).is_err());
        command.arg(OsString::from_vec(vec![0]));
        assert!(matches!(
            encode_command(&command, BaseEnvironment::Clear),
            Err(RunError::Configuration)
        ));
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.push(0);
        assert!(decode_command(&bad).is_err());
        let mut bad = Vec::new();
        bad.extend_from_slice(MAGIC);
        bad.extend_from_slice(&1u32.to_le_bytes());
        bad.push(b'x');
        bad.extend_from_slice(&u32::MAX.to_le_bytes());
        assert!(decode_command(&bad).is_err());
    }
}
