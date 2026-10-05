use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use izu_platform::Directory;
use serde::{Deserialize, Serialize};

use crate::types::{io_error, now_ms};
use crate::{
    CancellationReason, JobId, JobSnapshot, JobState, ResourceBudget, RuntimeConfig, RuntimeError,
    WorkspaceBinding,
};

pub(crate) const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RegistryState {
    pub format_version: u32,
    pub next_sequence: u64,
    #[serde(default)]
    pub budget: Option<ResourceBudget>,
    pub jobs: BTreeMap<JobId, JobSnapshot>,
    pub workspaces: Vec<WorkspaceBinding>,
    #[serde(default)]
    pub closed_workspaces: BTreeSet<izu_model::WorkspaceId>,
}
impl Default for RegistryState {
    fn default() -> Self {
        Self {
            format_version: FORMAT_VERSION,
            next_sequence: 0,
            budget: None,
            jobs: BTreeMap::new(),
            workspaces: Vec::new(),
            closed_workspaces: BTreeSet::new(),
        }
    }
}

pub(crate) struct Registry {
    pub root: PathBuf,
    directory: Directory,
    jobs_directory: Directory,

    max_bytes: u64,
}

fn require_persistent_directory(directory: &Directory, path: &Path) -> Result<(), RuntimeError> {
    directory.require_persistent_filesystem().map_err(|error| {
        if error.kind() == std::io::ErrorKind::Unsupported {
            RuntimeError::Unsupported(format!(
                "runtime registry requires persistent storage at {}: {error}",
                path.display()
            ))
        } else {
            io_error("check runtime registry persistence", path, error)
        }
    })
}

impl Registry {
    pub fn open(
        path: &Path,
        config: &RuntimeConfig,
    ) -> Result<(Self, RegistryState), RuntimeError> {
        let root = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()
                .map_err(|error| io_error("resolve registry base", path, error))?
                .join(path)
        };
        let parent_path = root
            .parent()
            .ok_or_else(|| RuntimeError::Invalid("registry root must have a parent".into()))?;
        let name = root
            .file_name()
            .ok_or_else(|| RuntimeError::Invalid("registry root must have a name".into()))?;
        let parent = Directory::open(parent_path).map_err(|error| {
            io_error("pin registry parent without symlinks", parent_path, error)
        })?;
        require_persistent_directory(&parent, parent_path)?;
        let directory = parent
            .ensure_dir(name)
            .map_err(|error| io_error("pin private registry", &root, error))?;
        require_persistent_directory(&directory, &root)?;
        crate::os::check_private_directory_handle(&directory, &root)?;

        let lock_path = root.join("owner.lock");
        let lock = directory
            .open_lock(OsStr::new("owner.lock"))
            .map_err(|error| io_error("open private registry lock", &lock_path, error))?;
        crate::os::check_private_file(&lock, &lock_path)?;

        let jobs_missing = match directory.metadata(OsStr::new("jobs")) {
            Ok(_) => false,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
            Err(error) => return Err(io_error("inspect registry artifacts", &root, error)),
        };
        let jobs_directory = directory
            .ensure_dir(OsStr::new("jobs"))
            .map_err(|error| io_error("pin job artifacts", &root, error))?;
        require_persistent_directory(&jobs_directory, &root.join("jobs"))?;
        crate::os::check_private_directory_handle(&jobs_directory, &root.join("jobs"))?;

