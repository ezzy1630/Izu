#![forbid(unsafe_code)]
//! Original izu domain types and version-one native encoding.
//!
//! Identity newtypes and filesystem paths validate when decoded. Metadata
//! crosses the persistence boundary through [`encode_metadata`] and
//! [`decode_metadata`], which enforce canonical bytes and configured limits.

mod error;
mod format;
mod ids;
mod limits;
mod names;
mod records;
mod references;

pub use error::ModelError;
pub use format::{
    FRAME_HEADER_LEN, FRAME_MAGIC, ObjectHasher, ObjectKind, decode_metadata, encode_metadata,
    frame_header, hash_object, parse_frame_header,
};
pub use ids::{
    CandidateId, ChangeId, CheckAttemptId, EvidenceId, ObjectId, OperationId, RevisionId, TreeId,
    WorkspaceId,
};
pub use limits::{CancellationToken, Limits, Validate};
pub use names::{RefName, RepoPath, SymlinkTarget};
pub use records::{
    ChangeState, CheckEvidence, CheckInputs, CheckOutcome, CheckSpec, ConflictReason, FileMode,
    Identity, IntegrationCandidate, Operation, RepositoryView, ResolvedTreeEntry, Revision,
    RevisionOrigin, SourceRecord, Tree, TreeEntry, WorkspaceRecord,
};
pub use references::{MetadataObject, ObjectReference, ReferencedObjects, decode_object_metadata};
