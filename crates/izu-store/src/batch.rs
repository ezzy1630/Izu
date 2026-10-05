//! Explicit provisional object staging. Only consuming `finish` acknowledges
//! persistence; this API never changes HEAD or weakens a strict object put.
use crate::durability::{ClosureSubmission, Identity};
use crate::objects::{StreamFrame, TemporaryEntry, object_name, object_parts};
use crate::{DurableBoundary, Result, Store, StoreError, durable_dir, io_error, kernel_file};
use izu_model::{CancellationToken, ObjectId, ObjectKind};
use std::fs::File;
use std::io::{self, Read};

/// A content address for encoding metadata, not a persistence acknowledgement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StagedObjectId(ObjectId);

impl StagedObjectId {
    pub fn object_id(self) -> ObjectId {
        self.0
    }
}

/// Every successfully staged input, including duplicate inputs, is covered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ObjectBatchReceipt {
    pub object_count: u64,
    pub payload_bytes: u64,
}

struct PreparedObject<'a> {
    id: ObjectId,
    kind: ObjectKind,
    length: u64,
    temporary: TemporaryEntry<'a>,
}

struct PublishedObject {
    id: ObjectId,
    identity: Identity,
}

#[derive(Clone, Copy)]
enum BatchState {
    Active,
    Aborted,
}

/// Holds the shared recovery lease until its prepared entries have been cleaned.
/// Payloads and per-object descriptors are not retained. Failed staging poisons
/// the batch, so catching an error cannot turn partial work into a batch receipt.
pub struct ObjectBatch<'a> {
    store: &'a Store,
    submission: ClosureSubmission,
    prepared: Vec<PreparedObject<'a>>,
    payload_bytes: u64,
    state: BatchState,
    // Declared last: entry cleanup precedes releasing recovery's exclusion.
    _lease: File,
}

impl Store {
    pub fn begin_object_batch(&self, cancel: &CancellationToken) -> Result<ObjectBatch<'_>> {
        cancel.check()?;
        let lease = self.acquire_lock("temporary.lock", true, cancel)?;
        let submission = ClosureSubmission::new(self)?;
        Ok(ObjectBatch {
            store: self,
            submission,
            prepared: Vec::new(),
            payload_bytes: 0,
            state: BatchState::Active,
            _lease: lease,
        })
    }
}

