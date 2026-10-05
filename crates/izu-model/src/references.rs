use crate::{
    CheckEvidence, IntegrationCandidate, Limits, ModelError, ObjectId, ObjectKind, Operation,
    RepositoryView, ResolvedTreeEntry, Revision, RevisionOrigin, Tree, TreeEntry, decode_metadata,
};

/// Kind accompanies every object edge. A hash alone cannot prove that an
/// attacker-supplied object has the schema required by its reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct ObjectReference {
    pub id: ObjectId,
    pub kind: ObjectKind,
}

/// Allocation-free object graph visitation. Callers handle duplicate edges and
/// graph retention policy; this trait enumerates the authoritative schema.
pub trait ReferencedObjects {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference));
}

fn edge(visitor: &mut dyn FnMut(ObjectReference), kind: ObjectKind, id: ObjectId) {
    visitor(ObjectReference { id, kind });
}

impl ReferencedObjects for ResolvedTreeEntry {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        match self {
            Self::File { blob, .. } => edge(visitor, ObjectKind::Blob, *blob),
            Self::Symlink { .. } | Self::Directory { .. } => {}
        }
    }
}

impl ReferencedObjects for TreeEntry {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        match self {
            Self::File { blob, .. } => edge(visitor, ObjectKind::Blob, *blob),
            Self::Symlink { .. } | Self::Directory { .. } => {}
            Self::Conflict {
                base, ours, theirs, ..
            } => {
                for alternative in [base, ours, theirs].into_iter().flatten() {
                    alternative.visit_references(visitor);
                }
            }
        }
    }
}

impl ReferencedObjects for Tree {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        for entry in self.entries.values() {
            entry.visit_references(visitor);
        }
    }
}

impl ReferencedObjects for Revision {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        edge(visitor, ObjectKind::Tree, self.tree.object_id());
        for parent in &self.parents {
            edge(visitor, ObjectKind::Revision, parent.object_id());
        }
        if let Some(RevisionOrigin::Git { raw_commit, .. }) = &self.origin {
            edge(visitor, ObjectKind::Blob, *raw_commit);
        }
    }
}

impl ReferencedObjects for RepositoryView {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        for change in self.changes.values() {
            for head in &change.heads {
                edge(visitor, ObjectKind::Revision, head.object_id());
            }
        }
        for revision in self.refs.values() {
            edge(visitor, ObjectKind::Revision, revision.object_id());
        }
        for workspace in self.workspaces.values() {
            edge(visitor, ObjectKind::Revision, workspace.head.object_id());
            for source in workspace.sources.values() {
                edge(visitor, ObjectKind::Tree, source.tree.object_id());
                if let Some(revision) = source.revision {
                    edge(visitor, ObjectKind::Revision, revision.object_id());
                }
            }
        }
        for candidate in &self.candidates {
            edge(visitor, ObjectKind::Candidate, candidate.object_id());
        }
        for (candidate, evidence) in &self.evidence {
            edge(visitor, ObjectKind::Candidate, candidate.object_id());
            for record in evidence {
                edge(visitor, ObjectKind::Evidence, record.object_id());
            }
        }
    }
}

impl ReferencedObjects for Operation {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        if let Some(parent) = self.parent {
            edge(visitor, ObjectKind::Operation, parent.object_id());
        }
        self.view.visit_references(visitor);
    }
}

impl ReferencedObjects for IntegrationCandidate {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        if let Some(target) = self.expected_target {
            edge(visitor, ObjectKind::Revision, target.object_id());
        }
        for source in &self.sources {
            edge(visitor, ObjectKind::Revision, source.object_id());
        }
        edge(visitor, ObjectKind::Revision, self.result.object_id());
        edge(visitor, ObjectKind::Tree, self.result_tree.object_id());
        for check in &self.checks {
            if let Some(environment) = check.environment {
                edge(visitor, ObjectKind::Blob, environment);
            }
        }
    }
}

impl ReferencedObjects for CheckEvidence {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        edge(visitor, ObjectKind::Candidate, self.candidate.object_id());
        edge(
            visitor,
            ObjectKind::Revision,
            self.inputs.revision.object_id(),
        );
        edge(visitor, ObjectKind::Tree, self.inputs.tree.object_id());
        if let Some(environment) = self.inputs.environment {
            edge(visitor, ObjectKind::Blob, environment);
        }
    }
}

/// Dispatch used by closure verification and native bundle transport.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataObject {
    Tree(Tree),
    Revision(Revision),
    Operation(Operation),
    Candidate(IntegrationCandidate),
    Evidence(CheckEvidence),
}

impl ReferencedObjects for MetadataObject {
    fn visit_references(&self, visitor: &mut dyn FnMut(ObjectReference)) {
        match self {
            Self::Tree(value) => value.visit_references(visitor),
            Self::Revision(value) => value.visit_references(visitor),
            Self::Operation(value) => value.visit_references(visitor),
            Self::Candidate(value) => value.visit_references(visitor),
            Self::Evidence(value) => value.visit_references(visitor),
        }
    }
}

pub fn decode_object_metadata(
    kind: ObjectKind,
    payload: &[u8],
    limits: &Limits,
) -> Result<MetadataObject, ModelError> {
    match kind {
        ObjectKind::Blob => Err(ModelError::InvalidMetadata {
            reason: "blob bytes have no metadata schema",
        }),
        ObjectKind::Tree => decode_metadata(payload, limits).map(MetadataObject::Tree),
        ObjectKind::Revision => decode_metadata(payload, limits).map(MetadataObject::Revision),
        ObjectKind::Operation => decode_metadata(payload, limits).map(MetadataObject::Operation),
        ObjectKind::Candidate => decode_metadata(payload, limits).map(MetadataObject::Candidate),
        ObjectKind::Evidence => decode_metadata(payload, limits).map(MetadataObject::Evidence),
    }
}
