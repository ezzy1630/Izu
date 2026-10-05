use crate::{BundleError, BundleOptions, ObjectSource, Result};
use izu_model::{
    CancellationToken, ChangeId, MetadataObject, ObjectId, ObjectKind, ObjectReference, Operation,
    OperationId, ReferencedObjects, RevisionId, TreeId, decode_object_metadata, hash_object,
};
use std::collections::HashMap;
use std::io::{self, Write};

#[derive(Clone, Copy, Debug)]
pub(crate) enum Binding {
    RevisionChange {
        revision: RevisionId,
        change: ChangeId,
    },
    RevisionTree {
        revision: RevisionId,
        tree: TreeId,
    },
    EvidenceCandidate {
        evidence: ObjectId,
        candidate: ObjectId,
    },
}

pub(crate) struct Node {
    pub kind: ObjectKind,
    pub payload_len: u64,
    pub payload_offset: u64,
    pub references: Vec<ObjectReference>,
    pub bindings: Vec<Binding>,
    pub revision_tree: Option<TreeId>,
    pub revision_change: Option<ChangeId>,
    pub evidence_candidate: Option<ObjectId>,
}

impl Node {
    pub fn blob(payload_len: u64, payload_offset: u64) -> Self {
        Self {
            kind: ObjectKind::Blob,
            payload_len,
            payload_offset,
            references: Vec::new(),
            bindings: Vec::new(),
            revision_tree: None,
            revision_change: None,
            evidence_candidate: None,
        }
    }

    pub fn metadata(
        kind: ObjectKind,
        payload_len: u64,
        payload_offset: u64,
        object: &MetadataObject,
        options: &BundleOptions,
        remaining_graph_bytes: u64,
    ) -> Result<Self> {
        let mut reference_count = 0_usize;
        let mut error = None;
        object.visit_references(&mut |_| {
            if error.is_some() {
                return;
            }
            if u64::try_from(reference_count).map_or(true, |len| len >= options.max_references) {
                error = Some(BundleError::Limit("object references"));
            } else {
                match reference_count.checked_add(1) {
                    Some(count) => reference_count = count,
                    None => error = Some(BundleError::Limit("object references")),
                }
            }
        });
        if let Some(error) = error {
            return Err(error);
        }
        let binding_count = match object {
            MetadataObject::Operation(operation) => {
                let mut count = 0_usize;
                for change in operation.view.changes.values() {
                    count = count
                        .checked_add(change.heads.len())
                        .ok_or(BundleError::Limit("object bindings"))?;
                }
                for workspace in operation.view.workspaces.values() {
                    count = count
                        .checked_add(
                            workspace
                                .sources
                                .values()
                                .filter(|source| source.revision.is_some())
                                .count(),
                        )
                        .ok_or(BundleError::Limit("object bindings"))?;
                }
                for evidence in operation.view.evidence.values() {
                    count = count
                        .checked_add(evidence.len())
                        .ok_or(BundleError::Limit("object bindings"))?;
                }
                count
            }
            MetadataObject::Candidate(_) | MetadataObject::Evidence(_) => 1,
            MetadataObject::Tree(_) | MetadataObject::Revision(_) => 0,
        };
        if charged_node_bytes(reference_count, binding_count)? > remaining_graph_bytes {
            return Err(BundleError::Limit("graph bytes"));
        }
        let mut references = Vec::new();
        references
            .try_reserve_exact(reference_count)
            .map_err(|_| BundleError::Allocation("object references"))?;
        object.visit_references(&mut |reference| references.push(reference));
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(binding_count)
            .map_err(|_| BundleError::Allocation("object bindings"))?;
        let mut node = Self {
            kind,
            payload_len,
            payload_offset,
            references,
            bindings,
            revision_tree: None,
            revision_change: None,
            evidence_candidate: None,
        };
        match object {
            MetadataObject::Tree(_) => {}
            MetadataObject::Revision(revision) => {
                node.revision_tree = Some(revision.tree);
                node.revision_change = Some(revision.change);
            }
            MetadataObject::Operation(operation) => {
                for (change, state) in &operation.view.changes {
                    for revision in &state.heads {
                        node.bind(Binding::RevisionChange {
                            revision: *revision,
                            change: *change,
                        })?;
                    }
                }
                for workspace in operation.view.workspaces.values() {
                    for source in workspace.sources.values() {
                        if let Some(revision) = source.revision {
                            node.bind(Binding::RevisionTree {
                                revision,
                                tree: source.tree,
                            })?;
                        }
                    }
                }
                for (candidate, records) in &operation.view.evidence {
                    for evidence in records {
                        node.bind(Binding::EvidenceCandidate {
                            evidence: evidence.object_id(),
                            candidate: candidate.object_id(),
                        })?;
                    }
                }
            }
            MetadataObject::Candidate(candidate) => node.bind(Binding::RevisionTree {
                revision: candidate.result,
                tree: candidate.result_tree,
            })?,
            MetadataObject::Evidence(evidence) => {
                node.evidence_candidate = Some(evidence.candidate.object_id());
                node.bind(Binding::RevisionTree {
                    revision: evidence.inputs.revision,
                    tree: evidence.inputs.tree,
                })?;
            }
        }
        Ok(node)
    }

