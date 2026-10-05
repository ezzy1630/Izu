use crate::cache::{elapsed_ms, verify_identity_lockfiles};
use crate::error::{check, io};
use crate::filesystem::*;
use crate::recipe::ArtifactRequest;
use crate::{CancellationToken, Digest, EnvironmentCache, EnvironmentError, Recipe, Result};
use izu_engine::{Repository, Selection, WorkspaceExpectation, WorkspaceGuard};
use izu_model::WorkspaceId;
use izu_platform::Directory;
use serde::{Deserialize, Serialize};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Constructed only by the shared engine's exact managed-workspace guard. The
/// live-writer lease and pinned descriptor remain held for this value's lifetime.
pub struct WorkspaceTarget<'a> {
    guard: WorkspaceGuard<'a>,
    source_identity: Digest,
}

impl<'a> WorkspaceTarget<'a> {
    pub fn checked(
        repository: &'a Repository,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check(cancel)?;
        let guard = repository.lock_workspace(id, expected, cancel)?;
        if guard.is_primary() {
            return Err(EnvironmentError::InvalidInput("environment materialization requires a private managed workspace, not the primary source workspace".into()));
        }
        let captured = guard.capture(Selection::All, cancel)?;
        let source_identity = Digest::from_hex(&captured.tree.to_string())?;
        Ok(Self {
            guard,
            source_identity,
        })
    }
    pub fn workspace_id(&self) -> WorkspaceId {
        self.guard.state().id
    }
    pub fn root_path(&self) -> &Path {
        self.guard.root_path()
    }
    pub fn source_identity(&self) -> Digest {
        self.source_identity
    }
    pub fn expected(&self) -> WorkspaceExpectation {
        self.guard.state().expected
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct MaterializationReport {
    pub key: Digest,
    pub manifest_digest: Digest,
    pub workspace: WorkspaceId,
    pub published_paths: Vec<String>,
    pub mode: MaterializationMode,
    pub cloned_files: u64,
    pub copied_files: u64,
    pub symlinks: u64,
    pub logical_bytes: u64,
    /// Per-file allocation sum, which double counts shared COW extents.
    pub allocated_file_bytes_sum: u64,
    pub elapsed_milliseconds: u64,
    pub private_file_inodes: bool,
    pub security_boundary: bool,
    pub retained_stage: Option<PathBuf>,
}

struct DestinationPlan {
    path: String,
    existing_empty: Option<NodeIdentity>,
}
struct PreparedDestination {
    directory: Directory,
    index: usize,
}
struct PublishedDestination {
    path: String,
    parent: Directory,
    name: OsString,
    directory: Directory,
}

impl EnvironmentCache {
    pub fn materialize(
        &self,
        recipe: &Recipe,
        target: &mut WorkspaceTarget<'_>,
        policy: SharingPolicy,
        cancel: &CancellationToken,
    ) -> Result<MaterializationReport> {
        self.materialize_request(recipe.request(), None, target, policy, cancel)
    }

    pub(crate) fn materialize_request(
        &self,
        recipe: ArtifactRequest<'_>,
        expected_manifest: Option<Digest>,
        target: &mut WorkspaceTarget<'_>,
        policy: SharingPolicy,
        cancel: &CancellationToken,
    ) -> Result<MaterializationReport> {
        let started = Instant::now();
        check(cancel)?;
        if recipe.identity().source_identity != target.source_identity {
            return Err(EnvironmentError::SourceMismatch);
        }
        target
            .guard
            .directory()
            .require_persistent_filesystem()
            .map_err(|error| io(target.root_path(), error))?;
        verify_identity_lockfiles(
            target.guard.directory(),
            recipe.identity(),
            &self.limits,
            cancel,
        )?;
        verify_root_binding(target)?;
        let artifact = self.borrow_request(recipe, cancel)?;
        if expected_manifest.is_some_and(|digest| digest != artifact.summary.manifest_digest) {
            return Err(EnvironmentError::Corrupt(
                "binding manifest digest differs from verified artifact".into(),
            ));
        }
        self.check_disk(target.guard.directory(), artifact.summary.logical_bytes)?;
        let mut plans = Vec::new();
        plans
            .try_reserve(recipe.declared_paths().count())
            .map_err(|_| EnvironmentError::Allocation("destination plans"))?;
        for path in recipe.declared_paths() {
            check(cancel)?;
            plans.push(DestinationPlan {
                path: path.into(),
                existing_empty: inspect_destination(target.guard.directory(), path)?,
            });
        }
        // The protected recovery namespace is shared with engine recovery. The
        // environment subdirectory is distinct and never recursively deleted.
        let recovery = target
            .guard
            .directory()
            .ensure_dir(OsStr::new(".izu-recovery"))
            .map_err(|error| io(target.root_path(), error))?;
        recovery
            .require_persistent_filesystem()
            .map_err(|error| io(target.root_path().join(".izu-recovery"), error))?;
        target
            .guard
            .directory()
            .sync()
            .map_err(|error| io(target.root_path(), error))?;
        let staging_parent = recovery
            .ensure_dir(OsStr::new("environment-staging"))
            .map_err(|error| io(target.root_path(), error))?;
        staging_parent
            .require_persistent_filesystem()
            .map_err(|error| {
                io(
                    target.root_path().join(".izu-recovery/environment-staging"),
                    error,
                )
            })?;
        recovery
            .sync()
            .map_err(|error| io(target.root_path(), error))?;
        let stage_name = unique_name(&format!("materialize-{}-", recipe.key()))?;
        let stage_path = target
            .root_path()
            .join(".izu-recovery")
            .join("environment-staging")
            .join(&stage_name);
        let stage = staging_parent
            .create_dir(&stage_name)
            .map_err(|error| io(&stage_path, error))?;
        staging_parent
            .sync()
            .map_err(|error| io(&stage_path, error))?;
        let mut published = Vec::new();
        published
            .try_reserve(plans.len())
            .map_err(|_| EnvironmentError::Allocation("published paths"))?;
        let operation = (|| -> Result<(TransferStats, Vec<PublishedDestination>)> {
            let mut stats = TransferStats::default();
            let mut destinations = Vec::new();
            destinations
                .try_reserve(plans.len())
                .map_err(|_| EnvironmentError::Allocation("published destinations"))?;
            let mut prepared = Vec::new();
            prepared
                .try_reserve(plans.len())
                .map_err(|_| EnvironmentError::Allocation("prepared destinations"))?;
            for (index, plan) in plans.iter().enumerate() {
                check(cancel)?;
                let root =
                    u32::try_from(index).map_err(|_| EnvironmentError::Allocation("root index"))?;
                let input = artifact
                    .payload
                    .open_dir(OsStr::new(&index.to_string()))
                    .map_err(|error| io(&plan.path, error))?;
                let output = stage
                    .create_dir(OsStr::new(&index.to_string()))
                    .map_err(|error| io(&stage_path, error))?;
                let (materialized, transferred) = transfer_root(
                    Some(&input),
                    &output,
                    entries_for_root(&artifact.inventory, root),
                    policy,
                    false,
                    &self.limits,
                    cancel,
                )?;
                let expected = artifact
                    .manifest
                    .entries
                    .iter()
                    .filter(|entry| entry.root == root);
                if materialized.iter().zip(expected).any(|(actual, expected)| {
                    actual.path != expected.path || actual.node != expected.node
                }) {
                    return Err(EnvironmentError::Corrupt(
                        "cache changed during materialization".into(),
                    ));
                }
                stats.add(&transferred)?;
                prepared.push(PreparedDestination {
                    directory: output,
                    index,
                });
            }
            stage.sync().map_err(|error| io(&stage_path, error))?;
            // Staging lives in the protected namespace, so a fresh source capture
            // remains meaningful and detects edits by non-cooperative editors.
            let captured = target.guard.capture(Selection::All, cancel)?;
            if Digest::from_hex(&captured.tree.to_string())? != target.source_identity {
                return Err(EnvironmentError::SourceMismatch);
            }
            verify_identity_lockfiles(
                target.guard.directory(),
                recipe.identity(),
                &self.limits,
                cancel,
            )?;
            verify_root_binding(target)?;
            self.verify_artifact_binding(&artifact)?;
            verify_staging_binding(
                target,
                &recovery,
                &staging_parent,
                &stage_name,
                &stage,
                &stage_path,
            )?;
            for (plan, prepared) in plans.iter().zip(prepared) {
                check(cancel)?;
                verify_staging_binding(
                    target,
                    &recovery,
                    &staging_parent,
                    &stage_name,
                    &stage,
                    &stage_path,
                )?;
                verify_directory_entry(
                    &stage,
                    OsStr::new(&prepared.index.to_string()),
                    &prepared.directory,
                    &stage_path.join(prepared.index.to_string()),
                )?;
                let (parent, name) = open_parent(target.guard.directory(), &plan.path, true)?;
                let parent_path = plan.path.rsplit_once('/').map_or("", |(parent, _)| parent);
                verify_directory_locator(&target.root_path().join(parent_path), &parent)?;
                match (plan.existing_empty, metadata_exists(&parent, &name)?) {
                    (None, None) => {}
                    (Some(expected), Some(metadata))
                        if metadata.is_dir() && stamp(&metadata)?.identity == expected =>
                    {
                        let directory = parent
                            .open_dir(&name)
                            .map_err(|error| io(&plan.path, error))?;
                        if !directory_empty(&directory)? {
                            return Err(EnvironmentError::DestinationNotEmpty(plan.path.clone()));
                        }
                        // Only an empty directory can be removed. Any new content
                        // makes the kernel refuse rather than losing a file.
                        parent
                            .remove_dir(&name)
                            .map_err(|error| io(&plan.path, error))?;
                    }
                    _ => return Err(EnvironmentError::DestinationNotEmpty(plan.path.clone())),
                }
                stage
                    .rename_noreplace(OsStr::new(&prepared.index.to_string()), &parent, &name)
                    .map_err(|error| io(&plan.path, error))?;
                published.push(plan.path.clone());
                parent
                    .sync()
                    .map_err(|source| EnvironmentError::DurabilityUncertain {
                        path: target.root_path().join(&plan.path),
                        source,
                    })?;
                stage
                    .sync()
                    .map_err(|source| EnvironmentError::DurabilityUncertain {
                        path: stage_path.clone(),
                        source,
                    })?;
                let destination = PublishedDestination {
                    path: plan.path.clone(),
                    parent,
                    name,
                    directory: prepared.directory,
                };
                verify_published_destination(&destination, target.root_path())?;
                destinations.push(destination);
            }
            verify_root_binding(target)?;
            self.verify_artifact_binding(&artifact)?;
            verify_staging_binding(
                target,
                &recovery,
                &staging_parent,
                &stage_name,
                &stage,
                &stage_path,
            )?;
            Ok((stats, destinations))
        })();
        let (stats, destinations) = match operation {
            Ok(stats) => stats,
            Err(source) => {
                return Err(EnvironmentError::MaterializationIncomplete {
                    published,
                    stages: vec![stage_path],
                    source: Box::new(source),
                });
            }
        };
        let retained_stage =
            match cleanup_empty_stage(&staging_parent, &stage_name, &stage, &stage_path) {
                Ok(retained) => retained,
                Err(source) => {
                    return Err(EnvironmentError::MaterializationIncomplete {
                        published,
                        stages: vec![stage_path],
                        source: Box::new(source),
                    });
                }
            };
        // Cleanup includes a persistence barrier. Recheck every retained locator
        // after it, including nested parents, before acknowledging the operation.
        let acknowledgement = (|| -> Result<()> {
            verify_root_binding(target)?;
            self.verify_artifact_binding(&artifact)?;
            verify_staging_parents(target, &recovery, &staging_parent)?;
            if retained_stage.is_some() {
                verify_directory_entry(&staging_parent, &stage_name, &stage, &stage_path)?;
            }
            for destination in &destinations {
                verify_published_destination(destination, target.root_path())?;
            }
            Ok(())
        })();
        if let Err(source) = acknowledgement {
            return Err(EnvironmentError::MaterializationIncomplete {
                published,
                stages: retained_stage.into_iter().collect(),
                source: Box::new(source),
            });
        }
        Ok(MaterializationReport {
            key: recipe.key(),
            manifest_digest: artifact.summary.manifest_digest,
            workspace: target.workspace_id(),
            published_paths: published,
            mode: stats.mode(),
            cloned_files: stats.cloned_files,
            copied_files: stats.copied_files,
            symlinks: stats.symlinks,
            logical_bytes: stats.logical_bytes,
            allocated_file_bytes_sum: stats.allocated_file_bytes_sum,
            elapsed_milliseconds: elapsed_ms(started)?,
            private_file_inodes: true,
            security_boundary: false,
            retained_stage,
        })
    }
}

fn verify_published_destination(
    destination: &PublishedDestination,
    root_path: &Path,
) -> Result<()> {
    let path = root_path.join(&destination.path);
    let parent_path = destination
        .path
        .rsplit_once('/')
        .map_or("", |(parent, _)| parent);
    verify_directory_locator(&root_path.join(parent_path), &destination.parent)
        .and_then(|()| {
            verify_directory_entry(
                &destination.parent,
                &destination.name,
                &destination.directory,
                &path,
            )
        })
        .map_err(|source| EnvironmentError::PublicationIdentityUncertain {
            path,
            source: Box::new(source),
        })
}

fn cleanup_empty_stage(
    parent: &Directory,
    name: &OsStr,
    stage: &Directory,
    path: &Path,
) -> Result<Option<PathBuf>> {
    let parent_path = path
        .parent()
        .ok_or_else(|| EnvironmentError::InvalidInput("staging locator has no parent".into()))?;
    verify_directory_locator(parent_path, parent)?;
    verify_directory_entry(parent, name, stage, path)?;
    if !directory_empty(stage)? {
        return Ok(Some(path.to_owned()));
    }
    match parent.remove_dir(name) {
        Ok(()) => {
            parent
                .sync()
                .map_err(|source| EnvironmentError::DurabilityUncertain {
                    path: path.to_owned(),
                    source,
                })?;
            verify_directory_locator(parent_path, parent)?;
            Ok(None)
        }
        Err(error) if error.kind() == std::io::ErrorKind::DirectoryNotEmpty => {
            Ok(Some(path.to_owned()))
        }
        Err(source) => Err(io(path, source)),
    }
}

fn verify_staging_binding(
    target: &WorkspaceTarget<'_>,
    recovery: &Directory,
    parent: &Directory,
    name: &OsStr,
    stage: &Directory,
    stage_path: &Path,
) -> Result<()> {
    verify_staging_parents(target, recovery, parent)?;
    verify_directory_entry(parent, name, stage, stage_path)
}

fn verify_staging_parents(
    target: &WorkspaceTarget<'_>,
    recovery: &Directory,
    parent: &Directory,
) -> Result<()> {
    for directory in [target.guard.directory(), recovery, parent] {
        directory
            .require_persistent_filesystem()
            .map_err(|error| io(target.root_path(), error))?;
    }
    let recovery_path = target.root_path().join(".izu-recovery");
    verify_directory_entry(
        target.guard.directory(),
        OsStr::new(".izu-recovery"),
        recovery,
        &recovery_path,
    )?;
    verify_directory_entry(
        recovery,
        OsStr::new("environment-staging"),
        parent,
        &recovery_path.join("environment-staging"),
    )
}

fn verify_root_binding(target: &WorkspaceTarget<'_>) -> Result<()> {
    let current =
        Directory::open(target.root_path()).map_err(|error| io(target.root_path(), error))?;
    let current = stamp(
        &current
            .metadata_self()
            .map_err(|error| io(target.root_path(), error))?,
    )?
    .identity;
    let pinned = stamp(
        &target
            .guard
            .directory()
            .metadata_self()
            .map_err(|error| io(target.root_path(), error))?,
    )?
    .identity;
    if current != pinned {
        return Err(EnvironmentError::InputChanged(
            "managed workspace root was replaced".into(),
        ));
    }
    Ok(())
}

fn inspect_destination(root: &Directory, path: &str) -> Result<Option<NodeIdentity>> {
    let mut components = path.split('/').peekable();
    let mut directory = duplicate_directory(root)?;
    while let Some(component) = components.next() {
        let name = OsStr::new(component);
        let Some(metadata) = metadata_exists(&directory, name)? else {
            return Ok(None);
        };
        if !metadata.is_dir() {
            return Err(EnvironmentError::DestinationNotEmpty(path.into()));
        }
        let next = directory.open_dir(name).map_err(|error| io(path, error))?;
        if components.peek().is_none() {
            if !directory_empty(&next)? {
                return Err(EnvironmentError::DestinationNotEmpty(path.into()));
            }
            return Ok(Some(
                stamp(&next.metadata_self().map_err(|error| io(path, error))?)?.identity,
            ));
        }
        directory = next;
    }
    Err(EnvironmentError::InvalidInput(
        "empty destination path".into(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn nested_parent_substitution_refuses_ack_and_preserves_original() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let root = Directory::open(temp.path()).unwrap();
        let parent = root.create_dir(OsStr::new("vendor")).unwrap();
        let installed = parent.create_dir(OsStr::new("deps")).unwrap();
        fs::write(temp.path().join("vendor/deps/original"), b"unique original").unwrap();
        let destination = PublishedDestination {
            path: "vendor/deps".into(),
            parent,
            name: "deps".into(),
            directory: installed,
        };
        root.rename_noreplace(OsStr::new("vendor"), &root, OsStr::new("retained-vendor"))
            .unwrap();
        root.create_dir(OsStr::new("vendor")).unwrap();
        assert!(matches!(
            verify_published_destination(&destination, temp.path()),
            Err(EnvironmentError::PublicationIdentityUncertain { .. })
        ));
        assert_eq!(
            fs::read(temp.path().join("retained-vendor/deps/original")).unwrap(),
            b"unique original"
        );
        assert!(temp.path().join("vendor").is_dir());
        assert!(!temp.path().join("vendor/deps").exists());
    }

    #[test]
    fn substituted_empty_stage_is_preserved_during_cleanup() {
        let temp = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
        let parent = Directory::open(temp.path()).unwrap();
        let name = OsStr::new("stage");
        let stage = parent.create_dir(name).unwrap();
        parent
            .rename_noreplace(name, &parent, OsStr::new("retained-original"))
            .unwrap();
        parent.create_dir(name).unwrap();
        assert!(matches!(
            cleanup_empty_stage(&parent, name, &stage, &temp.path().join(name)),
            Err(EnvironmentError::NamespaceChanged(_))
        ));
        assert!(temp.path().join("stage").is_dir());
        assert!(temp.path().join("retained-original").is_dir());
    }
}
