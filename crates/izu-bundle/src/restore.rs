use crate::codec::verify_reader;
use crate::error::io;
use crate::files::{
    destination_error, open_input, open_parent, require_absent, split_target, temporary_name,
    validate_path,
};
use crate::owned::Identity;
use crate::{BundleError, BundleOptions, DurableBoundary, Publication, Result, VerificationReport};
use izu_engine::{Repository, RepositoryOptions, RestoreReceipt};
use izu_model::{CancellationToken, WorkspaceId};
use izu_platform::Directory;
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

#[derive(Default)]
pub struct RestoreOptions {
    pub bundle: BundleOptions,
    pub repository: RepositoryOptions,
    /// Required for an archive whose root contains more than one workspace.
    pub workspace: Option<WorkspaceId>,
}

#[derive(Clone, Debug, Serialize)]
pub struct RestorationReceipt {
    pub destination: PathBuf,
    pub publication: Publication,
    pub archive: VerificationReport,
    pub recovery: RestoreReceipt,
}

/// Cold restoration into a new destination. All objects and the complete graph
/// are verified before initializing an owned sibling staging repository. The
/// shared engine publishes the recovered history and remaps its active location;
/// this crate only publishes the finished directory without replacing a target.
pub fn restore(
    input: &Path,
    destination: &Path,
    options: &RestoreOptions,
    cancel: &CancellationToken,
) -> Result<RestorationReceipt> {
    options.bundle.validate()?;
    cancel.check()?;
    validate_path(destination, &options.bundle)?;
    izu_platform::require_durable_platform()
        .map_err(|error| io("require durable restore platform", error))?;
    let mut file = open_input(input, &options.bundle)?;
    let verified = verify_reader(&mut file, &options.bundle, cancel)?;
    let operation = verified
        .graph
        .root_operation
        .as_ref()
        .ok_or(BundleError::Invalid("verified root operation is absent"))?;
    let selected = match options.workspace {
        Some(id) if operation.view.workspaces.contains_key(&id) => id,
        Some(_) => return Err(BundleError::UnknownWorkspace),
        None if operation.view.workspaces.len() == 1 => *operation
            .view
            .workspaces
            .keys()
            .next()
            .ok_or(BundleError::WorkspaceSelectionRequired)?,
        None => return Err(BundleError::WorkspaceSelectionRequired),
    };
    let (parent_path, target) = split_target(destination)?;
    let parent = open_parent(parent_path, "open restore parent")?;
    parent
        .require_persistent_filesystem()
        .map_err(|error| io("require persistent restore filesystem", error))?;
    let parent_identity = Identity::directory(&parent)
        .map_err(|error| io("inspect restore parent identity", error))?;
    require_absent(&parent, target)?;
    // Canonicalize an already nofollow-opened parent to make the *new* local
    // locator absolute. No historical locator supplies a destination path.
    let absolute_parent = parent_path
        .canonicalize()
        .map_err(|error| io("resolve restore parent", error))?;
    let final_path = absolute_parent.join(target);
    parent_identity
        .require_directory_path(&absolute_parent)
        .map_err(|error| io("verify canonical restore parent identity", error))?;
    if final_path.to_str().is_none() {
        return Err(BundleError::InvalidDestination(
            "workspace locator is not UTF-8",
        ));
    }
    let (stage_name, stage) = new_stage(&parent)?;
    let staging_path = absolute_parent.join(&stage_name);
    let mut guard = StageGuard {
        parent: &parent,
        name: &stage_name,
        directory: &stage,
        active: true,
    };
    let mut recovery_operation = None;
    let staged = (|| -> Result<(RestoreReceipt, Identity)> {
        options
            .bundle
            .boundary(DurableBoundary::RestoreStageCreated)?;
        cancel.check()?;
        let repository_options = RepositoryOptions {
            store: options.repository.store.clone(),
            source_limits: options.repository.source_limits.clone(),
        };
        let mut repository =
            Repository::initialize_archive_at(&stage, &staging_path, repository_options)?;
        for id in verified.graph.sorted_ids()? {
            cancel.check()?;
            let node = verified
                .graph
                .nodes
                .get(&id)
                .ok_or(BundleError::Invalid("verified index lost an object"))?;
            file.seek(SeekFrom::Start(node.payload_offset))
                .map_err(|error| io("seek verified object", error))?;
            let mut payload = (&mut file).take(node.payload_len);
            let imported =
                repository.import_object(node.kind, id, &mut payload, node.payload_len, cancel)?;
            if imported != id {
                return Err(izu_model::ModelError::HashMismatch.into());
            }
        }
        options
            .bundle
            .boundary(DurableBoundary::RestoreObjectsImported)?;
        cancel.check()?;
        let recovery = repository.adopt_archive_at(
            verified.report.manifest.root_operation,
            selected,
            &stage,
            &final_path,
            cancel,
        )?;
        recovery_operation = Some(recovery.operation);
        options.bundle.boundary(DurableBoundary::RestoreStaged)?;
        cancel.check()?;
        parent_identity
            .require_directory_path(&absolute_parent)
            .map_err(|error| io("verify restore parent location", error))?;
        require_stage(&parent, &stage_name, &stage)?;
        let identity = Identity::directory(&stage)
            .map_err(|error| io("inspect owned restore identity", error))?;
        // Import and engine materialization persist their own files and directory
        // entries. Only the owned root and its new parent publication remain.
        stage
            .require_persistent_filesystem()
            .map_err(|error| io("require persistent restore staging root", error))?;
        stage
            .sync()
            .map_err(|error| io("persist complete restore staging root", error))?;
        options
            .bundle
            .boundary(DurableBoundary::RestoreBeforePublication)?;
        cancel.check()?;
        parent_identity
            .require_directory_path(&absolute_parent)
            .map_err(|error| io("verify restore publication parent location", error))?;
        require_stage(&parent, &stage_name, &stage)?;
        parent
            .rename_noreplace(&stage_name, &parent, target)
            .map_err(destination_error)?;
        Ok((recovery, identity))
    })();
    let (recovery, identity) = match staged {
        Ok(recovery) => recovery,
        Err(failure) => {
            guard.active = false;
            if recovery_operation.is_none()
                && let BundleError::Engine(error) = &failure
            {
                recovery_operation = error
                    .uncertain_operation()
                    .or_else(|| error.recovery_operation());
            }
            if let Err(cleanup) = parent_identity.require_directory_path(&absolute_parent) {
                return Err(BundleError::StagingCleanup {
                    stage: staging_path,
                    failure: Box::new(failure),
                    cleanup: format!("{cleanup}; original stage preserved through pinned parent"),
                });
            }
            match remove_empty_stage(&parent, &stage_name, &stage) {
                Ok(true) => return Err(failure),
                Ok(false) => {
                    return Err(BundleError::StagingRetained {
                        stage: staging_path,
                        recovery_operation,
                        failure: Box::new(failure),
                    });
                }
                Err(cleanup) => {
                    return Err(BundleError::StagingCleanup {
                        stage: staging_path,
                        failure: Box::new(failure),
                        cleanup: cleanup.to_string(),
                    });
                }
            }
        }
    };
    guard.active = false;
    if let Err(error) = identity.require_name(&parent, target) {
        return Err(BundleError::PublicationUncertain {
            destination: final_path,
            reason: error.to_string(),
        });
    }
    let durability = (|| -> Result<()> {
        options.bundle.boundary(DurableBoundary::RestorePublished)?;
        parent_identity
            .require_directory_path(&absolute_parent)
            .map_err(|error| io("verify visible restore parent location", error))?;
        identity
            .require_name(&parent, target)
            .map_err(|error| io("verify visible restore identity", error))?;
        stage
            .require_persistent_filesystem()
            .map_err(|error| io("require persistent visible restore", error))?;
        parent
            .require_persistent_filesystem()
            .map_err(|error| io("require persistent restore directory", error))?;
        parent
            .sync()
            .map_err(|error| io("persist restored repository parent", error))?;
        options
            .bundle
            .boundary(DurableBoundary::RestoreParentSynced)?;
        parent_identity
            .require_directory_path(&absolute_parent)
            .map_err(|error| io("verify durable restore parent location", error))?;
        identity
            .require_name(&parent, target)
            .map_err(|error| io("verify durable restore identity", error))?;
        Ok(())
    })();
    Ok(RestorationReceipt {
        destination: final_path,
        archive: verified.report,
        recovery,
        publication: match durability {
            Ok(()) => Publication::Durable,
            Err(error) => Publication::VisibleButUncertain {
                reason: error.to_string(),
            },
        },
    })
}

