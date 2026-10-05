//! The CLI owns an exclusive byte-stream session on inherited stdio. Duplicating
//! an fd does not isolate its status flags: the NONBLOCK lease intentionally
//! affects every alias of that open file description and restores it on return.

use super::*;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fd::{AsFd, OwnedFd};
use rustix::fs::{FileType, OFlags, fcntl_getfl, fcntl_setfl, fstat};
use rustix::io::{Errno, fcntl_dupfd_cloexec, read, write};
use std::collections::VecDeque;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

const IO_CHUNK: usize = 16 * 1024;
const EOF_OUTPUT_BUDGET: Duration = Duration::from_secs(2);
const WOULD_BLOCK_BACKOFF: Duration = Duration::from_millis(10);
const MAX_DIAGNOSTIC_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy)]
enum EndpointKind {
    Pipe,
    File,
}

#[derive(Clone, Copy)]
enum EndpointDirection {
    Input,
    Output,
}

#[cfg(target_os = "linux")]
fn packet_writer(direction: EndpointDirection, flags: OFlags) -> bool {
    matches!(direction, EndpointDirection::Output) && flags.contains(OFlags::DIRECT)
}

#[cfg(not(target_os = "linux"))]
fn packet_writer(_: EndpointDirection, _: OFlags) -> bool {
    false
}

struct Endpoint {
    fd: OwnedFd,
    kind: EndpointKind,
    original_nonblock: bool,
    restore_needed: bool,
}

#[derive(Debug)]
enum EndpointError {
    PacketWriter(ApiError),
    Other(ApiError),
}

impl EndpointError {
    fn into_api(self) -> ApiError {
        match self {
            Self::PacketWriter(error) | Self::Other(error) => error,
        }
    }
}

impl From<ApiError> for EndpointError {
    fn from(error: ApiError) -> Self {
        Self::Other(error)
    }
}

impl Endpoint {
    fn acquire(
        fd: impl AsFd,
        label: &str,
        direction: EndpointDirection,
    ) -> Result<Self, EndpointError> {
        let fd = fcntl_dupfd_cloexec(fd, 3)
            .map_err(|error| transport_error(format!("Cannot duplicate MCP {label}: {error}")))?;
        let mode = fstat(&fd)
            .map_err(|error| transport_error(format!("Cannot inspect MCP {label}: {error}")))?;
        let kind = match FileType::from_raw_mode(mode.st_mode) {
            FileType::Fifo => EndpointKind::Pipe,
            FileType::RegularFile => EndpointKind::File,
            _ => {
                return Err(ApiError::unsupported(format!(
                    "MCP {label} descriptor; use a byte-stream pipe/FIFO or regular file"
                ))
                .into());
            }
        };
        let flags = fcntl_getfl(&fd).map_err(|error| {
            transport_error(format!("Cannot inspect MCP {label} status flags: {error}"))
        })?;
        if matches!(kind, EndpointKind::Pipe) && packet_writer(direction, flags) {
            return Err(EndpointError::PacketWriter(ApiError::unsupported(format!(
                "Packet-mode MCP {label} pipe"
            ))));
        }
        // Linux pipe2(O_DIRECT) sets the writer's flag, not the reader's.
        // A reader cannot attest upstream packet mode; byte-stream input is a
        // host precondition, alongside exclusive stream/flag ownership.
        let endpoint = Self {
            fd,
            kind,
            original_nonblock: flags.contains(OFlags::NONBLOCK),
            restore_needed: matches!(kind, EndpointKind::Pipe),
        };
        if endpoint.restore_needed && !endpoint.original_nonblock {
            fcntl_setfl(&endpoint.fd, flags | OFlags::NONBLOCK).map_err(|error| {
                transport_error(format!("Cannot lease nonblocking MCP {label}: {error}"))
            })?;
        }
        // fstat establishes type only. Exclusive stream and flag ownership is
        // the invocation contract, including named FIFOs and inherited aliases.
        Ok(endpoint)
    }

    fn pipe(&self) -> bool {
        matches!(self.kind, EndpointKind::Pipe)
    }

