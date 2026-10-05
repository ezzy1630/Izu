//! The entire process/file ownership boundary. No raw syscalls or unsafe izu code.
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::types::{DirectoryIdentity, io_error};
use crate::{CapturedOutput, JobOutput, RuntimeError};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub(crate) fn check_private_directory_handle(
    directory: &izu_platform::Directory,
    path: &Path,
) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        let metadata = directory
            .metadata_self()
            .map_err(|error| io_error("inspect private directory", path, error))?;
        if !metadata.is_dir()
            || metadata.uid() != rustix::process::geteuid().as_raw()
            || metadata.mode() & 0o077 != 0
        {
            return Err(RuntimeError::Invalid(format!(
                "registry directory must be a private, owned, real directory: {}",
                path.display()
            )));
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, path);
        Err(RuntimeError::Unsupported(
            "private registry ownership is currently Unix only".into(),
        ))
    }
}

#[cfg(unix)]
pub(crate) fn check_private_file(file: &File, path: &Path) -> Result<(), RuntimeError> {
    let metadata = file
        .metadata()
        .map_err(|error| io_error("inspect private file", path, error))?;
    if !metadata.is_file()
        || metadata.uid() != rustix::process::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(RuntimeError::Invalid(format!(
            "registry file must be a private owned regular file: {}",
            path.display()
        )));
    }
    Ok(())
}

pub(crate) fn lock_exclusive(file: &File, path: &Path) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        rustix::fs::flock(file, rustix::fs::FlockOperation::NonBlockingLockExclusive).map_err(
            |error| {
                if error == rustix::io::Errno::WOULDBLOCK {
                    RuntimeError::Busy(path.to_path_buf())
                } else {
                    io_error("lock runtime registry", path, error.into())
                }
            },
        )
    }
    #[cfg(not(unix))]
    {
        let _ = (file, path);
        Err(RuntimeError::Unsupported(
            "registry locking is currently Unix only".into(),
        ))
    }
}

pub(crate) fn directory_identity(path: &Path) -> Result<DirectoryIdentity, RuntimeError> {
    let directory = izu_platform::Directory::open(path)
        .map_err(|error| io_error("pin workspace directory", path, error))?;
    directory_identity_handle(&directory, path)
}