    fn bind(&mut self, binding: Binding) -> Result<()> {
        self.bindings
            .try_reserve(1)
            .map_err(|_| BundleError::Allocation("object bindings"))?;
        self.bindings.push(binding);
        Ok(())
    }

    fn charged_bytes(&self) -> Result<u64> {
        charged_node_bytes(self.references.capacity(), self.bindings.capacity())
    }
}

fn charged_node_bytes(references: usize, bindings: usize) -> Result<u64> {
    // Reference storage plus worst-case doubled pending-queue capacity; the
    // node allowance also covers indexes, ordering and iterative DFS frames.
    let base = std::mem::size_of::<Node>()
        .checked_add(256)
        .ok_or(BundleError::Limit("graph bytes"))?;
    let refs = references
        .checked_mul(std::mem::size_of::<ObjectReference>())
        .and_then(|n| n.checked_mul(3))
        .ok_or(BundleError::Limit("graph bytes"))?;
    let bindings = bindings
        .checked_mul(std::mem::size_of::<Binding>())
        .ok_or(BundleError::Limit("graph bytes"))?;
    let charged = base
        .checked_add(refs)
        .and_then(|n| n.checked_add(bindings))
        .ok_or(BundleError::Limit("graph bytes"))?;
    u64::try_from(charged).map_err(|_| BundleError::Limit("graph bytes"))
}

pub(crate) struct Graph {
    pub nodes: HashMap<ObjectId, Node>,
    pub root_operation: Option<Operation>,
    pub payload_bytes: u64,
    pub reference_count: u64,
    charged_bytes: u64,
}