        drop(lock);
        let registry = Self {
            root,
            directory,
            jobs_directory,
            max_bytes: config.max_registry_bytes,
        };
        let _transaction = registry.transaction()?;
        // Initialization publishes its budget only after the parent link and
        // structural children are acknowledged. Existing controllers reuse that
        // durable initialization instead of flushing unchanged directories.
        let uninitialized = registry.read_state(config)?.budget.is_none();
        if uninitialized {
            parent.sync().map_err(|error| {
                RuntimeError::DurabilityUncertain(format!(
                    "registry parent {}: {error}",
                    registry.root.display()
                ))
            })?;
        }
        if uninitialized || jobs_missing {
            registry.directory.sync().map_err(|error| {
                RuntimeError::DurabilityUncertain(format!(
                    "registry structure {}: {error}",
                    registry.root.display()
                ))
            })?;
        }
        let state = registry.refresh(config, &BTreeSet::new())?;
        registry.initialize_source_slots(config)?;
        Ok((registry, state))
    }

    // Called only while open's short registry transaction is held. Names remain
    // permanently linked: removing a live coordination file would split owners.
    fn initialize_source_slots(&self, config: &RuntimeConfig) -> Result<(), RuntimeError> {
        let mut created = false;
        for index in 0..config
            .budget
            .max_running_jobs
            .min(crate::types::MAX_SOURCE_OPERATIONS)
        {
            let name = format!("source-{index}.lock");
            match self.directory.metadata(OsStr::new(&name)) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => created = true,
                Err(error) => {
                    return Err(io_error(
                        "inspect source admission slot",
                        &self.root.join(&name),
                        error,
                    ));
                }
            }
            let lock = self
                .directory
                .open_lock(OsStr::new(&name))
                .map_err(|error| {
                    io_error(
                        "initialize source admission slot",
                        &self.root.join(&name),
                        error,
                    )
                })?;
            self.check_source_slot(&lock, OsStr::new(&name))?;
        }
        if created {
            self.directory.sync().map_err(|error| {
                RuntimeError::DurabilityUncertain(format!(
                    "source admission slots {}: {error}",
                    self.root.display()
                ))
            })?;
        }
        Ok(())
    }

    fn check_source_slot(&self, file: &File, name: &OsStr) -> Result<(), RuntimeError> {
        let path = self.root.join(name);
        crate::os::check_private_file(file, &path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let held = file
                .metadata()
                .map_err(|error| io_error("inspect held source admission slot", &path, error))?;
            let linked = self
                .directory
                .metadata(name)
                .map_err(|error| io_error("inspect linked source admission slot", &path, error))?;
            if !linked.is_file() || held.dev() != linked.dev() || held.ino() != linked.ino() {
                return Err(RuntimeError::RegistryLocatorChanged(path));
            }
            Ok(())
        }
        #[cfg(not(unix))]
        Err(RuntimeError::Unsupported(
            "source operation admission is currently Unix only".into(),
        ))
    }

    /// Queue outside the registry transaction and native history locks. Like
    /// queued jobs, waiting is cancellable; execution deadlines start at release.
    pub fn source_operation_permit(
        &self,
        limit: u32,
        cancel: &izu_model::CancellationToken,
    ) -> Result<crate::SourceOperationPermit, RuntimeError> {
        loop {
            if cancel.is_cancelled() {
                return Err(RuntimeError::Cancelled);
            }
            self.check_locator()?;
            for index in 0..limit {
                let name = format!("source-{index}.lock");
                let path = self.root.join(&name);
                let lock = self
                    .directory
                    .open_existing_lock(OsStr::new(&name))
                    .map_err(|error| io_error("open source admission slot", &path, error))?;
                self.check_source_slot(&lock, OsStr::new(&name))?;
                match crate::os::lock_exclusive(&lock, &path) {
                    Ok(()) => {
                        self.check_locator()?;
                        self.check_source_slot(&lock, OsStr::new(&name))?;
                        if cancel.is_cancelled() {
                            return Err(RuntimeError::Cancelled);
                        }
                        return Ok(crate::SourceOperationPermit { _lease: lock });
                    }
                    Err(RuntimeError::Busy(_)) => {}
                    Err(error) => return Err(error),
                }
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    /// The registry transaction lock is held only during bounded state updates,
    /// never while queued for source admission or running an expensive command.
    pub fn transaction(&self) -> Result<File, RuntimeError> {
        self.check_locator()?;
        let path = self.root.join("owner.lock");
        let lock = self
            .directory
            .open_existing_lock(OsStr::new("owner.lock"))
            .map_err(|error| io_error("open registry transaction lock", &path, error))?;
        crate::os::check_private_file(&lock, &path)?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            match crate::os::lock_exclusive(&lock, &path) {
                Ok(()) => return Ok(lock),
                Err(RuntimeError::Busy(_)) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(std::time::Duration::from_millis(5))
                }
                Err(error) => return Err(error),
            }
        }
    }
    pub fn read_state(&self, config: &RuntimeConfig) -> Result<RegistryState, RuntimeError> {
        self.check_locator()?;
        let state_name = OsStr::new("state.json");
        let state = match self.directory.metadata(state_name) {
            Ok(_) => read_json::<RegistryState>(
                &self.directory,
                state_name,
                &self.root.join(state_name),
                config.max_registry_bytes,
            )?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => RegistryState::default(),
            Err(error) => return Err(io_error("inspect pinned registry state", &self.root, error)),
        };
        validate_state(&state, config)?;
        Ok(state)
    }
    pub fn refresh(
        &self,
        config: &RuntimeConfig,
        owned: &BTreeSet<JobId>,
    ) -> Result<RegistryState, RuntimeError> {
        let mut state = self.read_state(config)?;
        let mut changed = state.budget.is_none();
        if changed {
            state.budget = Some(config.budget.clone());
        }
        let now = now_ms()?;
        for job in state.jobs.values_mut() {
            if owned.contains(&job.id) || self.controller_is_live(&job.id)? {
                continue;
            }
            let previous = job.state.clone();
            job.state = match &job.state {
                JobState::Starting { .. } => JobState::OwnershipUnknown { observed_at_unix_ms: now, worker_pid: None, reason: "controller exited during admitted launch setup; a durable source intent may exist before its token reached this registry".into() },
                JobState::Running { worker_pid, .. } => JobState::OwnershipUnknown { observed_at_unix_ms: now, worker_pid: Some(*worker_pid), reason: "job controller lease was lost; durable PID is not a live owned handle".into() },
                JobState::Queued { .. } if job.writer_intent.is_some() => JobState::OwnershipUnknown { observed_at_unix_ms: now, worker_pid: None, reason: "controller exited after acquiring source writer intent; exact token reconciliation required".into() },
                JobState::Queued { .. } => JobState::Cancelled { finished_at_unix_ms: now, reason: CancellationReason::RecoveredBeforeLaunch },
                other => other.clone(),
            };
            changed |= previous != job.state;
            // Overlays expire without source deletion or unknown-process signals.
            match self.jobs_directory.open_dir(OsStr::new(job.id.as_str())) {
                Ok(directory) => {
                    crate::os::check_private_directory_handle(&directory, &self.job_path(&job.id))?;
                    expire_ticket(&directory, &self.job_path(&job.id))?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(io_error(
                        "pin recovered ticket directory",
                        &self.job_path(&job.id),
                        error,
                    ));
                }
            }
        }
        if changed {
            self.save(&state)?;
        }
        Ok(state)
    }
    fn controller_is_live(&self, id: &JobId) -> Result<bool, RuntimeError> {
        let path = self.job_path(id);
        let directory = match self.jobs_directory.open_dir(OsStr::new(id.as_str())) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("pin job controller directory", &path, error)),
        };
        crate::os::check_private_directory_handle(&directory, &path)?;
        let lock = match directory.open_existing_lock(OsStr::new("controller.lock")) {
            Ok(lock) => lock,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(io_error("open job controller lease", &path, error)),
        };
        crate::os::check_private_file(&lock, &path)?;
        match crate::os::lock_exclusive(&lock, &path) {
            Ok(()) => Ok(false),
            Err(RuntimeError::Busy(_)) => Ok(true),
            Err(error) => Err(error),
        }
    }
    pub fn create_controller(&self, id: &JobId) -> Result<File, RuntimeError> {
        let directory = self.create_job_directory(id)?;
        let path = self.job_path(id).join("controller.lock");
        let lock = directory
            .open_lock(OsStr::new("controller.lock"))
            .map_err(|error| io_error("create job controller lease", &path, error))?;
        crate::os::check_private_file(&lock, &path)?;
        crate::os::lock_exclusive(&lock, &path)?;
        directory.sync().map_err(|error| {
            RuntimeError::DurabilityUncertain(format!(
                "job controller lease {}: {error}",
                path.display()
            ))
        })?;
        Ok(lock)
    }
    pub fn open_job_directory(&self, id: &JobId) -> Result<Directory, RuntimeError> {
        self.check_locator()?;
        let path = self.job_path(id);
        let directory = self
            .jobs_directory
            .open_dir(OsStr::new(id.as_str()))
            .map_err(|error| io_error("pin controller job artifacts", &path, error))?;
        crate::os::check_private_directory_handle(&directory, &path)?;
        Ok(directory)
    }
    pub fn check_locator(&self) -> Result<(), RuntimeError> {
        let current = Directory::open(&self.root)
            .map_err(|_| RuntimeError::RegistryLocatorChanged(self.root.clone()))?;
        let original = crate::os::directory_identity_handle(&self.directory, &self.root)?;
        if crate::os::directory_identity_handle(&current, &self.root)? != original {
            return Err(RuntimeError::RegistryLocatorChanged(self.root.clone()));
        }
        Ok(())
    }
    pub fn save(&self, state: &RegistryState) -> Result<(), RuntimeError> {
        self.check_locator()?;
        write_json_durable(
            &self.directory,
            OsStr::new("state.json"),
            &self.root.join("state.json"),
            state,
            self.max_bytes,
        )
    }
    pub fn job_path(&self, id: &JobId) -> PathBuf {
        self.root.join("jobs").join(id.as_str())
    }
    pub fn create_job_directory(&self, id: &JobId) -> Result<Directory, RuntimeError> {
        self.check_locator()?;
        let path = self.job_path(id);
        let directory = self
            .jobs_directory
            .create_dir(OsStr::new(id.as_str()))
            .map_err(|error| io_error("create unique job artifacts", &path, error))?;
        crate::os::check_private_directory_handle(&directory, &path)?;
        self.jobs_directory.sync().map_err(|error| {
            RuntimeError::DurabilityUncertain(format!("job directory {}: {error}", path.display()))
        })?;
        Ok(directory)
    }
    pub fn expire_job_ticket(&self, id: &JobId) -> Result<(), RuntimeError> {
        match self.jobs_directory.open_dir(OsStr::new(id.as_str())) {
            Ok(directory) => expire_ticket(&directory, &self.job_path(id)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io_error(
                "pin launch ticket for expiry",
                &self.job_path(id),
                error,
            )),
        }
    }
}

