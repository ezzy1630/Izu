#![forbid(unsafe_code)]
//! Original immutable object storage with atomic, durable operation publication.
//!
//! An operation is acknowledged only after both its objects and HEAD's directory
//! entry pass the platform persistence barrier. After HEAD becomes visible, a
//! failure is explicitly `VisibleButUncertain`; it is never reported as rollback.

#[cfg(all(feature = "fault-injection", not(debug_assertions)))]
compile_error!("izu-store fault injection is restricted to development builds");

mod admission;
mod batch;
mod durability;
mod objects;
mod transaction;
mod verify;

pub use batch::{ObjectBatch, ObjectBatchReceipt, StagedObjectId};
pub use transaction::{HeadExpectation, PublishOutcome, PublishReceipt, Transaction};
pub use verify::{ObjectInfo, RecoveryReport, VerifyReport};

use izu_model::{CancellationToken, Limits, ModelError, ObjectId, ObjectKind, OperationId};
use izu_platform::{Directory, sync_file};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

const STORE_FORMAT: &[u8] = b"IZU-STORE 1\n";
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct StoreOptions {
    pub limits: Limits,
    pub lock_timeout: Duration,
    pub max_in_memory_bytes: u64,
    pub max_scan_objects: u64,
    pub max_scan_bytes: u64,
    #[cfg(feature = "fault-injection")]
    pub fault_hook: Option<FaultHook>,
    /// Explicit development fixture mode. It does not establish persistent
    /// storage, and the feature cannot build in ordinary release mode.
    #[cfg(feature = "fault-injection")]
    pub allow_volatile_for_tests: bool,
}

