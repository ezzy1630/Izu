use crate::{Error, GitLimits, Result, WorkerLauncher};
use base64::Engine;
use izu_model::CancellationToken;
use izu_process::{BaseEnvironment, CommandSpec, OutputPolicy, Phase, RunOptions, Termination};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The source repository is never the execution directory. Every read uses an
/// owned bare cache and the executable is resolved before accepting its source.
#[derive(Clone, Debug)]
pub struct GitToolConfig {
    pub executable: PathBuf,
    pub limits: GitLimits,
    pub worker: Option<WorkerLauncher>,
    pub authentication: Option<HttpsAuthentication>,
    /// Explicit additional DER trust anchors. System/root helper credentials
    /// and TLS verification bypasses are never inferred from the environment.
    pub https_root_certificates: Vec<Vec<u8>>,
}
impl Default for GitToolConfig {
    fn default() -> Self {
        Self {
            executable: PathBuf::from("git"),
            limits: GitLimits::default(),
            worker: None,
            authentication: None,
            https_root_certificates: Vec::new(),
        }
    }
}

/// Explicit ephemeral credentials for one repository URL. No user helper,
/// credential file, keychain or persisted Git configuration is consulted.
#[derive(Clone)]
pub struct HttpsAuthentication {
    url: String,
    password: String,
    payload: String,
    header: String,
}
impl std::fmt::Debug for HttpsAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("HttpsAuthentication")
            .field("url", &self.url)
            .field("authorization", &"redacted")
            .finish()
    }
}
impl HttpsAuthentication {
    pub(crate) fn authorization(&self, url: &str) -> Result<String> {
        if self.url != url {
            return Err(Error::InvalidSource(
                "HTTPS credentials are scoped to a different explicit repository".into(),
            ));
        }
        Ok(format!("Basic {}", self.payload))
    }
    pub fn basic(url: &str, username: &str, password: String) -> Result<Self> {
        let source = crate::GitSource::https(url)?;
        let crate::GitSource::Https(url) = source else {
            return Err(Error::InvalidSource(
                "authentication requires an explicit HTTPS repository".into(),
            ));
        };
        if username.is_empty()
            || password.is_empty()
            || username.len() > 1024
            || password.len() > 8192
            || username.contains(':')
            || username.chars().any(char::is_control)
            || password.chars().any(char::is_control)
        {
            return Err(Error::InvalidSource(
                "invalid explicit HTTPS credentials".into(),
            ));
        }
        let payload =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let header = format!("Authorization: Basic {payload}");
        Ok(Self {
            url,
            password,
            payload,
            header,
        })
    }
    pub(crate) fn redact(&self, message: String) -> String {
        message
            .replace(&self.header, "[redacted authorization]")
            .replace(&self.payload, "[redacted credential]")
            .replace(&self.password, "[redacted credential]")
    }
}
pub(crate) struct GitTool {
    pub config: GitToolConfig,
}
pub(crate) struct CommandOutput {
    pub stdout: Vec<u8>,
    pub status: std::process::ExitStatus,
    pub stderr: String,
}