    fn restore(&mut self) -> Result<(), ApiError> {
        if self.restore_needed {
            let mut current = fcntl_getfl(&self.fd).map_err(|error| {
                transport_error(format!(
                    "Cannot read MCP lease flags for restoration: {error}"
                ))
            })?;
            current.set(OFlags::NONBLOCK, self.original_nonblock);
            fcntl_setfl(&self.fd, current).map_err(|error| {
                transport_error(format!("Cannot restore MCP NONBLOCK lease: {error}"))
            })?;
            self.restore_needed = false;
        }
        Ok(())
    }
}

impl Drop for Endpoint {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Default)]
struct Journal(Mutex<BTreeMap<String, Value>>);

impl Journal {
    fn started(&self, queued: &Queued) -> Result<(), ApiError> {
        if let Some(key) = &queued.key {
            self.lock()?.insert(
                key.clone(),
                json!({"request_id":valid_id(&queued.message),"stage":"executing"}),
            );
        }
        Ok(())
    }

    fn returned(&self, key: Option<&str>, response: &Value) -> Result<(), ApiError> {
        if let Some(key) = key {
            let id = response.get("id").cloned().unwrap_or(Value::Null);
            self.lock()?.insert(
                key.to_owned(),
                json!({"request_id":id,"stage":"returned","receipt":receipt_summary(response)}),
            );
        }
        Ok(())
    }

