use crate::codec::{inspect_reader, manifest, verify_reader, write_bundle};
use crate::error::io;
use crate::graph;
use crate::owned::Identity;
use crate::{
    BundleError, BundleOptions, DurableBoundary, Inspection, ObjectSource, Result,
    VerificationReport,
};
use izu_model::{CancellationToken, OperationId};
use izu_platform::{Directory, require_persistent_file, sync_file};
use serde::Serialize;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// An artifact can be visible before its directory entry is known durable. This
/// distinction is preserved in machine-readable receipts, including fault tests.
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Publication {
    Durable,
    VisibleButUncertain { reason: String },
}

#[derive(Clone, Debug, Serialize)]
pub struct BundleReceipt {
    pub destination: PathBuf,
    pub publication: Publication,
    pub verification: VerificationReport,
}

/// Creates a new private artifact without replacing any user file. Its source
/// is an already captured immutable root; this function does not capture edits.
pub fn create<S: ObjectSource + ?Sized>(
    source: &S,
    root: OperationId,
    output: &Path,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<BundleReceipt> {
    options.validate()?;
    cancel.check()?;
    validate_path(output, options)?;
    izu_platform::require_durable_platform()
        .map_err(|error| io("require durable artifact platform", error))?;
    let graph = graph::collect(source, root, options, cancel)?;
    let manifest = manifest(root, &graph)?;
    let (parent_path, name) = split_target(output)?;
    let parent = open_parent(parent_path, "open artifact parent")?;
    parent
        .require_persistent_filesystem()
        .map_err(|error| io("require persistent artifact filesystem", error))?;
    let parent_identity = Identity::directory(&parent)
        .map_err(|error| io("inspect artifact parent identity", error))?;
    require_absent(&parent, name)?;
    let (temporary, file) = new_temp_file(&parent)?;
    let mut guard = FileGuard {
        parent: &parent,
        name: &temporary,
        file,
        active: true,
    };
    let prepared = (|| -> Result<(VerificationReport, Identity)> {
        options.boundary(DurableBoundary::OutputCreated)?;
        let verification =
            write_bundle(source, &graph, &manifest, &mut guard.file, options, cancel)?;
        options.boundary(DurableBoundary::DataWritten)?;
        require_persistent_file(&guard.file)
            .map_err(|error| io("require persistent artifact file", error))?;
        sync_file(&guard.file).map_err(|error| io("persist artifact file", error))?;
        options.boundary(DurableBoundary::FileSynced)?;
        cancel.check()?;
        options.boundary(DurableBoundary::BeforePublication)?;
        cancel.check()?;
        parent_identity
            .require_directory_path(parent_path)
            .map_err(|error| io("verify artifact parent location", error))?;
        guard.require_current()?;
        let identity = Identity::file(&guard.file)
            .map_err(|error| io("inspect owned artifact identity", error))?;
        Ok((verification, identity))
    })();
    let (verification, identity) = match prepared {
        Ok(report) => report,
        Err(failure) => return Err(guard.failure(failure, parent_path.join(&temporary))),
    };
    if let Err(failure) = parent
        .hard_link(&temporary, &parent, name)
        .map_err(destination_error)
    {
        return Err(guard.failure(failure, parent_path.join(&temporary)));
    }
    if let Err(error) = identity.require_name(&parent, name) {
        guard.active = false;
        return Err(BundleError::PublicationUncertain {
            destination: output.to_path_buf(),
            reason: error.to_string(),
        });
    }
    let durability = (|| -> Result<()> {
        options.boundary(DurableBoundary::Published)?;
        parent_identity
            .require_directory_path(parent_path)
            .map_err(|error| io("verify visible artifact parent location", error))?;
        identity
            .require_name(&parent, name)
            .map_err(|error| io("verify visible artifact identity", error))?;
        guard
            .cleanup()
            .map_err(|error| io("remove owned artifact staging file", error))?;
        require_persistent_file(&guard.file)
            .map_err(|error| io("require persistent visible artifact", error))?;
        parent
            .require_persistent_filesystem()
            .map_err(|error| io("require persistent artifact directory", error))?;
        parent
            .sync()
            .map_err(|error| io("persist artifact directory", error))?;
        options.boundary(DurableBoundary::ParentSynced)?;
        parent_identity
            .require_directory_path(parent_path)
            .map_err(|error| io("verify durable artifact parent location", error))?;
        identity
            .require_name(&parent, name)
            .map_err(|error| io("verify durable artifact identity", error))?;
        Ok(())
    })();
    drop(guard);
    Ok(BundleReceipt {
        destination: output.to_path_buf(),
        verification,
        publication: match durability {
            Ok(()) => Publication::Durable,
            Err(error) => Publication::VisibleButUncertain {
                reason: error.to_string(),
            },
        },
    })
}

/// Reads the bounded manifest only. An inspection is explicitly distinct from
/// hash, trailer, schema, and transitive-closure verification.
pub fn inspect(
    input: &Path,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<Inspection> {
    options.validate()?;
    cancel.check()?;
    inspect_reader(open_input(input, options)?, options, cancel)
}

pub fn verify(
    input: &Path,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<VerificationReport> {
    options.validate()?;
    cancel.check()?;
    Ok(verify_reader(open_input(input, options)?, options, cancel)?.report)
}

pub(crate) fn open_input(input: &Path, options: &BundleOptions) -> Result<File> {
    validate_path(input, options)?;
    let (parent_path, name) = split_target(input)?;
    let parent = open_parent(parent_path, "open bundle parent")?;
    let file = parent
        .open_read(name)
        .map_err(|error| io("open bundle input", error))?;
    if file
        .metadata()
        .map_err(|error| io("inspect bundle length", error))?
        .len()
        > options.max_total_bytes
    {
        return Err(BundleError::Limit("bundle bytes"));
    }
    Ok(file)
}

pub(crate) fn open_parent(path: &Path, action: &'static str) -> Result<Directory> {
    Directory::open(path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotADirectory {
            BundleError::InvalidDestination("parent must be a real directory without symlink components; use its canonical path")
        } else {
            io(action, error)
        }
    })
}

pub(crate) fn validate_path(path: &Path, options: &BundleOptions) -> Result<()> {
    if path.as_os_str().as_encoded_bytes().len() > options.model_limits.max_path_bytes {
        return Err(BundleError::Limit("path bytes"));
    }
    Ok(())
}

pub(crate) fn split_target(path: &Path) -> Result<(&Path, &OsStr)> {
    let name = path.file_name().ok_or(BundleError::InvalidDestination(
        "needs a final ordinary filename",
    ))?;
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    Ok((parent, name))
}

pub(crate) fn require_absent(parent: &Directory, name: &OsStr) -> Result<()> {
    match parent.metadata(name) {
        Ok(_) => Err(BundleError::DestinationExists),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io("inspect destination", error)),
    }
}

pub(crate) fn destination_error(error: std::io::Error) -> BundleError {
    if error.kind() == std::io::ErrorKind::AlreadyExists {
        BundleError::DestinationExists
    } else {
        io("publish new destination", error)
    }
}

fn new_temp_file(parent: &Directory) -> Result<(OsString, File)> {
    for _ in 0..64 {
        let name = temporary_name("bundle")?;
        match parent.create_new_file(&name) {
            Ok(file) => return Ok((name, file)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(io("create private artifact staging file", error)),
        }
    }
    Err(BundleError::Limit("staging filename attempts"))
}

pub(crate) fn temporary_name(purpose: &str) -> Result<OsString> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let serial = COUNTER
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
            value.checked_add(1)
        })
        .map_err(|_| BundleError::Limit("staging filename counter"))?;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    Ok(OsString::from(format!(
        ".izu-{purpose}-{}-{serial}-{nanos}",
        std::process::id()
    )))
}

struct FileGuard<'a> {
    parent: &'a Directory,
    name: &'a OsStr,
    file: File,
    active: bool,
}

impl FileGuard<'_> {
    fn require_current(&self) -> Result<()> {
        let identity = Identity::file(&self.file)
            .map_err(|error| io("inspect owned artifact identity", error))?;
        match identity.matches_name(self.parent, self.name) {
            Ok(true) => Ok(()),
            Ok(false) => Err(BundleError::StagingChanged),
            Err(error) => Err(io("verify artifact staging identity", error)),
        }
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        self.active = false;
        Identity::file(&self.file)?.require_name(self.parent, self.name)?;
        self.parent.remove_file(self.name)
    }

    fn failure(&mut self, failure: BundleError, stage: PathBuf) -> BundleError {
        match self.cleanup() {
            Ok(()) => failure,
            Err(error) => BundleError::StagingCleanup {
                stage,
                failure: Box::new(failure),
                cleanup: error.to_string(),
            },
        }
    }
}

impl Drop for FileGuard<'_> {
    fn drop(&mut self) {
        if self.active {
            let _ = self.cleanup();
        }
    }
}
