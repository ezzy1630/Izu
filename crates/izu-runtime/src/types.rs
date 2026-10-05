use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use izu_model::{ObjectId, RevisionId, TreeId, WorkspaceId};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("invalid runtime input: {0}")]
    Invalid(String),
    #[error("runtime transaction or job is currently owned by another process: {0}")]
    Busy(PathBuf),
    #[error("registry locator was replaced or disappeared; pinned data remains preserved: {0}")]
    RegistryLocatorChanged(PathBuf),
    #[error("{operation} at {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("registry publication may be visible but is not durably acknowledged: {0}")]
    DurabilityUncertain(String),
    #[error("runtime capacity exceeded: {0}")]
    Capacity(String),
    #[error("unknown job {0}")]
    UnknownJob(JobId),
    #[error("workspace changed before execution: {0}")]
    StaleWorkspace(String),
    #[error("cannot close workspace: {0}")]
    UnsafeClose(String),
    #[error("engine: {0}")]
    Engine(#[from] izu_engine::EngineError),
    #[error("environment: {0}")]
    Environment(#[from] izu_environment::EnvironmentError),
    #[error("operation cancelled")]
    Cancelled,
    #[error("unsupported runtime operation: {0}")]
    Unsupported(String),
}
impl RuntimeError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Engine(error) => error.code(),
            Self::Environment(error) => match error {
                izu_environment::EnvironmentError::Workspace(error) => error.code(),
                izu_environment::EnvironmentError::Cancelled => "cancelled",
                izu_environment::EnvironmentError::DurabilityUncertain { .. } => {
                    "durability_uncertain"
                }
                izu_environment::EnvironmentError::Busy => "environment_busy",
                izu_environment::EnvironmentError::SourceMismatch => "environment_source_mismatch",
                _ => "environment_invalid",
            },
            Self::Invalid(_) => "invalid_input",
            Self::Busy(_) | Self::Capacity(_) => "runtime_busy",
            Self::Io { .. } => "io",
            Self::DurabilityUncertain(_) => "durability_uncertain",
            Self::RegistryLocatorChanged(_) => "registry_locator_changed",
            Self::UnknownJob(_) => "unknown_job",
            Self::StaleWorkspace(_) => "stale_workspace",
            Self::UnsafeClose(_) => "writer_ownership_unknown",
            Self::Cancelled => "cancelled",
            Self::Unsupported(_) => "unsupported",
        }
    }
    pub fn uncertain_operation(&self) -> Option<izu_model::OperationId> {
        match self {
            Self::Engine(error) => error.uncertain_operation(),
            Self::Environment(izu_environment::EnvironmentError::Workspace(error)) => {
                error.uncertain_operation()
            }
            _ => None,
        }
    }
}

pub(crate) fn io_error(
    operation: &'static str,
    path: &Path,
    source: std::io::Error,
) -> RuntimeError {
    RuntimeError::Io {
        operation,
        path: path.to_path_buf(),
        source,
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct JobId(String);

impl JobId {
    pub(crate) fn generate() -> Result<Self, RuntimeError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| RuntimeError::Invalid(error.to_string()))?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for JobId {
    type Err = RuntimeError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(RuntimeError::Invalid(
                "job ID must be 32 lowercase hexadecimal characters".into(),
            ));
        }
        Ok(Self(value.into()))
    }
}
impl TryFrom<String> for JobId {
    type Error = RuntimeError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}
