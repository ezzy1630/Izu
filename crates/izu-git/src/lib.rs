#![forbid(unsafe_code)]
//! Optional, original Git interoperability over an explicit installed Git tool.
//! Native history remains owned by izu-engine. No Git implementation is linked.

mod objects;
mod pack;
mod source;
mod tool;
mod transfer;
mod wire;

pub use izu_process::WorkerLauncher;
pub use tool::GitToolConfig;
pub use tool::HttpsAuthentication;
pub use transfer::{
    ExportOptions, ExportReport, GitAdapter, GitSignature, ImportOptions, ImportReport,
    NonFastForwardPolicy, PushReport, PushRequest, RefExport, RefImport,
};

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("Git executable is unavailable: {0}")]
    Unavailable(PathBuf),
    #[error("{action}: {source}")]
    Io {
        action: &'static str,
        source: std::io::Error,
    },
    #[error("Git operation was cancelled")]
    Cancelled,
    #[error("Git command exceeded its time limit")]
    TimedOut,
    #[error("Git {operation} failed (exit {code:?}): {stderr}")]
    CommandFailed {
        operation: String,
        code: Option<i32>,
        stderr: String,
    },
    #[error("Git process worker failed")]
    WorkerFailed,
    #[error("Git process could not be launched: {0}")]
    Process(#[source] izu_process::RunError),
    #[error("Git process cleanup is uncertain: {0}")]
    CleanupUncertain(String),
    #[error("bounded Git input exceeded limit: {0}")]
    Limit(&'static str),
    #[error("invalid Git source: {0}")]
    InvalidSource(String),
    #[error("unsupported Git capabilities: {0:?}")]
    Unsupported(Vec<String>),
    #[error("invalid Git object {object}: {reason}")]
    InvalidObject { object: String, reason: String },
    #[error("invalid Git reference: {0}")]
    InvalidRef(String),
    #[error("native {operation} failed: {source}")]
    Native {
        operation: &'static str,
        #[source]
        source: izu_engine::EngineError,
    },
    #[error("reference lease mismatch: expected {expected:?}, observed {observed:?}")]
    LeaseMismatch {
        expected: Option<GitObjectId>,
        observed: Option<GitObjectId>,
    },
    #[error("publication would replace divergent history")]
    NonFastForward,
    #[error("publication may have become visible and its outcome is uncertain: {0}")]
    PublicationUncertain(String),
    #[error("a new native commit requires an explicit Git committer signature")]
    MissingCommitter,
}

impl Error {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unavailable(_) => "git_unavailable",
            Self::Io { .. } => "git_io",
            Self::Cancelled => "cancelled",
            Self::TimedOut => "git_timeout",
            Self::CommandFailed { .. } => "git_command_failed",
            Self::WorkerFailed => "git_worker_failed",
            Self::Process(_) => "git_process",
            Self::CleanupUncertain(_) => "git_cleanup_uncertain",
            Self::Limit(_) => "resource_limit",
            Self::InvalidSource(_) => "invalid_git_source",
            Self::Unsupported(_) => "unsupported_git_capability",
            Self::InvalidObject { .. } => "invalid_git_object",
            Self::InvalidRef(_) => "invalid_git_ref",
            Self::Native { source, .. } => source.code(),
            Self::LeaseMismatch { .. } => "git_lease_mismatch",
            Self::NonFastForward => "git_non_fast_forward",
            Self::PublicationUncertain(_) => "publication_uncertain",
            Self::MissingCommitter => "git_committer_required",
        }
    }
    pub fn uncertain_operation(&self) -> Option<izu_model::OperationId> {
        match self {
            Self::Native { source, .. } => source.uncertain_operation(),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct GitLimits {
    /// Compressed pack bytes admitted before any indexing or cache disk write.
    pub max_pack_bytes: usize,
    pub max_delta_depth: usize,
    pub max_object_bytes: usize,
    pub max_total_object_bytes: usize,
    pub max_objects: usize,
    pub max_refs: usize,
    pub max_tree_entries: usize,
    pub max_tree_depth: usize,
    pub max_config_bytes: usize,
    pub max_stderr_bytes: usize,
    pub command_timeout: Duration,
}

impl Default for GitLimits {
    fn default() -> Self {
        Self {
            max_pack_bytes: 128 * 1024 * 1024,
            max_delta_depth: 64,
            max_object_bytes: 16 * 1024 * 1024,
            max_total_object_bytes: 128 * 1024 * 1024,
            max_objects: 50_000,
            max_refs: 10_000,
            max_tree_entries: 100_000,
            max_tree_depth: 128,
            max_config_bytes: 1024 * 1024,
            max_stderr_bytes: 64 * 1024,
            command_timeout: Duration::from_secs(30),
        }
    }
}
impl GitLimits {
    pub fn validate(&self) -> Result<()> {
        if self.max_pack_bytes < 32
            || self.max_pack_bytes > 512 * 1024 * 1024
            || self.max_delta_depth == 0
            || self.max_delta_depth > 256
            || self.max_object_bytes == 0
            || self.max_object_bytes > 64 * 1024 * 1024
            || self.max_total_object_bytes == 0
            || self.max_total_object_bytes > 1024 * 1024 * 1024
            || self.max_objects == 0
            || self.max_objects > 100_000
            || self.max_refs == 0
            || self.max_refs > 100_000
            || self.max_tree_entries == 0
            || self.max_tree_entries > 250_000
            || self.max_tree_depth == 0
            || self.max_tree_depth > 256
            || self.max_config_bytes == 0
            || self.max_config_bytes > 64 * 1024 * 1024
            || self.max_stderr_bytes == 0
            || self.max_stderr_bytes > 1024 * 1024
            || self.command_timeout.is_zero()
            || self.command_timeout > Duration::from_secs(3600)
        {
            return Err(Error::InvalidSource(
                "Git resource limits exceed supported bounds".into(),
            ));
        }
        Ok(())
    }
}

/// SHA-1 Git IDs are kept separate from native SHA-256 IDs.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GitObjectId([u8; 20]);

impl GitObjectId {
    pub fn as_bytes(&self) -> &[u8; 20] {
        &self.0
    }
    pub(crate) fn from_bytes(bytes: [u8; 20]) -> Self {
        Self(bytes)
    }
    pub(crate) fn zero_hex() -> &'static str {
        "0000000000000000000000000000000000000000"
    }
}

impl fmt::Display for GitObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl fmt::Debug for GitObjectId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, formatter)
    }
}
impl FromStr for GitObjectId {
    type Err = Error;
    fn from_str(value: &str) -> Result<Self> {
        if value.len() != 40
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(Error::InvalidObject {
                object: value.chars().take(80).collect(),
                reason: "expected 40 lowercase hexadecimal digits".into(),
            });
        }
        let mut bytes = [0_u8; 20];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            let digit = |byte: u8| {
                if byte <= b'9' {
                    byte - b'0'
                } else {
                    byte - b'a' + 10
                }
            };
            bytes[index] = digit(pair[0]) * 16 + digit(pair[1]);
        }
        Ok(Self(bytes))
    }
}
impl Serialize for GitObjectId {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}
impl<'de> Deserialize<'de> for GitObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GitSource {
    Local(PathBuf),
    Https(String),
}