impl Graph {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
            root_operation: None,
            payload_bytes: 0,
            reference_count: 0,
            charged_bytes: 0,
        }
    }

    pub fn remaining_bytes(&self, options: &BundleOptions) -> Result<u64> {
        options
            .max_graph_bytes
            .checked_sub(self.charged_bytes)
            .ok_or(BundleError::Limit("graph bytes"))
    }

    pub fn retain_root(
        &mut self,
        operation: Operation,
        payload_len: u64,
        options: &BundleOptions,
    ) -> Result<()> {
        let charge = payload_len
            .checked_mul(8)
            .ok_or(BundleError::Limit("root metadata graph bytes"))?;
        self.charged_bytes = self
            .charged_bytes
            .checked_add(charge)
            .ok_or(BundleError::Limit("graph bytes"))?;
        if self.charged_bytes > options.max_graph_bytes {
            return Err(BundleError::Limit("graph bytes"));
        }
        self.root_operation = Some(operation);
        Ok(())
    }

    pub fn insert(&mut self, id: ObjectId, node: Node, options: &BundleOptions) -> Result<()> {
        if self.nodes.contains_key(&id) {
            return Err(BundleError::DuplicateObject(id));
        }
        let count = u64::try_from(self.nodes.len())
            .map_err(|_| BundleError::Limit("objects"))?
            .checked_add(1)
            .ok_or(BundleError::Limit("objects"))?;
        if count > options.max_objects {
            return Err(BundleError::Limit("objects"));
        }
        self.payload_bytes = self
            .payload_bytes
            .checked_add(node.payload_len)
            .ok_or(BundleError::Limit("payload bytes"))?;
        if self.payload_bytes > options.max_total_bytes {
            return Err(BundleError::Limit("payload bytes"));
        }
        self.reference_count = self
            .reference_count
            .checked_add(
                u64::try_from(node.references.len())
                    .map_err(|_| BundleError::Limit("references"))?,
            )
            .ok_or(BundleError::Limit("references"))?;
        if self.reference_count > options.max_references {
            return Err(BundleError::Limit("references"));
        }
        self.charged_bytes = self
            .charged_bytes
            .checked_add(node.charged_bytes()?)
            .ok_or(BundleError::Limit("graph bytes"))?;
        if self.charged_bytes > options.max_graph_bytes {
            return Err(BundleError::Limit("graph bytes"));
        }
        self.nodes
            .try_reserve(1)
            .map_err(|_| BundleError::Allocation("object index"))?;
        self.nodes.insert(id, node);
        Ok(())
    }

    pub fn sorted_ids(&self) -> Result<Vec<ObjectId>> {
        let mut ids = Vec::new();
        ids.try_reserve_exact(self.nodes.len())
            .map_err(|_| BundleError::Allocation("object ordering"))?;
        ids.extend(self.nodes.keys().copied());
        ids.sort_unstable();
        Ok(ids)
    }

    pub fn validate(
        &self,
        root: OperationId,
        options: &BundleOptions,
        cancel: &CancellationToken,
    ) -> Result<usize> {
        let root = root.object_id();
        let root_node = self.nodes.get(&root).ok_or(BundleError::MissingObject {
            id: root,
            referenced_by: root,
        })?;
        require_kind(root, root_node.kind, ObjectKind::Operation)?;
        for (id, node) in &self.nodes {
            cancel.check()?;
            for reference in &node.references {
                cancel.check()?;
                let target = self
                    .nodes
                    .get(&reference.id)
                    .ok_or(BundleError::MissingObject {
                        id: reference.id,
                        referenced_by: *id,
                    })?;
                require_kind(reference.id, target.kind, reference.kind)?;
            }
            for binding in &node.bindings {
                cancel.check()?;
                match binding {
                    Binding::RevisionChange { revision, change } => {
                        let target = self.nodes.get(&revision.object_id()).ok_or(
                            BundleError::MissingObject {
                                id: revision.object_id(),
                                referenced_by: *id,
                            },
                        )?;
                        if target.revision_change != Some(*change) {
                            return Err(BundleError::Invalid(
                                "revision head belongs to a different logical change",
                            ));
                        }
                    }
                    Binding::RevisionTree { revision, tree } => {
                        let target = self.nodes.get(&revision.object_id()).ok_or(
                            BundleError::MissingObject {
                                id: revision.object_id(),
                                referenced_by: *id,
                            },
                        )?;
                        if target.revision_tree != Some(*tree) {
                            return Err(BundleError::Invalid(
                                "referenced revision tree differs from its asserted tree",
                            ));
                        }
                    }
                    Binding::EvidenceCandidate {
                        evidence,
                        candidate,
                    } => {
                        let target =
                            self.nodes.get(evidence).ok_or(BundleError::MissingObject {
                                id: *evidence,
                                referenced_by: *id,
                            })?;
                        if target.evidence_candidate != Some(*candidate) {
                            return Err(BundleError::Invalid(
                                "evidence map candidate differs from its evidence record",
                            ));
                        }
                    }
                }
            }
        }
        self.depth_and_reachability(root, options, cancel)
    }

    fn depth_and_reachability(
        &self,
        root: ObjectId,
        options: &BundleOptions,
        cancel: &CancellationToken,
    ) -> Result<usize> {
        #[derive(Clone, Copy)]
        enum Visit {
            Active,
            Done(usize),
        }
        let mut visited = HashMap::new();
        visited
            .try_reserve(self.nodes.len())
            .map_err(|_| BundleError::Allocation("graph traversal"))?;
        let mut stack = Vec::new();
        stack
            .try_reserve(1)
            .map_err(|_| BundleError::Allocation("graph traversal"))?;
        stack.push((root, 0_usize, 1_usize));
        visited.insert(root, Visit::Active);
        while let Some(&(id, edge_index, mut depth)) = stack.last() {
            cancel.check()?;
            let node = self
                .nodes
                .get(&id)
                .ok_or(BundleError::Invalid("graph traversal object missing"))?;
            if let Some(edge) = node.references.get(edge_index) {
                match visited.get(&edge.id).copied() {
                    Some(Visit::Active) => {
                        return Err(BundleError::Invalid("object graph contains a cycle"));
                    }
                    Some(Visit::Done(child_depth)) => {
                        depth = depth.max(
                            child_depth
                                .checked_add(1)
                                .ok_or(BundleError::Limit("graph depth"))?,
                        );
                        if depth > options.max_depth {
                            return Err(BundleError::Limit("graph depth"));
                        }
                        if let Some(frame) = stack.last_mut() {
                            frame.1 += 1;
                            frame.2 = depth;
                        }
                    }
                    None => {
                        if stack.len() >= options.max_depth {
                            return Err(BundleError::Limit("graph depth"));
                        }
                        stack
                            .try_reserve(1)
                            .map_err(|_| BundleError::Allocation("graph traversal"))?;
                        stack.push((edge.id, 0, 1));
                        visited.insert(edge.id, Visit::Active);
                    }
                }
            } else {
                visited.insert(id, Visit::Done(depth));
                stack.pop();
            }
        }
        if visited.len() != self.nodes.len()
            && let Some(id) = self.nodes.keys().find(|id| !visited.contains_key(id))
        {
            return Err(BundleError::UnreachableObject(*id));
        }
        match visited.get(&root) {
            Some(Visit::Done(depth)) => Ok(*depth),
            _ => Err(BundleError::Invalid("root traversal did not finish")),
        }
    }
}