    fn finish(&self, key: Option<&str>) -> Result<(), ApiError> {
        if let Some(key) = key {
            self.lock()?.remove(key);
        }
        Ok(())
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, BTreeMap<String, Value>>, ApiError> {
        self.0
            .lock()
            .map_err(|_| transport_error("MCP receipt journal is unavailable"))
    }

    fn snapshot(&self, registry: &Registry) -> Value {
        let Ok(records) = self.lock() else {
            return json!({"unavailable":true});
        };
        let Ok(requests) = registry.lock() else {
            return json!({"unavailable":true,"returned":records.values().take(MAX_TRACKED).collect::<Vec<_>>()});
        };
        let pending: Vec<_> = requests
            .keys()
            .take(MAX_TRACKED)
            .map(|key| {
                records.get(key).cloned().unwrap_or_else(|| {
                    json!({
                        "request_id":serde_json::from_str::<Value>(key).unwrap_or(Value::Null),
                        "stage":"admitted_without_returned_receipt"
                    })
                })
            })
            .collect();
        json!({"pending":pending})
    }
}

/// Uses a temporary NONBLOCK lease on exclusively owned pipe/FIFO stdio.
/// Initialization and terminal diagnostics stay inside the same lease; callers
/// must not print a fallback error to inherited blocking stdout/stderr.
pub fn serve_stdio(initialize: impl FnOnce() -> Result<Api, ApiError>) -> Result<(), ApiError> {
    // An unsupported/blocking device stderr is deliberately skipped. There is
    // no safe bounded diagnostic delivery promise for arbitrary inherited IO.
    let mut diagnostic =
        match Endpoint::acquire(std::io::stderr(), "stderr", EndpointDirection::Output) {
            Ok(endpoint) => Some(endpoint),
            Err(EndpointError::PacketWriter(error)) => return Err(error),
            Err(EndpointError::Other(_)) => None,
        };
    let mut input = None;
    let mut output = None;
    let registry = Arc::new(Registry::default());
    let journal = Arc::new(Journal::default());
    let mut result = (|| {
        input = Some(
            Endpoint::acquire(std::io::stdin(), "stdin", EndpointDirection::Input)
                .map_err(EndpointError::into_api)?,
        );
        output = Some(
            Endpoint::acquire(std::io::stdout(), "stdout", EndpointDirection::Output)
                .map_err(EndpointError::into_api)?,
        );
        let api = initialize()?;
        // These matches follow the successful acquisition above, without a
        // panic if a future edit changes acquisition order.
        match (&input, &output) {
            (Some(input), Some(output)) => run(input, output, &api, &registry, &journal),
            _ => Err(transport_error("MCP stdio acquisition is incomplete")),
        }
    })();
    if let Err(error) = &result {
        diagnose(diagnostic.as_ref(), error, journal.snapshot(&registry));
    }
    for error in release_streams(&mut input, &mut output) {
        diagnose(diagnostic.as_ref(), &error, journal.snapshot(&registry));
        result = Err(error);
    }
    if let Some(mut endpoint) = diagnostic.take()
        && let Err(error) = endpoint.restore()
    {
        // Never fall back to a possibly blocking println after restoration.
        result = Err(error);
    }
    result
}

fn release_streams(input: &mut Option<Endpoint>, output: &mut Option<Endpoint>) -> Vec<ApiError> {
    let mut errors = Vec::new();
    // Leases can overlap the same OFD. Reverse acquisition order is essential:
    // a later lease may have observed NONBLOCK set by an earlier one. Consume
    // each endpoint here so even a failed restore's Drop retry completes before
    // restoring the preceding lease. Diagnostic was acquired first and is last.
    for slot in [output, input] {
        if let Some(mut endpoint) = slot.take() {
            let restored = endpoint.restore();
            drop(endpoint);
            if let Err(error) = restored {
                errors.push(error);
            }
        }
    }
    errors
}

fn diagnose(endpoint: Option<&Endpoint>, error: &ApiError, receipts: Value) {
    let Some(endpoint) = endpoint else {
        return;
    };
    let message: String = error.message.chars().take(1024).collect();
    let value = json!({"izu_mcp_transport_error":message,"next_action":"Transport failure is not rollback. Inspect native status, operation history and exact publication targets before retrying.","undelivered":receipts});
    let Ok(mut bytes) = bounded_json(&value, MAX_DIAGNOSTIC_BYTES - 1) else {
        return;
    };
    bytes.push(b'\n');
    let mut offset = 0;
    while offset < bytes.len() {
        let end = (offset + IO_CHUNK).min(bytes.len());
        match write(&endpoint.fd, &bytes[offset..end]) {
            Ok(0) | Err(_) => break,
            Ok(written) => offset += written,
        }
    }
    // Pipe diagnostics are best effort: EAGAIN discards the remainder. Regular
    // file calls have the same possible uninterruptible kernel IO as native IO.
}

fn run(
    input: &Endpoint,
    output: &Endpoint,
    api: &Api,
    registry: &Arc<Registry>,
    journal: &Arc<Journal>,
) -> Result<(), ApiError> {
    let (wake_read, wake_write) = UnixStream::pair().map_err(|error| {
        transport_error(format!("Cannot create MCP coordinator wakeup: {error}"))
    })?;
    wake_read
        .set_nonblocking(true)
        .and_then(|()| wake_write.set_nonblocking(true))
        .map_err(|error| {
            transport_error(format!("Cannot configure private MCP wakeup: {error}"))
        })?;
    let (request_sender, request_receiver) = mpsc::sync_channel::<Queued>(MAX_PENDING);
    let (response_sender, response_receiver) = mpsc::sync_channel::<Outbound>(1);
    let mut requests = Some(request_sender);
    let mut responses = Some(response_receiver);
    std::thread::scope(|scope| {
        let worker_registry = registry.clone();
        let worker_journal = journal.clone();
        let worker = std::thread::Builder::new()
            .name("izu-mcp-operation".into())
            .spawn_scoped(scope, move || -> Result<(), ApiError> {
                let mut session = Session::default();
                for queued in request_receiver {
                    worker_journal.started(&queued)?;
                    let response = handle_message(queued.message, api, &queued.token, &mut session);
                    if let Some(response) = response {
                        // Record exact returned provenance before a bounded send
                        // can block, or the coordinator can discard the receiver.
                        worker_journal.returned(queued.key.as_deref(), &response)?;
                        let frame =
                            Outbound::response(response, queued.key, Some(queued.suppress))?;
                        if response_sender.send(frame).is_err() {
                            return Ok(());
                        }
                        let _ = write(&wake_write, b"w");
                    } else {
                        worker_registry.finish(queued.key.as_deref())?;
                        worker_journal.finish(queued.key.as_deref())?;
                    }
                }
                Ok(())
            })
            .map_err(|error| {
                transport_error(format!("Cannot start MCP operation worker: {error}"))
            })?;
        let result = coordinate(
            input,
            output,
            &wake_read,
            &mut requests,
            &mut responses,
            registry,
            journal,
        );
        let cancellation = registry.cancel_all();
        // Both ends must be dropped BEFORE join. Otherwise a full response
        // channel, or an idle request receiver, can strand scoped shutdown.
        drop(requests.take());
        drop(responses.take());
        let joined = worker
            .join()
            .map_err(|_| transport_error("MCP operation worker terminated unexpectedly"));
        result?;
        cancellation?;
        joined?
    })
}

struct Writing {
    frame: Outbound,
    offset: usize,
}

impl Writing {
    fn suppressible(&self) -> bool {
        self.offset == 0 && self.frame.suppressed()
    }
}

#[derive(Default)]
struct DrainClock(Option<Instant>);

impl DrainClock {
    fn update(&mut self, eof: bool, pending: bool, now: Instant) -> Option<Instant> {
        if eof && pending {
            Some(*self.0.get_or_insert(now + EOF_OUTPUT_BUDGET))
        } else {
            self.0 = None;
            None
        }
    }
}

fn coordinate(
    input: &Endpoint,
    output: &Endpoint,
    wake: &UnixStream,
    requests: &mut Option<mpsc::SyncSender<Queued>>,
    responses: &mut Option<mpsc::Receiver<Outbound>>,
    registry: &Registry,
    journal: &Journal,
) -> Result<(), ApiError> {
    let mut framer = Framer::default();
    let mut local = VecDeque::new();
    let mut current: Option<Writing> = None;
    let mut eof = false;
    let mut input_failure = None;
    let mut worker_done = false;
    let mut wake_closed = false;
    let mut output_retry = None;
    let mut clock = DrainClock::default();
    let mut input_bytes = [0_u8; IO_CHUNK];
    loop {
        let mut progress = false;
        if current.is_none() {
            if let Some(receiver) = responses.as_ref() {
                match receiver.try_recv() {
                    Ok(frame) => current = Some(Writing { frame, offset: 0 }),
                    Err(mpsc::TryRecvError::Disconnected) => worker_done = true,
                    Err(mpsc::TryRecvError::Empty) => {}
                }
            }
            if current.is_none() {
                current = local.pop_front().map(|frame| Writing { frame, offset: 0 });
            }
        }
        let now = Instant::now();
        let drain_deadline = clock.update(eof, current.is_some() || !local.is_empty(), now);
        if drain_deadline.is_some_and(|deadline| now >= deadline) {
            return Err(transport_error(
                "MCP EOF output drain exceeded two seconds; undelivered responses were discarded",
            ));
        }
        if current.as_ref().is_some_and(Writing::suppressible) {
            if let Some(writing) = current.take() {
                registry.finish(writing.frame.key.as_deref())?;
                journal.finish(writing.frame.key.as_deref())?;
            }
            output_retry = None;
            continue;
        }
        if let Some(writing) = current.as_mut()
            && output_retry.is_none_or(|retry| now >= retry)
        {
            let end = (writing.offset + IO_CHUNK).min(writing.frame.bytes.len());
            match write(&output.fd, &writing.frame.bytes[writing.offset..end]) {
                Ok(0) => return Err(transport_error("MCP stdout returned a zero-length write")),
                Ok(written) => {
                    writing.offset += written;
                    output_retry = None;
                    progress = true;
                    if writing.offset == writing.frame.bytes.len() {
                        registry.finish(writing.frame.key.as_deref())?;
                        journal.finish(writing.frame.key.as_deref())?;
                        current = None;
                    }
                }
                Err(Errno::AGAIN) => output_retry = Some(now + WOULD_BLOCK_BACKOFF),
                Err(Errno::INTR) => output_retry = Some(now + WOULD_BLOCK_BACKOFF),
                Err(error) => {
                    return Err(transport_error(format!(
                        "Cannot write MCP response: {error}"
                    )));
                }
            }
        }
        if !eof {
            match read(&input.fd, &mut input_bytes) {
                Ok(0) => {
                    if let Some(frame) = framer.eof() {
                        accept(frame, requests.as_ref(), registry, &mut local)?;
                    }
                    eof = true;
                    registry.cancel_all()?;
                    drop(requests.take());
                    progress = true;
                }
                Ok(length) => {
                    let mut consumed = 0;
                    while consumed < length {
                        let (used, frame) = framer.push(&input_bytes[consumed..length]);
                        consumed += used;
                        if let Some(frame) = frame {
                            accept(frame, requests.as_ref(), registry, &mut local)?;
                        }
                    }
                    progress = true;
                }
                Err(Errno::AGAIN | Errno::INTR) => {}
                Err(error) => {
                    input_failure =
                        Some(transport_error(format!("Cannot read MCP input: {error}")));
                    eof = true;
                    registry.cancel_all()?;
                    drop(requests.take());
                    progress = true;
                }
            }
        }
        if worker_done && current.is_none() && local.is_empty() {
            return if eof {
                input_failure.map_or(Ok(()), Err)
            } else {
                Err(transport_error(
                    "MCP operation worker stopped while input was open",
                ))
            };
        }
        // Regular files are handled directly above. Pipe errors are monitored
        // even when no output is pending; writable readiness is requested only
        // for useful writes. XNU can report OUT while a small pipe is actually
        // full; EAGAIN installs a timer backoff instead of spinning on that flag.
        let now = Instant::now();
        let mut fds = Vec::with_capacity(3);
        let output_index = if output.pipe() {
            let flags = output_poll_flags(
                current.is_some() && output_retry.is_none_or(|retry| now >= retry),
            );
            fds.push(PollFd::new(&output.fd, flags));
            Some(fds.len() - 1)
        } else {
            None
        };
        if !eof && input.pipe() {
            fds.push(PollFd::new(&input.fd, PollFlags::IN));
        }
        let wake_index = if !wake_closed {
            fds.push(PollFd::new(wake, PollFlags::IN));
            Some(fds.len() - 1)
        } else {
            None
        };
        let timeout = if progress {
            // Even a continuous notification/input stream must not starve
            // idle stdout error monitoring. Useful IO permits a zero-time poll;
            // otherwise this loop blocks until an event or a retry deadline.
            Some(Timespec {
                tv_sec: 0,
                tv_nsec: 0,
            })
        } else {
            [drain_deadline, output_retry]
                .into_iter()
                .flatten()
                .min()
                .map(|deadline| timeout_until(deadline, now))
        };
        match poll(&mut fds, timeout.as_ref()) {
            Ok(_) => {}
            Err(Errno::INTR) => continue,
            Err(error) => {
                return Err(transport_error(format!(
                    "Cannot wait for MCP stdio: {error}"
                )));
            }
        }
        if output_index.is_some_and(|index| {
            fds[index]
                .revents()
                .intersects(PollFlags::ERR | PollFlags::HUP | PollFlags::NVAL)
        }) {
            return Err(transport_error("MCP stdout closed or became unavailable"));
        }
        if let Some(index) = wake_index
            && !fds[index].revents().is_empty()
        {
            let mut notifications = [0_u8; 64];
            loop {
                match read(wake, &mut notifications) {
                    Ok(0) => {
                        wake_closed = true;
                        break;
                    }
                    Ok(_) => {}
                    Err(Errno::AGAIN) => break,
                    Err(Errno::INTR) => continue,
                    Err(error) => {
                        return Err(transport_error(format!("Cannot read MCP wakeup: {error}")));
                    }
                }
            }
        }
        // Input HUP is not EOF until read returns zero: buffered bytes still
        // contain complete requests and cancellation notifications to process.
    }
}

fn accept(
    frame: Result<Vec<u8>, FrameError>,
    sender: Option<&mpsc::SyncSender<Queued>>,
    registry: &Registry,
    local: &mut VecDeque<Outbound>,
) -> Result<(), ApiError> {
    let reply = match admit(frame, registry)? {
        Admission::Execute(queued) => {
            let sender = sender.ok_or_else(|| transport_error("MCP input admission is closed"))?;
            submit(sender, queued)?
        }
        Admission::Reply(reply) => Some(reply),
        Admission::Ignore => None,
    };
    if let Some(reply) = reply {
        if local.len() >= MAX_LOCAL_REPLIES {
            return Err(transport_error(
                "MCP error response capacity is exhausted; transport closed without blocking input",
            ));
        }
        local.push_back(reply);
    }
    Ok(())
}

fn timeout_until(deadline: Instant, now: Instant) -> Timespec {
    let duration = deadline.saturating_duration_since(now);
    Timespec {
        tv_sec: duration.as_secs().min(i64::MAX as u64) as _,
        tv_nsec: duration.subsec_nanos() as _,
    }
}

fn output_poll_flags(write_pending: bool) -> PollFlags {
    // Darwin poll registers no filter for an empty mask and misses disconnect.
    // HUP enables observation without making a healthy writable pipe ready.
    let disconnect = PollFlags::ERR | PollFlags::HUP;
    if write_pending {
        disconnect | PollFlags::OUT
    } else {
        disconnect
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_output_poll_waits_for_disconnect_without_writable_spin() {
        let (reader, writer) = std::io::pipe().expect("owned anonymous pipe");
        let flags = fcntl_getfl(&writer).expect("owned pipe flags");
        fcntl_setfl(&writer, flags | OFlags::NONBLOCK).expect("owned nonblocking writer");
        let idle = output_poll_flags(false);
        assert!(!idle.contains(PollFlags::OUT));
        let mut fds = [PollFd::new(&writer, idle)];
        let timeout = Timespec {
            tv_sec: 0,
            tv_nsec: 25_000_000,
        };
        assert_eq!(poll(&mut fds, Some(&timeout)).expect("healthy poll"), 0);
        assert!(fds[0].revents().is_empty());
        drop(reader);
        assert_eq!(poll(&mut fds, Some(&timeout)).expect("disconnect poll"), 1);
        assert!(fds[0].revents().intersects(PollFlags::ERR | PollFlags::HUP));
        assert!(output_poll_flags(true).contains(PollFlags::OUT));
    }

    fn create_owned_fifo(path: &std::path::Path) {
        use std::process::{Command, Stdio};
        let mut child = Command::new("/usr/bin/mkfifo")
            .args(["-m", "700"])
            .arg(path)
            .current_dir(path.parent().expect("owned FIFO parent"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("owned FIFO creation");
        let deadline = Instant::now() + Duration::from_secs(5);
        let outcome = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(None) => break Err("FIFO creation timed out".to_owned()),
                Err(error) => break Err(format!("Cannot observe FIFO creation: {error}")),
            }
        };
        if outcome.is_err() {
            let _ = child.kill();
            let cleanup_deadline = Instant::now() + Duration::from_secs(2);
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => break,
                    Ok(None) if Instant::now() < cleanup_deadline => {
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    other => panic!("Cannot reap owned FIFO fixture {}: {other:?}", child.id()),
                }
            }
        }
        assert!(outcome.expect("owned FIFO fixture completed").success());
    }

    #[test]
    fn overlapping_fifo_leases_restore_in_reverse_order_for_both_original_states() {
        use std::os::unix::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().expect("private overlapping lease fixture");
        let path = temp.path().join("aliased-stdio-fifo");
        create_owned_fifo(&path);
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OFlags::NONBLOCK.bits() as _)
            .open(&path)
            .expect("reader");
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlags::NONBLOCK.bits() as _)
            .open(&path)
            .expect("writer");
        for original in [false, true] {
            let mut flags = fcntl_getfl(&reader).expect("original flags");
            flags.set(OFlags::NONBLOCK, original);
            fcntl_setfl(&reader, flags).expect("original state");
            let mut input = Some(
                Endpoint::acquire(&reader, "aliased stdin", EndpointDirection::Input)
                    .expect("first lease"),
            );
            let mut output = Some(
                Endpoint::acquire(&reader, "aliased stdout", EndpointDirection::Output)
                    .expect("second lease"),
            );
            let current = fcntl_getfl(&reader).expect("leased flags") | OFlags::APPEND;
            fcntl_setfl(&reader, current).expect("unrelated current flag");
            assert!(release_streams(&mut input, &mut output).is_empty());
            assert!(input.is_none() && output.is_none());
            let restored = fcntl_getfl(&reader).expect("restored flags");
            assert_eq!(restored.contains(OFlags::NONBLOCK), original);
            assert!(restored.contains(OFlags::APPEND));
        }
    }

    #[test]
    fn pipe_nonblock_lease_restores_shared_alias_and_preserves_other_flags() {
        use std::os::unix::fs::OpenOptionsExt;
        let temp = tempfile::tempdir().expect("private lease fixture");
        let path = temp.path().join("stdio-fifo");
        create_owned_fifo(&path);
        let reader = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(OFlags::NONBLOCK.bits() as _)
            .open(&path)
            .expect("nonblocking reader open");
        let _writer = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(OFlags::NONBLOCK.bits() as _)
            .open(&path)
            .expect("writer open");
        let alias = reader.try_clone().expect("same OFD alias");
        for original in [false, true] {
            let mut before = fcntl_getfl(&reader).expect("flags");
            before.set(OFlags::NONBLOCK, original);
            fcntl_setfl(&reader, before).expect("original state");
            let mut lease =
                Endpoint::acquire(&reader, "test stdin", EndpointDirection::Input).expect("lease");
            assert!(
                fcntl_getfl(&alias)
                    .expect("shared flags")
                    .contains(OFlags::NONBLOCK)
            );
            let current = fcntl_getfl(&alias).expect("current flags") | OFlags::APPEND;
            fcntl_setfl(&alias, current).expect("independent flag under same owner");
            lease
                .restore()
                .expect("explicit normal/error return restoration");
            let restored = fcntl_getfl(&alias).expect("restored flags");
            assert_eq!(restored.contains(OFlags::NONBLOCK), original);
            assert!(restored.contains(OFlags::APPEND));
        }
        let flags = fcntl_getfl(&reader).expect("drop test flags") & !OFlags::NONBLOCK;
        fcntl_setfl(&reader, flags).expect("drop original flags");
        drop(
            Endpoint::acquire(&reader, "test error cleanup", EndpointDirection::Input)
                .expect("lease"),
        );
        assert!(
            !fcntl_getfl(&alias)
                .expect("drop restoration")
                .contains(OFlags::NONBLOCK)
        );
    }

    #[test]
    fn eof_drain_clock_excludes_engine_settling_and_does_not_extend_on_progress() {
        let start = Instant::now();
        let mut clock = DrainClock::default();
        assert_eq!(clock.update(true, false, start), None);
        let ready = start + Duration::from_secs(20);
        let deadline = clock.update(true, true, ready).expect("pending output");
        assert_eq!(deadline, ready + EOF_OUTPUT_BUDGET);
        assert_eq!(
            clock.update(true, true, ready + Duration::from_secs(1)),
            Some(deadline)
        );
        assert_eq!(clock.update(true, false, deadline), None);
    }

    #[test]
    fn partial_frame_cancellation_cannot_suppress_the_remaining_bytes() {
        let suppress = Arc::new(AtomicBool::new(false));
        let frame = Outbound::response(
            json!({"id":1,"result":{}}),
            Some("1".into()),
            Some(suppress.clone()),
        )
        .expect("frame");
        let mut writing = Writing { frame, offset: 0 };
        suppress.store(true, Ordering::Release);
        assert!(writing.suppressible());
        writing.offset = 1;
        assert!(!writing.suppressible());
    }
}
