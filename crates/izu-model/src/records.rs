use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::error::{check_len, check_u64};
use crate::ids::validate_git_id;
use crate::names::validate_reference_name;
use crate::{
    CandidateId, ChangeId, CheckAttemptId, EvidenceId, Limits, ModelError, ObjectId, OperationId,
    RefName, RepoPath, RevisionId, SymlinkTarget, TreeId, Validate, WorkspaceId,
};

macro_rules! record {
    ($(#[$meta:meta])* $name:ident, $raw:ident, $raw_name:literal, {$($field:ident: $ty:ty),* $(,)?}) => {
        #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
        #[serde(deny_unknown_fields, try_from = $raw_name)]
        $(#[$meta])*
        pub struct $name { $(pub $field: $ty,)* }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct $raw { $($field: $ty,)* }
        impl TryFrom<$raw> for $name {
            type Error = ModelError;
            fn try_from(raw: $raw) -> Result<Self, Self::Error> {
                let result = Self { $($field: raw.$field,)* };
                result.validate(&Limits::decoding_ceiling())?;
                Ok(result)
            }
        }
    };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    deny_unknown_fields,
    try_from = "RawFileMode"
)]
pub enum FileMode {
    Regular,
    Executable,
    Unix { permissions: u16 },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum RawFileMode {
    Regular,
    Executable,
    Unix { permissions: u16 },
}

impl TryFrom<RawFileMode> for FileMode {
    type Error = ModelError;
    fn try_from(raw: RawFileMode) -> Result<Self, Self::Error> {
        let mode = match raw {
            RawFileMode::Regular => Self::Regular,
            RawFileMode::Executable => Self::Executable,
            RawFileMode::Unix { permissions } => Self::Unix { permissions },
        };
        mode.validate(&Limits::decoding_ceiling())?;
        Ok(mode)
    }
}

impl FileMode {
    pub fn from_unix_permissions(permissions: u32) -> Result<Self, ModelError> {
        let mode = match permissions {
            0o644 => Self::Regular,
            0o755 => Self::Executable,
            0..=0o7777 => Self::Unix {
                permissions: u16::try_from(permissions).map_err(|_| {
                    ModelError::InvalidMetadata {
                        reason: "invalid POSIX permissions",
                    }
                })?,
            },
            _ => {
                return Err(ModelError::InvalidMetadata {
                    reason: "invalid POSIX permissions",
                });
            }
        };
        Ok(mode)
    }
    pub fn unix_permissions(self) -> u16 {
        match self {
            Self::Regular => 0o644,
            Self::Executable => 0o755,
            Self::Unix { permissions } => permissions,
        }
    }
    pub fn is_executable(self) -> bool {
        self.unix_permissions() & 0o111 != 0
    }
}

impl Validate for FileMode {
    fn validate(&self, _: &Limits) -> Result<(), ModelError> {
        if let Self::Unix { permissions } = self
            && (*permissions > 0o7777 || *permissions == 0o644 || *permissions == 0o755)
        {
            return Err(ModelError::InvalidMetadata {
                reason: "invalid or noncanonical POSIX permissions",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum TreeEntry {
    File {
        blob: ObjectId,
        mode: FileMode,
    },
    Symlink {
        target: SymlinkTarget,
    },
    Directory {
        mode: FileMode,
    },
    Conflict {
        base: Option<ResolvedTreeEntry>,
        ours: Option<ResolvedTreeEntry>,
        theirs: Option<ResolvedTreeEntry>,
        reason: ConflictReason,
    },
}

/// Resolved alternatives are nonrecursive, so a conflict cannot hide nested
/// conflicts or trigger an unbounded recursive representation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResolvedTreeEntry {
    File { blob: ObjectId, mode: FileMode },
    Symlink { target: SymlinkTarget },
    Directory { mode: FileMode },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictReason {
    Content,
    Binary,
    DeleteModify,
    AddAdd,
    Mode,
    Type,
    Path,
}

impl Validate for ResolvedTreeEntry {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        match self {
            Self::File { mode, .. } | Self::Directory { mode } => mode.validate(limits),
            Self::Symlink { .. } => Ok(()),
        }
    }
}

impl Validate for TreeEntry {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        match self {
            Self::File { mode, .. } | Self::Directory { mode } => mode.validate(limits),
            Self::Symlink { .. } => Ok(()),
            Self::Conflict {
                base, ours, theirs, ..
            } => {
                if base.is_none() && ours.is_none() && theirs.is_none() {
                    return Err(ModelError::InvalidMetadata {
                        reason: "conflict has no alternatives",
                    });
                }
                for alternative in [base, ours, theirs].into_iter().flatten() {
                    alternative.validate(limits)?;
                }
                Ok(())
            }
        }
    }
}

record!(#[derive(Default)] Tree, RawTree, "RawTree", { entries: BTreeMap<RepoPath, TreeEntry> });

impl Validate for Tree {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        limits.validate()?;
        check_len("tree entries", self.entries.len(), limits.max_tree_entries)?;
        let mut namespace = BTreeSet::new();
        let mut path_bytes = 0u64;
        for (path, entry) in &self.entries {
            path.validate(limits)?;
            entry.validate(limits)?;
            path_bytes = path_bytes
                .checked_add(u64::try_from(path.as_str().len()).map_err(|_| {
                    ModelError::InvalidMetadata {
                        reason: "path byte count cannot be represented",
                    }
                })?)
                .ok_or(ModelError::InvalidMetadata {
                    reason: "tree path byte count overflow",
                })?;
            check_u64("tree path bytes", path_bytes, limits.max_metadata_bytes)?;
            if !namespace.insert(path.namespace_key()?) {
                return Err(ModelError::InvalidMetadata {
                    reason: "tree contains aliased namespace paths",
                });
            }
            // A directory entry may have descendants. Files and links cannot.
            for (index, byte) in path.as_str().bytes().enumerate() {
                if byte == b'/' {
                    let ancestor = &path.as_str()[..index];
                    let found = self
                        .entries
                        .get(ancestor)
                        .ok_or(ModelError::InvalidMetadata {
                            reason: "nested tree path has no explicit ancestor entry",
                        })?;
                    if !matches!(
                        found,
                        TreeEntry::Directory { .. } | TreeEntry::Conflict { .. }
                    ) {
                        return Err(ModelError::InvalidMetadata {
                            reason: "file or symlink has a descendant entry",
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

record!(Identity, RawIdentity, "RawIdentity", { name: String, email: String });

impl Validate for Identity {
    fn validate(&self, _: &Limits) -> Result<(), ModelError> {
        for value in [&self.name, &self.email] {
            check_len("identity field bytes", value.len(), 1024)?;
            if value.is_empty() || value.chars().any(char::is_control) {
                return Err(ModelError::InvalidMetadata {
                    reason: "identity is empty or contains control characters",
                });
            }
        }
        Ok(())
    }
}

/// Original transport data remains a separate immutable blob. Its presence
/// does not mean a subsequently rewritten native revision is unchanged Git data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RevisionOrigin {
    Bootstrap,
    Git {
        object_id: String,
        raw_commit: ObjectId,
    },
}

impl Validate for RevisionOrigin {
    fn validate(&self, _: &Limits) -> Result<(), ModelError> {
        match self {
            Self::Bootstrap => Ok(()),
            Self::Git { object_id, .. } => validate_git_id(object_id),
        }
    }
}

record!(Revision, RawRevision, "RawRevision", {
    change: ChangeId,
    tree: TreeId,
    parents: Vec<RevisionId>,
    description: String,
    author: Identity,
    created_at_unix_ms: i64,
    origin: Option<RevisionOrigin>,
});

impl Validate for Revision {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        check_len(
            "revision parents",
            self.parents.len(),
            limits.max_revision_parents,
        )?;
        check_unique(&self.parents, "revision has duplicate parents")?;
        if matches!(self.origin, Some(RevisionOrigin::Bootstrap)) && !self.parents.is_empty() {
            return Err(ModelError::InvalidMetadata {
                reason: "bootstrap revision has parents",
            });
        }
        text(&self.description, limits, false)?;
        self.author.validate(limits)?;
        if let Some(origin) = &self.origin {
            origin.validate(limits)?;
        }
        Ok(())
    }
}

// One logical change can have several exact heads. No winner is inferred from
// hash ordering: callers must explicitly resolve a divergent change.
record!(ChangeState, RawChangeState, "RawChangeState", { heads: BTreeSet<RevisionId> });

impl ChangeState {
    pub fn resolved(revision: RevisionId) -> Self {
        Self {
            heads: BTreeSet::from([revision]),
        }
    }
    pub fn is_divergent(&self) -> bool {
        self.heads.len() > 1
    }
    pub fn single_head(&self) -> Option<RevisionId> {
        if self.heads.len() == 1 {
            self.heads.first().copied()
        } else {
            None
        }
    }
}

impl Validate for ChangeState {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        if self.heads.is_empty() {
            return Err(ModelError::InvalidMetadata {
                reason: "logical change has no heads",
            });
        }
        check_len(
            "heads of a change",
            self.heads.len(),
            limits.max_revisions_per_change,
        )
    }
}

record!(SourceRecord, RawSourceRecord, "RawSourceRecord", {
    path: Option<RepoPath>,
    tree: TreeId,
    revision: Option<RevisionId>,
});

impl Validate for SourceRecord {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        if let Some(path) = &self.path {
            path.validate(limits)?;
        }
        Ok(())
    }
}

record!(WorkspaceRecord, RawWorkspaceRecord, "RawWorkspaceRecord", {
    name: String,
    root: String,
    head: RevisionId,
    sources: BTreeMap<String, SourceRecord>,
});

impl Validate for WorkspaceRecord {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        validate_reference_name(&self.name)?;
        check_len(
            "workspace root bytes",
            self.root.len(),
            limits.max_path_bytes,
        )?;
        if !self.root.starts_with('/') || self.root.contains('\0') {
            return Err(ModelError::InvalidMetadata {
                reason: "workspace root must be an absolute UTF-8 POSIX path without NUL",
            });
        }
        check_len(
            "workspace sources",
            self.sources.len(),
            limits.max_tree_entries,
        )?;
        for (name, source) in &self.sources {
            validate_reference_name(name)?;
            source.validate(limits)?;
        }
        Ok(())
    }
}

record!(#[derive(Default)] RepositoryView, RawRepositoryView, "RawRepositoryView", {
    changes: BTreeMap<ChangeId, ChangeState>,
    refs: BTreeMap<RefName, RevisionId>,
    workspaces: BTreeMap<WorkspaceId, WorkspaceRecord>,
    candidates: BTreeSet<CandidateId>,
    evidence: BTreeMap<CandidateId, BTreeSet<EvidenceId>>,
});

impl Validate for RepositoryView {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        check_len("logical changes", self.changes.len(), limits.max_changes)?;
        check_len("references", self.refs.len(), limits.max_refs)?;
        check_len("workspaces", self.workspaces.len(), limits.max_workspaces)?;
        check_len("candidates", self.candidates.len(), limits.max_candidates)?;
        check_len(
            "candidate evidence entries",
            self.evidence.len(),
            limits.max_candidates,
        )?;
        for change in self.changes.values() {
            change.validate(limits)?;
        }
        for workspace in self.workspaces.values() {
            workspace.validate(limits)?;
        }
        let mut total = 0usize;
        for (candidate, evidence) in &self.evidence {
            if !self.candidates.contains(candidate) {
                return Err(ModelError::InvalidMetadata {
                    reason: "evidence references a candidate outside the view",
                });
            }
            if evidence.is_empty() {
                return Err(ModelError::InvalidMetadata {
                    reason: "empty evidence set must be omitted",
                });
            }
            total = total
                .checked_add(evidence.len())
                .ok_or(ModelError::InvalidMetadata {
                    reason: "evidence count overflow",
                })?;
        }
        check_len("evidence records", total, limits.max_evidence)
    }
}

record!(Operation, RawOperation, "RawOperation", {
    parent: Option<OperationId>,
    view: RepositoryView,
    description: String,
    created_at_unix_ms: i64,
});

impl Validate for Operation {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        self.view.validate(limits)?;
        text(&self.description, limits, false)
    }
}

record!(CheckSpec, RawCheckSpec, "RawCheckSpec", {
    name: String,
    argv: Vec<String>,
    environment: Option<ObjectId>,
});

impl Validate for CheckSpec {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        validate_reference_name(&self.name)?;
        argv(&self.argv, limits)
    }
}

record!(IntegrationCandidate, RawIntegrationCandidate, "RawIntegrationCandidate", {
    target: RefName,
    expected_target: Option<RevisionId>,
    sources: Vec<RevisionId>,
    result: RevisionId,
    result_tree: TreeId,
    checks: Vec<CheckSpec>,
    created_at_unix_ms: i64,
});

impl Validate for IntegrationCandidate {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        if self.sources.is_empty() {
            return Err(ModelError::InvalidMetadata {
                reason: "candidate has no source revisions",
            });
        }
        check_len(
            "candidate sources",
            self.sources.len(),
            limits.max_revision_parents,
        )?;
        check_unique(&self.sources, "candidate has duplicate source revisions")?;
        check_len("candidate checks", self.checks.len(), 1024)?;
        let mut names = BTreeSet::new();
        for check in &self.checks {
            check.validate(limits)?;
            if !names.insert(&check.name) {
                return Err(ModelError::InvalidMetadata {
                    reason: "candidate has duplicate check names",
                });
            }
        }
        Ok(())
    }
}

record!(CheckInputs, RawCheckInputs, "RawCheckInputs", {
    revision: RevisionId,
    tree: TreeId,
    environment: Option<ObjectId>,
});

impl Validate for CheckInputs {
    fn validate(&self, _: &Limits) -> Result<(), ModelError> {
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckOutcome {
    Pending,
    Passed,
    Failed { exit_code: Option<i32> },
    Cancelled,
}

record!(CheckEvidence, RawCheckEvidence, "RawCheckEvidence", {
    candidate: CandidateId,
    attempt: CheckAttemptId,
    check: String,
    inputs: CheckInputs,
    outcome: CheckOutcome,
    argv: Vec<String>,
    started_at_unix_ms: i64,
    finished_at_unix_ms: Option<i64>,
});

impl CheckEvidence {
    /// Compare immutable identities and requested execution, never just a check
    /// name or a mutable logical change. Unpinned environments remain unproven.
    pub fn validate_for_candidate(
        &self,
        id: CandidateId,
        candidate: &IntegrationCandidate,
    ) -> Result<(), ModelError> {
        self.validate_inputs_for_candidate(id, candidate)?;
        if self.outcome != CheckOutcome::Passed {
            return Err(ModelError::EvidenceMismatch {
                reason: "check did not pass",
            });
        }
        Ok(())
    }

    /// Check exact requested inputs for either a pending launch or a completed
    /// execution; this does not establish successful execution.
    pub fn validate_inputs_for_candidate(
        &self,
        id: CandidateId,
        candidate: &IntegrationCandidate,
    ) -> Result<(), ModelError> {
        self.validate(&Limits::decoding_ceiling())?;
        let mismatch = |reason| ModelError::EvidenceMismatch { reason };
        if self.candidate != id {
            return Err(mismatch("candidate ID differs"));
        }
        if self.inputs.revision != candidate.result || self.inputs.tree != candidate.result_tree {
            return Err(mismatch("exact revision or tree differs"));
        }
        let check = candidate
            .checks
            .iter()
            .find(|check| check.name == self.check)
            .ok_or_else(|| mismatch("check is not requested"))?;
        if self.argv != check.argv {
            return Err(mismatch("command differs"));
        }
        if let Some(environment) = check.environment
            && self.inputs.environment != Some(environment)
        {
            return Err(mismatch("required environment differs or is unknown"));
        }
        Ok(())
    }

    /// At the engine publication boundary, a terminal record must complete the
    /// exact currently pending attempt. History containing an earlier pass is
    /// not authority to complete a later attempt or restore that pass.
    pub fn validate_for_pending_attempt(&self, pending: &CheckEvidence) -> Result<(), ModelError> {
        let ceiling = Limits::decoding_ceiling();
        self.validate(&ceiling)?;
        pending.validate(&ceiling)?;
        let mismatch = |reason| ModelError::EvidenceMismatch { reason };
        if pending.outcome != CheckOutcome::Pending {
            return Err(mismatch("active check is not pending"));
        }
        if self.outcome == CheckOutcome::Pending {
            return Err(mismatch("attempt completion is not terminal"));
        }
        if self.attempt != pending.attempt {
            return Err(mismatch("active attempt token differs"));
        }
        if self.candidate != pending.candidate || self.check != pending.check {
            return Err(mismatch("attempt candidate or check differs"));
        }
        if self.inputs != pending.inputs || self.argv != pending.argv {
            return Err(mismatch("attempt inputs or command differ"));
        }
        if self.started_at_unix_ms != pending.started_at_unix_ms {
            return Err(mismatch("attempt start time differs"));
        }
        Ok(())
    }
}

impl Validate for CheckEvidence {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        validate_reference_name(&self.check)?;
        self.inputs.validate(limits)?;
        argv(&self.argv, limits)?;
        match (&self.outcome, self.finished_at_unix_ms) {
            (CheckOutcome::Pending, None) => {}
            (CheckOutcome::Pending, Some(_)) => {
                return Err(ModelError::InvalidMetadata {
                    reason: "pending check has a finish timestamp",
                });
            }
            (_, None) => {
                return Err(ModelError::InvalidMetadata {
                    reason: "terminal check has no finish timestamp",
                });
            }
            (_, Some(finished)) if finished < self.started_at_unix_ms => {
                return Err(ModelError::InvalidMetadata {
                    reason: "check finishes before it starts",
                });
            }
            (_, Some(_)) => {}
        }
        if matches!(self.outcome, CheckOutcome::Failed { exit_code: Some(0) }) {
            return Err(ModelError::InvalidMetadata {
                reason: "failed check cannot have a zero exit code",
            });
        }
        Ok(())
    }
}

fn text(value: &str, limits: &Limits, forbid_nul: bool) -> Result<(), ModelError> {
    check_len("text bytes", value.len(), limits.max_text_bytes)?;
    if forbid_nul && value.contains('\0') {
        return Err(ModelError::InvalidMetadata {
            reason: "text contains NUL",
        });
    }
    Ok(())
}

fn argv(values: &[String], limits: &Limits) -> Result<(), ModelError> {
    check_len("command arguments", values.len(), 1024)?;
    if values.first().is_none_or(String::is_empty) {
        return Err(ModelError::InvalidMetadata {
            reason: "command has no executable",
        });
    }
    let mut total = 0usize;
    for value in values {
        text(value, limits, true)?;
        total = total
            .checked_add(value.len())
            .ok_or(ModelError::InvalidMetadata {
                reason: "argument size overflow",
            })?;
    }
    check_len("total command argument bytes", total, limits.max_text_bytes)
}

fn check_unique<T: Ord>(items: &[T], reason: &'static str) -> Result<(), ModelError> {
    let values: BTreeSet<_> = items.iter().collect();
    if values.len() != items.len() {
        return Err(ModelError::InvalidMetadata { reason });
    }
    Ok(())
}
