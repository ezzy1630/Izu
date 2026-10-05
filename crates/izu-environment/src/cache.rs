use crate::error::{check, io};
use crate::filesystem::*;
use crate::recipe::{ArtifactRequest, PersistedIdentity};
use crate::{
    CancellationToken, Digest, EnvironmentError, EnvironmentLimits, QuiescenceAcknowledgement,
    Recipe, Result,
};
use izu_platform::{Directory, sync_file};
use serde::{Deserialize, Serialize};
use std::ffi::OsStr;
use std::fs::{File, TryLockError};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

const ARTIFACT_VERSION: u32 = 1;

#[derive(Clone, Copy)]
enum LeaseMode {
    Shared,
    Exclusive,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ArtifactSummary {
    pub key: Digest,
    pub manifest_digest: Digest,
    pub entries: u64,
    pub logical_bytes: u64,
    pub cooperative_quiescence_only: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum CacheStatus {
    Missing { key: Digest },
    Building { key: Digest, retained_stages: u64 },
    InUse { key: Digest },
    Incomplete { key: Digest, retained_stages: u64 },
    Ready(ArtifactSummary),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RetentionPolicy {
    RetainAllArtifactsAndIncompleteStages,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ImportReceipt {
    pub artifact: ArtifactSummary,
    pub reused: bool,
    pub mode: MaterializationMode,
    pub cloned_files: u64,
    pub copied_files: u64,
    pub logical_bytes: u64,
    pub allocated_file_bytes_sum: u64,
    pub elapsed_milliseconds: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Manifest {
    pub version: u32,
    pub key: Digest,
    pub identity: PersistedIdentity,
    pub cooperative_quiescence_only: bool,
    pub entries: Vec<ManifestEntry>,
    pub logical_bytes: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadyMarker {
    version: u32,
    key: Digest,
    manifest_digest: Digest,
    manifest_identity: NodeIdentity,
}

pub(crate) struct VerifiedArtifact {
    pub _lease: File,
    directory: Directory,
    pub payload: Directory,
    pub manifest: Manifest,
    pub inventory: Vec<InventoryEntry>,
    pub summary: ArtifactSummary,
}

/// An optional native cache, separate from source history and default bundles.
/// Ready keys are never overwritten and nothing is automatically garbage-collected.
pub struct EnvironmentCache {
    pub(crate) path: PathBuf,
    root: Directory,
    pub(crate) artifacts: Directory,
    locks: Directory,
    pub(crate) limits: EnvironmentLimits,
}

impl EnvironmentCache {
    pub fn open(path: impl AsRef<Path>, limits: EnvironmentLimits) -> Result<Self> {
        limits.validate()?;
        izu_platform::require_durable_platform().map_err(|error| io(path.as_ref(), error))?;
        let root = ensure_cache_root(path.as_ref())?;
        let artifacts = root
            .ensure_dir(OsStr::new("artifacts"))
            .map_err(|error| io(path.as_ref(), error))?;
        artifacts
            .require_persistent_filesystem()
            .map_err(|error| io(path.as_ref().join("artifacts"), error))?;
        let locks = root
            .ensure_dir(OsStr::new("locks"))
            .map_err(|error| io(path.as_ref(), error))?;
        locks
            .require_persistent_filesystem()
            .map_err(|error| io(path.as_ref().join("locks"), error))?;
        root.sync().map_err(|error| io(path.as_ref(), error))?;
        Ok(Self {
            path: path.as_ref().to_owned(),
            root,
            artifacts,
            locks,
            limits,
        })
    }

    pub fn root_path(&self) -> &Path {
        &self.path
    }
    pub fn retention_policy(&self) -> RetentionPolicy {
        RetentionPolicy::RetainAllArtifactsAndIncompleteStages
    }

    pub fn status(&self, recipe: &Recipe, cancel: &CancellationToken) -> Result<CacheStatus> {
        check(cancel)?;
        self.verify_cache_binding()?;
        self.check_platform(recipe.request())?;
        let stages = self.stage_count(recipe.key(), cancel)?;
        let lease = self.lock_file(&recipe.key().to_string())?;
        match lease.try_lock_shared() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Ok(if stages > 0 {
                    CacheStatus::Building {
                        key: recipe.key(),
                        retained_stages: stages,
                    }
                } else {
                    CacheStatus::InUse { key: recipe.key() }
                });
            }
            Err(TryLockError::Error(error)) => return Err(io(&self.path, error)),
        }
        match self.verify_with_lease(recipe.request(), lease, cancel) {
            Ok(artifact) => Ok(CacheStatus::Ready(artifact.summary)),
            Err(EnvironmentError::Missing(_)) => Ok(if stages > 0 {
                CacheStatus::Incomplete {
                    key: recipe.key(),
                    retained_stages: stages,
                }
            } else {
                CacheStatus::Missing { key: recipe.key() }
            }),
            Err(error) => Err(error),
        }
    }

    pub fn import_quiescent(
        &self,
        recipe: &Recipe,
        prepared_root: impl AsRef<Path>,
        _: QuiescenceAcknowledgement,
        policy: SharingPolicy,
        cancel: &CancellationToken,
    ) -> Result<ImportReceipt> {
        let started = Instant::now();
        check(cancel)?;
        self.verify_cache_binding()?;
        self.check_platform(recipe.request())?;
        let lease = self.acquire_key(recipe.key(), cancel)?;
        match self.verify_with_lease(recipe.request(), lease, cancel) {
            Ok(artifact) => {
                return Ok(ImportReceipt {
                    artifact: artifact.summary,
                    reused: true,
                    mode: MaterializationMode::Empty,
                    cloned_files: 0,
                    copied_files: 0,
                    logical_bytes: 0,
                    allocated_file_bytes_sum: 0,
                    elapsed_milliseconds: elapsed_ms(started)?,
                });
            }
            Err(EnvironmentError::Missing(_)) => {}
            Err(error) => return Err(error),
        }
        // The verification function consumes its lease. Acquire it again and
        // recheck under the lock before building, preventing duplicate publishers.
        let lease = self.acquire_key(recipe.key(), cancel)?;
        if metadata_exists(&self.artifacts, OsStr::new(&recipe.key().to_string()))?.is_some() {
            let artifact = self.verify_with_lease(recipe.request(), lease, cancel)?;
            return Ok(ImportReceipt {
                artifact: artifact.summary,
                reused: true,
                mode: MaterializationMode::Empty,
                cloned_files: 0,
                copied_files: 0,
                logical_bytes: 0,
                allocated_file_bytes_sum: 0,
                elapsed_milliseconds: elapsed_ms(started)?,
            });
        }
        let key_lease = lease;
        let _admission = self.acquire_named("admission", cancel)?;
        let prepared = Directory::open(prepared_root.as_ref())
            .map_err(|error| io(prepared_root.as_ref(), error))?;
        verify_lockfiles(&prepared, recipe, &self.limits, cancel)?;
        let inventory = collect_source(&prepared, recipe, &self.limits, cancel)?;
        let incoming = logical_bytes(&inventory)?;
        self.admit(incoming, inventory.len(), cancel)?;
        let stage_name = unique_name(&format!(".build-{}-", recipe.key()))?;
        let stage_path = self.path.join("artifacts").join(&stage_name);
        let stage = self
            .artifacts
            .create_dir(&stage_name)
            .map_err(|error| io(&stage_path, error))?;
        self.artifacts
            .sync()
            .map_err(|error| io(&stage_path, error))?;
        let preparation = self.prepare(recipe, &prepared, &inventory, &stage, policy, cancel);
        let (summary, stats) = match preparation {
            Ok(result) => result,
            Err(source) => {
                return Err(EnvironmentError::PreparationIncomplete {
                    stage: stage_path,
                    source: Box::new(source),
                });
            }
        };
        check(cancel).map_err(|source| EnvironmentError::PreparationIncomplete {
            stage: stage_path.clone(),
            source: Box::new(source),
        })?;
        self.publish_prepared_stage(&stage_name, &stage, recipe.key())?;
        #[cfg(test)]
        tests::after_import_publication(self, recipe.key());
        let published = self.path.join("artifacts").join(recipe.key().to_string());
        let acknowledgement = (|| {
            // Publication is visible. Verify through the consumer path while the
            // same exclusive key lease remains held; refusal cannot imply rollback.
            let artifact = self.verify_with_lease(recipe.request(), key_lease, cancel)?;
            if artifact.summary != summary {
                return Err(EnvironmentError::Corrupt(
                    "published artifact summary differs from preparation".into(),
                ));
            }
            self.verify_artifact_binding(&artifact)?;
            Ok(ImportReceipt {
                artifact: summary,
                reused: false,
                mode: stats.mode(),
                cloned_files: stats.cloned_files,
                copied_files: stats.copied_files,
                logical_bytes: stats.logical_bytes,
                allocated_file_bytes_sum: stats.allocated_file_bytes_sum,
                elapsed_milliseconds: elapsed_ms(started)?,
            })
        })();
        acknowledgement.map_err(|source| EnvironmentError::PublicationIdentityUncertain {
            path: published,
            source: Box::new(source),
        })
    }

    fn publish_prepared_stage(
        &self,
        stage_name: &OsStr,
        stage: &Directory,
        key: Digest,
    ) -> Result<()> {
        let stage_path = self.path.join("artifacts").join(stage_name);
        self.verify_cache_binding()
            .and_then(|()| verify_directory_entry(&self.artifacts, stage_name, stage, &stage_path))
            .map_err(|source| EnvironmentError::PreparationIncomplete {
                stage: stage_path.clone(),
                source: Box::new(source),
            })?;
        self.artifacts
            .rename_noreplace(stage_name, &self.artifacts, OsStr::new(&key.to_string()))
            .map_err(|source| EnvironmentError::PreparationIncomplete {
                stage: stage_path.clone(),
                source: Box::new(io(&stage_path, source)),
            })?;
        let published = self.path.join("artifacts").join(key.to_string());
        self.verify_published_stage(stage, key)?;
        self.artifacts
            .sync()
            .map_err(|source| EnvironmentError::DurabilityUncertain {
                path: published.clone(),
                source,
            })?;
        self.verify_published_stage(stage, key)
    }

    fn verify_published_stage(&self, stage: &Directory, key: Digest) -> Result<()> {
        let published = self.path.join("artifacts").join(key.to_string());
        self.verify_cache_binding()
            .and_then(|()| {
                verify_directory_entry(
                    &self.artifacts,
                    OsStr::new(&key.to_string()),
                    stage,
                    &published,
                )
            })
            .map_err(|source| EnvironmentError::PublicationIdentityUncertain {
                path: published,
                source: Box::new(source),
            })
    }

    fn prepare(
        &self,
        recipe: &Recipe,
        prepared: &Directory,
        inventory: &[InventoryEntry],
        stage: &Directory,
        policy: SharingPolicy,
        cancel: &CancellationToken,
    ) -> Result<(ArtifactSummary, TransferStats)> {
        write_json(stage, "BUILDING", &recipe.key(), 4096)?;
        let payload = stage
            .create_dir(OsStr::new("payload"))
            .map_err(|error| io("payload", error))?;
        let mut entries = Vec::new();
        entries
            .try_reserve(inventory.len())
            .map_err(|_| EnvironmentError::Allocation("manifest"))?;
        let mut stats = TransferStats::default();
        for (index, path) in recipe.declared_paths().enumerate() {
            check(cancel)?;
            let root_index =
                u32::try_from(index).map_err(|_| EnvironmentError::Allocation("root index"))?;
            let root_entries = entries_for_root(inventory, root_index);
            let source = match open_directory(prepared, path) {
                Ok(directory) => Some(directory),
                Err(EnvironmentError::Io { source, .. })
                    if source.kind() == std::io::ErrorKind::NotFound
                        && recipe.spec().outputs.iter().any(|output| output == path) =>
                {
                    None
                }
                Err(error) => return Err(error),
            };
            let destination = payload
                .create_dir(OsStr::new(&index.to_string()))
                .map_err(|error| io(path, error))?;
            let (manifest, transferred) = transfer_root(
                source.as_ref(),
                &destination,
                root_entries,
                policy,
                true,
                &self.limits,
                cancel,
            )?;
            entries.extend(manifest);
            stats.add(&transferred)?;
        }
        // Metadata observations catch ordinary edits/replacements. They do not
        // prove the absence of writers; the explicit quiescence contract does.
        if collect_source(prepared, recipe, &self.limits, cancel)? != inventory {
            return Err(EnvironmentError::InputChanged(
                "prepared directory inventory".into(),
            ));
        }
        verify_lockfiles(prepared, recipe, &self.limits, cancel)?;
        set_mode(payload.file(), 0o555)?;
        payload.sync().map_err(|error| io("payload", error))?;
        let manifest = Manifest {
            version: ARTIFACT_VERSION,
            key: recipe.key(),
            identity: recipe.identity().clone(),
            cooperative_quiescence_only: true,
            entries,
            logical_bytes: stats.logical_bytes,
        };
        let (manifest_digest, manifest_identity) = write_json(
            stage,
            "manifest.json",
            &manifest,
            self.limits.max_manifest_bytes,
        )?;
        let marker = ReadyMarker {
            version: ARTIFACT_VERSION,
            key: recipe.key(),
            manifest_digest,
            manifest_identity,
        };
        write_json(stage, "READY", &marker, 4096)?;
        stage
            .remove_file(OsStr::new("BUILDING"))
            .map_err(|error| io("BUILDING", error))?;
        set_mode(stage.file(), 0o555)?;
        stage.sync().map_err(|error| io("artifact stage", error))?;
        let summary = ArtifactSummary {
            key: recipe.key(),
            manifest_digest,
            entries: manifest.entries.len() as u64,
            logical_bytes: manifest.logical_bytes,
            cooperative_quiescence_only: true,
        };
        Ok((summary, stats))
    }

    pub(crate) fn borrow_artifact(
        &self,
        recipe: &Recipe,
        cancel: &CancellationToken,
    ) -> Result<VerifiedArtifact> {
        self.borrow_request(recipe.request(), cancel)
    }

    pub(crate) fn borrow_request(
        &self,
        recipe: ArtifactRequest<'_>,
        cancel: &CancellationToken,
    ) -> Result<VerifiedArtifact> {
        self.verify_cache_binding()?;
        self.check_platform(recipe)?;
        let lease =
            self.acquire_named_mode(&recipe.key().to_string(), LeaseMode::Shared, cancel)?;
        self.verify_with_lease(recipe, lease, cancel)
    }

    fn verify_with_lease(
        &self,
        recipe: ArtifactRequest<'_>,
        lease: File,
        cancel: &CancellationToken,
    ) -> Result<VerifiedArtifact> {
        check(cancel)?;
        let name = recipe.key().to_string();
        let artifact = match self.artifacts.open_dir(OsStr::new(&name)) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(EnvironmentError::Missing(name));
            }
            Err(error) => return Err(io(self.path.join("artifacts").join(&name), error)),
        };
        if stamp(
            &artifact
                .metadata_self()
                .map_err(|error| io("artifact", error))?,
        )?
        .mode
            != 0o555
        {
            return Err(EnvironmentError::Corrupt(
                "artifact directory seal changed".into(),
            ));
        }
        let marker_file = artifact.open_read(OsStr::new("READY")).map_err(|error| {
            EnvironmentError::Corrupt(format!("READY marker unavailable: {error}"))
        })?;
        verify_metadata_seal(&marker_file, "READY")?;
        let marker: ReadyMarker =
            serde_json::from_slice(&read_bounded(marker_file, 4096, cancel)?)?;
        if marker.version != ARTIFACT_VERSION || marker.key != recipe.key() {
            return Err(EnvironmentError::Corrupt("wrong READY version/key".into()));
        }
        let manifest_file = artifact
            .open_read(OsStr::new("manifest.json"))
            .map_err(|error| io("manifest.json", error))?;
        verify_metadata_seal(&manifest_file, "manifest.json")?;
        let observed_manifest_identity = stamp(
            &manifest_file
                .metadata()
                .map_err(|error| io("manifest.json", error))?,
        )?
        .identity;
        if observed_manifest_identity != marker.manifest_identity {
            return Err(EnvironmentError::Corrupt(format!(
                "manifest inode/device identity differs: expected device={}, inode={}; observed device={}, inode={}",
                marker.manifest_identity.device,
                marker.manifest_identity.inode,
                observed_manifest_identity.device,
                observed_manifest_identity.inode,
            )));
        }
        let manifest_bytes = read_bounded(manifest_file, self.limits.max_manifest_bytes, cancel)?;
        if Digest::of_bytes(&manifest_bytes) != marker.manifest_digest {
            return Err(EnvironmentError::Corrupt("manifest digest mismatch".into()));
        }
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)?;
        if manifest.version != ARTIFACT_VERSION
            || manifest.key != recipe.key()
            || &manifest.identity != recipe.identity()
            || !manifest.cooperative_quiescence_only
        {
            return Err(EnvironmentError::Corrupt(
                "manifest identity does not match trusted recipe".into(),
            ));
        }
        if manifest.entries.len() > self.limits.max_entries
            || manifest.logical_bytes > self.limits.max_artifact_bytes
        {
            return Err(EnvironmentError::Limit {
                resource: "artifact manifest",
                limit: self.limits.max_artifact_bytes,
            });
        }
        let mut root_entries = 0;
        for entry in artifact.entries().map_err(|error| io("artifact", error))? {
            check(cancel)?;
            let entry = entry.map_err(|error| io("artifact", error))?;
            if !matches!(
                entry.name.to_str(),
                Some("payload" | "manifest.json" | "READY")
            ) {
                return Err(EnvironmentError::Corrupt("extra artifact entry".into()));
            }
            root_entries += 1;
        }
        if root_entries != 3 {
            return Err(EnvironmentError::Corrupt(
                "incomplete artifact entries".into(),
            ));
        }
        let payload = artifact
            .open_dir(OsStr::new("payload"))
            .map_err(|error| io("payload", error))?;
        if stamp(
            &payload
                .metadata_self()
                .map_err(|error| io("payload", error))?,
        )?
        .mode
            != 0o555
        {
            return Err(EnvironmentError::Corrupt(
                "payload directory seal changed".into(),
            ));
        }
        let mut inventory = collect_payload(
            &payload,
            recipe.declared_paths().count(),
            &self.limits,
            cancel,
        )?;
        if inventory.len() != manifest.entries.len() {
            return Err(EnvironmentError::Corrupt(
                "payload entry count differs".into(),
            ));
        }
        let mut logical = 0_u64;
        for (observed, record) in inventory.iter_mut().zip(&manifest.entries) {
            check(cancel)?;
            if observed.root != record.root
                || observed.path != record.path
                || observed.stamp.as_ref().map(|stamp| stamp.identity) != Some(record.identity)
            {
                return Err(EnvironmentError::Corrupt(format!(
                    "payload entry name/order or inode/device identity differs: {}",
                    record.path
                )));
            }
            let directory = payload
                .open_dir(OsStr::new(&record.root.to_string()))
                .map_err(|error| io("payload root", error))?;
            match (&mut observed.kind, &record.node) {
                (InventoryKind::Directory { mode }, ManifestKind::Directory { mode: original }) => {
                    if *mode != 0o555 || *original > 0o777 {
                        return Err(EnvironmentError::Corrupt(
                            "directory seal/mode mismatch".into(),
                        ));
                    }
                    *mode = *original;
                }
                (
                    InventoryKind::File { mode, bytes },
                    ManifestKind::File {
                        mode: original,
                        bytes: expected,
                        digest,
                    },
                ) => {
                    if bytes != expected
                        || *mode != (0o444 | (*original & 0o111))
                        || *original > 0o777
                    {
                        return Err(EnvironmentError::Corrupt(format!(
                            "file seal/size mismatch: {}",
                            record.path
                        )));
                    }
                    let mut file = open_regular(&directory, &record.path)?;
                    let before = stamp(&file.metadata().map_err(|error| io(&record.path, error))?)?;
                    if before.identity != record.identity {
                        return Err(EnvironmentError::Corrupt(
                            "file inode/device identity changed while opening".into(),
                        ));
                    }
                    #[cfg(unix)]
                    if file
                        .metadata()
                        .map_err(|error| io(&record.path, error))?
                        .nlink()
                        != 1
                    {
                        return Err(EnvironmentError::Corrupt(
                            "cache file has a writable hard-link alias".into(),
                        ));
                    }
                    let (actual_digest, actual_bytes) =
                        hash_file(&mut file, self.limits.max_file_bytes, cancel)?;
                    if actual_digest != *digest
                        || actual_bytes != *expected
                        || stamp(&file.metadata().map_err(|error| io(&record.path, error))?)?
                            != before
                    {
                        return Err(EnvironmentError::Corrupt(format!(
                            "file digest changed: {}",
                            record.path
                        )));
                    }
                    logical = logical
                        .checked_add(actual_bytes)
                        .ok_or(EnvironmentError::Limit {
                            resource: "artifact bytes",
                            limit: self.limits.max_artifact_bytes,
                        })?;
                    *mode = *original;
                }
                (InventoryKind::Symlink { target }, ManifestKind::Symlink { target: expected })
                    if target == expected => {}
                _ => {
                    return Err(EnvironmentError::Corrupt(format!(
                        "payload kind/target mismatch: {}",
                        record.path
                    )));
                }
            }
        }
        if logical != manifest.logical_bytes {
            return Err(EnvironmentError::Corrupt(
                "artifact byte count mismatch".into(),
            ));
        }
        let summary = ArtifactSummary {
            key: recipe.key(),
            manifest_digest: marker.manifest_digest,
            entries: manifest.entries.len() as u64,
            logical_bytes: logical,
            cooperative_quiescence_only: true,
        };
        self.verify_cache_binding()?;
        verify_directory_entry(
            &self.artifacts,
            OsStr::new(&name),
            &artifact,
            &self.path.join("artifacts").join(&name),
        )?;
        Ok(VerifiedArtifact {
            _lease: lease,
            directory: artifact,
            payload,
            manifest,
            inventory,
            summary,
        })
    }

    fn check_platform(&self, recipe: ArtifactRequest<'_>) -> Result<()> {
        if recipe.identity().platform.os != std::env::consts::OS
            || recipe.identity().platform.architecture != std::env::consts::ARCH
        {
            return Err(EnvironmentError::InvalidInput(
                "recipe platform differs from current OS/architecture".into(),
            ));
        }
        Ok(())
    }

    pub(crate) fn verify_cache_binding(&self) -> Result<()> {
        for (directory, path) in [
            (&self.root, self.path.clone()),
            (&self.artifacts, self.path.join("artifacts")),
            (&self.locks, self.path.join("locks")),
        ] {
            directory
                .require_persistent_filesystem()
                .map_err(|error| io(path, error))?;
        }
        verify_directory_locator(&self.path, &self.root)?;
        verify_directory_entry(
            &self.root,
            OsStr::new("artifacts"),
            &self.artifacts,
            &self.path.join("artifacts"),
        )?;
        verify_directory_entry(
            &self.root,
            OsStr::new("locks"),
            &self.locks,
            &self.path.join("locks"),
        )
    }

    pub(crate) fn verify_artifact_binding(&self, artifact: &VerifiedArtifact) -> Result<()> {
        self.verify_cache_binding()?;
        let locator = self
            .path
            .join("artifacts")
            .join(artifact.summary.key.to_string());
        verify_directory_entry(
            &self.artifacts,
            OsStr::new(&artifact.summary.key.to_string()),
            &artifact.directory,
            &locator,
        )?;
        verify_directory_entry(
            &artifact.directory,
            OsStr::new("payload"),
            &artifact.payload,
            &locator.join("payload"),
        )
    }

    pub(crate) fn check_disk(&self, directory: &Directory, incoming: u64) -> Result<()> {
        let required =
            incoming
                .checked_add(self.limits.min_free_bytes)
                .ok_or(EnvironmentError::Limit {
                    resource: "free disk reservation",
                    limit: u64::MAX,
                })?;
        if available_bytes(directory)? < required {
            return Err(EnvironmentError::Limit {
                resource: "available disk bytes",
                limit: required,
            });
        }
        Ok(())
    }

    fn admit(&self, incoming: u64, entries: usize, cancel: &CancellationToken) -> Result<()> {
        // Logical quota/reservations intentionally overestimate physical COW
        // storage. A failed clone must still have enough space for a full copy.
        let overhead = (entries as u64)
            .checked_mul(4608)
            .and_then(|n| n.checked_add(65536))
            .ok_or(EnvironmentError::Limit {
                resource: "manifest reservation",
                limit: u64::MAX,
            })?;
        let reservation = incoming
            .checked_add(overhead)
            .ok_or(EnvironmentError::Limit {
                resource: "cache bytes",
                limit: self.limits.max_cache_bytes,
            })?;
        let used = self.cache_logical_bytes(cancel)?;
        if used
            .checked_add(reservation)
            .is_none_or(|total| total > self.limits.max_cache_bytes)
        {
            return Err(EnvironmentError::Limit {
                resource: "cache logical byte quota",
                limit: self.limits.max_cache_bytes,
            });
        }
        self.check_disk(&self.artifacts, reservation)
    }

    fn cache_logical_bytes(&self, cancel: &CancellationToken) -> Result<u64> {
        fn sum(
            directory: &Directory,
            depth: usize,
            count: &mut usize,
            total: &mut u64,
            limits: &EnvironmentLimits,
            cancel: &CancellationToken,
        ) -> Result<()> {
            if depth > limits.max_depth + 4 {
                return Err(EnvironmentError::Limit {
                    resource: "cache traversal depth",
                    limit: limits.max_depth as u64,
                });
            }
            for entry in directory
                .entries()
                .map_err(|error| io("cache quota", error))?
            {
                check(cancel)?;
                *count = count
                    .checked_add(1)
                    .ok_or(EnvironmentError::Allocation("cache entry count"))?;
                if *count > limits.max_entries.saturating_mul(16) {
                    return Err(EnvironmentError::Limit {
                        resource: "cache entries",
                        limit: limits.max_entries.saturating_mul(16) as u64,
                    });
                }
                let entry = entry.map_err(|error| io("cache quota", error))?;
                let metadata = directory
                    .metadata(&entry.name)
                    .map_err(|error| io("cache quota", error))?;
                if metadata.is_dir() {
                    sum(
                        &directory
                            .open_dir(&entry.name)
                            .map_err(|error| io("cache quota", error))?,
                        depth + 1,
                        count,
                        total,
                        limits,
                        cancel,
                    )?;
                } else if metadata.is_file() {
                    *total = total
                        .checked_add(metadata.len())
                        .ok_or(EnvironmentError::Limit {
                            resource: "cache bytes",
                            limit: limits.max_cache_bytes,
                        })?;
                } else if !metadata.is_symlink() {
                    return Err(EnvironmentError::InvalidInput(
                        "special file in environment cache".into(),
                    ));
                }
            }
            Ok(())
        }
        let mut count = 0;
        let mut total = 0;
        sum(
            &self.artifacts,
            0,
            &mut count,
            &mut total,
            &self.limits,
            cancel,
        )?;
        Ok(total)
    }

    fn stage_count(&self, key: Digest, cancel: &CancellationToken) -> Result<u64> {
        let prefix = format!(".build-{key}-");
        let mut count = 0_u64;
        let mut visited = 0;
        for entry in self
            .artifacts
            .entries()
            .map_err(|error| io(&self.path, error))?
        {
            check(cancel)?;
            visited += 1;
            if visited > self.limits.max_entries {
                return Err(EnvironmentError::Limit {
                    resource: "artifact keys",
                    limit: self.limits.max_entries as u64,
                });
            }
            let entry = entry.map_err(|error| io(&self.path, error))?;
            if entry
                .name
                .to_str()
                .is_some_and(|name| name.starts_with(&prefix))
            {
                count += 1;
            }
        }
        Ok(count)
    }

    fn lock_file(&self, name: &str) -> Result<File> {
        let filename = format!("{name}.lock");
        let path = self.path.join("locks").join(&filename);
        let file = self
            .locks
            .open_lock(OsStr::new(&filename))
            .map_err(|source| EnvironmentError::LockIo {
                operation: "open",
                path: path.clone(),
                source,
            })?;
        sync_file(&file).map_err(|source| EnvironmentError::LockIo {
            operation: "sync-file",
            path: path.clone(),
            source,
        })?;
        self.locks
            .sync()
            .map_err(|source| EnvironmentError::LockIo {
                operation: "sync-directory",
                path: self.path.join("locks"),
                source,
            })?;
        Ok(file)
    }

    fn acquire_named(&self, name: &str, cancel: &CancellationToken) -> Result<File> {
        self.acquire_named_mode(name, LeaseMode::Exclusive, cancel)
    }

    fn acquire_named_mode(
        &self,
        name: &str,
        mode: LeaseMode,
        cancel: &CancellationToken,
    ) -> Result<File> {
        let file = self.lock_file(name)?;
        let started = Instant::now();
        loop {
            check(cancel)?;
            let attempt = match mode {
                LeaseMode::Shared => file.try_lock_shared(),
                LeaseMode::Exclusive => file.try_lock(),
            };
            match attempt {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => {
                    let remaining = self
                        .limits
                        .lock_timeout
                        .checked_sub(started.elapsed())
                        .ok_or(EnvironmentError::Busy)?;
                    if remaining.is_zero() {
                        return Err(EnvironmentError::Busy);
                    }
                    std::thread::sleep(remaining.min(std::time::Duration::from_millis(5)));
                }
                Err(TryLockError::Error(source)) => {
                    return Err(EnvironmentError::LockIo {
                        operation: match mode {
                            LeaseMode::Shared => "try-shared",
                            LeaseMode::Exclusive => "try-exclusive",
                        },
                        path: self.path.join("locks").join(format!("{name}.lock")),
                        source,
                    });
                }
            }
        }
    }
    fn acquire_key(&self, key: Digest, cancel: &CancellationToken) -> Result<File> {
        self.acquire_named(&key.to_string(), cancel)
    }
}

pub(crate) fn verify_lockfiles(
    directory: &Directory,
    recipe: &Recipe,
    limits: &EnvironmentLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    verify_identity_lockfiles(directory, recipe.identity(), limits, cancel)
}

pub(crate) fn verify_identity_lockfiles(
    directory: &Directory,
    identity: &PersistedIdentity,
    limits: &EnvironmentLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    for lockfile in &identity.lockfiles {
        check(cancel)?;
        let mut file = open_regular(directory, &lockfile.path)?;
        let before = stamp(&file.metadata().map_err(|error| io(&lockfile.path, error))?)?;
        let (digest, _) = hash_file(&mut file, limits.max_file_bytes, cancel)?;
        if digest != lockfile.digest
            || stamp(&file.metadata().map_err(|error| io(&lockfile.path, error))?)? != before
        {
            return Err(EnvironmentError::LockfileMismatch(lockfile.path.clone()));
        }
    }
    Ok(())
}

pub(crate) fn elapsed_ms(started: Instant) -> Result<u64> {
    u64::try_from(started.elapsed().as_millis()).map_err(|_| EnvironmentError::Limit {
        resource: "elapsed milliseconds",
        limit: u64::MAX,
    })
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
}

pub(crate) fn json_bytes<T: Serialize>(value: &T, limit: usize) -> Result<Vec<u8>> {
    let mut buffer = LimitedBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value)?;
    Ok(buffer.bytes)
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > self.limit)
        {
            return Err(std::io::Error::other(
                "JSON metadata exceeds configured byte limit",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|error| std::io::Error::other(error.to_string()))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn write_json(
    directory: &Directory,
    name: &str,
    value: &impl Serialize,
    limit: usize,
) -> Result<(Digest, NodeIdentity)> {
    let mut buffer = LimitedBuffer {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut buffer, value)?;
    let digest = Digest::of_bytes(&buffer.bytes);
    let mut file = directory
        .create_new_file(OsStr::new(name))
        .map_err(|error| io(name, error))?;
    file.write_all(&buffer.bytes)
        .map_err(|error| io(name, error))?;
    set_mode(&file, 0o444)?;
    sync_file(&file).map_err(|error| io(name, error))?;
    let identity = stamp(&file.metadata().map_err(|error| io(name, error))?)?.identity;
    directory.sync().map_err(|error| io(name, error))?;
    Ok((digest, identity))
}

fn read_bounded(mut file: File, limit: usize, cancel: &CancellationToken) -> Result<Vec<u8>> {
    let before = stamp(&file.metadata().map_err(|error| io("metadata", error))?)?;
    let length =
        usize::try_from(before.length).map_err(|_| EnvironmentError::Allocation("metadata"))?;
    if length > limit {
        return Err(EnvironmentError::Limit {
            resource: "manifest bytes",
            limit: limit as u64,
        });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve(length)
        .map_err(|_| EnvironmentError::Allocation("metadata"))?;
    let mut buffer = [0_u8; 65536];
    loop {
        check(cancel)?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| io("metadata", error))?;
        if count == 0 {
            break;
        }
        if bytes
            .len()
            .checked_add(count)
            .is_none_or(|size| size > limit)
        {
            return Err(EnvironmentError::Limit {
                resource: "manifest bytes",
                limit: limit as u64,
            });
        }
        bytes
            .try_reserve(count)
            .map_err(|_| EnvironmentError::Allocation("metadata"))?;
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() != length
        || stamp(&file.metadata().map_err(|error| io("metadata", error))?)? != before
    {
        return Err(EnvironmentError::Corrupt(
            "metadata changed during read".into(),
        ));
    }
    Ok(bytes)
}

fn verify_metadata_seal(file: &File, name: &str) -> Result<()> {
    let metadata = file.metadata().map_err(|error| io(name, error))?;
    if stamp(&metadata)?.mode != 0o444 {
        return Err(EnvironmentError::Corrupt(format!(
            "metadata seal changed: {name}"
        )));
    }
    #[cfg(unix)]
    if metadata.nlink() != 1 {
        return Err(EnvironmentError::Corrupt(format!(
            "metadata has a hard-link alias: {name}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PlatformIdentity, RecipeSpec, TrustAcknowledgement};
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    type ImportPublicationHook = Box<dyn FnOnce(&EnvironmentCache, Digest)>;

    std::thread_local! {
        static IMPORT_PUBLICATION_HOOK: std::cell::RefCell<Option<ImportPublicationHook>> =
            const { std::cell::RefCell::new(None) };
    }

    pub(super) fn after_import_publication(cache: &EnvironmentCache, key: Digest) {
        let hook = IMPORT_PUBLICATION_HOOK.with(|slot| slot.borrow_mut().take());
        if let Some(hook) = hook {
            hook(cache, key);
        }
    }

    #[cfg(unix)]
    mod fresh_import {
        use super::*;
        use crate::MaterializationMode;

        const ORIGINAL: &[u8] = b"prepared-original\n";
        const CHANGED: &[u8] = b"prepared-tampered\n";

        struct Fixture {
            temp: Option<tempfile::TempDir>,
            prepared: PathBuf,
            cache: EnvironmentCache,
            recipe: Recipe,
            cancel: CancellationToken,
        }

        impl Fixture {
            fn new() -> Self {
                let temp =
                    tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
                let prepared = temp.path().join("prepared");
                fs::create_dir_all(prepared.join("dependencies")).unwrap();
                fs::write(prepared.join("dependencies/file"), ORIGINAL).unwrap();
                fs::set_permissions(
                    prepared.join("dependencies/file"),
                    fs::Permissions::from_mode(0o644),
                )
                .unwrap();
                let recipe = Recipe::trusted(
                    RecipeSpec {
                        schema_version: 1,
                        source_identity: Digest::of_bytes(b"source"),
                        lockfiles: Vec::new(),
                        toolchain_identity: Digest::of_bytes(b"toolchain"),
                        platform: PlatformIdentity::current("test"),
                        recipe_identity: Digest::of_bytes(b"recipe"),
                        trust_domain: "publication-test".into(),
                        argv: vec!["never-executed".into()],
                        dependencies: vec!["dependencies".into()],
                        outputs: Vec::new(),
                    },
                    TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
                )
                .unwrap();
                let cache = EnvironmentCache::open(
                    temp.path().join("cache"),
                    EnvironmentLimits {
                        min_free_bytes: 0,
                        lock_timeout: std::time::Duration::ZERO,
                        ..EnvironmentLimits::default()
                    },
                )
                .unwrap();
                Self {
                    temp: Some(temp),
                    prepared,
                    cache,
                    recipe,
                    cancel: CancellationToken::new(),
                }
            }

            fn published(&self) -> PathBuf {
                self.cache
                    .path
                    .join("artifacts")
                    .join(self.recipe.key().to_string())
            }

            fn import(&self) -> Result<ImportReceipt> {
                self.cache.import_quiescent(
                    &self.recipe,
                    &self.prepared,
                    QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                    SharingPolicy::Copy,
                    &self.cancel,
                )
            }

            fn uncertainty(&self, result: Result<ImportReceipt>) -> EnvironmentError {
                match result.unwrap_err() {
                    EnvironmentError::PublicationIdentityUncertain { path, source } => {
                        assert_eq!(path, self.published());
                        assert!(path.join("READY").is_file());
                        assert_eq!(
                            fs::read(self.prepared.join("dependencies/file")).unwrap(),
                            ORIGINAL
                        );
                        *source
                    }
                    error => panic!("expected retained publication uncertainty, got {error:?}"),
                }
            }
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                let Some(temp) = self.temp.take() else {
                    return;
                };
                if std::thread::panicking() {
                    let retained = temp.keep();
                    eprintln!("retained failed import fixture at {}", retained.display());
                    return;
                }
                fn writable_directories(path: &Path) {
                    let metadata = fs::symlink_metadata(path).unwrap();
                    if !metadata.is_dir() {
                        return;
                    }
                    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
                    for entry in fs::read_dir(path).unwrap() {
                        writable_directories(&entry.unwrap().path());
                    }
                }
                let path = temp.path().to_owned();
                writable_directories(&path);
                temp.close().unwrap();
                assert_eq!(
                    fs::symlink_metadata(&path).unwrap_err().kind(),
                    std::io::ErrorKind::NotFound
                );
            }
        }

        struct HookGuard;

        impl Drop for HookGuard {
            fn drop(&mut self) {
                IMPORT_PUBLICATION_HOOK.with(|slot| {
                    drop(slot.borrow_mut().take());
                });
            }
        }

        fn on_publication(hook: impl FnOnce(&EnvironmentCache, Digest) + 'static) -> HookGuard {
            IMPORT_PUBLICATION_HOOK.with(|slot| {
                assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
            });
            HookGuard
        }

        fn rewrite_sealed(path: &Path, bytes: &[u8]) {
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
            fs::write(path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o444)).unwrap();
        }

        #[test]
        fn changed_manifest_before_acknowledgement_is_retained_and_refused() {
            let fixture = Fixture::new();
            let _hook = on_publication(|cache, key| {
                let artifact = cache.path.join("artifacts").join(key.to_string());
                rewrite_sealed(&artifact.join("manifest.json"), b"changed manifest\n");
            });
            assert!(matches!(
                fixture.uncertainty(fixture.import()),
                EnvironmentError::Corrupt(message) if message == "manifest digest mismatch"
            ));
            assert_eq!(
                fs::read(fixture.published().join("manifest.json")).unwrap(),
                b"changed manifest\n"
            );
            assert_eq!(
                fs::read(fixture.published().join("payload/0/file")).unwrap(),
                ORIGINAL
            );
        }

        #[test]
        fn changed_payload_before_acknowledgement_is_retained_and_refused() {
            let fixture = Fixture::new();
            let _hook = on_publication(|cache, key| {
                let artifact = cache.path.join("artifacts").join(key.to_string());
                rewrite_sealed(&artifact.join("payload/0/file"), CHANGED);
            });
            assert!(matches!(
                fixture.uncertainty(fixture.import()),
                EnvironmentError::Corrupt(message) if message.starts_with("file digest changed:")
            ));
            assert_eq!(
                fs::read(fixture.published().join("payload/0/file")).unwrap(),
                CHANGED
            );
            assert!(fixture.published().join("manifest.json").is_file());
        }

        #[test]
        fn same_content_replacements_before_acknowledgement_preserve_both_files() {
            for relative in ["manifest.json", "payload/0/file"] {
                let fixture = Fixture::new();
                let _hook = on_publication(move |cache, key| {
                    let artifact = cache.path.join("artifacts").join(key.to_string());
                    let file = artifact.join(relative);
                    let original = fs::read(&file).unwrap();
                    let identity = stamp(&fs::metadata(&file).unwrap()).unwrap().identity;
                    let parent = file.parent().unwrap();
                    fs::set_permissions(parent, fs::Permissions::from_mode(0o755)).unwrap();
                    fs::rename(&file, cache.path.join("retained-original-file")).unwrap();
                    fs::write(&file, &original).unwrap();
                    fs::set_permissions(&file, fs::Permissions::from_mode(0o444)).unwrap();
                    fs::set_permissions(parent, fs::Permissions::from_mode(0o555)).unwrap();
                    assert_ne!(
                        stamp(&fs::metadata(&file).unwrap()).unwrap().identity,
                        identity
                    );
                });
                assert!(matches!(
                    fixture.uncertainty(fixture.import()),
                    EnvironmentError::Corrupt(message) if message.contains("inode/device identity differs")
                ));
                assert_eq!(
                    fs::read(fixture.published().join(relative)).unwrap(),
                    fs::read(fixture.cache.path.join("retained-original-file")).unwrap()
                );
                assert_eq!(
                    fs::read(fixture.published().join("payload/0/file")).unwrap(),
                    ORIGINAL
                );
            }
        }

        #[test]
        fn self_consistent_manifest_rewrite_cannot_change_the_prepared_summary() {
            let fixture = Fixture::new();
            let _hook = on_publication(|cache, key| {
                let artifact = cache.path.join("artifacts").join(key.to_string());
                let path = artifact.join("manifest.json");
                let mut manifest: Manifest =
                    serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                let mut changed = false;
                for entry in &mut manifest.entries {
                    if let ManifestKind::File { mode, .. } = &mut entry.node {
                        *mode ^= 0o200;
                        changed = true;
                        break;
                    }
                }
                assert!(changed);
                let bytes = serde_json::to_vec(&manifest).unwrap();
                rewrite_sealed(&path, &bytes);
                let ready_path = artifact.join("READY");
                let mut marker: ReadyMarker =
                    serde_json::from_slice(&fs::read(&ready_path).unwrap()).unwrap();
                marker.manifest_digest = Digest::of_bytes(&bytes);
                rewrite_sealed(&ready_path, &serde_json::to_vec(&marker).unwrap());
            });
            assert!(matches!(
                fixture.uncertainty(fixture.import()),
                EnvironmentError::Corrupt(message) if message == "published artifact summary differs from preparation"
            ));
            // The rewritten artifact passes the ordinary reader. The import must
            // still acknowledge only the exact summary it originally prepared.
            assert!(
                fixture
                    .cache
                    .binding(&fixture.recipe, &fixture.cancel)
                    .is_ok()
            );
            assert_eq!(
                fs::read(fixture.published().join("payload/0/file")).unwrap(),
                ORIGINAL
            );
        }

        #[test]
        fn cancellation_after_publication_reports_uncertainty_and_retains_ready_data() {
            let fixture = Fixture::new();
            let cancel = fixture.cancel.clone();
            let _hook = on_publication(move |_, _| cancel.cancel());
            assert!(matches!(
                fixture.uncertainty(fixture.import()),
                EnvironmentError::Cancelled
            ));
            assert!(
                fixture
                    .cache
                    .binding(&fixture.recipe, &CancellationToken::new())
                    .is_ok()
            );
            assert_eq!(
                fs::read(fixture.published().join("payload/0/file")).unwrap(),
                ORIGINAL
            );
        }

        #[test]
        fn healthy_fresh_import_binds_and_reused_import_keeps_its_receipt_semantics() {
            let fixture = Fixture::new();
            let receipt = fixture.import().unwrap();
            assert!(!receipt.reused);
            assert_eq!(receipt.mode, MaterializationMode::Copy);
            assert_eq!(receipt.copied_files, 1);
            assert_eq!(receipt.artifact.logical_bytes, ORIGINAL.len() as u64);
            let binding = fixture
                .cache
                .binding(&fixture.recipe, &fixture.cancel)
                .unwrap();
            assert_eq!(binding.key(), receipt.artifact.key);
            assert_eq!(binding.manifest_digest(), receipt.artifact.manifest_digest);
            let _hook = on_publication(|_, _| panic!("reused import reached fresh publication"));
            let reused = fixture.import().unwrap();
            assert!(reused.reused);
            assert_eq!(reused.artifact, receipt.artifact);
            assert_eq!(reused.mode, MaterializationMode::Empty);
            assert_eq!(reused.copied_files, 0);
            assert_eq!(reused.cloned_files, 0);
            assert_eq!(reused.logical_bytes, 0);
            assert_eq!(reused.allocated_file_bytes_sum, 0);
        }
    }

    #[test]
    fn preparation_stage_substitution_is_refused_without_removing_original() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let cache = EnvironmentCache::open(temp.path().join("cache"), EnvironmentLimits::default())
            .unwrap();
        let name = OsStr::new(".build-owned");
        let moved = OsStr::new(".build-original-retained");
        let stage = cache.artifacts.create_dir(name).unwrap();
        fs::write(
            cache.path.join("artifacts/.build-owned/READY"),
            b"prepared original",
        )
        .unwrap();
        cache
            .artifacts
            .rename_noreplace(name, &cache.artifacts, moved)
            .unwrap();
        cache.artifacts.create_dir(name).unwrap();
        let key = Digest::of_bytes(b"stage-substitution-test");
        assert!(cache.publish_prepared_stage(name, &stage, key).is_err());
        assert!(!cache.path.join("artifacts").join(key.to_string()).exists());
        assert_eq!(
            fs::read(cache.path.join("artifacts/.build-original-retained/READY")).unwrap(),
            b"prepared original"
        );
        assert!(cache.path.join("artifacts/.build-owned").is_dir());
    }

    #[test]
    fn visible_stage_substitution_reports_uncertainty_and_preserves_both_directories() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let cache = EnvironmentCache::open(temp.path().join("cache"), EnvironmentLimits::default())
            .unwrap();
        let name = OsStr::new(".build-owned");
        let key = Digest::of_bytes(b"visible-stage-substitution");
        let stage = cache.artifacts.create_dir(name).unwrap();
        fs::write(
            cache.path.join("artifacts/.build-owned/READY"),
            b"prepared original",
        )
        .unwrap();
        cache.publish_prepared_stage(name, &stage, key).unwrap();
        cache
            .artifacts
            .rename_noreplace(
                OsStr::new(&key.to_string()),
                &cache.artifacts,
                OsStr::new("retained-original"),
            )
            .unwrap();
        cache
            .artifacts
            .create_dir(OsStr::new(&key.to_string()))
            .unwrap();
        assert!(matches!(
            cache.verify_published_stage(&stage, key),
            Err(EnvironmentError::PublicationIdentityUncertain { .. })
        ));
        assert_eq!(
            fs::read(cache.path.join("artifacts/retained-original/READY")).unwrap(),
            b"prepared original"
        );
        assert!(cache.path.join("artifacts").join(key.to_string()).is_dir());
    }

    #[test]
    fn replacing_cache_or_artifacts_locator_refuses_acknowledgement() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let cache = EnvironmentCache::open(temp.path().join("cache"), EnvironmentLimits::default())
            .unwrap();
        fs::rename(
            cache.path.join("artifacts"),
            cache.path.join("retained-artifacts"),
        )
        .unwrap();
        fs::create_dir(cache.path.join("artifacts")).unwrap();
        assert!(matches!(
            cache.verify_cache_binding(),
            Err(EnvironmentError::NamespaceChanged(_))
        ));
        let second =
            EnvironmentCache::open(temp.path().join("second"), EnvironmentLimits::default())
                .unwrap();
        fs::rename(&second.path, temp.path().join("retained-second")).unwrap();
        fs::create_dir(&second.path).unwrap();
        assert!(matches!(
            second.verify_cache_binding(),
            Err(EnvironmentError::NamespaceChanged(_))
        ));
        assert!(temp.path().join("retained-second/artifacts").is_dir());
    }

    #[test]
    fn immutable_artifact_has_concurrent_read_leases_and_excludes_writer() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let prepared = temp.path().join("prepared");
        fs::create_dir_all(prepared.join("dependencies")).unwrap();
        fs::write(
            prepared.join("dependencies/file"),
            b"immutable prepared bytes",
        )
        .unwrap();
        let recipe = Recipe::trusted(
            RecipeSpec {
                schema_version: 1,
                source_identity: Digest::of_bytes(b"source"),
                lockfiles: Vec::new(),
                toolchain_identity: Digest::of_bytes(b"toolchain"),
                platform: PlatformIdentity::current("test"),
                recipe_identity: Digest::of_bytes(b"recipe"),
                trust_domain: "lease-test".into(),
                argv: vec!["never-executed".into()],
                dependencies: vec!["dependencies".into()],
                outputs: Vec::new(),
            },
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
        )
        .unwrap();
        let cache = EnvironmentCache::open(
            temp.path().join("cache"),
            EnvironmentLimits {
                min_free_bytes: 0,
                lock_timeout: std::time::Duration::ZERO,
                ..EnvironmentLimits::default()
            },
        )
        .unwrap();
        let cancel = CancellationToken::new();
        cache
            .import_quiescent(
                &recipe,
                &prepared,
                QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                SharingPolicy::Copy,
                &cancel,
            )
            .unwrap();
        let first = cache.borrow_artifact(&recipe, &cancel).unwrap();
        let second = cache.borrow_artifact(&recipe, &cancel).unwrap();
        assert_eq!(first.summary, second.summary);
        assert!(matches!(
            cache.status(&recipe, &cancel).unwrap(),
            CacheStatus::Ready(_)
        ));
        assert!(matches!(
            cache.acquire_key(recipe.key(), &cancel),
            Err(EnvironmentError::Busy)
        ));
        cache
            .artifacts
            .rename_noreplace(
                OsStr::new(&recipe.key().to_string()),
                &cache.artifacts,
                OsStr::new("retained-original-artifact"),
            )
            .unwrap();
        cache
            .artifacts
            .create_dir(OsStr::new(&recipe.key().to_string()))
            .unwrap();
        assert!(matches!(
            cache.verify_artifact_binding(&first),
            Err(EnvironmentError::NamespaceChanged(_))
        ));
        drop(first);
        drop(second);
        drop(cache);
        fn writable(path: &Path) {
            let metadata = fs::symlink_metadata(path).unwrap();
            #[cfg(unix)]
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(if metadata.is_dir() { 0o700 } else { 0o600 }),
            )
            .unwrap();
            if metadata.is_dir() {
                for entry in fs::read_dir(path).unwrap() {
                    writable(&entry.unwrap().path());
                }
            }
        }
        writable(temp.path());
    }
}