impl GitSource {
    pub fn local(path: impl Into<PathBuf>) -> Self {
        Self::Local(path.into())
    }
    /// Explicit HTTPS URLs only. Userinfo, queries, fragments and control bytes
    /// are rejected, so URLs cannot act as credentials or command options.
    pub fn https(value: impl Into<String>) -> Result<Self> {
        let value = value.into();
        let rest = value
            .strip_prefix("https://")
            .ok_or_else(|| Error::InvalidSource("expected an explicit HTTPS URL".into()))?;
        let (host, path) = rest
            .split_once('/')
            .ok_or_else(|| Error::InvalidSource("HTTPS repository URL requires a path".into()))?;
        if host.is_empty()
            || path.is_empty()
            || !host
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-:[]".contains(&byte))
            || value
                .bytes()
                .any(|byte| byte <= 32 || byte == 127 || b"@?#\\".contains(&byte))
            || value.len() > 4096
        {
            return Err(Error::InvalidSource(
                "unsupported or credential-bearing HTTPS repository URL".into(),
            ));
        }
        let parsed = reqwest::Url::parse(&value)
            .map_err(|_| Error::InvalidSource("invalid explicit HTTPS repository URL".into()))?;
        if parsed.as_str() != value || parsed.host_str().is_none() {
            return Err(Error::InvalidSource(
                "HTTPS repository URL must be canonical and contain no normalized path segments"
                    .into(),
            ));
        }
        Ok(Self::Https(value))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct GitInventory {
    pub object_format: String,
    pub branches: BTreeMap<String, GitObjectId>,
    /// Every advertised ref outside an explicitly selected branch scope.
    /// These refs were not adopted, including tags and pull/request refs.
    pub omitted_refs: BTreeMap<String, GitObjectId>,
    pub symbolic_head: Option<String>,
    pub commits: usize,
    pub trees: usize,
    pub blobs: usize,
    pub unsupported: Vec<String>,
}

pub(crate) fn validate_git_ref(value: &str) -> Result<()> {
    if value.len() > 1024
        || !value.starts_with("refs/")
        || value.ends_with('.')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("@{")
        || value
            .bytes()
            .any(|byte| byte <= 32 || byte == 127 || b"~^:?*[\\".contains(&byte))
        || value
            .split('/')
            .any(|part| part.is_empty() || part.starts_with('.') || part.ends_with(".lock"))
    {
        return Err(Error::InvalidRef(value.chars().take(120).collect()));
    }
    Ok(())
}

pub(crate) fn native_error(
    operation: &'static str,
    error: impl Into<izu_engine::EngineError>,
) -> Error {
    Error::Native {
        operation,
        source: error.into(),
    }
}