fn validate_state(state: &RegistryState, config: &RuntimeConfig) -> Result<(), RuntimeError> {
    if state
        .budget
        .as_ref()
        .is_some_and(|budget| budget != &config.budget)
    {
        return Err(RuntimeError::Invalid(
            "runtime budget differs from the durable shared registry budget".into(),
        ));
    }
    if state.format_version != FORMAT_VERSION {
        return Err(RuntimeError::Invalid(format!(
            "unsupported registry format {}",
            state.format_version
        )));
    }
    let open_workspaces = state
        .workspaces
        .iter()
        .filter(|binding| !state.closed_workspaces.contains(&binding.id))
        .count();
    let total_capacity = config
        .budget
        .max_retained_jobs
        .checked_add(config.budget.max_workspaces)
        .ok_or_else(|| RuntimeError::Invalid("registry capacity overflow".into()))?;
    if state.jobs.len()
        > usize::try_from(config.budget.max_retained_jobs)
            .map_err(|_| RuntimeError::Invalid("retention cannot fit usize".into()))?
        || open_workspaces
            > usize::try_from(config.budget.max_workspaces)
                .map_err(|_| RuntimeError::Invalid("workspace capacity cannot fit usize".into()))?
        || state.workspaces.len()
            > usize::try_from(total_capacity)
                .map_err(|_| RuntimeError::Invalid("total capacity cannot fit usize".into()))?
    {
        return Err(RuntimeError::Capacity(
            "persisted registry exceeds configured history/workspace capacity".into(),
        ));
    }
    let mut sequences = std::collections::BTreeSet::new();
    let mut workspaces = std::collections::BTreeSet::new();
    for workspace in &state.workspaces {
        if !workspace.cwd.is_absolute()
            || !workspace.repository.is_absolute()
            || !workspaces.insert(workspace.id)
        {
            return Err(RuntimeError::Invalid(
                "invalid or duplicate persisted workspace binding".into(),
            ));
        }
    }
    for (id, job) in &state.jobs {
        if id != &job.id
            || !sequences.insert(job.sequence)
            || job.sequence >= state.next_sequence
            || !workspaces.contains(&job.request.workspace.id)
        {
            return Err(RuntimeError::Invalid(
                "registry job identity/sequence/workspace is inconsistent".into(),
            ));
        }
        let request = crate::RunRequest {
            workspace: job.request.workspace.clone(),
            argv: job.request.argv.clone(),
            environment: job.request.environment,
            env_overlay: job
                .request
                .environment_keys
                .iter()
                .map(|key| (key.clone(), String::new()))
                .collect(),
            resources: job.request.resources.clone(),
            timeout_ms: job.request.timeout_ms,
            environment_binding: job.request.environment_binding,
        };
        crate::scheduler::validate_request(&request, config)?;
        if job.starting_environment.as_ref().is_some_and(|receipt| {
            job.request.environment_binding.is_none()
                || receipt.workspace != job.request.workspace.id
                || receipt.security_boundary
        }) || (job.request.environment_binding.is_some()
            && job.starting_environment.is_none()
            && matches!(
                job.state,
                JobState::Running { .. }
                    | JobState::Finished {
                        outcome: crate::CommandOutcome::Exit { .. },
                        ..
                    }
            ))
        {
            return Err(RuntimeError::Invalid(
                "persisted starting-environment receipt is inconsistent with its job".into(),
            ));
        }
        if job.output.stdout.bytes.len() > config.output_bytes_per_stream
            || job.output.stderr.bytes.len() > config.output_bytes_per_stream
        {
            return Err(RuntimeError::Invalid(
                "persisted output exceeds configured bound".into(),
            ));
        }
        if let JobState::Running { worker_pid, .. }
        | JobState::OwnershipUnknown {
            worker_pid: Some(worker_pid),
            ..
        } = job.state
            && worker_pid <= 1
        {
            return Err(RuntimeError::Invalid(
                "invalid worker PID in registry".into(),
            ));
        }
    }
    Ok(())
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    directory: &Directory,
    name: &OsStr,
    path: &Path,
    max_bytes: u64,
) -> Result<T, RuntimeError> {
    let file = directory
        .open_read(name)
        .map_err(|error| io_error("open pinned private file", path, error))?;
    crate::os::check_private_file(&file, path)?;
    let size = file
        .metadata()
        .map_err(|error| io_error("stat private file", path, error))?
        .len();
    if size > max_bytes {
        return Err(RuntimeError::Invalid(format!(
            "file exceeds read bound: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    let limit = max_bytes
        .checked_add(1)
        .ok_or_else(|| RuntimeError::Invalid("read bound overflow".into()))?;
    file.take(limit)
        .read_to_end(&mut bytes)
        .map_err(|error| io_error("read private file", path, error))?;
    if u64::try_from(bytes.len()).is_ok_and(|len| len > max_bytes) {
        return Err(RuntimeError::Invalid("file grew beyond read bound".into()));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        RuntimeError::Invalid(format!(
            "invalid runtime JSON at {}: {error}",
            path.display()
        ))
    })
}

pub(crate) fn write_json_durable<T: Serialize>(
    directory: &Directory,
    name: &OsStr,
    path: &Path,
    value: &T,
    max_bytes: u64,
) -> Result<(), RuntimeError> {
    publish_json(directory, name, path, value, max_bytes, || directory.sync())
}

fn publish_json<T: Serialize>(
    directory: &Directory,
    name: &OsStr,
    path: &Path,
    value: &T,
    max_bytes: u64,
    sync_after_rename: impl FnOnce() -> std::io::Result<()>,
) -> Result<(), RuntimeError> {
    let limit = usize::try_from(max_bytes)
        .map_err(|_| RuntimeError::Invalid("publication bound cannot fit usize".into()))?;
    let mut writer = BoundedJson {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut writer, value).map_err(|error| {
        RuntimeError::Capacity(format!("bounded runtime serialization failed: {error}"))
    })?;
    let bytes = writer.bytes;
    crate::os::check_private_directory_handle(directory, path)?;
    let temporary = format!(".publish-{}", JobId::generate()?);
    let result = (|| {
        let mut file = directory
            .create_new_file(OsStr::new(&temporary))
            .map_err(|error| io_error("create pinned publication", path, error))?;
        file.write_all(&bytes)
            .map_err(|error| io_error("write publication", path, error))?;
        izu_platform::sync_file(&file)
            .map_err(|error| io_error("sync publication", path, error))?;
        directory
            .rename_replace(OsStr::new(&temporary), directory, name)
            .map_err(|error| io_error("publish pinned registry", path, error))?;
        sync_after_rename().map_err(|error| {
            RuntimeError::DurabilityUncertain(format!("{}: {error}", path.display()))
        })?;
        Ok(())
    })();
    let _ = directory.remove_file(OsStr::new(&temporary));
    result
}

struct BoundedJson {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for BoundedJson {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|size| size > self.limit)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::FileTooLarge,
                "runtime JSON exceeds configured bound",
            ));
        }
        self.bytes.try_reserve(bytes.len()).map_err(|error| {
            std::io::Error::new(std::io::ErrorKind::OutOfMemory, error.to_string())
        })?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub(crate) fn expire_ticket(directory: &Directory, path: &Path) -> Result<(), RuntimeError> {
    match directory.remove_file(OsStr::new("ticket.json")) {
        Ok(()) => directory.sync().map_err(|error| {
            RuntimeError::DurabilityUncertain(format!(
                "launch ticket expiry {}: {error}",
                path.display()
            ))
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(io_error("expire private launch ticket", path, error)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    fn owned_tempdir() -> tempfile::TempDir {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
        std::fs::create_dir_all(&path).unwrap();
        tempfile::tempdir_in(path).unwrap()
    }
    #[test]
    fn fifo_is_rejected_without_waiting_for_a_writer() {
        let fixture = owned_tempdir();
        let path = fixture.path().canonicalize().unwrap();
        let directory = Directory::open(&path).unwrap();
        assert!(
            std::process::Command::new("/usr/bin/mkfifo")
                .arg(path.join("state.json"))
                .status()
                .unwrap()
                .success()
        );
        let started = Instant::now();
        assert!(
            read_json::<RegistryState>(
                &directory,
                OsStr::new("state.json"),
                &path.join("state.json"),
                1024
            )
            .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    #[test]
    fn pinned_publication_cannot_follow_a_replaced_root() {
        let fixture = owned_tempdir();
        let base = fixture.path().canonicalize().unwrap();
        std::fs::create_dir(base.join("owned")).unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(base.join("owned"), std::fs::Permissions::from_mode(0o700))
                .unwrap();
        }
        std::fs::create_dir(base.join("outside")).unwrap();
        let directory = Directory::open(&base.join("owned")).unwrap();
        std::fs::rename(base.join("owned"), base.join("retained")).unwrap();
        std::os::unix::fs::symlink(base.join("outside"), base.join("owned")).unwrap();
        write_json_durable(
            &directory,
            OsStr::new("state.json"),
            &base.join("owned/state.json"),
            &RegistryState::default(),
            4096,
        )
        .unwrap();
        assert!(base.join("retained/state.json").is_file());
        assert!(!base.join("outside/state.json").exists());
    }
    #[test]
    fn post_rename_sync_failure_is_uncertain_with_visible_new_state() {
        let fixture = owned_tempdir();
        let path = fixture.path().canonicalize().unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let directory = Directory::open(&path).unwrap();
        let result = publish_json(
            &directory,
            OsStr::new("state.json"),
            &path.join("state.json"),
            &RegistryState::default(),
            4096,
            || Err(std::io::Error::other("injected post-rename sync failure")),
        );
        assert!(
            matches!(result, Err(RuntimeError::DurabilityUncertain(_))),
            "{result:?}"
        );
        assert_eq!(
            read_json::<RegistryState>(
                &directory,
                OsStr::new("state.json"),
                &path.join("state.json"),
                4096
            )
            .unwrap()
            .format_version,
            FORMAT_VERSION
        );
    }
}
