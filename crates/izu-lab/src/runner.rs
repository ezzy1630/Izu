use izu_process::{
    BaseEnvironment, CommandSpec, OutputPolicy, RunOptions, Termination, WorkerLauncher,
};
use serde::Serialize;
use std::{
    fs, io,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone)]
pub struct Runner {
    pub executable: PathBuf,
    pub timeout: Duration,
    pub output_limit: usize,
    pub measure_rss: bool,
    pub measure_allocation: bool,
    pub worker: WorkerLauncher,
    pub worker_sha256: String,
}

#[derive(Debug, Serialize)]
pub struct Run {
    pub executable: PathBuf,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    /// Target spawn through exit; does not include harness worker setup/cleanup.
    pub elapsed_ns: u64,
    pub command_elapsed_ns: Option<u64>,
    pub launcher_elapsed_ns: u64,
    pub cleanup_elapsed_ns: u64,
    pub started_monotonic_ns: u64,
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: String,
    pub stderr: String,
    pub output_truncated: bool,
    pub output_capture_error: Option<String>,
    pub process_termination: String,
    pub cleanup_status: String,
    pub worker_executable: PathBuf,
    pub worker_sha256: String,
    pub allocated_bytes_before: Option<u64>,
    pub allocated_bytes_after: Option<u64>,
    pub maximum_rss_bytes: Option<u64>,
    pub measurement_wrapper: Option<PathBuf>,
    pub rss_method: String,
    pub bytes_read: Option<u64>,
    pub bytes_written: Option<u64>,
}

impl Run {
    pub fn succeeded(&self) -> bool {
        self.exit_code == Some(0)
            && !self.timed_out
            && self.output_capture_error.is_none()
            && !self.output_truncated
            && self.command_elapsed_ns.is_some()
    }
}

fn maximum_rss(stderr: &str) -> Option<u64> {
    if cfg!(target_os = "macos") {
        stderr
            .lines()
            .find(|line| line.contains("maximum resident set size"))?
            .split_whitespace()
            .next()?
            .parse()
            .ok()
    } else if cfg!(target_os = "linux") {
        stderr
            .lines()
            .find(|line| line.contains("Maximum resident set size (kbytes):"))?
            .split_once(':')?
            .1
            .trim()
            .parse::<u64>()
            .ok()?
            .checked_mul(1024)
    } else {
        None
    }
}

/// Counts physical allocation in 512-byte POSIX blocks without following links.
pub fn allocated_bytes(root: &Path) -> io::Result<Option<u64>> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = fs::symlink_metadata(root)?;
        let mut total = metadata
            .blocks()
            .checked_mul(512)
            .ok_or_else(|| io::Error::other("allocation overflow"))?;
        if metadata.is_dir() {
            for entry in fs::read_dir(root)? {
                let size = allocated_bytes(&entry?.path())?.unwrap_or(0);
                total = total
                    .checked_add(size)
                    .ok_or_else(|| io::Error::other("allocation overflow"))?;
            }
        }
        Ok(Some(total))
    }
    #[cfg(not(unix))]
    {
        let _ = root;
        Ok(None)
    }
}

/// All helper/child tempfile use inherits TMPDIR; require the caller's chosen
/// temporary root and evidence destination to share the scratch filesystem.
pub fn validate_storage(scratch: &Path, output: &Path) -> io::Result<PathBuf> {
    let scratch = fs::canonicalize(scratch)?;
    let temp = std::env::var_os("TMPDIR").ok_or_else(|| {
        io::Error::other(
            "set TMPDIR explicitly to owned temporary storage on the scratch filesystem",
        )
    })?;
    let temp = fs::canonicalize(temp)?;
    let parent = fs::canonicalize(
        output
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new(".")),
    )?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let device = fs::metadata(&scratch)?.dev();
        if fs::metadata(temp)?.dev() != device || fs::metadata(parent)?.dev() != device {
            return Err(io::Error::other(
                "TMPDIR and output must be on the explicitly selected scratch filesystem",
            ));
        }
    }
    Ok(scratch)
}