impl GitTool {
    pub fn new(mut config: GitToolConfig) -> Result<Self> {
        config.limits.validate()?;
        let executable =
            if config.executable.is_absolute() || config.executable.components().count() > 1 {
                std::fs::canonicalize(&config.executable)
                    .map_err(|_| Error::Unavailable(config.executable.clone()))?
            } else {
                let path = std::env::var_os("PATH")
                    .ok_or_else(|| Error::Unavailable(config.executable.clone()))?;
                std::env::split_paths(&path)
                    .filter(|part| part.is_absolute())
                    .map(|part| part.join(&config.executable))
                    .find_map(|candidate| {
                        let resolved = std::fs::canonicalize(candidate).ok()?;
                        let metadata = std::fs::metadata(&resolved).ok()?;
                        if !metadata.is_file() {
                            return None;
                        }
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::PermissionsExt;
                            if metadata.permissions().mode() & 0o111 == 0 {
                                return None;
                            }
                        }
                        Some(resolved)
                    })
                    .ok_or_else(|| Error::Unavailable(config.executable.clone()))?
            };
        if config.worker.is_none() {
            return Err(Error::Unsupported(vec![
                "Git requires an explicit verified process worker launcher".into(),
            ]));
        }
        config.executable = executable;
        Ok(Self { config })
    }
    pub fn run(
        &self,
        directory: Option<&Path>,
        operation: &str,
        arguments: &[OsString],
        input: &[u8],
        output_limit: usize,
        cancel: &CancellationToken,
    ) -> Result<CommandOutput> {
        self.run_inner(
            directory,
            operation,
            arguments,
            input,
            output_limit,
            cancel,
            false,
            self.config.limits.max_object_bytes,
        )
    }
    pub fn run_pack(
        &self,
        directory: &Path,
        operation: &str,
        arguments: &[OsString],
        input: &[u8],
        output_limit: usize,
        cancel: &CancellationToken,
    ) -> Result<CommandOutput> {
        self.run_inner(
            Some(directory),
            operation,
            arguments,
            input,
            output_limit,
            cancel,
            false,
            self.config.limits.max_pack_bytes,
        )
    }
    pub fn run_publication(
        &self,
        directory: &Path,
        arguments: &[OsString],
        cancel: &CancellationToken,
    ) -> Result<CommandOutput> {
        self.run_inner(
            Some(directory),
            "publish explicit leased remote ref",
            arguments,
            &[],
            64 * 1024,
            cancel,
            true,
            self.config.limits.max_object_bytes,
        )
    }
    #[allow(clippy::too_many_arguments)] // The explicit process boundary keeps policy separate from diagnostic text.
    fn run_inner(
        &self,
        directory: Option<&Path>,
        operation: &str,
        arguments: &[OsString],
        input: &[u8],
        output_limit: usize,
        cancel: &CancellationToken,
        preserve_server_policy: bool,
        input_limit: usize,
    ) -> Result<CommandOutput> {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let worker = self.config.worker.as_ref().ok_or_else(|| {
            Error::Unsupported(vec!["Git process worker launcher is unavailable".into()])
        })?;
        let mut command = CommandSpec::new(self.config.executable.as_os_str());
        // Only PATH is inherited, for installed Git transport helpers. User Git,
        // shell, askpass, credential and replace-object environments are absent.
        if let Some(path) = std::env::var_os("PATH") {
            command.env("PATH", path);
        }
        if let Some(directory) = std::env::var_os("TMPDIR") {
            command.env("TMPDIR", directory);
        }
        command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null_device())
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_NO_REPLACE_OBJECTS", "1")
            .env("LC_ALL", "C")
            .env("GIT_OPTIONAL_LOCKS", "0");
        command.arg("--no-replace-objects").args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.attributesFile=/dev/null",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "-c",
            "protocol.file.allow=always",
            "-c",
            "protocol.https.allow=always",
            "-c",
            "http.followRedirects=false",
            "-c",
            "core.fsync=all",
            "-c",
            "core.fsyncMethod=fsync",
        ]);
        if !preserve_server_policy {
            command.args(["-c", "core.hooksPath=/nonexistent-izu-hooks"]);
        }
        if let Some(url) = arguments
            .iter()
            .filter_map(|arg| arg.to_str())
            .find(|arg| arg.starts_with("https://"))
            && let Some(authentication) = &self.config.authentication
        {
            if authentication.url != url {
                return Err(Error::InvalidSource(
                    "HTTPS credentials are scoped to a different explicit repository".into(),
                ));
            }
            command
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", format!("http.{url}.extraheader"))
                .env("GIT_CONFIG_VALUE_0", &authentication.header);
        }
        if let Some(directory) = directory {
            command.arg("--git-dir").arg(directory);
        }
        command.args(arguments);
        let options = RunOptions {
            stdin_limit: input_limit,
            stdout_limit: output_limit,
            stderr_limit: self.config.limits.max_stderr_bytes,
            timeout: self.config.limits.command_timeout,
            cleanup_timeout: Duration::from_secs(2),
            output_policy: OutputPolicy::Terminate,
            base_environment: BaseEnvironment::Clear,
        };
        let result = izu_process::run(&command, input, &options, worker, &|| cancel.is_cancelled())
            .map_err(|error| match error {
                izu_process::RunError::Cancelled => Error::Cancelled,
                izu_process::RunError::InputLimit => Error::Limit("Git command input"),
                error => Error::Process(error),
            })?;
        if !result.cleanup.cooperative_stop_succeeded() || !result.stdout.eof || !result.stderr.eof
        {
            return Err(Error::CleanupUncertain(format!(
                "termination {:?}, group {:?}, leader_reaped={}, stdout_eof={}, stderr_eof={}",
                result.termination,
                result.cleanup.group_signal,
                result.cleanup.leader_reaped,
                result.stdout.eof,
                result.stderr.eof
            )));
        }
        match result.termination {
            Termination::Cancelled => return Err(Error::Cancelled),
            Termination::TimedOut => return Err(Error::TimedOut),
            Termination::OutputLimit(Phase::Stdout) => return Err(Error::Limit("Git stdout")),
            Termination::OutputLimit(Phase::Stderr) => return Err(Error::Limit("Git stderr")),
            Termination::OutputLimit(_) => return Err(Error::Limit("Git process output")),
            Termination::Io { .. } | Termination::Protocol => return Err(Error::WorkerFailed),
            Termination::Completed => {}
        }
        if result.stdout.dropped != 0 {
            return Err(Error::Limit("Git stdout"));
        }
        if result.stderr.dropped != 0 {
            return Err(Error::Limit("Git stderr"));
        }
        let status = result.exit.ok_or(Error::WorkerFailed)?;
        let message = String::from_utf8_lossy(&result.stderr.bytes)
            .trim()
            .to_owned();
        let stderr = self.config.authentication.as_ref().map_or_else(
            || message.clone(),
            |authentication| authentication.redact(message.clone()),
        );
        if !status.success() && !preserve_server_policy {
            return Err(Error::CommandFailed {
                operation: operation.to_owned(),
                code: status.code(),
                stderr,
            });
        }
        if result.input_written != input.len() {
            return Err(Error::InvalidSource(
                "Git did not consume its complete bounded input".into(),
            ));
        }
        Ok(CommandOutput {
            stdout: result.stdout.bytes,
            status,
            stderr,
        })
    }
    pub fn args(arguments: &[&str]) -> Vec<OsString> {
        arguments
            .iter()
            .map(OsStr::new)
            .map(OsStr::to_owned)
            .collect()
    }
}

#[cfg(windows)]
fn null_device() -> &'static str {
    "NUL"
}
#[cfg(not(windows))]
fn null_device() -> &'static str {
    "/dev/null"
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_scoped_validated_and_redacted() {
        let credentials = HttpsAuthentication::basic(
            "https://example.test/owned.git",
            "x-access-token",
            "fixture-secret".into(),
        )
        .expect("fixture credential");
        let debug = format!("{credentials:?}");
        assert!(!debug.contains("fixture-secret"));
        assert!(!debug.contains(&credentials.payload));
        assert!(
            !credentials
                .redact(format!(
                    "{} {} {}",
                    credentials.header, credentials.payload, credentials.password
                ))
                .contains("fixture-secret")
        );
        assert!(
            HttpsAuthentication::basic(
                "https://user:password@example.test/repo.git",
                "user",
                "password".into()
            )
            .is_err()
        );
        assert!(
            HttpsAuthentication::basic(
                "https://example.test/repo.git",
                "user",
                "unsafe\nheader".into()
            )
            .is_err()
        );
    }
}