impl Default for StoreOptions {
    fn default() -> Self {
        Self {
            limits: Limits::default(),
            lock_timeout: Duration::from_secs(10),
            max_in_memory_bytes: 64 * 1024 * 1024,
            max_scan_objects: 1_000_000,
            max_scan_bytes: 64 * 1024 * 1024 * 1024,
            #[cfg(feature = "fault-injection")]
            fault_hook: None,
            #[cfg(feature = "fault-injection")]
            allow_volatile_for_tests: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("{action}: {source}")]
    Io {
        action: &'static str,
        #[source]
        source: io::Error,
    },
    #[error(transparent)]
    Model(#[from] ModelError),
    #[error("durable publication is unavailable: {0}")]
    UnsupportedDurability(io::Error),
    #[cfg(feature = "fault-injection")]
    #[error("explicit volatile test mode never acknowledges persistent storage")]
    VolatileTestMode,
    #[error("unsupported {kind} format version")]
    UnsupportedVersion { kind: &'static str },
    #[error("corrupt {kind}: {reason}")]
    Corrupt {
        kind: &'static str,
        reason: &'static str,
    },
    #[error("object {id} has kind {actual:?}; expected {expected:?}")]
    UnexpectedKind {
        id: ObjectId,
        expected: ObjectKind,
        actual: ObjectKind,
    },
    #[error("HEAD changed: expected {expected:?}, observed {actual:?}")]
    HeadConflict {
        expected: HeadExpectation,
        actual: Option<OperationId>,
    },
    #[error("operation {operation} has parent {parent:?}, but current HEAD is {current:?}")]
    HistoryMismatch {
        operation: OperationId,
        parent: Option<OperationId>,
        current: Option<OperationId>,
    },
    #[error("{0} resource limit exceeded")]
    LimitExceeded(&'static str),
    #[error("allocation failed")]
    Allocation,
    #[error("timed out acquiring {0} lock")]
    LockTimeout(&'static str),
    #[error("invalid store path: {0}")]
    InvalidPath(&'static str),
    #[error("blob reader length does not match declared length")]
    InputLengthMismatch,
    #[error("object batch was aborted by an earlier staging failure")]
    BatchAborted,
}

pub type Result<T> = std::result::Result<T, StoreError>;

pub(crate) fn io_error(action: &'static str, source: io::Error) -> StoreError {
    StoreError::Io { action, source }
}

fn check_persistent(file: &File, options: &StoreOptions) -> Result<()> {
    #[cfg(feature = "fault-injection")]
    if options.allow_volatile_for_tests {
        return Ok(());
    }
    let _ = options;
    izu_platform::require_persistent_file(file).map_err(StoreError::UnsupportedDurability)
}

pub(crate) fn durable_file(file: &File, options: &StoreOptions) -> Result<()> {
    check_persistent(file, options)?;
    sync_file(file).map_err(|source| {
        if source.kind() == io::ErrorKind::Unsupported
            || source.kind() == io::ErrorKind::InvalidInput
        {
            StoreError::UnsupportedDurability(source)
        } else {
            io_error("persist file", source)
        }
    })
}

pub(crate) fn durable_dir(dir: &Directory, options: &StoreOptions) -> Result<()> {
    check_persistent(dir.file(), options)?;
    dir.sync().map_err(|source| {
        if source.kind() == io::ErrorKind::Unsupported
            || source.kind() == io::ErrorKind::InvalidInput
        {
            StoreError::UnsupportedDurability(source)
        } else {
            io_error("persist directory", source)
        }
    })
}

pub(crate) fn kernel_file(file: &File, options: &StoreOptions) -> Result<()> {
    check_persistent(file, options)?;
    izu_platform::sync_kernel(file).map_err(|source| {
        if source.kind() == io::ErrorKind::Unsupported
            || source.kind() == io::ErrorKind::InvalidInput
        {
            StoreError::UnsupportedDurability(source)
        } else {
            io_error("submit file or directory persistence", source)
        }
    })
}

#[derive(Debug)]
pub struct Store {
    pub(crate) root: Directory,
    pub(crate) objects: Directory,
    pub(crate) temporary: Directory,
    pub(crate) options: StoreOptions,
}

impl Store {
    /// Creates a new store. Existing paths are never reinitialized or overwritten.
    /// A failed initialization may leave an incomplete directory for inspection.
    pub fn init(path: &Path, options: &StoreOptions) -> Result<Self> {
        validate_options(options)?;
        izu_platform::require_durable_platform().map_err(StoreError::UnsupportedDurability)?;
        let name = path.file_name().ok_or(StoreError::InvalidPath(
            "store needs a final directory name",
        ))?;
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent =
            Directory::open(parent_path).map_err(|error| io_error("open store parent", error))?;
        Self::init_at(&parent, name, options)
    }

    /// Creates a new store within an already anchored parent directory. This is
    /// the staging/restore boundary: a renamed or substituted pathname cannot
    /// redirect initialization away from the caller's original directory handle.
    pub fn init_at(parent: &Directory, name: &OsStr, options: &StoreOptions) -> Result<Self> {
        validate_options(options)?;
        izu_platform::require_durable_platform().map_err(StoreError::UnsupportedDurability)?;
        check_persistent(parent.file(), options)?;
        let root = parent
            .create_dir(name)
            .map_err(|error| io_error("create store", error))?;
        let objects = root
            .create_dir(OsStr::new("objects"))
            .map_err(|error| io_error("create objects directory", error))?;
        let temporary = root
            .create_dir(OsStr::new("tmp"))
            .map_err(|error| io_error("create temporary directory", error))?;
        admission::initialize_new(&root, options)?;
        let mut format = root
            .create_new_file(OsStr::new("FORMAT"))
            .map_err(|error| io_error("create store format", error))?;
        format
            .write_all(STORE_FORMAT)
            .map_err(|error| io_error("write store format", error))?;
        durable_file(&format, options)?;
        for name in ["head.lock", "temporary.lock"] {
            let lock = root
                .create_new_file(OsStr::new(name))
                .map_err(|error| io_error("create store lock", error))?;
            durable_file(&lock, options)?;
        }
        durable_dir(&objects, options)?;
        durable_dir(&temporary, options)?;
        durable_dir(&root, options)?;
        durable_dir(parent, options)?;
        Ok(Self {
            root,
            objects,
            temporary,
            options: options.clone(),
        })
    }

    /// Opens a previously initialized store. The format and primary locks must
    /// exist; confirming a missing HEAD may initialize private admission coordination.
    pub fn open(path: &Path, options: &StoreOptions) -> Result<Self> {
        validate_options(options)?;
        izu_platform::require_durable_platform().map_err(StoreError::UnsupportedDurability)?;
        let name = path.file_name().ok_or(StoreError::InvalidPath(
            "store needs a final directory name",
        ))?;
        let parent_path = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let parent =
            Directory::open(parent_path).map_err(|error| io_error("open store parent", error))?;
        let root = parent
            .open_dir(name)
            .map_err(|error| io_error("open store", error))?;
        let mut format = root
            .open_read(OsStr::new("FORMAT"))
            .map_err(|error| io_error("open store format", error))?;
        let mut bytes = [0_u8; 64];
        let mut count = 0_usize;
        loop {
            let available = bytes
                .get_mut(count..)
                .ok_or(StoreError::LimitExceeded("store format"))?;
            if available.is_empty() {
                return Err(StoreError::LimitExceeded("store format"));
            }
            let n = format
                .read(available)
                .map_err(|error| io_error("read store format", error))?;
            if n == 0 {
                break;
            }
            count = count
                .checked_add(n)
                .ok_or(StoreError::LimitExceeded("store format"))?;
        }
        if bytes.get(..count) != Some(STORE_FORMAT) {
            return Err(StoreError::UnsupportedVersion { kind: "store" });
        }
        let objects = root
            .open_dir(OsStr::new("objects"))
            .map_err(|error| io_error("open objects directory", error))?;
        let temporary = root
            .open_dir(OsStr::new("tmp"))
            .map_err(|error| io_error("open temporary directory", error))?;
        for name in ["head.lock", "temporary.lock"] {
            root.open_read(OsStr::new(name))
                .map_err(|error| io_error("validate lock file", error))?;
        }
        // Successful flushes probe the OS capability, not the storage hardware.
        durable_dir(&root, options)?;
        durable_dir(&objects, options)?;
        durable_dir(&temporary, options)?;
        // An interrupted init can leave a valid layout before its final parent
        // barrier. Opening must establish that entry's persistence as well.
        durable_dir(&parent, options)?;
        let store = Self {
            root,
            objects,
            temporary,
            options: options.clone(),
        };
        store.current_head(&CancellationToken::new())?;
        Ok(store)
    }

    pub fn options(&self) -> &StoreOptions {
        &self.options
    }

    /// The pinned native metadata directory for engine-owned locks and intents.
    /// Callers must preserve store format/lock ownership and identity invariants.
    pub fn root_directory(&self) -> &Directory {
        &self.root
    }

    pub fn current_head(&self, cancel: &CancellationToken) -> Result<Option<OperationId>> {
        transaction::read_head(self, cancel)
    }

    pub(crate) fn acquire_lock(
        &self,
        name: &'static str,
        shared: bool,
        cancel: &CancellationToken,
    ) -> Result<File> {
        let file = self
            .root
            .open_existing_lock(OsStr::new(name))
            .map_err(|error| io_error("open lock", error))?;
        let start = Instant::now();
        loop {
            cancel.check()?;
            let result = if shared {
                file.try_lock_shared()
            } else {
                file.try_lock()
            };
            match result {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let elapsed = start.elapsed();
                    let remaining = self
                        .options
                        .lock_timeout
                        .checked_sub(elapsed)
                        .ok_or(StoreError::LockTimeout(name))?;
                    if remaining.is_zero() {
                        return Err(StoreError::LockTimeout(name));
                    }
                    thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(io_error("acquire lock", error));
                }
            }
        }
    }

    pub(crate) fn boundary(&self, boundary: DurableBoundary) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        if let Some(hook) = &self.options.fault_hook {
            (hook.0)(boundary).map_err(|error| io_error("injected durable boundary", error))?;
        }
        let _ = boundary;
        Ok(())
    }
}

fn validate_options(options: &StoreOptions) -> Result<()> {
    options.limits.validate()?;
    if options.lock_timeout > Duration::from_secs(60) {
        return Err(StoreError::LimitExceeded(
            "lock timeout (maximum 60 seconds)",
        ));
    }
    if options.max_in_memory_bytes == 0
        || options.max_scan_objects == 0
        || options.max_scan_bytes == 0
    {
        return Err(StoreError::LimitExceeded("positive store budgets"));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DurableBoundary {
    #[cfg(feature = "fault-injection")]
    HeadAdmissionRegistered,
    ObjectTempCreated,
    ObjectDataWritten,
    ObjectFileSynced,
    ObjectLinked,
    ObjectDirectorySynced,
    BatchObjectKernelSynced,
    BatchTemporaryKernelSynced,
    BatchDataSynced,
    BatchObjectLinked,
    BatchFinalObjectKernelSynced,
    BatchDirectoriesKernelSynced,
    BatchDirectorySynced,
    ClosureObjectSubmitted,
    ClosureDirectorySubmitted,
    HeadDataWritten,
    HeadFileKernelSynced,
    HeadFileSynced,
    HeadReplaced,
    HeadDirectoriesKernelSynced,
    HeadDirectorySynced,
}

#[cfg(feature = "fault-injection")]
#[derive(Clone)]
pub struct FaultHook(pub std::sync::Arc<dyn Fn(DurableBoundary) -> io::Result<()> + Send + Sync>);

#[cfg(feature = "fault-injection")]
impl std::fmt::Debug for FaultHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FaultHook(..)")
    }
}
