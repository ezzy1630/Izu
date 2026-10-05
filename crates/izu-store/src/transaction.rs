use crate::admission::{HeadLease, HeadMode};
use crate::durability::{Identity, check_file_entry};
use crate::objects::TemporaryFile;
use crate::{
    DurableBoundary, Result, Store, StoreError, durable_dir, durable_file, io_error, kernel_file,
};
use izu_model::{CancellationToken, ObjectId, ObjectKind, Operation, OperationId, decode_metadata};
use sha2::{Digest, Sha256};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};

const HEAD_MAGIC: &[u8; 8] = b"IZUHEAD1";
const HEAD_LEN: usize = 72;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeadExpectation {
    /// Read the latest immutable state under the publication lock.
    Any,
    Absent,
    At(OperationId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PublishReceipt {
    pub previous: Option<OperationId>,
    pub current: OperationId,
}

#[derive(Debug)]
pub enum PublishOutcome {
    Durable(PublishReceipt),
    /// HEAD is visible. The error means persistence has not been acknowledged.
    /// Callers must retain the receipt and reread/recover; rollback is not implied.
    VisibleButUncertain {
        receipt: PublishReceipt,
        error: StoreError,
    },
}

#[derive(Debug)]
pub struct Transaction<'a> {
    store: &'a Store,
    current: Option<OperationId>,
    _lock: HeadLease,
}

impl Store {
    pub fn begin(
        &self,
        expected: HeadExpectation,
        cancel: &CancellationToken,
    ) -> Result<Transaction<'_>> {
        let lock = self.acquire_head(HeadMode::Exclusive, cancel)?;
        let current = read_head_file(self, cancel)?;
        let matches = match expected {
            HeadExpectation::Any => true,
            HeadExpectation::Absent => current.is_none(),
            HeadExpectation::At(id) => current == Some(id),
        };
        if !matches {
            return Err(StoreError::HeadConflict {
                expected,
                actual: current,
            });
        }
        Ok(Transaction {
            store: self,
            current,
            _lock: lock,
        })
    }

    /// Cold-restore boundary for an owned, unpublished staging store. HEAD must
    /// be absent under the lock. The complete archived operation chain is verified
    /// and persisted before seeding HEAD. This never replaces an active history.
    /// The engine must remap workspace locations and finish restoration before
    /// publishing the staging directory as a usable repository.
    pub fn bootstrap_archive(
        &self,
        archived_root: OperationId,
        cancel: &CancellationToken,
    ) -> Result<PublishOutcome> {
        self.begin(HeadExpectation::Absent, cancel)?
            .publish_checked(archived_root, cancel, false)
    }
}

impl Transaction<'_> {
    pub fn current_head(&self) -> Option<OperationId> {
        self.current
    }
    pub fn store(&self) -> &Store {
        self.store
    }

    /// Consumes the transaction, ensuring callers cannot publish twice under a
    /// stale expected head. Cancellation is honored before the atomic rename;
    /// afterward persistence is completed and the actual publication is reported.
    pub fn publish(
        self,
        operation: OperationId,
        cancel: &CancellationToken,
    ) -> Result<PublishOutcome> {
        self.publish_checked(operation, cancel, true)
    }

    fn publish_checked(
        self,
        operation: OperationId,
        cancel: &CancellationToken,
        enforce_parent: bool,
    ) -> Result<PublishOutcome> {
        cancel.check()?;
        let payload = self
            .store
            .get(operation.object_id(), ObjectKind::Operation, cancel)?;
        let decoded: Operation = decode_metadata(&payload, &self.store.options.limits)?;
        if enforce_parent && decoded.parent != self.current {
            return Err(StoreError::HistoryMismatch {
                operation,
                parent: decoded.parent,
                current: self.current,
            });
        }
        let submission = self.store.submit_reachable(operation, cancel)?;
        let _lease = self.store.acquire_lock("temporary.lock", true, cancel)?;
        let mut temp = TemporaryFile::new(&self.store.temporary)?;
        submission.check_file(&temp.file)?;
        let head = encode_head(operation);
        temp.file
            .write_all(&head)
            .map_err(|error| io_error("write prepared HEAD", error))?;
        self.store.boundary(DurableBoundary::HeadDataWritten)?;
        cancel.check()?;
        kernel_file(&temp.file, &self.store.options)?;
        self.store.boundary(DurableBoundary::HeadFileKernelSynced)?;
        cancel.check()?;
        submission.check_layout(self.store)?;
        // On macOS this full flush follows every closure/entry submission AND
        // the prepared selector data. No HEAD rename precedes this barrier.
        durable_file(&temp.file, &self.store.options)?;
        self.store.boundary(DurableBoundary::HeadFileSynced)?;
        cancel.check()?;
        submission.check_layout(self.store)?;
        check_head_bytes(&mut temp.file, &head)?;
        temp.check_entry()?;
        self.store
            .temporary
            .rename_replace(temp.name()?, &self.store.root, OsStr::new("HEAD"))
            .map_err(|error| io_error("publish HEAD", error))?;
        let receipt = PublishReceipt {
            previous: self.current,
            current: operation,
        };
        // Past this point, even cancellation or directory-sync failure cannot
        // truthfully be described as a prepublication failure.
        let persisted = (|| {
            self.store.boundary(DurableBoundary::HeadReplaced)?;
            submission.check_layout(self.store)?;
            check_head_binding(self.store, &temp.file, &head)?;
            submission.check_file(self.store.root.file())?;
            submission.check_file(self.store.temporary.file())?;
            kernel_file(self.store.root.file(), &self.store.options)?;
            // Persist both sides of the cross-directory rename.
            kernel_file(self.store.temporary.file(), &self.store.options)?;
            self.store
                .boundary(DurableBoundary::HeadDirectoriesKernelSynced)?;
            submission.check_layout(self.store)?;
            // A single strongest barrier completes both prior directory syncs
            // on the checked same device. Errors here still report visibility.
            durable_dir(&self.store.root, &self.store.options)?;
            self.store.boundary(DurableBoundary::HeadDirectorySynced)?;
            submission.check_layout(self.store)?;
            check_head_binding(self.store, &temp.file, &head)?;
            Ok(())
        })();
        match persisted {
            Ok(()) => {
                #[cfg(feature = "fault-injection")]
                if self.store.options.allow_volatile_for_tests {
                    return Ok(PublishOutcome::VisibleButUncertain {
                        receipt,
                        error: StoreError::VolatileTestMode,
                    });
                }
                Ok(PublishOutcome::Durable(receipt))
            }
            Err(error) => Ok(PublishOutcome::VisibleButUncertain { receipt, error }),
        }
    }
}

