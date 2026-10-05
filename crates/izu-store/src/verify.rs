use crate::durability::{ClosureSubmission, check_file_entry};
use crate::objects::{object_name, object_parts};
use crate::{Result, Store, StoreError, durable_dir, durable_file, io_error};
use izu_model::{
    CancellationToken, ObjectId, ObjectKind, OperationId, ReferencedObjects, decode_object_metadata,
};
use izu_platform::EntryKind;
use std::collections::HashMap;
use std::ffi::OsStr;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectInfo {
    pub id: ObjectId,
    pub kind: ObjectKind,
    pub payload_len: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VerifyReport {
    pub head: Option<OperationId>,
    pub object_count: u64,
    pub payload_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RecoveryReport {
    pub verified: VerifyReport,
    pub removed_temporary_files: u64,
    pub unknown_temporary_entries: u64,
    pub retained_objects: u64,
}

impl Store {
    pub fn object_info(&self, id: ObjectId, cancel: &CancellationToken) -> Result<ObjectInfo> {
        let mut file = self.open_object(id)?;
        let header = self.verify_file(&mut file, id, None, cancel)?;
        self.validate_metadata_payload(id, header.kind, cancel)?;
        Ok(ObjectInfo {
            id,
            kind: header.kind,
            payload_len: header.payload_len,
        })
    }

    /// Verifies every finalized object, including retained uncommitted objects.
    /// The scan is bounded and does not allocate proportional to blob length.
    pub fn list_objects(&self, cancel: &CancellationToken) -> Result<Vec<ObjectInfo>> {
        let mut result = Vec::new();
        let mut bytes = 0_u64;
        let mut shard_count = 0_u64;
        for entry in self
            .objects
            .entries()
            .map_err(|error| io_error("list object shards", error))?
        {
            cancel.check()?;
            let entry = entry.map_err(|error| io_error("read object shard entry", error))?;
            shard_count = shard_count
                .checked_add(1)
                .ok_or(StoreError::LimitExceeded("object shards"))?;
            if shard_count > 256 {
                return Err(StoreError::LimitExceeded("object shards"));
            }
            let prefix = entry
                .name
                .to_str()
                .filter(|name| name.len() == 2 && is_hex(name.as_bytes()))
                .ok_or(StoreError::Corrupt {
                    kind: "objects directory",
                    reason: "invalid shard name",
                })?;
            if entry.kind != EntryKind::Directory {
                return Err(StoreError::Corrupt {
                    kind: "objects directory",
                    reason: "shard is not a directory",
                });
            }
            let shard = self
                .objects
                .open_dir(OsStr::new(prefix))
                .map_err(|error| io_error("open listed shard", error))?;
            for child in shard
                .entries()
                .map_err(|error| io_error("list shard objects", error))?
            {
                cancel.check()?;
                let child = child.map_err(|error| io_error("read object entry", error))?;
                let suffix = child
                    .name
                    .to_str()
                    .filter(|name| name.len() == 62 && is_hex(name.as_bytes()))
                    .ok_or(StoreError::Corrupt {
                        kind: "object shard",
                        reason: "invalid object name",
                    })?;
                if child.kind != EntryKind::File {
                    return Err(StoreError::Corrupt {
                        kind: "object shard",
                        reason: "object is not a regular file",
                    });
                }
                if u64::try_from(result.len())
                    .map_err(|_| StoreError::LimitExceeded("object count"))?
                    >= self.options.max_scan_objects
                {
                    return Err(StoreError::LimitExceeded("object count"));
                }
                let mut name = [0_u8; 64];
                name[..2].copy_from_slice(prefix.as_bytes());
                name[2..].copy_from_slice(suffix.as_bytes());
                let text = std::str::from_utf8(&name)
                    .map_err(|_| StoreError::InvalidPath("object identifier is not ASCII"))?;
                let id: ObjectId = text.parse()?;
                let mut file = self.open_object(id)?;
                let header = self.read_header(&mut file, id, None)?;
                bytes = bytes
                    .checked_add(header.payload_len)
                    .ok_or(StoreError::LimitExceeded("scan bytes"))?;
                if bytes > self.options.max_scan_bytes {
                    return Err(StoreError::LimitExceeded("scan bytes"));
                }
                let info = self.object_info(id, cancel)?;
                result.try_reserve(1).map_err(|_| StoreError::Allocation)?;
                result.push(info);
            }
        }
        Ok(result)
    }

    pub fn verify(&self, cancel: &CancellationToken) -> Result<VerifyReport> {
        let head = self.current_head(cancel)?;
        self.verify_at_head(head, cancel)
    }

    fn verify_at_head(
        &self,
        head: Option<OperationId>,
        cancel: &CancellationToken,
    ) -> Result<VerifyReport> {
        let objects = self.list_objects(cancel)?;
        if let Some(head) = head {
            self.verify_reachable(head, cancel)?;
        }
        let mut bytes = 0_u64;
        for object in &objects {
            bytes = bytes
                .checked_add(object.payload_len)
                .ok_or(StoreError::LimitExceeded("scan bytes"))?;
        }
        Ok(VerifyReport {
            head,
            object_count: objects.len() as u64,
            payload_bytes: bytes,
        })
    }

    pub fn verify_reachable(
        &self,
        root: OperationId,
        cancel: &CancellationToken,
    ) -> Result<VerifyReport> {
        self.walk_reachable(root, None, cancel)
    }

    pub(crate) fn submit_reachable(
        &self,
        root: OperationId,
        cancel: &CancellationToken,
    ) -> Result<ClosureSubmission> {
        let mut submission = ClosureSubmission::new(self)?;
        self.walk_reachable(root, Some(&mut submission), cancel)?;
        submission.submit_directories(self, cancel)?;
        Ok(submission)
    }

    fn walk_reachable(
        &self,
        root: OperationId,
        mut submission: Option<&mut ClosureSubmission>,
        cancel: &CancellationToken,
    ) -> Result<VerifyReport> {
        let mut graph = Graph::new(self.options.max_scan_objects);
        graph.add(root.object_id(), ObjectKind::Operation)?;
        let mut bytes = 0_u64;
        let mut count = 0_u64;
        while let Some((id, expected)) = graph.pending.pop() {
            cancel.check()?;
            let (mut file, shard) = self.open_object_with_shard(id)?;
            let header = self.read_header(&mut file, id, Some(expected))?;
            bytes = bytes
                .checked_add(header.payload_len)
                .ok_or(StoreError::LimitExceeded("reachable bytes"))?;
            if bytes > self.options.max_scan_bytes {
                return Err(StoreError::LimitExceeded("reachable bytes"));
            }
            if expected == ObjectKind::Blob {
                self.verify_file(&mut file, id, Some(expected), cancel)?;
            } else {
                let payload = self.read_payload(&mut file, id, header, cancel)?;
                let name = object_name(id);
                let (_, suffix) = object_parts(&name)?;
                check_file_entry(&shard, suffix, &file)?;
                self.collect_links(expected, &payload, &mut graph, cancel)?;
            }
            if let Some(submission) = submission.as_deref_mut() {
                submission.submit_object(self, id, &file, &shard)?;
            }
            count = count
                .checked_add(1)
                .ok_or(StoreError::LimitExceeded("reachable objects"))?;
        }
        Ok(VerifyReport {
            head: Some(root),
            object_count: count,
            payload_bytes: bytes,
        })
    }

    /// Retains all immutable objects. Recovery cleans only recognized temporary
    /// entries while holding a process-crash-released exclusive writer lease.
    pub fn recover(&self, cancel: &CancellationToken) -> Result<RecoveryReport> {
        let _lease = self.acquire_lock("temporary.lock", false, cancel)?;
        let head = crate::transaction::read_head_file(self, cancel)?;
        let verified = self.verify_at_head(head, cancel)?;
        // Publications also hold the shared temporary lease. The exclusive
        // recovery lease therefore keeps this HEAD stable without reversing the
        // head-lock -> temporary-lock acquisition order used by transactions.
        let submission = verified
            .head
            .map(|head| self.submit_reachable(head, cancel))
            .transpose()?;
        if let Some(submission) = &submission {
            submission.check_layout(self)?;
            let head_file = self
                .root
                .open_read(OsStr::new("HEAD"))
                .map_err(|error| io_error("open recovered HEAD", error))?;
            submission.check_file(&head_file)?;
            // Completes the closure's earlier kernel-only submissions as well
            // as the selected HEAD data. Recovery still issues a full barrier.
            durable_file(&head_file, &self.options)?;
            submission.check_layout(self)?;
            durable_dir(&self.root, &self.options)?;
            submission.check_layout(self)?;
        }
        let mut removed = 0_u64;
        let mut unknown = 0_u64;
        let mut visited = 0_u64;
        for entry in self
            .temporary
            .entries()
            .map_err(|error| io_error("list temporary entries", error))?
        {
            cancel.check()?;
            visited = visited
                .checked_add(1)
                .ok_or(StoreError::LimitExceeded("temporary entries"))?;
            if visited > self.options.max_scan_objects {
                return Err(StoreError::LimitExceeded("temporary entries"));
            }
            let entry = entry.map_err(|error| io_error("read temporary entry", error))?;
            let owned = entry.name.to_str().is_some_and(|name| {
                name.len() == 40
                    && name.starts_with("izu-")
                    && name.ends_with(".tmp")
                    && is_hex(&name.as_bytes()[4..36])
            });
            if owned && matches!(entry.kind, EntryKind::File | EntryKind::Symlink) {
                self.temporary
                    .remove_file(&entry.name)
                    .map_err(|error| io_error("remove stale temporary entry", error))?;
                removed = removed
                    .checked_add(1)
                    .ok_or(StoreError::LimitExceeded("removed entries"))?;
            } else {
                unknown = unknown
                    .checked_add(1)
                    .ok_or(StoreError::LimitExceeded("unknown entries"))?;
            }
        }
        durable_dir(&self.temporary, &self.options)?;
        // Keep the submitted closure's identities through cleanup: a recovery
        // receipt must still name the history whose persistence was completed.
        if let Some(submission) = &submission {
            submission.check_layout(self)?;
        }
        Ok(RecoveryReport {
            verified,
            removed_temporary_files: removed,
            unknown_temporary_entries: unknown,
            retained_objects: verified.object_count,
        })
    }

    fn validate_metadata_payload(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if kind == ObjectKind::Blob {
            return Ok(());
        }
        let payload = self.get(id, kind, cancel)?;
        decode_object_metadata(kind, &payload, &self.options.limits)?;
        Ok(())
    }

    fn collect_links(
        &self,
        kind: ObjectKind,
        payload: &[u8],
        graph: &mut Graph,
        cancel: &CancellationToken,
    ) -> Result<()> {
        cancel.check()?;
        let metadata = decode_object_metadata(kind, payload, &self.options.limits)?;
        let mut result = Ok(());
        metadata.visit_references(&mut |reference| {
            if result.is_ok() {
                result = graph.add(reference.id, reference.kind);
            }
        });
        result
    }
}

struct Graph {
    discovered: HashMap<ObjectId, ObjectKind>,
    pending: Vec<(ObjectId, ObjectKind)>,
    limit: u64,
}

impl Graph {
    fn new(limit: u64) -> Self {
        Self {
            discovered: HashMap::new(),
            pending: Vec::new(),
            limit,
        }
    }
    fn add(&mut self, id: ObjectId, expected: ObjectKind) -> Result<()> {
        if let Some(actual) = self.discovered.get(&id) {
            if *actual != expected {
                return Err(StoreError::UnexpectedKind {
                    id,
                    actual: *actual,
                    expected,
                });
            }
            return Ok(());
        }
        if self.discovered.len() as u64 >= self.limit {
            return Err(StoreError::LimitExceeded("reachable objects"));
        }
        self.discovered
            .try_reserve(1)
            .map_err(|_| StoreError::Allocation)?;
        self.pending
            .try_reserve(1)
            .map_err(|_| StoreError::Allocation)?;
        self.discovered.insert(id, expected);
        self.pending.push((id, expected));
        Ok(())
    }
}

fn is_hex(bytes: &[u8]) -> bool {
    bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
}