impl Runner {
    pub fn run(&self, args: &[String], cwd: &Path, stdin: Option<&[u8]>) -> io::Result<Run> {
        self.run_interactive(args, cwd, stdin.unwrap_or_default(), &mut |_, _| true)
    }
    pub fn run_interactive(
        &self,
        args: &[String],
        cwd: &Path,
        input: &[u8],
        close_stdin: &mut dyn FnMut(&[u8], &[u8]) -> bool,
    ) -> io::Result<Run> {
        let before = self
            .measure_allocation
            .then(|| allocated_bytes(cwd).ok().flatten())
            .flatten();
        let mut command = if self.measure_rss {
            let mut command = CommandSpec::new("/usr/bin/time");
            command
                .arg(if cfg!(target_os = "macos") {
                    "-l"
                } else {
                    "-v"
                })
                .arg(self.executable.as_os_str());
            command
        } else {
            CommandSpec::new(self.executable.as_os_str())
        };
        command.args(args.iter()).current_dir(cwd);
        let temp = std::env::var_os("TMPDIR")
            .ok_or_else(|| io::Error::other("controlled TMPDIR is required"))?;
        command
            .env("TMPDIR", temp)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin");
        for key in ["CARGO_HOME", "RUSTUP_HOME"] {
            if let Some(value) = std::env::var_os(key) {
                command.env(key, value);
            }
        }
        command
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        let options = RunOptions {
            stdin_limit: 64 * 1024,
            stdout_limit: self.output_limit,
            stderr_limit: self.output_limit,
            timeout: self.timeout,
            cleanup_timeout: Duration::from_secs(2),
            output_policy: OutputPolicy::TruncateDrain,
            base_environment: BaseEnvironment::Clear,
        };
        static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        let epoch = *EPOCH.get_or_init(Instant::now);
        let started_monotonic_ns =
            u64::try_from(Instant::now().duration_since(epoch).as_nanos()).unwrap_or(u64::MAX);
        let outcome = izu_process::run_interactive(
            &command,
            input,
            &options,
            &self.worker,
            &|| false,
            close_stdin,
        )
        .map_err(io::Error::other)?;
        let nanos = |duration: Duration| u64::try_from(duration.as_nanos()).unwrap_or(u64::MAX);
        let command_elapsed_ns = outcome.command_elapsed.map(nanos);
        let mut errors = Vec::new();
        if !outcome.cleanup.cooperative_stop_succeeded() {
            errors.push("owned cooperative process group shutdown uncertain".to_owned());
        }
        if !matches!(
            outcome.termination,
            Termination::Completed | Termination::TimedOut
        ) {
            errors.push(format!("process terminated: {:?}", outcome.termination));
        }
        if !outcome.stdout.eof || !outcome.stderr.eof {
            errors.push("output stream EOF not observed".to_owned());
        }
        let timed_out = matches!(outcome.termination, Termination::TimedOut);
        let stderr = String::from_utf8_lossy(&outcome.stderr.bytes).into_owned();
        let maximum_rss_bytes = if self.measure_rss && !timed_out && outcome.stderr.dropped == 0 {
            maximum_rss(&stderr)
        } else {
            None
        };
        Ok(Run {
            executable: self.executable.clone(),
            args: args.to_vec(),
            cwd: cwd.to_owned(),
            elapsed_ns: command_elapsed_ns.unwrap_or_else(|| nanos(outcome.elapsed)),
            command_elapsed_ns,
            launcher_elapsed_ns: nanos(outcome.elapsed),
            cleanup_elapsed_ns: nanos(outcome.cleanup_elapsed),
            started_monotonic_ns,
            exit_code: outcome.exit.and_then(|status| status.code()),
            timed_out,
            stdout: String::from_utf8_lossy(&outcome.stdout.bytes).into_owned(),
            stderr,
            output_truncated: outcome.stdout.dropped > 0 || outcome.stderr.dropped > 0,
            output_capture_error: (!errors.is_empty()).then(|| errors.join("; ")),
            process_termination: format!("{:?}", outcome.termination),
            cleanup_status: format!("{:?}", outcome.cleanup),
            worker_executable: outcome.worker_executable,
            worker_sha256: self.worker_sha256.clone(),
            allocated_bytes_before: before,
            allocated_bytes_after: self
                .measure_allocation
                .then(|| allocated_bytes(cwd).ok().flatten())
                .flatten(),
            maximum_rss_bytes,
            measurement_wrapper: self.measure_rss.then(|| PathBuf::from("/usr/bin/time")),
            rss_method: if self.measure_rss {
                if cfg!(target_os = "macos") {
                    "/usr/bin/time -l; target latency includes time wrapper".into()
                } else {
                    "/usr/bin/time -v; kbytes converted to bytes; target latency includes time wrapper".into()
                }
            } else {
                "unavailable: unwrapped latency pass; use --measure-rss separately".into()
            },
            bytes_read: None,
            bytes_written: None,
        })
    }
}
