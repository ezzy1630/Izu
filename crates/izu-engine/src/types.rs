use izu_model::*;
use izu_store::StoreOptions;
use serde::{Deserialize, Serialize};

/// Source walking limits are independent of store object framing limits.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SourceLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_blob_bytes: u64,
    pub max_total_bytes: u64,
    pub max_ignore_bytes: u64,
    pub max_history: usize,
    pub max_history_bytes: u64,
    pub max_merge_bytes: u64,
}

impl Default for SourceLimits {
    fn default() -> Self {
        Self {
            max_entries: 100_000,
            max_depth: 128,
            max_blob_bytes: 256 * 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_ignore_bytes: 1024 * 1024,
            max_history: 100_000,
            max_history_bytes: 32 * 1024 * 1024,
            max_merge_bytes: 16 * 1024 * 1024,
        }
    }
}

#[derive(Default)]
pub struct RepositoryOptions {
    pub store: StoreOptions,
    pub source_limits: SourceLimits,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum Selection {
    All,
    Paths(Vec<RepoPath>),
}
impl Selection {
    pub(crate) fn includes(&self, path: &RepoPath) -> bool {
        match self {
            Self::All => true,
            Self::Paths(paths) => paths.iter().any(|prefix| {
                path == prefix
                    || path
                        .as_str()
                        .strip_prefix(prefix.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
            }),
        }
    }
    pub(crate) fn may_descend(&self, directory: &RepoPath) -> bool {
        self.includes(directory)
            || match self {
                Self::All => true,
                Self::Paths(paths) => paths.iter().any(|path| {
                    path.as_str()
                        .strip_prefix(directory.as_str())
                        .is_some_and(|rest| rest.starts_with('/'))
                }),
            }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceExpectation {
    pub head: RevisionId,
    pub working_tree: TreeId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceState {
    pub id: WorkspaceId,
    pub record: WorkspaceRecord,
    pub expected: WorkspaceExpectation,
}
/// Retains the fork publication even if a later launch step fails or history advances.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkspaceForkReceipt {
    pub workspace: WorkspaceState,
    pub operation: OperationId,
}
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub struct CaptureStats {
    pub entries: u64,
    pub bytes: u64,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CapturedTree {
    pub workspace: WorkspaceId,
    pub expected: WorkspaceExpectation,
    pub tree: TreeId,
    pub stats: CaptureStats,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckpointReceipt {
    pub operation: OperationId,
    pub workspace: WorkspaceId,
    pub head: RevisionId,
    pub tree: TreeId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CommitReceipt {
    pub operation: OperationId,
    pub workspace: WorkspaceId,
    pub revision: RevisionId,
    pub change: ChangeId,
    pub tree: TreeId,
    pub working_tree: TreeId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RestoreReceipt {
    pub operation: OperationId,
    pub recovery_operation: OperationId,
    pub workspace: WorkspaceId,
    pub head: RevisionId,
    pub tree: TreeId,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RevertPreparation {
    Applied {
        receipt: RestoreReceipt,
        revision: RevisionId,
        tree: TreeId,
    },
    Conflicted {
        operation: OperationId,
        revision: RevisionId,
        tree: TreeId,
        conflicts: Vec<TreeConflict>,
    },
}
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PathChangeKind {
    Added,
    Modified,
    Deleted,
    TypeChanged,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PathChange {
    pub path: RepoPath,
    pub kind: PathChangeKind,
    pub before: Option<TreeEntry>,
    pub after: Option<TreeEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Status {
    pub workspace: WorkspaceId,
    pub head: RevisionId,
    pub checkpoint_tree: TreeId,
    pub captured_tree: TreeId,
    pub entries: Vec<PathChange>,
    pub stats: CaptureStats,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RevisionSummary {
    pub id: RevisionId,
    pub revision: Revision,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct OperationSummary {
    pub id: OperationId,
    pub operation: Operation,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefUpdate {
    pub name: RefName,
    pub expected: Option<RevisionId>,
    pub new: Option<RevisionId>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RefExpectation {
    pub name: RefName,
    pub expected: Option<RevisionId>,
}
pub type RefUpdateExpectation = RefExpectation;
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TreeConflict {
    pub path: RepoPath,
    pub reason: String,
    pub base: Option<TreeEntry>,
    pub ours: Option<TreeEntry>,
    pub theirs: Option<TreeEntry>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum MergePreparation {
    Ready {
        candidate: CandidateId,
        revision: RevisionId,
        tree: TreeId,
    },
    Conflicted {
        candidate: CandidateId,
        revision: RevisionId,
        tree: TreeId,
        base: Option<RevisionId>,
        ours: Option<RevisionId>,
        theirs: RevisionId,
        conflicts: Vec<TreeConflict>,
    },
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LandReceipt {
    pub operation: OperationId,
    pub candidate: CandidateId,
    pub target: RefName,
    pub revision: RevisionId,
    pub tree: TreeId,
    pub evidence: Vec<EvidenceId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckAttemptReceipt {
    pub operation: OperationId,
    pub attempt: CheckAttemptId,
    pub evidence: EvidenceId,
    pub pending: CheckEvidence,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct VerificationReport {
    pub operations: u64,
    pub revisions: u64,
    pub trees: u64,
    pub blobs: u64,
    pub candidates: u64,
    pub evidence: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ObjectInfo {
    pub id: ObjectId,
    pub kind: ObjectKind,
    pub payload_len: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RecoveryReport {
    pub head: Option<OperationId>,
    pub verified_objects: u64,
    pub verified_bytes: u64,
    pub removed_temporary_files: u64,
    pub unknown_temporary_entries: u64,
    pub retained_objects: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WriterToken(String);

impl WriterToken {
    pub fn new(value: impl Into<String>) -> crate::Result<Self> {
        let value = value.into();
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(crate::EngineError::InvalidInput(
                "writer token must be 32 lowercase hex characters".into(),
            ));
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for WriterToken {
    fn deserialize<D: serde::Deserializer<'de>>(
        deserializer: D,
    ) -> std::result::Result<Self, D::Error> {
        Self::new(String::deserialize(deserializer)?).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WriterIntent {
    pub workspace: WorkspaceId,
    pub token: WriterToken,
    pub created_at_unix_ms: i64,
}
