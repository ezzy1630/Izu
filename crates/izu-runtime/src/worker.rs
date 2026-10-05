use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use izu_platform::Directory;
use serde::{Deserialize, Serialize};

use crate::registry::{FORMAT_VERSION, read_json, write_json_durable};
use crate::types::{DirectoryIdentity, io_error, now_ms, validate_argv};
use crate::{CommandOutcome, EnvironmentPolicy, JobId, RunRequest, RuntimeError};

const MAX_TICKET_BYTES: u64 = 32 * 1024 * 1024;
const MAX_RESULT_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerTicket {
    format_version: u32,
    pub id: JobId,
    pub cwd: PathBuf,
    directory: DirectoryIdentity,
    argv: Vec<String>,
    timeout_ms: u64,
    environment: EnvironmentPolicy,
    env_overlay: BTreeMap<String, String>,
}
impl WorkerTicket {
    pub fn new(id: JobId, request: &RunRequest) -> Self {
        Self {
            format_version: FORMAT_VERSION,
            id,
            cwd: request.workspace.cwd.clone(),
            directory: request.workspace.directory,
            argv: request.argv.clone(),
            timeout_ms: request.timeout_ms,
            environment: request.environment,
            env_overlay: request.env_overlay.clone(),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkerResult {
    pub format_version: u32,
    pub id: JobId,
    pub finished_at_unix_ms: i64,
    pub outcome: CommandOutcome,
}

pub(crate) fn write_ticket(
    directory: &Directory,
    path: &Path,
    id: JobId,
    request: &RunRequest,
) -> Result<(), RuntimeError> {
    write_json_durable(
        directory,
        OsStr::new("ticket.json"),
        path,
        &WorkerTicket::new(id, request),
        MAX_TICKET_BYTES,
    )
}
pub(crate) fn read_result(
    directory: &Directory,
    path: &Path,
    id: &JobId,
) -> Result<WorkerResult, RuntimeError> {
    let result: WorkerResult =
        read_json(directory, OsStr::new("result.json"), path, MAX_RESULT_BYTES)?;
    if result.format_version != FORMAT_VERSION || &result.id != id {
        return Err(RuntimeError::Invalid(
            "worker result identity/version mismatch".into(),
        ));
    }
    Ok(result)
}

/// Entry point for a hidden CLI subcommand or the included worker binary.
///
/// The worker is a retained group leader, not a daemon. It does not launch the
/// selected writer until its parent has durably registered the PID and sends `G`.
/// After reporting the command's exit, it remains unreaped until the parent kills
/// its still-owned group. Losing the control pipe stops the cooperating group.
pub fn worker_main(ticket_path: &Path) -> Result<(), RuntimeError> {
    if !ticket_path.is_absolute() {
        return Err(RuntimeError::Invalid(
            "worker ticket must be absolute".into(),
        ));
    }
    let parent = ticket_path
        .parent()
        .ok_or_else(|| RuntimeError::Invalid("ticket has no parent".into()))?;
    // Identity arrives over the owned control pipe, independently of paths that
    // another process could rename. No selected source command precedes this ACK.
    let mut handshake = [0_u8; 49];
    std::io::stdin()
        .read_exact(&mut handshake)
        .map_err(|error| io_error("read durable launch handshake", ticket_path, error))?;
    if handshake[0] != b'G' {
        return Err(RuntimeError::Invalid("invalid launch handshake".into()));
    }
    let expected_device = u64::from_le_bytes(
        handshake[1..9]
            .try_into()
            .map_err(|_| RuntimeError::Invalid("invalid directory device bytes".into()))?,
    );
    let expected_inode = u64::from_le_bytes(
        handshake[9..17]
            .try_into()
            .map_err(|_| RuntimeError::Invalid("invalid directory inode bytes".into()))?,
    );
    let expected_id: JobId = std::str::from_utf8(&handshake[17..])
        .map_err(|error| RuntimeError::Invalid(error.to_string()))?
        .parse()?;
    let directory = Directory::open(parent)
        .map_err(|error| io_error("pin worker ticket directory", parent, error))?;
    crate::os::check_private_directory_handle(&directory, parent)?;
    if crate::os::directory_identity_handle(&directory, parent)?
        != (DirectoryIdentity {
            device: expected_device,
            inode: expected_inode,
        })
    {
        return Err(RuntimeError::RegistryLocatorChanged(parent.to_path_buf()));
    }
    let ticket: WorkerTicket = read_json(
        &directory,
        OsStr::new("ticket.json"),
        ticket_path,
        MAX_TICKET_BYTES,
    )?;
    if ticket.id != expected_id {
        return Err(RuntimeError::Invalid(
            "ticket does not match owned launch identity".into(),
        ));
    }
    if ticket.format_version != FORMAT_VERSION
        || ticket_path.file_name() != Some(std::ffi::OsStr::new("ticket.json"))
        || parent.file_name() != Some(std::ffi::OsStr::new(ticket.id.as_str()))
    {
        return Err(RuntimeError::Invalid(
            "worker ticket version/path/identity mismatch".into(),
        ));
    }
    validate_argv(&ticket.argv, 16 * 1024 * 1024, true)?;
    let mut env_bytes = 0_usize;
    if ticket.env_overlay.len() > 4096 {
        return Err(RuntimeError::Invalid(
            "worker environment entry bound exceeded".into(),
        ));
    }
    for (key, value) in &ticket.env_overlay {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(RuntimeError::Invalid(
                "worker environment contains invalid keys/NUL".into(),
            ));
        }
        env_bytes = env_bytes
            .checked_add(key.len())
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| RuntimeError::Invalid("environment size overflow".into()))?;
    }
    if env_bytes > 16 * 1024 * 1024 {
        return Err(RuntimeError::Invalid(
            "worker environment byte bound exceeded".into(),
        ));
    }
    let current_directory = Directory::open(Path::new("."))
        .map_err(|error| io_error("pin actual worker cwd", &ticket.cwd, error))?;
    if crate::os::directory_identity_handle(&current_directory, &ticket.cwd)? != ticket.directory {
        return Err(RuntimeError::StaleWorkspace(
            "worker was not launched at its bound cwd".into(),
        ));
    }
    // The worker alone changes its cwd. The selected child inherits this pinned
    // directory; no second path-based chdir can redirect it after verification.
    rustix::process::fchdir(current_directory.file())
        .map_err(|error| io_error("bind pinned worker cwd", &ticket.cwd, error.into()))?;
    let parent_alive = Arc::new(AtomicBool::new(true));
    let watcher = parent_alive.clone();
    thread::Builder::new()
        .name("izu-parent-watch".into())
        .spawn(move || {
            let mut byte = [0_u8; 1];
            let mut stdin = std::io::stdin();
            loop {
                match stdin.read(&mut byte) {
                    Ok(0) => break,
                    Ok(_) => {}
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            watcher.store(false, Ordering::Release);
        })
        .map_err(|error| io_error("spawn parent watchdog", ticket_path, error))?;
    if ticket.timeout_ms == 0 || ticket.timeout_ms > 24 * 60 * 60 * 1000 {
        return Err(RuntimeError::Invalid(
            "worker timeout is outside supported range".into(),
        ));
    }
    let execution_deadline = std::time::Instant::now()
        .checked_add(Duration::from_millis(ticket.timeout_ms))
        .ok_or_else(|| RuntimeError::Invalid("worker deadline overflow".into()))?;
    let report_path = parent.join("result.json");
    let mut command = Command::new(&ticket.argv[0]);
    command
        .args(&ticket.argv[1..])
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if ticket.environment == EnvironmentPolicy::Clear {
        command.env_clear();
    }
    command.envs(&ticket.env_overlay);
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let result = WorkerResult {
                format_version: FORMAT_VERSION,
                id: ticket.id,
                finished_at_unix_ms: now_ms()?,
                outcome: CommandOutcome::SpawnFailed {
                    message: error.to_string(),
                },
            };
            write_json_durable(
                &directory,
                OsStr::new("result.json"),
                &report_path,
                &result,
                MAX_RESULT_BYTES,
            )?;
            return retain_leader(&parent_alive);
        }
    };
    loop {
        if !parent_alive.load(Ordering::Acquire) || std::time::Instant::now() >= execution_deadline
        {
            return crate::os::kill_current_group();
        }
        if let Some(status) = child
            .try_wait()
            .map_err(|error| io_error("observe selected command", &ticket.cwd, error))?
        {
            #[cfg(unix)]
            let signal = {
                use std::os::unix::process::ExitStatusExt;
                status.signal()
            };
            #[cfg(not(unix))]
            let signal = None;
            let result = WorkerResult {
                format_version: FORMAT_VERSION,
                id: ticket.id,
                finished_at_unix_ms: now_ms()?,
                outcome: CommandOutcome::Exit {
                    code: status.code(),
                    signal,
                },
            };
            write_json_durable(
                &directory,
                OsStr::new("result.json"),
                &report_path,
                &result,
                MAX_RESULT_BYTES,
            )?;
            return retain_leader(&parent_alive);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

fn retain_leader(parent_alive: &AtomicBool) -> Result<(), RuntimeError> {
    while parent_alive.load(Ordering::Acquire) {
        thread::sleep(Duration::from_millis(5));
    }
    crate::os::kill_current_group()
}

pub(crate) fn release_writer(
    pipe: &mut std::process::ChildStdin,
    directory: &Directory,
    path: &Path,
    id: &JobId,
) -> Result<(), RuntimeError> {
    let identity = crate::os::directory_identity_handle(directory, path)?;
    let mut handshake = Vec::with_capacity(49);
    handshake.push(b'G');
    handshake.extend_from_slice(&identity.device.to_le_bytes());
    handshake.extend_from_slice(&identity.inode.to_le_bytes());
    handshake.extend_from_slice(id.as_str().as_bytes());
    pipe.write_all(&handshake).map_err(|error| {
        io_error(
            "release durably registered writer",
            Path::new("worker stdin"),
            error,
        )
    })
}