fn check_head_binding(store: &Store, prepared: &File, head: &[u8; HEAD_LEN]) -> Result<()> {
    let mut actual = store
        .root
        .open_read(OsStr::new("HEAD"))
        .map_err(|error| io_error("reopen published HEAD", error))?;
    if Identity::file(&actual)? != Identity::file(prepared)? {
        return Err(StoreError::Corrupt {
            kind: "HEAD entry",
            reason: "entry no longer names the prepared selector",
        });
    }
    check_head_bytes(&mut actual, head)?;
    check_file_entry(&store.root, OsStr::new("HEAD"), &actual)
}

fn check_head_bytes(file: &mut File, head: &[u8; HEAD_LEN]) -> Result<()> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io_error("rewind prepared HEAD", error))?;
    let mut bytes = [0_u8; HEAD_LEN];
    file.read_exact(&mut bytes)
        .map_err(|error| io_error("verify prepared HEAD", error))?;
    let mut extra = [0_u8; 1];
    if &bytes != head
        || file
            .read(&mut extra)
            .map_err(|error| io_error("verify prepared HEAD EOF", error))?
            != 0
    {
        return Err(StoreError::Corrupt {
            kind: "HEAD",
            reason: "selector differs from the proposed operation",
        });
    }
    Ok(())
}

fn encode_head(operation: OperationId) -> [u8; HEAD_LEN] {
    let mut bytes = [0_u8; HEAD_LEN];
    bytes[..8].copy_from_slice(HEAD_MAGIC);
    bytes[8..40].copy_from_slice(operation.object_id().as_bytes());
    let checksum = Sha256::digest(&bytes[..40]);
    bytes[40..].copy_from_slice(&checksum);
    bytes
}

pub(crate) fn read_head(store: &Store, cancel: &CancellationToken) -> Result<Option<OperationId>> {
    if let Some(head) = read_head_file(store, cancel)? {
        return Ok(Some(head));
    }
    // A lock-free open can report ENOENT during selector replacement on the
    // supported host-backed filesystem. Confirm absence while excluding that
    // replacement; other read/validation errors still propagate immediately.
    let _publication = store.acquire_head(HeadMode::Shared, cancel)?;
    read_head_file(store, cancel)
}

/// Reads one pinned selector. Callers must exclude publication before treating
/// `None` as confirmed absence: begin holds head.lock exclusively, and recovery
/// holds temporary.lock exclusively. Neither guarded path may relock head.lock.
pub(crate) fn read_head_file(
    store: &Store,
    cancel: &CancellationToken,
) -> Result<Option<OperationId>> {
    cancel.check()?;
    let mut file = match store.root.open_read(OsStr::new("HEAD")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error("open HEAD", error)),
    };
    let mut bytes = [0_u8; HEAD_LEN];
    file.read_exact(&mut bytes)
        .map_err(|error| io_error("read HEAD", error))?;
    let mut extra = [0_u8; 1];
    if file
        .read(&mut extra)
        .map_err(|error| io_error("read HEAD EOF", error))?
        != 0
    {
        return Err(StoreError::Corrupt {
            kind: "HEAD",
            reason: "trailing bytes",
        });
    }
    if &bytes[..8] != HEAD_MAGIC {
        return Err(StoreError::UnsupportedVersion { kind: "HEAD" });
    }
    let checksum = Sha256::digest(&bytes[..40]);
    if bytes[40..] != checksum[..] {
        return Err(StoreError::Corrupt {
            kind: "HEAD",
            reason: "checksum mismatch",
        });
    }
    let mut object = [0_u8; 32];
    object.copy_from_slice(&bytes[8..40]);
    let id = ObjectId::from_bytes(object);
    let payload = store.get(id, ObjectKind::Operation, cancel)?;
    let _: Operation = decode_metadata(&payload, &store.options.limits)?;
    Ok(Some(OperationId::from_object(id)))
}