impl ObjectBatch<'_> {
    pub fn stage_blob<R: Read>(
        &mut self,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<StagedObjectId> {
        self.stage_stream(ObjectKind::Blob, reader, length, cancel)
    }

    pub fn stage(
        &mut self,
        kind: ObjectKind,
        payload: &[u8],
        cancel: &CancellationToken,
    ) -> Result<StagedObjectId> {
        let length = match u64::try_from(payload.len()) {
            Ok(length) => length,
            Err(_) => {
                self.state = BatchState::Aborted;
                return Err(StoreError::LimitExceeded("object length"));
            }
        };
        self.stage_stream(kind, &mut io::Cursor::new(payload), length, cancel)
    }

    fn stage_stream<R: Read>(
        &mut self,
        kind: ObjectKind,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<StagedObjectId> {
        if matches!(self.state, BatchState::Aborted) {
            return Err(StoreError::BatchAborted);
        }
        let result = self.stage_checked(kind, reader, length, cancel);
        if result.is_err() {
            self.state = BatchState::Aborted;
        }
        result
    }

    fn stage_checked<R: Read>(
        &mut self,
        kind: ObjectKind,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<StagedObjectId> {
        cancel.check()?;
        let frame = StreamFrame::new(kind, length, self.store)?;
        let total = self.reserve_record(length)?;
        self.submission.check_layout(self.store)?;
        let (id, temp) = self.store.prepare_object(reader, frame, cancel)?;
        temp.check_entry()?;
        self.submission.check_file(&temp.file)?;
        cancel.check()?;
        kernel_file(&temp.file, &self.store.options)?;
        self.store
            .boundary(DurableBoundary::BatchObjectKernelSynced)?;
        temp.check_entry()?;
        self.prepared.push(PreparedObject {
            id,
            kind,
            length,
            temporary: temp.into_entry(),
        });
        self.payload_bytes = total;
        Ok(StagedObjectId(id))
    }

    // Reserve both the prepared records and the eventual identity ledger before
    // any source read. Geometric growth stays under the explicit heap bound.
    fn reserve_record(&mut self, length: u64) -> Result<u64> {
        let next = self
            .prepared
            .len()
            .checked_add(1)
            .ok_or(StoreError::LimitExceeded("batch object count"))?;
        if u64::try_from(next).map_err(|_| StoreError::LimitExceeded("batch object count"))?
            > self.store.options.max_scan_objects
        {
            return Err(StoreError::LimitExceeded("batch object count"));
        }
        let bytes = self
            .payload_bytes
            .checked_add(length)
            .ok_or(StoreError::LimitExceeded("batch payload bytes"))?;
        if bytes > self.store.options.max_scan_bytes {
            return Err(StoreError::LimitExceeded("batch payload bytes"));
        }
        let record_bytes = std::mem::size_of::<PreparedObject<'_>>()
            .checked_add(std::mem::size_of::<PublishedObject>())
            .ok_or(StoreError::LimitExceeded("batch bookkeeping"))?;
        let record_bytes = u64::try_from(record_bytes)
            .map_err(|_| StoreError::LimitExceeded("batch bookkeeping"))?;
        let capacity_limit = self.store.options.max_in_memory_bytes / record_bytes;
        let capacity_limit = usize::try_from(capacity_limit.min(usize::MAX as u64))
            .map_err(|_| StoreError::LimitExceeded("batch bookkeeping"))?;
        if next > capacity_limit {
            return Err(StoreError::LimitExceeded("batch bookkeeping"));
        }
        if next > self.prepared.capacity() {
            let capacity = self
                .prepared
                .capacity()
                .max(1)
                .checked_mul(2)
                .unwrap_or(capacity_limit)
                .min(capacity_limit);
            let additional = capacity
                .checked_sub(self.prepared.len())
                .ok_or(StoreError::LimitExceeded("batch bookkeeping"))?;
            self.prepared
                .try_reserve_exact(additional)
                .map_err(|_| StoreError::Allocation)?;
        }
        Ok(bytes)
    }

    /// Completes ordered file/namespace persistence for the entire staged set.
    /// An error can retain finalized unreferenced objects; it never changes HEAD
    /// or acknowledges the batch. Drop cleans only unchanged owned temp names.
    pub fn finish(mut self, cancel: &CancellationToken) -> Result<ObjectBatchReceipt> {
        if matches!(self.state, BatchState::Aborted) {
            return Err(StoreError::BatchAborted);
        }
        cancel.check()?;
        #[cfg(feature = "fault-injection")]
        if self.store.options.allow_volatile_for_tests {
            return Err(StoreError::VolatileTestMode);
        }
        let mut published = Vec::new();
        published
            .try_reserve_exact(self.prepared.len())
            .map_err(|_| StoreError::Allocation)?;
        self.submission.check_layout(self.store)?;
        for prepared in &self.prepared {
            cancel.check()?;
            let mut file = prepared.temporary.open()?;
            self.submission.check_file(&file)?;
            let header =
                self.store
                    .verify_file(&mut file, prepared.id, Some(prepared.kind), cancel)?;
            if header.payload_len != prepared.length {
                return Err(StoreError::Corrupt {
                    kind: "prepared object",
                    reason: "length changed",
                });
            }
        }
        // No new immutable name precedes completion of the staged file data.
        kernel_file(self.store.temporary.file(), &self.store.options)?;
        self.store
            .boundary(DurableBoundary::BatchTemporaryKernelSynced)?;
        cancel.check()?;
        self.submission.check_layout(self.store)?;
        durable_dir(&self.store.temporary, &self.store.options)?;
        self.store.boundary(DurableBoundary::BatchDataSynced)?;
        self.submission.check_layout(self.store)?;
        for prepared in &self.prepared {
            cancel.check()?;
            let mut file = prepared.temporary.open()?;
            self.submission.check_file(&file)?;
            let name = object_name(prepared.id);
            let (prefix, suffix) = object_parts(&name)?;
            let shard = self
                .store
                .objects
                .ensure_dir(prefix)
                .map_err(|error| io_error("create batch object shard", error))?;
            self.submission.check_file(shard.file())?;
            match self
                .store
                .temporary
                .hard_link(prepared.temporary.name()?, &shard, suffix)
            {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(io_error("publish batch immutable object", error)),
            }
            let mut actual = shard
                .open_read(suffix)
                .map_err(|error| io_error("open batch immutable object", error))?;
            self.store.verify_duplicate(
                &mut actual,
                &mut file,
                prepared.id,
                prepared.kind,
                prepared.length,
                cancel,
            )?;
            prepared.temporary.check()?;
            self.submission
                .record_object(prepared.id, &actual, &shard)?;
            let identity = Identity::file(&actual)?;
            self.store.boundary(DurableBoundary::BatchObjectLinked)?;
            cancel.check()?;
            // This includes duplicates from a writer that has not yet ACKed.
            kernel_file(&actual, &self.store.options)?;
            self.store
                .boundary(DurableBoundary::BatchFinalObjectKernelSynced)?;
            published.push(PublishedObject {
                id: prepared.id,
                identity,
            });
            prepared.temporary.remove()?;
        }
        self.submission.submit_directories(self.store, cancel)?;
        kernel_file(self.store.temporary.file(), &self.store.options)?;
        kernel_file(self.store.root.file(), &self.store.options)?;
        self.store
            .boundary(DurableBoundary::BatchDirectoriesKernelSynced)?;
        cancel.check()?;
        self.check_published(&published, cancel)?;
        durable_dir(&self.store.root, &self.store.options)?;
        self.store.boundary(DurableBoundary::BatchDirectorySynced)?;
        self.check_published(&published, cancel)?;
        Ok(ObjectBatchReceipt {
            object_count: u64::try_from(self.prepared.len())
                .map_err(|_| StoreError::LimitExceeded("batch object count"))?,
            payload_bytes: self.payload_bytes,
        })
    }

    fn check_published(
        &self,
        published: &[PublishedObject],
        cancel: &CancellationToken,
    ) -> Result<()> {
        self.submission.check_layout(self.store)?;
        for entry in published {
            cancel.check()?;
            let (file, _) = self.store.open_object_with_shard(entry.id)?;
            if Identity::file(&file)? != entry.identity {
                return Err(StoreError::Corrupt {
                    kind: "batch object",
                    reason: "entry no longer names the submitted object",
                });
            }
        }
        Ok(())
    }
}