pub(crate) fn require_kind(id: ObjectId, actual: ObjectKind, expected: ObjectKind) -> Result<()> {
    if actual == expected {
        Ok(())
    } else {
        Err(BundleError::UnexpectedKind {
            id,
            expected,
            actual,
        })
    }
}

pub(crate) fn collect<S: ObjectSource + ?Sized>(
    source: &S,
    root: OperationId,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<Graph> {
    let mut graph = Graph::new();
    let mut pending = Vec::new();
    pending
        .try_reserve(1)
        .map_err(|_| BundleError::Allocation("graph queue"))?;
    pending.push(ObjectReference {
        id: root.object_id(),
        kind: ObjectKind::Operation,
    });
    while let Some(reference) = pending.pop() {
        cancel.check()?;
        if let Some(node) = graph.nodes.get(&reference.id) {
            require_kind(reference.id, node.kind, reference.kind)?;
            continue;
        }
        let info = source.object_info(reference.id, cancel)?;
        require_kind(reference.id, info.kind, reference.kind)?;
        options
            .model_limits
            .check_payload(info.kind, info.payload_len)?;
        let node = if info.kind == ObjectKind::Blob {
            Node::blob(info.payload_len, 0)
        } else {
            let mut payload =
                BoundedBuffer::new(options.model_limits.metadata_len(info.payload_len)?);
            let read = source.read_object_to(reference.id, info.kind, &mut payload, cancel)?;
            if read != info.payload_len {
                return Err(BundleError::Invalid("source payload length changed"));
            }
            if hash_object(info.kind, &payload.bytes, &options.model_limits)? != reference.id {
                return Err(izu_model::ModelError::HashMismatch.into());
            }
            let object = decode_object_metadata(info.kind, &payload.bytes, &options.model_limits)?;
            let node = Node::metadata(
                info.kind,
                info.payload_len,
                0,
                &object,
                options,
                graph.remaining_bytes(options)?,
            )?;
            if reference.id == root.object_id()
                && let MetadataObject::Operation(operation) = object
            {
                graph.retain_root(operation, info.payload_len, options)?;
            }
            node
        };
        let pending_len = pending
            .len()
            .checked_add(node.references.len())
            .ok_or(BundleError::Limit("graph queue"))?;
        if u64::try_from(pending_len).map_or(true, |len| len > options.max_references) {
            return Err(BundleError::Limit("graph queue"));
        }
        pending
            .try_reserve(node.references.len())
            .map_err(|_| BundleError::Allocation("graph queue"))?;
        pending.extend(node.references.iter().copied());
        graph.insert(reference.id, node, options)?;
    }
    graph.validate(root, options, cancel)?;
    Ok(graph)
}

pub(crate) struct BoundedBuffer {
    pub bytes: Vec<u8>,
    limit: usize,
}

impl BoundedBuffer {
    pub fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
        }
    }
}

impl Write for BoundedBuffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let total = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or_else(|| io::Error::other("metadata size overflow"))?;
        if total > self.limit {
            return Err(io::Error::other("metadata size exceeds bound"));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| io::Error::other("metadata allocation failed"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
