use izu_model::{OperationId, RefName, RepoPath, RevisionId, WorkspaceId};
use std::path::PathBuf;

pub type Result<T> = std::result::Result<T, EngineError>;

#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    #[error("source I/O failed at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Model(#[from] izu_model::ModelError),
    #[error(transparent)]
    Store(#[from] izu_store::StoreError),
    #[error("operation cancelled")]
    Cancelled,
    #[error("resource limit {resource} exceeded (limit {limit})")]
    Limit { resource: &'static str, limit: u64 },
    #[error("cannot allocate bounded {resource}")]
    Allocation { resource: &'static str },
    #[error("not an izu repository: {0}")]
    NotRepository(PathBuf),
    #[error("repository has no initial durable operation")]
    Uninitialized,
    #[error("repository already exists: {0}")]
    AlreadyExists(PathBuf),
    #[error("workspace {0} is not registered")]
    UnknownWorkspace(WorkspaceId),
    #[error("workspace name already exists: {0}")]
    WorkspaceNameExists(String),
    #[error("workspace {workspace} changed since the expected state")]
    StaleWorkspace { workspace: WorkspaceId },
    #[error("workspace {workspace} has an active cooperative {kind} lease")]
    WorkspaceBusy {
        workspace: WorkspaceId,
        kind: &'static str,
    },
    #[error("reference {name} changed: expected {expected:?}, observed {actual:?}")]
    StaleRef {
        name: RefName,
        expected: Option<RevisionId>,
        actual: Option<RevisionId>,
    },
    #[error("source changed while being read: {0}")]
    SourceChanged(PathBuf),
    #[error(
        "workspace {workspace} has uncommitted source at {path}; preserve, commit, or reconcile these changes before replacing source"
    )]
    UncommittedSource {
        workspace: WorkspaceId,
        path: RepoPath,
    },
    #[error("unsupported source entry: {0}")]
    UnsupportedEntry(PathBuf),
    #[error("protected internal source path: {0}")]
    ProtectedPath(String),
    #[error("source path cannot be represented losslessly: {0:?}")]
    IncompatiblePath(PathBuf),
    #[error("source tree has incompatible or colliding paths: {0}")]
    PathCollision(String),
    #[error("source directory must be empty: {0}")]
    DirectoryNotEmpty(PathBuf),
    #[error("restoration would overwrite unselected source: {0}")]
    UnselectedCollision(String),
    #[error("invalid workspace marker: {0}")]
    InvalidMarker(String),
    #[error("invalid engine input: {0}")]
    InvalidInput(String),
    #[error("revision cannot be resolved: {0}")]
    UnknownRevision(String),
    #[error("change has divergent revision heads: {0}")]
    DivergentChange(String),
    #[error("integration candidate is not registered: {0}")]
    UnknownCandidate(String),
    #[error("candidate has no declared required checks")]
    NoRequiredChecks,
    #[error("required check has no passing evidence for these exact inputs: {0}")]
    MissingCheck(String),
    #[error("check evidence does not match the candidate: {0}")]
    EvidenceMismatch(String),
    #[error("check attempt changed: current {current}, received {received}")]
    StaleCheckAttempt {
        current: izu_model::CheckAttemptId,
        received: izu_model::CheckAttemptId,
    },
    #[error("merge failed: {0}")]
    Merge(String),
    #[error("operation {operation} became visible but acknowledgement is uncertain: {reason}")]
    PublicationUncertain {
        operation: OperationId,
        reason: String,
    },
    #[error("source restoration failed after recovery operation {recovery}: {reason}")]
    RestorationFailed {
        recovery: OperationId,
        reason: String,
    },
    #[error(
        "source restoration changed files but metadata acknowledgement is uncertain (visible operation {operation:?}); recovery operation {recovery}: {reason}"
    )]
    RestorationUncertain {
        recovery: OperationId,
        operation: Option<OperationId>,
        reason: String,
    },
    #[error("unsupported platform capability: {0}")]
    UnsupportedPlatform(&'static str),
}

impl EngineError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "source_io",
            Self::Model(_) => "invalid_model",
            Self::Store(_) => "store",
            Self::Cancelled => "cancelled",
            Self::Limit { .. } => "resource_limit",
            Self::Allocation { .. } => "allocation",
            Self::NotRepository(_) => "not_repository",
            Self::Uninitialized => "uninitialized",
            Self::AlreadyExists(_) => "already_exists",
            Self::UnknownWorkspace(_) => "unknown_workspace",
            Self::WorkspaceNameExists(_) => "workspace_name_exists",
            Self::StaleWorkspace { .. } => "stale_workspace",
            Self::StaleRef { .. } => "stale_ref",
            Self::WorkspaceBusy { .. } => "workspace_busy",
            Self::SourceChanged(_) => "source_changed",
            Self::UncommittedSource { .. } => "uncommitted_source",
            Self::UnsupportedEntry(_) => "unsupported_entry",
            Self::ProtectedPath(_) => "protected_path",
            Self::IncompatiblePath(_) => "incompatible_path",
            Self::PathCollision(_) => "path_collision",
            Self::DirectoryNotEmpty(_) => "directory_not_empty",
            Self::UnselectedCollision(_) => "unselected_collision",
            Self::InvalidMarker(_) => "invalid_marker",
            Self::InvalidInput(_) => "invalid_input",
            Self::UnknownRevision(_) => "unknown_revision",
            Self::DivergentChange(_) => "divergent_change",
            Self::UnknownCandidate(_) => "unknown_candidate",
            Self::NoRequiredChecks => "no_required_checks",
            Self::MissingCheck(_) => "missing_check",
            Self::EvidenceMismatch(_) => "evidence_mismatch",
            Self::StaleCheckAttempt { .. } => "stale_check_attempt",
            Self::Merge(_) => "merge",
            Self::PublicationUncertain { .. } => "publication_uncertain",
            Self::RestorationFailed { .. } => "restoration_failed",
            Self::RestorationUncertain { .. } => "restoration_uncertain",
            Self::UnsupportedPlatform(_) => "unsupported_platform",
        }
    }

    pub fn uncertain_operation(&self) -> Option<OperationId> {
        match self {
            Self::PublicationUncertain { operation, .. } => Some(*operation),
            Self::RestorationUncertain { operation, .. } => *operation,
            _ => None,
        }
    }

    pub fn recovery_operation(&self) -> Option<OperationId> {
        match self {
            Self::RestorationFailed { recovery, .. }
            | Self::RestorationUncertain { recovery, .. } => Some(*recovery),
            _ => None,
        }
    }
}

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> EngineError {
    EngineError::Io {
        path: path.into(),
        source,
    }
}