pub(crate) fn directory_identity_handle(
    directory: &izu_platform::Directory,
    path: &Path,
) -> Result<DirectoryIdentity, RuntimeError> {
    #[cfg(unix)]
    {
        let metadata = directory
            .metadata_self()
            .map_err(|error| io_error("inspect workspace directory", path, error))?;
        if !metadata.is_dir() {
            return Err(RuntimeError::Invalid(
                "workspace cwd must be a real directory".into(),
            ));
        }
        Ok(DirectoryIdentity {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = (directory, path);
        Err(RuntimeError::Unsupported(
            "directory identity is currently Unix only".into(),
        ))
    }
}

pub(crate) fn spawn_group(
    command: &mut Command,
) -> Result<izu_process::OwnedProcess, std::io::Error> {
    #[cfg(unix)]
    {
        command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        izu_process::OwnedProcess::spawn(command)
    }
    #[cfg(not(unix))]
    {
        let _ = command;
        Err(std::io::Error::new(
            ErrorKind::Unsupported,
            "owned process groups require Unix",
        ))
    }
}

pub(crate) fn kill_owned_group(child: &mut izu_process::OwnedProcess) -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        let report = child.stop(Duration::from_secs(5));
        if report.cooperative_stop_succeeded() {
            Ok(())
        } else {
            Err(RuntimeError::UnsafeClose(format!(
                "cooperating group shutdown uncertain: {report:?}"
            )))
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child;
        Err(RuntimeError::Unsupported(
            "group shutdown requires Unix".into(),
        ))
    }
}

pub(crate) fn kill_current_group() -> Result<(), RuntimeError> {
    #[cfg(unix)]
    {
        rustix::process::kill_process_group(
            rustix::process::getpid(),
            rustix::process::Signal::KILL,
        )
        .map_err(|error| {
            RuntimeError::UnsafeClose(format!("worker watchdog could not stop its group: {error}"))
        })
    }
    #[cfg(not(unix))]
    {
        Err(RuntimeError::Unsupported(
            "group shutdown requires Unix".into(),
        ))
    }
}

#[derive(Clone, Default)]
pub(crate) struct Capture(Arc<Mutex<CapturedOutput>>);
impl Capture {
    fn append(&self, bytes: &[u8], limit: usize) {
        let mut capture = match self.0.lock() {
            Ok(capture) => capture,
            Err(poisoned) => poisoned.into_inner(),
        };
        let keep = bytes.len().min(limit.saturating_sub(capture.bytes.len()));
        capture.bytes.extend_from_slice(&bytes[..keep]);
        let dropped = u64::try_from(bytes.len() - keep).unwrap_or(u64::MAX);
        capture.dropped_bytes = capture.dropped_bytes.saturating_add(dropped);
    }
    fn snapshot(&self) -> CapturedOutput {
        match self.0.lock() {
            Ok(capture) => capture.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }
}

pub(crate) struct OutputReaders {
    stdout: Capture,
    stderr: Capture,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
}
impl OutputReaders {
    pub fn start(
        child: &mut izu_process::OwnedProcess,
        limit: usize,
    ) -> Result<Self, RuntimeError> {
        let stdout = child
            .take_stdout()
            .ok_or_else(|| RuntimeError::Invalid("worker stdout pipe missing".into()))?;
        let stderr = child
            .take_stderr()
            .ok_or_else(|| RuntimeError::Invalid("worker stderr pipe missing".into()))?;
        let output = Self {
            stdout: Capture::default(),
            stderr: Capture::default(),
            stop: Arc::new(AtomicBool::new(false)),
            threads: Vec::new(),
        };
        let mut threads = Vec::new();
        threads.push(reader(
            stdout,
            output.stdout.clone(),
            output.stop.clone(),
            limit,
        )?);
        match reader(stderr, output.stderr.clone(), output.stop.clone(), limit) {
            Ok(thread) => threads.push(thread),
            Err(error) => {
                output.stop.store(true, Ordering::Release);
                for thread in threads {
                    let _ = thread.join();
                }
                return Err(error);
            }
        }
        Ok(Self { threads, ..output })
    }
    pub fn snapshot(&self) -> JobOutput {
        JobOutput {
            stdout: self.stdout.snapshot(),
            stderr: self.stderr.snapshot(),
        }
    }
    pub fn stop(mut self) -> JobOutput {
        self.stop.store(true, Ordering::Release);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
        self.snapshot()
    }
}

fn reader<R: Read + std::os::fd::AsFd + Send + 'static>(
    mut pipe: R,
    capture: Capture,
    stop: Arc<AtomicBool>,
    limit: usize,
) -> Result<JoinHandle<()>, RuntimeError> {
    izu_process::set_nonblocking(&pipe)
        .map_err(|error| io_error("make log pipe nonblocking", Path::new("pipe"), error))?;
    thread::Builder::new()
        .name("izu-log-drain".into())
        .spawn(move || {
            let mut bytes = [0_u8; 8192];
            let mut reads_after_stop = 0_u16;
            loop {
                let stopping = stop.load(Ordering::Acquire);
                if stopping {
                    reads_after_stop = reads_after_stop.saturating_add(1);
                    if reads_after_stop > 256 {
                        break;
                    }
                }
                match pipe.read(&mut bytes) {
                    Ok(0) => break,
                    Ok(size) => capture.append(&bytes[..size], limit),
                    Err(error) if error.kind() == ErrorKind::WouldBlock => {
                        if stopping {
                            break;
                        }
                        thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })
        .map_err(|error| io_error("spawn log drain", Path::new("thread"), error))
}