fn new_stage(parent: &Directory) -> Result<(OsString, Directory)> {
    for _ in 0..64 {
        let name = temporary_name("restore")?;
        match parent.create_dir(&name) {
            Ok(directory) => return Ok((name, directory)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io("create private restore staging directory", error)),
        }
    }
    Err(BundleError::Limit("staging directory attempts"))
}

fn require_stage(parent: &Directory, name: &OsStr, directory: &Directory) -> Result<()> {
    let identity = Identity::directory(directory)
        .map_err(|error| io("inspect owned restore identity", error))?;
    match identity.matches_name(parent, name) {
        Ok(true) => Ok(()),
        Ok(false) => Err(BundleError::StagingChanged),
        Err(error) => Err(io("verify restore staging identity", error)),
    }
}

fn remove_empty_stage(
    parent: &Directory,
    name: &OsStr,
    directory: &Directory,
) -> std::io::Result<bool> {
    let identity = Identity::directory(directory)?;
    identity.require_name(parent, name)?;
    if directory.entries()?.next().transpose()?.is_some() {
        return Ok(false);
    }
    identity.require_name(parent, name)?;
    parent.remove_dir(name)?;
    Ok(true)
}

struct StageGuard<'a> {
    parent: &'a Directory,
    name: &'a OsStr,
    directory: &'a Directory,
    active: bool,
}

impl Drop for StageGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = remove_empty_stage(self.parent, self.name, self.directory);
        }
    }
}
