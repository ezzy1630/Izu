//! Explicit installed OpenSSH boundary. The repository never supplies command
//! names or executable policy; only a validated path reaches the remote shell.
use crate::tool::GitTool;
use crate::{Error, GitSource, Result};
use izu_model::CancellationToken;
use izu_process::{BaseEnvironment, CommandSpec, OutputPolicy, Phase, RunOptions, Termination};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct SshTransport {
    pub url: String,
    pub executable: PathBuf,
    pub identity_file: PathBuf,
    pub known_hosts_file: PathBuf,
}
impl SshTransport {
    pub(crate) fn validate(&mut self) -> Result<()> {
        GitSource::ssh(&self.url)?;
        for path in [
            &mut self.executable,
            &mut self.identity_file,
            &mut self.known_hosts_file,
        ] {
            if !path.is_absolute() {
                return Err(Error::InvalidSource("SSH executable, identity and known-hosts paths must be explicit absolute paths".into()));
            }
            let metadata = std::fs::symlink_metadata(&*path).map_err(|source| Error::Io {
                action: "inspect explicit SSH tool or credential path",
                source,
            })?;
            if !metadata.is_file() || metadata.file_type().is_symlink() {
                return Err(Error::InvalidSource(
                    "SSH tool and credential paths must be nonsymbolic regular files".into(),
                ));
            }
            *path = std::fs::canonicalize(&*path).map_err(|source| Error::Io {
                action: "resolve explicit SSH path",
                source,
            })?;
            if path
                .as_os_str()
                .to_str()
                .is_none_or(|value| value.chars().any(char::is_control))
            {
                return Err(Error::InvalidSource(
                    "SSH paths must be UTF-8 without control characters".into(),
                ));
            }
        }
        for (path, maximum) in [
            (&self.identity_file, 64 * 1024_u64),
            (&self.known_hosts_file, 1024 * 1024_u64),
        ] {
            let metadata = std::fs::metadata(path).map_err(|source| Error::Io {
                action: "inspect SSH credentials bounds",
                source,
            })?;
            if metadata.len() > maximum {
                return Err(Error::Limit("SSH credential file"));
            }
        }
        Ok(())
    }
}

pub(crate) fn exchange(
    tool: &GitTool,
    url: &str,
    service: &str,
    input: &[u8],
    output_limit: usize,
    cancel: &CancellationToken,
) -> Result<Vec<u8>> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    GitSource::ssh(url)?;
    let transport = tool.config.ssh.as_ref().ok_or_else(|| {
        Error::Unsupported(vec![
            "SSH requires an explicit scoped installed OpenSSH transport".into(),
        ])
    })?;
    if transport.url != url {
        return Err(Error::InvalidSource(
            "SSH transport is scoped to a different explicit repository".into(),
        ));
    }
    if !matches!(service, "git-upload-pack" | "git-receive-pack") {
        return Err(Error::InvalidSource("invalid SSH Git service".into()));
    }
    let parsed =
        reqwest::Url::parse(url).map_err(|_| Error::InvalidSource("invalid SSH URL".into()))?;
    let host = parsed
        .host_str()
        .ok_or_else(|| Error::InvalidSource("missing SSH host".into()))?;
    let worker = tool.config.worker.as_ref().ok_or_else(|| {
        Error::Unsupported(vec!["SSH requires the verified process worker".into()])
    })?;
    let mut command = CommandSpec::new(transport.executable.as_os_str());
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(directory) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", directory);
    }
    command.env("LC_ALL", "C").args(["-F", "/dev/null", "-T"]);
    for option in [
        "BatchMode=yes",
        "StrictHostKeyChecking=yes",
        "GlobalKnownHostsFile=/dev/null",
        "IdentitiesOnly=yes",
        "IdentityAgent=none",
        "ClearAllForwardings=yes",
        "PermitLocalCommand=no",
        "ProxyCommand=none",
        "ProxyJump=none",
        "ControlMaster=no",
        "ControlPath=none",
        "UpdateHostKeys=no",
        "PasswordAuthentication=no",
        "KbdInteractiveAuthentication=no",
    ] {
        command.args(["-o", option]);
    }
    // No shell quoting is needed for local arguments; remote path syntax was
    // restricted before constructing this one remote shell command.
    let known_hosts = transport
        .known_hosts_file
        .to_str()
        .ok_or_else(|| Error::InvalidSource("non-UTF8 known-hosts path".into()))?;
    let known_hosts = known_hosts.replace('\\', "\\\\").replace('"', "\\\"");
    command
        .arg("-o")
        .arg(format!("UserKnownHostsFile=\"{known_hosts}\""))
        .arg("-i")
        .arg(&transport.identity_file)
        .arg("-p")
        .arg(parsed.port().unwrap_or(22).to_string())
        .arg("--")
        .arg(format!("{}@{host}", parsed.username()))
        .arg(format!("{service} '{}'", parsed.path()));
    let input_limit = tool
        .config
        .limits
        .max_pack_bytes
        .checked_add(64 * 1024)
        .ok_or(Error::Limit("SSH request"))?;
    let options = RunOptions {
        stdin_limit: input_limit,
        stdout_limit: output_limit,
        stderr_limit: tool.config.limits.max_stderr_bytes,
        timeout: tool.config.limits.command_timeout,
        cleanup_timeout: Duration::from_secs(2),
        output_policy: OutputPolicy::Terminate,
        base_environment: BaseEnvironment::Clear,
    };
    let result = izu_process::run(&command, input, &options, worker, &|| cancel.is_cancelled())
        .map_err(|error| match error {
            izu_process::RunError::Cancelled => Error::Cancelled,
            izu_process::RunError::InputLimit => Error::Limit("SSH command input"),
            error => Error::Process(error),
        })?;
    if !result.cleanup.cooperative_stop_succeeded() || !result.stdout.eof || !result.stderr.eof {
        return Err(Error::CleanupUncertain(
            "SSH process group cleanup or pipe completion failed".into(),
        ));
    }
    match result.termination {
        Termination::Cancelled => return Err(Error::Cancelled),
        Termination::TimedOut => return Err(Error::TimedOut),
        Termination::OutputLimit(Phase::Stdout) => return Err(Error::Limit("SSH stdout")),
        Termination::OutputLimit(_) => return Err(Error::Limit("SSH process output")),
        Termination::Io { .. } | Termination::Protocol => return Err(Error::WorkerFailed),
        Termination::Completed => {}
    }
    let status = result.exit.ok_or(Error::WorkerFailed)?;
    if !status.success() {
        return Err(Error::CommandFailed {
            operation: "explicit SSH Git service".into(),
            code: status.code(),
            stderr: String::from_utf8_lossy(&result.stderr.bytes)
                .trim()
                .to_owned(),
        });
    }
    Ok(result.stdout.bytes)
}
