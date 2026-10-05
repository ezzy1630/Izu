#![forbid(unsafe_code)]
//! Original native source-versioning engine shared by all izu interfaces.
//!
//! Immutable source preparation is separated from guarded metadata publication.
//! A successful mutation receipt identifies the exact durably published operation.

mod error;
mod repository;
mod source;
mod types;

pub use error::{EngineError, Result};
pub use izu_model::{
    CancellationToken, CandidateId, ChangeId, CheckAttemptId, CheckEvidence, CheckInputs,
    CheckOutcome, CheckSpec, ConflictReason, EvidenceId, FileMode, Identity, IntegrationCandidate,
    ObjectId, OperationId, RefName, RepoPath, RepositoryView, ResolvedTreeEntry, Revision,
    RevisionId, SourceRecord, Tree, TreeEntry, TreeId, WorkspaceId, WorkspaceRecord,
};
pub use izu_store::StoreOptions;
pub use repository::{Repository, WorkspaceGuard, WorkspaceWriterLease};
pub use types::*;
