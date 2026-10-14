//! Exact-selection reclamation. Unknown and historic unique objects are retained.
use crate::{Result, Store, StoreError};
use izu_model::{
    CancellationToken, ObjectId, ObjectKind, ReferencedObjects, decode_object_metadata,
};
use std::collections::HashMap;

/// A verified selection, never inferred from age or lack of HEAD reachability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclamationPlan {
    pub selected: Vec<ObjectId>,
    pub payload_bytes: u64,
    pub retained_objects: u64,
}

impl Store {
    /// Must run under the exclusive retention lease through subsequent deletion.
    /// All finalized objects outside the exact selection remain retention roots.
    pub(crate) fn plan_selected_reclamation(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclamationPlan> {
        if selected.len() as u64 > self.options.max_scan_objects {
            return Err(StoreError::LimitExceeded("reclamation selection"));
        }
        let mut selection = HashMap::new();
        selection
            .try_reserve(selected.len())
            .map_err(|_| StoreError::Allocation)?;
        for id in selected {
            cancel.check()?;
            if selection.insert(*id, false).is_some() {
                return Err(StoreError::Corrupt {
                    kind: "reclamation selection",
                    reason: "duplicate object identity",
                });
            }
        }
        let head = crate::transaction::read_head_file(self, cancel)?;
        if head.is_some_and(|head| selection.contains_key(&head.object_id())) {
            return Err(StoreError::Corrupt {
                kind: "reclamation selection",
                reason: "selected object is the current HEAD",
            });
        }
        // Verifies names, kinds, frames, identities and canonical metadata before
        // making any mutation. A corrupt unreachable root still fails closed.
        let objects = self.list_objects(cancel)?;
        let mut payload_bytes = 0_u64;
        let mut retained_objects = 0_u64;
        let mut all = HashMap::new();
        all.try_reserve(objects.len())
            .map_err(|_| StoreError::Allocation)?;
        for object in &objects {
            all.insert(object.id, object.kind);
        }
        for object in &objects {
            cancel.check()?;
            let discarded = selection.contains_key(&object.id);
            if discarded && object.kind == ObjectKind::Operation {
                return Err(StoreError::Corrupt {
                    kind: "reclamation selection",
                    reason: "operation history is always retained",
                });
            }
            if let Some(found) = selection.get_mut(&object.id) {
                *found = true;
            }
            if discarded {
                payload_bytes = payload_bytes
                    .checked_add(object.payload_len)
                    .ok_or(StoreError::LimitExceeded("reclamation bytes"))?;
            } else {
                retained_objects = retained_objects
                    .checked_add(1)
                    .ok_or(StoreError::LimitExceeded("retained objects"))?;
            }
            if object.kind == ObjectKind::Blob {
                continue;
            }
            let payload = self.get(object.id, object.kind, cancel)?;
            let metadata = decode_object_metadata(object.kind, &payload, &self.options.limits)?;
            let mut result = Ok(());
            metadata.visit_references(&mut |reference| {
                if result.is_err() {
                    return;
                }
                result = match all.get(&reference.id) {
                    Some(actual) if *actual != reference.kind => Err(StoreError::UnexpectedKind {
                        id: reference.id,
                        expected: reference.kind,
                        actual: *actual,
                    }),
                    None => Err(StoreError::Corrupt {
                        kind: "retention graph",
                        reason: "referenced object is absent",
                    }),
                    _ if !discarded && selection.contains_key(&reference.id) => {
                        Err(StoreError::Corrupt {
                            kind: "reclamation selection",
                            reason: "retained object references selected object",
                        })
                    }
                    _ => Ok(()),
                };
            });
            result?;
        }
        if selection.values().any(|found| !found) {
            return Err(StoreError::Corrupt {
                kind: "reclamation selection",
                reason: "selected object is absent",
            });
        }
        let mut ids = Vec::new();
        ids.try_reserve_exact(selected.len())
            .map_err(|_| StoreError::Allocation)?;
        ids.extend_from_slice(selected);
        ids.sort_unstable();
        Ok(ReclamationPlan {
            selected: ids,
            payload_bytes,
            retained_objects,
        })
    }
}

use crate::durability::{ClosureSubmission, check_directory_entry, check_file_entry};
use crate::objects::{object_name, object_parts};
use crate::{DurableBoundary, durable_dir, durable_file, io_error};
use izu_platform::{Directory, EntryKind};
use std::ffi::OsStr;
use std::io;

const QUARANTINE: &str = "reclamation";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReclamationReceipt {
    /// Exact requested identities; inspect them even when completion is uncertain.
    pub selected: Vec<ObjectId>,
    pub payload_bytes: u64,
    pub retained_objects: u64,
}

#[derive(Debug)]
pub enum ReclaimOutcome {
    Durable(ReclamationReceipt),
    /// At least one canonical object entry was removed. Quarantine may retain
    /// recovery copies. Neither rollback nor durable completion is implied.
    VisibleButUncertain {
        receipt: ReclamationReceipt,
        error: StoreError,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuarantineRecoveryReport {
    pub restored_objects: u64,
    pub retained_unknown_entries: u64,
}

impl Store {
    /// Read-only exact-selection validation. The plan is advisory; reclaim
    /// always recomputes it while excluding retention leases and publications.
    fn plan_reclamation_locked(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclamationPlan> {
        if let Some(quarantine) = self.open_quarantine()? {
            let (objects, _) = self.scan_quarantine(&quarantine, cancel)?;
            if !objects.is_empty() {
                return Err(StoreError::Corrupt {
                    kind: "reclamation quarantine",
                    reason: "recover quarantine before planning",
                });
            }
        }
        self.plan_selected_reclamation(selected, cancel)
    }

    /// Explicitly discards only the exact selected, incoming-reference-free set.
    /// No object is selected automatically. A complete retention lease must cover
    /// callers' reads and preparation through publication or abandonment.
    fn reclaim_selected_locked(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclaimOutcome> {
        self.recover_reclamation_locked(cancel)?;
        let plan = self.plan_selected_reclamation(selected, cancel)?;
        let receipt = ReclamationReceipt {
            selected: plan.selected,
            payload_bytes: plan.payload_bytes,
            retained_objects: plan.retained_objects,
        };
        if receipt.selected.is_empty() {
            return Ok(ReclaimOutcome::Durable(receipt));
        }
        cancel.check()?;
        let layout = ClosureSubmission::new(self)?;
        let quarantine = match self.open_quarantine()? {
            Some(directory) => directory,
            None => {
                let directory = self
                    .root
                    .create_dir(OsStr::new(QUARANTINE))
                    .map_err(|error| io_error("create reclamation quarantine", error))?;
                durable_dir(&directory, &self.options)?;
                durable_dir(&self.root, &self.options)?;
                directory
            }
        };
        layout.check_file(quarantine.file())?;
        // Every selected object gets a durable second name before the first
        // canonical name is removed. On interruption these copies are restored.
        for id in &receipt.selected {
            cancel.check()?;
            layout.check_layout(self)?;
            check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
            let (file, shard) = self.open_object_with_shard(*id)?;
            let name = object_name(*id);
            let (_, suffix) = object_parts(&name)?;
            let text = std::str::from_utf8(&name)
                .map_err(|_| StoreError::InvalidPath("object identity is not ASCII"))?;
            check_file_entry(&shard, suffix, &file)?;
            shard
                .hard_link(suffix, &quarantine, OsStr::new(text))
                .map_err(|error| io_error("quarantine selected object", error))?;
            check_file_entry(&quarantine, OsStr::new(text), &file)?;
            durable_file(&file, &self.options)?;
        }
        durable_dir(&quarantine, &self.options)?;
        self.boundary(DurableBoundary::ReclamationQuarantineSynced)?;
        layout.check_layout(self)?;
        check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
        cancel.check()?;
        // Ignore cancellation after first visible removal until persistence has
        // been attempted. Any later failure carries the exact selection receipt.
        let mut visible = false;
        let result = (|| {
            for id in &receipt.selected {
                if !visible {
                    cancel.check()?;
                }
                layout.check_layout(self)?;
                check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
                let (file, shard) = self.open_object_with_shard(*id)?;
                let name = object_name(*id);
                let (_, suffix) = object_parts(&name)?;
                let text = std::str::from_utf8(&name)
                    .map_err(|_| StoreError::InvalidPath("object identity is not ASCII"))?;
                check_file_entry(&quarantine, OsStr::new(text), &file)?;
                check_file_entry(&shard, suffix, &file)?;
                shard
                    .remove_file(suffix)
                    .map_err(|error| io_error("remove selected object entry", error))?;
                visible = true;
                self.boundary(DurableBoundary::ReclamationObjectRemoved)?;
                // Persist source removal while the durable quarantine survives.
                durable_dir(&shard, &self.options)?;
            }
            layout.check_layout(self)?;
            check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
            self.boundary(DurableBoundary::ReclamationObjectsSynced)?;
            for id in &receipt.selected {
                let name = id.to_string();
                let file = quarantine
                    .open_read(OsStr::new(&name))
                    .map_err(|error| io_error("open quarantine before purge", error))?;
                check_file_entry(&quarantine, OsStr::new(&name), &file)?;
                quarantine
                    .remove_file(OsStr::new(&name))
                    .map_err(|error| io_error("purge selected quarantine entry", error))?;
            }
            durable_dir(&quarantine, &self.options)?;
            self.boundary(DurableBoundary::ReclamationQuarantinePurged)?;
            layout.check_layout(self)?;
            check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
            #[cfg(feature = "fault-injection")]
            if self.options.allow_volatile_for_tests {
                return Err(StoreError::VolatileTestMode);
            }
            Ok(())
        })();
        match result {
            Ok(()) => Ok(ReclaimOutcome::Durable(receipt)),
            Err(error) if visible => Ok(ReclaimOutcome::VisibleButUncertain { receipt, error }),
            Err(error) => Err(error),
        }
    }

    fn open_quarantine(&self) -> Result<Option<Directory>> {
        match self.root.open_dir(OsStr::new(QUARANTINE)) {
            Ok(directory) => Ok(Some(directory)),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(io_error("open reclamation quarantine", error)),
        }
    }

    fn scan_quarantine(
        &self,
        quarantine: &Directory,
        cancel: &CancellationToken,
    ) -> Result<(Vec<ObjectId>, u64)> {
        check_directory_entry(&self.root, OsStr::new(QUARANTINE), quarantine)?;
        let mut recognized = Vec::new();
        let mut unknown = 0_u64;
        let mut visited = 0_u64;
        let mut bytes = 0_u64;
        for entry in quarantine
            .entries()
            .map_err(|error| io_error("list reclamation quarantine", error))?
        {
            cancel.check()?;
            visited = visited
                .checked_add(1)
                .ok_or(StoreError::LimitExceeded("quarantine entries"))?;
            if visited > self.options.max_scan_objects {
                return Err(StoreError::LimitExceeded("quarantine entries"));
            }
            let entry = entry.map_err(|error| io_error("read quarantine entry", error))?;
            let id = entry
                .name
                .to_str()
                .and_then(|name| name.parse::<ObjectId>().ok());
            let Some(id) = id.filter(|_| entry.kind == EntryKind::File) else {
                unknown = unknown
                    .checked_add(1)
                    .ok_or(StoreError::LimitExceeded("unknown quarantine entries"))?;
                continue;
            };
            let mut file = quarantine
                .open_read(&entry.name)
                .map_err(|error| io_error("open quarantined object", error))?;
            let header = self.read_header(&mut file, id, None)?;
            bytes = bytes
                .checked_add(header.payload_len)
                .ok_or(StoreError::LimitExceeded("quarantine bytes"))?;
            if bytes > self.options.max_scan_bytes {
                return Err(StoreError::LimitExceeded("quarantine bytes"));
            }
            if header.kind == ObjectKind::Blob {
                self.verify_file(&mut file, id, None, cancel)?;
            } else {
                let kind = header.kind;
                let payload = self.read_payload(&mut file, id, header, cancel)?;
                decode_object_metadata(kind, &payload, &self.options.limits)?;
            }
            check_file_entry(quarantine, &entry.name, &file)?;
            recognized
                .try_reserve(1)
                .map_err(|_| StoreError::Allocation)?;
            recognized.push(id);
        }
        Ok((recognized, unknown))
    }

    pub(crate) fn recover_reclamation_locked(
        &self,
        cancel: &CancellationToken,
    ) -> Result<QuarantineRecoveryReport> {
        let Some(quarantine) = self.open_quarantine()? else {
            return Ok(QuarantineRecoveryReport {
                restored_objects: 0,
                retained_unknown_entries: 0,
            });
        };
        // Verify every recognized object before changing any namespace entry.
        let (objects, retained_unknown_entries) = self.scan_quarantine(&quarantine, cancel)?;
        let layout = ClosureSubmission::new(self)?;
        layout.check_file(quarantine.file())?;
        let mut restored_objects = 0_u64;
        for id in objects {
            cancel.check()?;
            layout.check_layout(self)?;
            check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
            let name = object_name(id);
            let (prefix, suffix) = object_parts(&name)?;
            let full = id.to_string();
            let source = quarantine
                .open_read(OsStr::new(&full))
                .map_err(|error| io_error("open recovery object", error))?;
            check_file_entry(&quarantine, OsStr::new(&full), &source)?;
            let shard = match self.objects.open_dir(prefix) {
                Ok(shard) => shard,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let shard = self
                        .objects
                        .create_dir(prefix)
                        .map_err(|error| io_error("create recovered object shard", error))?;
                    durable_dir(&shard, &self.options)?;
                    durable_dir(&self.objects, &self.options)?;
                    shard
                }
                Err(error) => return Err(io_error("open recovered object shard", error)),
            };
            check_directory_entry(&self.objects, prefix, &shard)?;
            match quarantine.hard_link(OsStr::new(&full), &shard, suffix) {
                Ok(()) => {
                    check_file_entry(&shard, suffix, &source)?;
                    restored_objects = restored_objects
                        .checked_add(1)
                        .ok_or(StoreError::LimitExceeded("restored objects"))?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    self.object_info(id, cancel)?;
                }
                Err(error) => return Err(io_error("restore quarantined object", error)),
            }
            durable_file(&source, &self.options)?;
            durable_dir(&shard, &self.options)?;
            check_directory_entry(&self.objects, prefix, &shard)?;
            check_file_entry(&quarantine, OsStr::new(&full), &source)?;
            quarantine
                .remove_file(OsStr::new(&full))
                .map_err(|error| io_error("remove restored quarantine entry", error))?;
            durable_dir(&quarantine, &self.options)?;
        }
        layout.check_layout(self)?;
        check_directory_entry(&self.root, OsStr::new(QUARANTINE), &quarantine)?;
        Ok(QuarantineRecoveryReport {
            restored_objects,
            retained_unknown_entries,
        })
    }
}

/// Excludes readers, preparations and publication through policy checks and mutation.
#[derive(Debug)]
pub struct ExclusiveReclamationGuard<'a> {
    store: &'a Store,
    _lease: std::fs::File,
}
impl ExclusiveReclamationGuard<'_> {
    pub fn plan_selected(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclamationPlan> {
        self.store.plan_reclamation_locked(selected, cancel)
    }
    pub fn reclaim_selected(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclaimOutcome> {
        self.store.reclaim_selected_locked(selected, cancel)
    }
    pub fn recover(&self, cancel: &CancellationToken) -> Result<QuarantineRecoveryReport> {
        self.store.recover_reclamation_locked(cancel)
    }
}
impl Store {
    pub fn exclusive_reclamation(
        &self,
        cancel: &CancellationToken,
    ) -> Result<ExclusiveReclamationGuard<'_>> {
        Ok(ExclusiveReclamationGuard {
            store: self,
            _lease: self.acquire_lock("temporary.lock", false, cancel)?,
        })
    }
    pub fn plan_reclamation(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclamationPlan> {
        self.exclusive_reclamation(cancel)?
            .plan_selected(selected, cancel)
    }
    pub fn reclaim_selected(
        &self,
        selected: &[ObjectId],
        cancel: &CancellationToken,
    ) -> Result<ReclaimOutcome> {
        self.exclusive_reclamation(cancel)?
            .reclaim_selected(selected, cancel)
    }
    pub fn recover_reclamation(
        &self,
        cancel: &CancellationToken,
    ) -> Result<QuarantineRecoveryReport> {
        self.exclusive_reclamation(cancel)?.recover(cancel)
    }
}
