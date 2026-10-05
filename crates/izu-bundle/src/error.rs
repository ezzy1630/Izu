use izu_model::{ObjectId, ObjectKind};
use std::io;

pub type Result<T> = std::result::Result<T, BundleError>;

#[derive(Debug, thiserror::Error)]
pub enum BundleError {
    #[error("{action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Model(#[from] izu_model::ModelError),
    #[error(transparent)]
    Store(#[from] izu_store::StoreError),
    #[error(transparent)]
    Engine(#[from] izu_engine::EngineError),
    #[error("unsupported bundle version or flags")]
    UnsupportedVersion,
    #[error("invalid bundle: {0}")]
    Invalid(&'static str),
    #[error("truncated bundle")]
    Truncated,
    #[error("bundle integrity trailer does not match")]
    TrailerMismatch,
    #[error("duplicate bundle object {0}")]
    DuplicateObject(ObjectId),
    #[error("missing object {id}, referenced by {referenced_by}")]
    MissingObject {
        id: ObjectId,
        referenced_by: ObjectId,
    },
    #[error("object {id} has kind {actual:?}, expected {expected:?}")]
    UnexpectedKind {
        id: ObjectId,
        expected: ObjectKind,
        actual: ObjectKind,
    },
    #[error("object {0} is outside the declared root closure")]
    UnreachableObject(ObjectId),
    #[error("resource limit {0} exceeded")]
    Limit(&'static str),
    #[error("cannot allocate bounded {0}")]
    Allocation(&'static str),
    #[error("destination already exists")]
    DestinationExists,
    #[error("unsupported destination path: {0}")]
    InvalidDestination(&'static str),
    #[error("archive restoration needs an explicit workspace selection")]
    WorkspaceSelectionRequired,
    #[error("selected workspace is absent from the archived operation")]
    UnknownWorkspace,
    #[error("owned staging identity changed; foreign entry preserved")]
    StagingChanged,
    #[error("destination {destination} became visible but its identity is uncertain: {reason}")]
    PublicationUncertain {
        destination: std::path::PathBuf,
        reason: String,
    },
    #[error(
        "restore failed: {failure}; nonempty owned stage retained for recovery at {stage}; recovery operation {recovery_operation:?}"
    )]
    StagingRetained {
        stage: std::path::PathBuf,
        recovery_operation: Option<izu_model::OperationId>,
        failure: Box<BundleError>,
    },
    #[error(
        "bundle operation failed: {failure}; owned staging cleanup failed at {stage}: {cleanup}"
    )]
    StagingCleanup {
        stage: std::path::PathBuf,
        failure: Box<BundleError>,
        cleanup: String,
    },
}

impl BundleError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Io { .. } => "bundle_io",
            Self::Model(izu_model::ModelError::Cancelled) => "cancelled",
            Self::Model(izu_model::ModelError::Allocation { .. }) => "allocation",
            Self::Model(izu_model::ModelError::LimitExceeded { .. }) => "resource_limit",
            Self::Model(_) => "invalid_native_object",
            Self::Store(_) => "store",
            Self::Engine(error) => error.code(),
            Self::UnsupportedVersion => "unsupported_bundle_version",
            Self::Invalid(_) => "invalid_bundle",
            Self::Truncated => "truncated_bundle",
            Self::TrailerMismatch => "bundle_integrity",
            Self::DuplicateObject(_) => "duplicate_object",
            Self::MissingObject { .. } => "missing_object",
            Self::UnexpectedKind { .. } => "object_kind",
            Self::UnreachableObject(_) => "unreachable_object",
            Self::Limit(_) => "resource_limit",
            Self::Allocation(_) => "allocation",
            Self::DestinationExists => "destination_exists",
            Self::InvalidDestination(_) => "invalid_destination",
            Self::WorkspaceSelectionRequired => "workspace_selection_required",
            Self::UnknownWorkspace => "unknown_workspace",
            Self::StagingChanged => "staging_changed",
            Self::PublicationUncertain { .. } => "publication_uncertain",
            Self::StagingRetained { .. } => "staging_retained",
            Self::StagingCleanup { .. } => "staging_cleanup",
        }
    }
}

pub(crate) fn io(action: &'static str, source: io::Error) -> BundleError {
    BundleError::Io { action, source }
}