impl From<JobId> for String {
    fn from(value: JobId) -> Self {
        value.0
    }
}
impl fmt::Display for JobId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// A binding may only be constructed from the engine's registered workspace.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceBinding {
    pub(crate) id: WorkspaceId,
    pub(crate) repository: PathBuf,
    pub(crate) cwd: PathBuf,
    pub(crate) head: RevisionId,
    pub(crate) tree: TreeId,
    pub(crate) directory: DirectoryIdentity,
}
impl WorkspaceBinding {
    pub fn id(&self) -> WorkspaceId {
        self.id
    }
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
    pub fn head(&self) -> RevisionId {
        self.head
    }
    pub fn tree(&self) -> TreeId {
        self.tree
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct DirectoryIdentity {
    pub device: u64,
    pub inode: u64,
}

/// Reservations are admission accounting, not OS memory/CPU/disk containment.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceRequest {
    pub cpu_slots: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub ports: Vec<u16>,
}
impl Default for ResourceRequest {
    fn default() -> Self {
        Self {
            cpu_slots: 1,
            memory_bytes: 256 * 1024 * 1024,
            disk_bytes: 0,
            ports: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub max_running_jobs: u32,
    pub cpu_slots: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub max_workspaces: u32,
    pub max_queued_jobs: u32,
    pub max_retained_jobs: u32,
}

/// A live source-operation grant, independent of running-job reservations.
/// Dropping it or exiting the owning process releases its descriptor-held lock.
/// It acknowledges neither source publication nor a durable job submission.
#[derive(Debug)]
#[must_use = "retain the permit through source setup or close, but not command execution"]
pub struct SourceOperationPermit {
    pub(crate) _lease: std::fs::File,
}

pub(crate) const MAX_SOURCE_OPERATIONS: u32 = 4;
impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            max_running_jobs: 4,
            cpu_slots: 8,
            memory_bytes: 8 * 1024 * 1024 * 1024,
            disk_bytes: 20 * 1024 * 1024 * 1024,
            max_workspaces: 32,
            max_queued_jobs: 128,
            max_retained_jobs: 4096,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EnvironmentPolicy {
    Inherit,
    Clear,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RunRequest {
    pub workspace: WorkspaceBinding,
    /// Exact executable and arguments. No shell is inserted by the runtime.
    pub argv: Vec<String>,
    pub environment: EnvironmentPolicy,
    pub env_overlay: BTreeMap<String, String>,
    /// Native bounded EnvironmentBinding blob, never a bare cache key.
    pub environment_binding: Option<ObjectId>,
    pub resources: ResourceRequest,
    pub timeout_ms: u64,
}

/// Durable/returned command metadata deliberately excludes overlay values.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobRequestSummary {
    pub workspace: WorkspaceBinding,
    pub argv: Vec<String>,
    pub environment: EnvironmentPolicy,
    pub environment_keys: Vec<String>,
    #[serde(default)]
    pub environment_binding: Option<ObjectId>,
    pub resources: ResourceRequest,
    pub timeout_ms: u64,
}
impl From<&RunRequest> for JobRequestSummary {
    fn from(request: &RunRequest) -> Self {
        Self {
            workspace: request.workspace.clone(),
            argv: request.argv.clone(),
            environment: request.environment,
            environment_keys: request.env_overlay.keys().cloned().collect(),
            environment_binding: request.environment_binding,
            resources: request.resources.clone(),
            timeout_ms: request.timeout_ms,
        }
    }
}
impl RunRequest {
    pub fn new(workspace: WorkspaceBinding, argv: Vec<String>) -> Self {
        Self {
            workspace,
            argv,
            environment: EnvironmentPolicy::Inherit,
            env_overlay: BTreeMap::new(),
            environment_binding: None,
            resources: ResourceRequest::default(),
            timeout_ms: 300_000,
        }
    }
    pub fn with_timeout(mut self, timeout: Duration) -> Result<Self, RuntimeError> {
        self.timeout_ms = u64::try_from(timeout.as_millis())
            .map_err(|_| RuntimeError::Invalid("timeout exceeds u64 milliseconds".into()))?;
        Ok(self)
    }
}

#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    /// Explicit trusted runtime worker executable; never discovered from a repo recipe.
    pub worker_executable: PathBuf,
    pub worker_prefix: Vec<String>,
    pub budget: ResourceBudget,
    pub output_bytes_per_stream: usize,
    pub max_registry_bytes: u64,
    pub max_command_bytes: usize,
    pub max_timeout_ms: u64,
}
impl RuntimeConfig {
    pub fn new(worker_executable: PathBuf) -> Self {
        Self {
            worker_executable,
            worker_prefix: Vec::new(),
            budget: ResourceBudget::default(),
            output_bytes_per_stream: 64 * 1024,
            max_registry_bytes: 64 * 1024 * 1024,
            max_command_bytes: 128 * 1024,
            max_timeout_ms: 24 * 60 * 60 * 1000,
        }
    }
    pub(crate) fn validate(&self) -> Result<(), RuntimeError> {
        let budget = &self.budget;
        if budget.max_running_jobs == 0
            || budget.cpu_slots == 0
            || budget.max_workspaces == 0
            || budget.max_queued_jobs == 0
            || budget.max_retained_jobs == 0
        {
            return Err(RuntimeError::Invalid(
                "job, CPU, queue, retention, and workspace capacities must be positive".into(),
            ));
        }
        if self.output_bytes_per_stream > 16 * 1024 * 1024
            || self.max_registry_bytes > 512 * 1024 * 1024
            || self.max_registry_bytes < 1024
            || self.max_command_bytes == 0
            || self.max_command_bytes > 16 * 1024 * 1024
            || self.max_timeout_ms == 0
            || self.max_timeout_ms > 24 * 60 * 60 * 1000
        {
            return Err(RuntimeError::Invalid(
                "runtime bounds are outside supported limits".into(),
            ));
        }
        validate_argv(&self.worker_prefix, self.max_command_bytes, false)?;
        Ok(())
    }
}

pub(crate) fn validate_argv(
    argv: &[String],
    max_bytes: usize,
    nonempty: bool,
) -> Result<usize, RuntimeError> {
    if (nonempty && (argv.is_empty() || argv[0].is_empty())) || argv.len() > 4096 {
        return Err(RuntimeError::Invalid(
            "argv must have a nonempty executable and at most 4096 arguments".into(),
        ));
    }
    let mut bytes = 0_usize;
    for value in argv {
        if value.contains('\0') {
            return Err(RuntimeError::Invalid("argv contains NUL".into()));
        }
        bytes = bytes
            .checked_add(value.len())
            .and_then(|size| size.checked_add(1))
            .ok_or_else(|| RuntimeError::Invalid("command size overflow".into()))?;
    }
    if bytes > max_bytes {
        return Err(RuntimeError::Invalid(
            "command exceeds configured byte limit".into(),
        ));
    }
    Ok(bytes)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum AdmissionBlock {
    RunningJobs,
    CpuSlots,
    MemoryReservation,
    DiskReservation,
    PortClaim { port: u16 },
    ExternalPortBusy { port: u16 },
    WorkspaceWriter,
    UnknownWriter,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CancellationReason {
    Requested,
    Deadline,
    Shutdown,
    RecoveredBeforeLaunch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CommandOutcome {
    Exit {
        code: Option<i32>,
        signal: Option<i32>,
    },
    SpawnFailed {
        message: String,
    },
}
impl CommandOutcome {
    pub fn passed(&self) -> bool {
        matches!(
            self,
            Self::Exit {
                code: Some(0),
                signal: None
            }
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum JobState {
    Queued {
        blocked: Option<AdmissionBlock>,
    },
    Starting {
        reserved_at_unix_ms: i64,
    },
    Running {
        worker_pid: u32,
        started_at_unix_ms: i64,
        /// None until execution release is observed by the owning controller.
        deadline_unix_ms: Option<i64>,
    },
    Finished {
        started_at_unix_ms: i64,
        finished_at_unix_ms: i64,
        outcome: CommandOutcome,
    },
    Cancelled {
        finished_at_unix_ms: i64,
        reason: CancellationReason,
    },
    /// No saved PID is ever signalled on reopen. A previous handle is not ownership.
    OwnershipUnknown {
        observed_at_unix_ms: i64,
        worker_pid: Option<u32>,
        reason: String,
    },
    /// An explicit operator assertion, kept distinct from observed process shutdown.
    RecoveredStopped {
        observed_at_unix_ms: i64,
        operator_note: String,
        engine_operation: Option<izu_model::OperationId>,
    },
}
impl JobState {
    pub fn is_terminal(&self) -> bool {
        !matches!(
            self,
            Self::Queued { .. } | Self::Starting { .. } | Self::Running { .. }
        )
    }
    pub(crate) fn reserves_resources(&self) -> bool {
        matches!(
            self,
            Self::Starting { .. } | Self::Running { .. } | Self::OwnershipUnknown { .. }
        )
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CapturedOutput {
    pub bytes: Vec<u8>,
    pub dropped_bytes: u64,
}
impl CapturedOutput {
    pub fn text_lossy(&self) -> String {
        String::from_utf8_lossy(&self.bytes).into_owned()
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobOutput {
    pub stdout: CapturedOutput,
    pub stderr: CapturedOutput,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobSnapshot {
    pub id: JobId,
    pub sequence: u64,
    pub request: JobRequestSummary,
    pub submitted_at_unix_ms: i64,
    pub state: JobState,
    pub output: JobOutput,
    #[serde(default)]
    pub writer_intent: Option<izu_engine::WriterToken>,
    #[serde(default)]
    pub starting_environment: Option<izu_environment::StartingEnvironmentReceipt>,
}

/// Bounded administration metadata without command arguments or log contents.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct JobSummary {
    pub id: JobId,
    pub sequence: u64,
    pub workspace: WorkspaceBinding,
    pub state: JobState,
    pub resources: ResourceRequest,
    pub writer_intent: Option<izu_engine::WriterToken>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct ResourceUsage {
    pub running_jobs: u32,
    pub cpu_slots: u32,
    pub memory_bytes: u64,
    pub disk_bytes: u64,
    pub ports: Vec<u16>,
    pub unknown_writers: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct RuntimeCapabilities {
    pub platform: String,
    pub process_groups: bool,
    pub running_job_limit: bool,
    pub bounded_output: bool,
    pub hard_cpu_limit: bool,
    pub hard_memory_limit: bool,
    pub hard_disk_limit: bool,
    pub race_free_port_handoff: bool,
    pub security_containment: bool,
}
impl RuntimeCapabilities {
    pub fn current() -> Self {
        Self {
            platform: std::env::consts::OS.into(),
            process_groups: cfg!(unix),
            running_job_limit: true,
            bounded_output: true,
            hard_cpu_limit: false,
            hard_memory_limit: false,
            hard_disk_limit: false,
            race_free_port_handoff: false,
            security_containment: false,
        }
    }
}

pub(crate) fn now_ms() -> Result<i64, RuntimeError> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| RuntimeError::Invalid(error.to_string()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| RuntimeError::Invalid("wall clock exceeds supported range".into()))
}
