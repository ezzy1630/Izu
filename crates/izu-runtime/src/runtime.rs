use std::collections::BTreeMap;
use std::net::{Ipv4Addr, TcpListener};
use std::path::Path;
use std::process::{ChildStdin, Command};
use std::thread;
use std::time::{Duration, Instant};

use izu_model::{CancellationToken, WorkspaceId};

use crate::os::OutputReaders;
use crate::registry::{Registry, RegistryState};
use crate::types::{io_error, now_ms};
use crate::worker::{read_result, release_writer, write_ticket};
use crate::*;

struct OwnedWorker {
    child: izu_process::OwnedProcess,
    control: ChildStdin,
    output: OutputReaders,
    deadline: Option<Instant>,
    execution_deadline_unix_ms: Option<i64>,
    directory: izu_platform::Directory,
    lease: izu_engine::WorkspaceWriterLease,
    _environment: Option<izu_environment::StartingEnvironmentProof>,
}

// Setup and fallback cleanup must preserve the same source identity invariant
// as ordinary completion, even when the registry can no longer be read.
fn finish_stopped_source(lease: izu_engine::WorkspaceWriterLease) -> Result<(), RuntimeError> {
    let pinned = crate::os::directory_identity_handle(lease.source_directory(), lease.root_path())?;
    if crate::os::directory_identity(lease.root_path())? != pinned {
        return Err(RuntimeError::StaleWorkspace(
            "workspace source directory was replaced; original files and writer intent retained"
                .into(),
        ));
    }
    lease.finish_stopped().map_err(RuntimeError::Engine)
}

/// Controllers share global admission transactions and independently own jobs.
/// The source engine works without a runtime or daemon.
pub struct Runtime {
    pub(crate) registry: Registry,
    pub(crate) state: RegistryState,
    config: RuntimeConfig,
    active: BTreeMap<JobId, OwnedWorker>,
    pending: BTreeMap<JobId, RunRequest>,
    poisoned: bool,
    controllers: BTreeMap<JobId, std::fs::File>,
    transaction: Option<std::fs::File>,
}

impl Runtime {
    pub fn open(path: impl AsRef<Path>, mut config: RuntimeConfig) -> Result<Self, RuntimeError> {
        config.validate()?;
        if !RuntimeCapabilities::current().process_groups {
            return Err(RuntimeError::Unsupported(
                "runtime process ownership is currently Unix only".into(),
            ));
        }
        config.worker_executable = config.worker_executable.canonicalize().map_err(|error| {
            io_error(
                "resolve explicitly selected worker",
                &config.worker_executable,
                error,
            )
        })?;
        if !config.worker_executable.is_file() {
            return Err(RuntimeError::Invalid(
                "worker executable is not a file".into(),
            ));
        }
        let (registry, state) = Registry::open(path.as_ref(), &config)?;
        Ok(Self {
            registry,
            state,
            config,
            active: BTreeMap::new(),
            pending: BTreeMap::new(),
            poisoned: false,
            controllers: BTreeMap::new(),
            transaction: None,
        })
    }
    pub fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities::current()
    }
    pub fn budget(&self) -> &ResourceBudget {
        &self.config.budget
    }
    pub fn usage(&self) -> Result<ResourceUsage, RuntimeError> {
        scheduler::usage(self.current_state()?.jobs.values())
    }
    pub fn registry_path(&self) -> &Path {
        &self.registry.root
    }
    /// Bound source setup and close independently of command reservations.
    /// Keep the grant through source mutations and release before submit/wait.
    /// Queue waiting has cancellation semantics, not an execution deadline.
    pub fn source_operation_permit(
        &self,
        cancel: &CancellationToken,
    ) -> Result<SourceOperationPermit, RuntimeError> {
        self.ensure_usable()?;
        if self.transaction.is_some() {
            return Err(RuntimeError::Invalid(
                "source admission cannot wait inside a registry transaction".into(),
            ));
        }
        self.registry.source_operation_permit(
            self.config
                .budget
                .max_running_jobs
                .min(crate::types::MAX_SOURCE_OPERATIONS),
            cancel,
        )
    }
    pub fn jobs(&self) -> Result<Vec<JobSnapshot>, RuntimeError> {
        let mut jobs: Vec<_> = self.current_state()?.jobs.values().cloned().collect();
        jobs.sort_by_key(|job| job.sequence);
        Ok(jobs)
    }
    pub fn jobs_summary(&self, limit: usize) -> Result<Vec<JobSummary>, RuntimeError> {
        self.jobs_summary_for(None, limit)
    }
    /// Filter before selecting the newest records; never serialize argv or logs.
    pub fn jobs_summary_for(
        &self,
        workspace: Option<WorkspaceId>,
        limit: usize,
    ) -> Result<Vec<JobSummary>, RuntimeError> {
        if limit == 0 || limit > 1000 {
            return Err(RuntimeError::Invalid(
                "job summary limit must be between 1 and 1000".into(),
            ));
        }
        let mut jobs: Vec<_> = self
            .current_state()?
            .jobs
            .into_values()
            .filter(|job| workspace.is_none_or(|workspace| job.request.workspace.id == workspace))
            .map(|job| JobSummary {
                id: job.id,
                sequence: job.sequence,
                workspace: job.request.workspace,
                state: job.state,
                resources: job.request.resources,
                writer_intent: job.writer_intent,
            })
            .collect();
        jobs.sort_by_key(|job| std::cmp::Reverse(job.sequence));
        jobs.truncate(limit);
        Ok(jobs)
    }
    pub fn status(&self, id: &JobId) -> Result<JobSnapshot, RuntimeError> {
        let mut job = self
            .current_state()?
            .jobs
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
        if let Some(active) = self.active.get(id) {
            job.output = active.output.snapshot();
            if let JobState::Running {
                deadline_unix_ms, ..
            } = &mut job.state
            {
                *deadline_unix_ms = active.execution_deadline_unix_ms;
            }
        }
        Ok(job)
    }

    pub fn submit(&mut self, request: RunRequest) -> Result<JobId, RuntimeError> {
        scheduler::validate_request(&request, &self.config)?;
        request.workspace.revalidate(&CancellationToken::new())?;
        self.transact(|runtime| runtime.submit_locked(request))
    }
    fn submit_locked(&mut self, request: RunRequest) -> Result<JobId, RuntimeError> {
        self.ensure_usable()?;
        scheduler::validate_request(&request, &self.config)?;
        let queued = self
            .state
            .jobs
            .values()
            .filter(|job| matches!(job.state, JobState::Queued { .. }))
            .count();
        if queued
            >= usize::try_from(self.config.budget.max_queued_jobs)
                .map_err(|_| RuntimeError::Invalid("queue capacity cannot fit usize".into()))?
        {
            return Err(RuntimeError::Capacity("queue is full".into()));
        }
        if self.state.jobs.len()
            >= usize::try_from(self.config.budget.max_retained_jobs)
                .map_err(|_| RuntimeError::Invalid("history capacity cannot fit usize".into()))?
        {
            return Err(RuntimeError::Capacity("durable job history is full".into()));
        }
        let mut next = self.state.clone();
        match next
            .workspaces
            .iter_mut()
            .find(|binding| binding.id == request.workspace.id)
        {
            Some(binding) if binding != &request.workspace => {
                if next.jobs.values().any(|job| {
                    job.request.workspace.id == request.workspace.id
                        && (job.state.reserves_resources()
                            || matches!(job.state, JobState::Queued { .. }))
                }) {
                    return Err(RuntimeError::Capacity(
                        "workspace already has queued or active work under another binding".into(),
                    ));
                }
                *binding = request.workspace.clone();
            }
            Some(_) => {}
            None => {
                self.ensure_workspace_capacity()?;
                next.workspaces.push(request.workspace.clone());
            }
        }
        let id = JobId::generate()?;
        if next.jobs.contains_key(&id) {
            return Err(RuntimeError::Invalid(
                "random job identity collision; retry submission".into(),
            ));
        }
        let sequence = next.next_sequence;
        next.next_sequence = sequence
            .checked_add(1)
            .ok_or_else(|| RuntimeError::Capacity("job sequence exhausted".into()))?;
        next.jobs.insert(
            id.clone(),
            JobSnapshot {
                id: id.clone(),
                sequence,
                request: JobRequestSummary::from(&request),
                submitted_at_unix_ms: now_ms()?,
                state: JobState::Queued { blocked: None },
                output: JobOutput::default(),
                writer_intent: None,
                starting_environment: None,
            },
        );
        let controller = self.registry.create_controller(&id)?;
        self.commit(next)?;
        self.controllers.insert(id.clone(), controller);
        self.pending.insert(id.clone(), request);
        Ok(id)
    }

    /// Polling performs admission and observation. There is no background daemon.
    pub fn poll(&mut self) -> Result<Vec<JobSnapshot>, RuntimeError> {
        self.poll_with_cancel(&CancellationToken::new())
    }
    /// Cancellation also reaches source and starting-environment verification.
    pub fn poll_with_cancel(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Vec<JobSnapshot>, RuntimeError> {
        self.ensure_usable()?;
        for id in self.active.keys().cloned().collect::<Vec<_>>() {
            self.observe_active(&id)?;
        }
        let admitted = self.transact(Self::admit_locked)?;
        let mut first_error = None;
        for id in admitted {
            if let Err(error) = self.start_job(&id, cancel)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
            // A failed durable publication cannot authorize further launches.
            // Ordinary per-workspace setup failures must not strand other grants.
            if self.poisoned {
                break;
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => self.jobs(),
        }
    }
    fn admit_locked(&mut self) -> Result<Vec<JobId>, RuntimeError> {
        self.ensure_usable()?;
        let mut admitted = Vec::new();
        let mut queued: Vec<_> = self
            .state
            .jobs
            .values()
            .filter(|job| {
                matches!(job.state, JobState::Queued { .. }) && self.pending.contains_key(&job.id)
            })
            .map(|job| (job.sequence, job.id.clone()))
            .collect();
        queued.sort_by_key(|entry| entry.0);
        for (_, id) in queued {
            let job = self
                .state
                .jobs
                .get(&id)
                .cloned()
                .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
            let block = if self.state.jobs.values().any(|other| {
                other.request.workspace.id == job.request.workspace.id
                    && matches!(other.state, JobState::OwnershipUnknown { .. })
            }) {
                Some(AdmissionBlock::UnknownWriter)
            } else if self.state.jobs.values().any(|other| {
                other.request.workspace.id == job.request.workspace.id
                    && matches!(
                        other.state,
                        JobState::Starting { .. } | JobState::Running { .. }
                    )
            }) {
                Some(AdmissionBlock::WorkspaceWriter)
            } else {
                scheduler::admission(&job.request.resources, &self.usage()?, &self.config.budget)
            };
            if let Some(block) = block {
                self.set_blocked(&id, block)?;
                continue;
            }
            let mut probes = Vec::new();
            let mut port_busy = None;
            for port in &job.request.resources.ports {
                match TcpListener::bind((Ipv4Addr::LOCALHOST, *port)) {
                    Ok(listener) => probes.push(listener),
                    Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
                        port_busy = Some(*port);
                        break;
                    }
                    Err(error) => {
                        self.fail_before_launch(&id, format!("port preflight {port}: {error}"))?;
                        port_busy = Some(*port);
                        break;
                    }
                }
            }
            if let Some(port) = port_busy {
                if matches!(self.status(&id)?.state, JobState::Queued { .. }) {
                    self.set_blocked(&id, AdmissionBlock::ExternalPortBusy { port })?;
                }
                continue;
            }
            // No socket activation protocol is claimed. These are logical claims
            // plus an OS collision probe; an external bind may race this release.
            drop(probes);
            self.replace_state(
                &id,
                JobState::Starting {
                    reserved_at_unix_ms: now_ms()?,
                },
                None,
            )?;
            admitted.push(id);
        }
        Ok(admitted)
    }

    pub fn wait(
        &mut self,
        id: &JobId,
        cancel: &CancellationToken,
    ) -> Result<JobSnapshot, RuntimeError> {
        loop {
            let job = self.status(id)?;
            if job.state.is_terminal() {
                return Ok(job);
            }
            if cancel.is_cancelled() {
                self.cancel(id)?;
                return self.status(id);
            }
            if let Err(error) = self.poll_with_cancel(cancel) {
                if cancel.is_cancelled() {
                    self.cancel(id)?;
                    return self.status(id);
                }
                return Err(error);
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    pub fn cancel(&mut self, id: &JobId) -> Result<JobSnapshot, RuntimeError> {
        self.transact(|runtime| runtime.cancel_locked(id))
    }
    fn cancel_locked(&mut self, id: &JobId) -> Result<JobSnapshot, RuntimeError> {
        self.ensure_usable()?;
        let state = self.status(id)?.state;
        if !state.is_terminal() && !self.controllers.contains_key(id) {
            return Err(RuntimeError::Busy(self.registry.job_path(id)));
        }
        match state {
            JobState::Queued { .. } => self.replace_state(id, JobState::Cancelled { finished_at_unix_ms: now_ms()?, reason: CancellationReason::Requested }, None)?,
            JobState::Starting { .. } => {
                let job = self.status(id)?;
                let repository = izu_engine::Repository::open(&job.request.workspace.repository, izu_engine::RepositoryOptions::default())?;
                if repository.writer_intent(job.request.workspace.id, &CancellationToken::new())?.is_some() {
                    self.replace_state(id, JobState::OwnershipUnknown { observed_at_unix_ms: now_ms()?, worker_pid: None, reason: "admitted setup lost its launch handle; source intent must be reconciled explicitly".into() }, None)?;
                } else { self.replace_state(id, JobState::Cancelled { finished_at_unix_ms: now_ms()?, reason: CancellationReason::Requested }, None)?; }
            }
            JobState::Running { .. } => self.finish_owned(id, Finish::Cancel(CancellationReason::Requested))?,
            JobState::OwnershipUnknown { .. } => return Err(RuntimeError::UnsafeClose("saved PID cannot authorize cancellation; verify external writers stopped and explicitly acknowledge recovery".into())),
            _ => {},
        }
        self.status(id)
    }

    /// Explicit human assertion; it is never inferred from a PID or process age.
    pub fn acknowledge_stopped(
        &mut self,
        id: &JobId,
        operator_note: String,
    ) -> Result<JobSnapshot, RuntimeError> {
        self.transact(|runtime| runtime.acknowledge_stopped_locked(id, operator_note))
    }
    fn acknowledge_stopped_locked(
        &mut self,
        id: &JobId,
        operator_note: String,
    ) -> Result<JobSnapshot, RuntimeError> {
        self.ensure_usable()?;
        if operator_note.trim().is_empty() || operator_note.len() > 16 * 1024 {
            return Err(RuntimeError::Invalid(
                "recovery requires a nonempty operator note of at most 16 KiB".into(),
            ));
        }
        let job = self.status(id)?;
        if !matches!(job.state, JobState::OwnershipUnknown { .. }) {
            return Err(RuntimeError::Invalid(
                "only unknown writer ownership can be acknowledged".into(),
            ));
        }
        let repository = izu_engine::Repository::open(
            &job.request.workspace.repository,
            izu_engine::RepositoryOptions::default(),
        )?;
        let cancel = CancellationToken::new();
        let engine_operation = match (job.writer_intent, repository.writer_intent(job.request.workspace.id, &cancel)?) {
            (Some(token), Some(intent)) if token == intent.token => Some(repository.acknowledge_writer_stopped_with_note(job.request.workspace.id, &token, operator_note.clone(), &cancel)?),
            (_, None) => None,
            _ => return Err(RuntimeError::UnsafeClose("runtime record does not own the current engine writer intent; select and reconcile that exact token explicitly".into())),
        };
        self.replace_state(
            id,
            JobState::RecoveredStopped {
                observed_at_unix_ms: now_ms()?,
                operator_note,
                engine_operation,
            },
            None,
        )?;
        self.status(id)
    }

    pub fn shutdown(&mut self) -> Result<(), RuntimeError> {
        self.transact(Self::shutdown_locked)
    }
    fn shutdown_locked(&mut self) -> Result<(), RuntimeError> {
        self.ensure_usable()?;
        let ids: Vec<_> = self
            .state
            .jobs
            .values()
            .filter(|job| {
                self.controllers.contains_key(&job.id)
                    && matches!(
                        job.state,
                        JobState::Queued { .. }
                            | JobState::Starting { .. }
                            | JobState::Running { .. }
                    )
            })
            .map(|job| job.id.clone())
            .collect();
        for id in ids {
            if self.active.contains_key(&id) {
                self.finish_owned(&id, Finish::Cancel(CancellationReason::Shutdown))?;
            } else {
                self.cancel_locked(&id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn stop_workspace(&mut self, id: WorkspaceId) -> Result<(), RuntimeError> {
        self.transact(|runtime| runtime.stop_workspace_locked(id))
    }
    pub(crate) fn registered_binding(
        &self,
        id: WorkspaceId,
    ) -> Result<Option<WorkspaceBinding>, RuntimeError> {
        Ok(self
            .current_state()?
            .workspaces
            .into_iter()
            .find(|binding| binding.id == id))
    }
    fn stop_workspace_locked(&mut self, id: WorkspaceId) -> Result<(), RuntimeError> {
        if self.state.jobs.values().any(|job| {
            job.request.workspace.id == id && matches!(job.state, JobState::OwnershipUnknown { .. })
        }) {
            return Err(RuntimeError::UnsafeClose(
                "workspace has a writer whose process ownership was lost".into(),
            ));
        }
        let jobs: Vec<_> = self
            .state
            .jobs
            .values()
            .filter(|job| {
                job.request.workspace.id == id
                    && matches!(
                        job.state,
                        JobState::Queued { .. }
                            | JobState::Starting { .. }
                            | JobState::Running { .. }
                    )
            })
            .map(|job| job.id.clone())
            .collect();
        for job in jobs {
            self.cancel(&job)?;
        }
        Ok(())
    }
    pub(crate) fn ensure_workspace_capacity(&self) -> Result<(), RuntimeError> {
        let state = self.current_state()?;
        let open = state
            .workspaces
            .iter()
            .filter(|binding| !state.closed_workspaces.contains(&binding.id))
            .count();
        if open
            >= usize::try_from(self.config.budget.max_workspaces)
                .map_err(|_| RuntimeError::Invalid("workspace capacity cannot fit usize".into()))?
            || state.jobs.len()
                >= usize::try_from(self.config.budget.max_retained_jobs).map_err(|_| {
                    RuntimeError::Invalid("history capacity cannot fit usize".into())
                })?
        {
            return Err(RuntimeError::Capacity(
                "workspace or job history registry is full".into(),
            ));
        }
        Ok(())
    }

    fn observe_active(&mut self, id: &JobId) -> Result<(), RuntimeError> {
        let report = self.registry.job_path(id).join("result.json");
        let active = self
            .active
            .get(id)
            .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
        match active
            .directory
            .metadata(std::ffi::OsStr::new("result.json"))
        {
            Ok(_) => {
                let result = match read_result(&active.directory, &report, id) {
                    Ok(result) => result,
                    Err(error) => {
                        self.finish_owned(
                            id,
                            Finish::Failure(format!("worker report rejected: {error}")),
                        )?;
                        return Ok(());
                    }
                };
                self.finish_owned(id, Finish::Result(result))?;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let active = self
                    .active
                    .get_mut(id)
                    .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
                if active
                    .child
                    .observe_exit()
                    .map_err(|error| io_error("observe retained worker", &report, error))?
                {
                    let finish = if active
                        .deadline
                        .is_some_and(|deadline| Instant::now() >= deadline)
                    {
                        Finish::Cancel(CancellationReason::Deadline)
                    } else {
                        Finish::Failure("worker exited without a durable command result".into())
                    };
                    self.finish_owned(id, finish)?;
                } else if active.deadline.is_some_and(|deadline| {
                    Instant::now().saturating_duration_since(deadline) >= Duration::from_secs(10)
                }) {
                    self.finish_owned(id, Finish::Cancel(CancellationReason::Deadline))?;
                }
            }
            Err(error) => return Err(io_error("inspect worker report", &report, error)),
        }
        Ok(())
    }

    fn start_job(&mut self, id: &JobId, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        if cancel.is_cancelled() {
            self.cancel(id)?;
            return Ok(());
        }
        let job = self.status(id)?;
        let request = self.pending.get(id).cloned().ok_or_else(|| {
            RuntimeError::Invalid(
                "queued launch inputs are absent; recovered queues must remain cancelled".into(),
            )
        })?;
        if let Err(error) = job.request.workspace.revalidate(cancel) {
            if cancel.is_cancelled() {
                self.cancel(id)?;
                return Ok(());
            }
            self.fail_before_launch(id, error.to_string())?;
            return Ok(());
        }
        // No secret-bearing artifact is created until the engine grants the
        // exclusive live-writer lease. A busy native writer leaves a retryable queue.
        let repository = match izu_engine::Repository::open(
            &request.workspace.repository,
            izu_engine::RepositoryOptions::default(),
        ) {
            Ok(repository) => repository,
            Err(error) => {
                self.fail_before_launch(id, error.to_string())?;
                return Err(error.into());
            }
        };
        let lease = match repository.lease_workspace(
            request.workspace.id,
            request.workspace.expectation(),
            cancel,
        ) {
            Ok(lease) => lease,
            Err(error) => {
                if matches!(error, izu_engine::EngineError::WorkspaceBusy { .. }) {
                    self.replace_state(
                        id,
                        JobState::Queued {
                            blocked: Some(AdmissionBlock::WorkspaceWriter),
                        },
                        None,
                    )?;
                } else {
                    // Lease setup can publish intent before returning a handle.
                    // Do not release the admission reservation if that publication
                    // or its cleanup cannot be ruled out with fresh source state.
                    let intent =
                        repository.writer_intent(request.workspace.id, &CancellationToken::new());
                    match intent {
                        Ok(None) if cancel.is_cancelled() => self.replace_state(
                            id,
                            JobState::Cancelled {
                                finished_at_unix_ms: now_ms()?,
                                reason: CancellationReason::Requested,
                            },
                            None,
                        )?,
                        Ok(None) => self.fail_before_launch(id, error.to_string())?,
                        _ => self.replace_state(
                            id,
                            JobState::OwnershipUnknown {
                                observed_at_unix_ms: now_ms()?,
                                worker_pid: None,
                                reason: format!(
                                    "source lease setup failed; writer intent may exist without an owned handle: {error}"
                                ),
                            },
                            None,
                        )?,
                    }
                }
                return Err(error.into());
            }
        };
        let token = lease.intent().token.clone();
        if let Err(error) = self.transact(|runtime| {
            let mut next = runtime.state.clone();
            next.jobs
                .get_mut(id)
                .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?
                .writer_intent = Some(token);
            runtime.commit(next)
        }) {
            let _ = finish_stopped_source(lease);
            return Err(error);
        }
        if let Err(error) = request
            .workspace
            .validate_leased(&repository, &lease, cancel)
        {
            if cancel.is_cancelled() {
                self.finish_unlaunched(id, lease, None)?;
            } else {
                self.fail_unlaunched(id, lease, error.to_string())?;
            }
            return Err(error);
        }
        let starting_environment = match request.environment_binding {
            Some(environment_id) => {
                let verified = (|| {
                    let binding =
                        crate::environment::read_binding(&repository, environment_id, cancel)?;
                    let cache = crate::environment::open_cache(&repository)?;
                    Ok::<_, RuntimeError>(cache.verify_starting_environment(
                        &binding,
                        &repository,
                        &lease,
                        cancel,
                    )?)
                })();
                match verified {
                    Ok(proof) => Some(proof),
                    Err(error) => {
                        if cancel.is_cancelled() {
                            self.finish_unlaunched(id, lease, None)?;
                        } else {
                            self.fail_unlaunched(id, lease, error.to_string())?;
                        }
                        return Err(error);
                    }
                }
            }
            None => None,
        };
        if let Some(proof) = &starting_environment {
            let receipt = proof.receipt().clone();
            if let Err(error) = self.transact(|runtime| {
                let mut next = runtime.state.clone();
                next.jobs
                    .get_mut(id)
                    .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?
                    .starting_environment = Some(receipt);
                runtime.commit(next)
            }) {
                let _ = finish_stopped_source(lease);
                return Err(error);
            }
        }
        if cancel.is_cancelled() {
            self.finish_unlaunched(id, lease, None)?;
            return Err(RuntimeError::Cancelled);
        }
        let dir = self.registry.job_path(id);
        let directory = match self.registry.open_job_directory(id) {
            Ok(directory) => directory,
            Err(error) => {
                self.fail_unlaunched(id, lease, error.to_string())?;
                return Err(error);
            }
        };
        let ticket = dir.join("ticket.json");
        if let Err(error) = write_ticket(&directory, &ticket, id.clone(), &request) {
            self.fail_unlaunched(id, lease, error.to_string())?;
            return Err(error);
        }
        let started = match now_ms() {
            Ok(started) => started,
            Err(error) => {
                self.fail_unlaunched(id, lease, error.to_string())?;
                return Err(error);
            }
        };
        let duration = Duration::from_millis(job.request.timeout_ms);
        if Instant::now().checked_add(duration).is_none()
            || i64::try_from(job.request.timeout_ms)
                .ok()
                .and_then(|ms| started.checked_add(ms))
                .is_none()
        {
            self.fail_unlaunched(id, lease, "execution deadline overflow".into())?;
            return Err(RuntimeError::Invalid("execution deadline overflow".into()));
        }
        let mut command = Command::new(&self.config.worker_executable);
        command
            .args(&self.config.worker_prefix)
            .arg(&ticket)
            .current_dir(job.request.workspace.cwd());
        let mut child = match crate::os::spawn_group(&mut command) {
            Ok(child) => child,
            Err(error) => {
                self.fail_unlaunched(id, lease, error.to_string())?;
                return Ok(());
            }
        };
        let control = match child.take_stdin() {
            Some(control) => control,
            None => {
                return self.fail_spawned_setup(
                    id,
                    child,
                    lease,
                    "owned worker control pipe missing".into(),
                );
            }
        };
        let output = match OutputReaders::start(&mut child, self.config.output_bytes_per_stream) {
            Ok(output) => output,
            Err(error) => return self.fail_spawned_setup(id, child, lease, error.to_string()),
        };
        let state = JobState::Running {
            worker_pid: child.id(),
            started_at_unix_ms: started,
            deadline_unix_ms: None,
        };
        self.active.insert(
            id.clone(),
            OwnedWorker {
                child,
                control,
                output,
                deadline: None,
                execution_deadline_unix_ms: None,
                directory,
                lease,
                _environment: starting_environment,
            },
        );
        if let Err(error) = self.replace_state(id, state, None) {
            if let Some(mut active) = self.active.remove(id) {
                let stopped = crate::os::kill_owned_group(&mut active.child);
                active.output.stop();
                if stopped.is_ok() {
                    let _ = finish_stopped_source(active.lease);
                }
                if let Err(expiry_error) = crate::registry::expire_ticket(&active.directory, &dir) {
                    return Err(RuntimeError::DurabilityUncertain(format!(
                        "{error}; launch ticket cleanup also failed: {expiry_error}"
                    )));
                }
            }
            return Err(error);
        }
        let release_time = now_ms().and_then(|time| {
            i64::try_from(job.request.timeout_ms)
                .map_err(|_| RuntimeError::Invalid("timeout exceeds clock range".into()))
                .and_then(|ms| {
                    time.checked_add(ms)
                        .ok_or_else(|| RuntimeError::Invalid("execution deadline overflow".into()))
                })
        });
        let deadline = Instant::now().checked_add(duration);
        let execution_deadline = match (release_time, deadline) {
            (Ok(time), Some(deadline)) => (time, deadline),
            _ => {
                self.finish_owned(id, Finish::Failure("execution deadline unavailable".into()))?;
                return Err(RuntimeError::Invalid(
                    "execution deadline unavailable".into(),
                ));
            }
        };
        let active = self
            .active
            .get_mut(id)
            .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
        active.deadline = Some(execution_deadline.1);
        active.execution_deadline_unix_ms = Some(execution_deadline.0);
        if let Err(error) = release_writer(&mut active.control, &active.directory, &dir, id) {
            self.finish_owned(id, Finish::Failure(error.to_string()))?;
        }
        self.pending.remove(id);
        Ok(())
    }

    fn finish_owned(&mut self, id: &JobId, finish: Finish) -> Result<(), RuntimeError> {
        let mut active = self
            .active
            .remove(id)
            .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
        let started = match self.status(id)?.state {
            JobState::Running {
                started_at_unix_ms, ..
            } => started_at_unix_ms,
            _ => {
                return Err(RuntimeError::Invalid(
                    "owned worker has no running registry record".into(),
                ));
            }
        };
        let mut stopped = crate::os::kill_owned_group(&mut active.child);
        let output = active.output.stop();
        if stopped.is_ok() {
            stopped = finish_stopped_source(active.lease);
        }
        if let Err(error) =
            crate::registry::expire_ticket(&active.directory, &self.registry.job_path(id))
        {
            self.poisoned = true;
            return Err(error);
        }
        let state = match stopped {
            Err(error) => JobState::OwnershipUnknown {
                observed_at_unix_ms: now_ms()?,
                worker_pid: Some(active.child.id()),
                reason: error.to_string(),
            },
            Ok(()) => match finish {
                Finish::Result(result) => JobState::Finished {
                    started_at_unix_ms: started,
                    finished_at_unix_ms: result.finished_at_unix_ms,
                    outcome: result.outcome,
                },
                Finish::Cancel(reason) => JobState::Cancelled {
                    finished_at_unix_ms: now_ms()?,
                    reason,
                },
                Finish::Failure(message) => JobState::Finished {
                    started_at_unix_ms: started,
                    finished_at_unix_ms: now_ms()?,
                    outcome: CommandOutcome::SpawnFailed { message },
                },
            },
        };
        self.replace_state(id, state, Some(output))
    }

    fn fail_unlaunched(
        &mut self,
        id: &JobId,
        lease: izu_engine::WorkspaceWriterLease,
        message: String,
    ) -> Result<(), RuntimeError> {
        self.finish_unlaunched(id, lease, Some(message))
    }
    fn finish_unlaunched(
        &mut self,
        id: &JobId,
        lease: izu_engine::WorkspaceWriterLease,
        failure: Option<String>,
    ) -> Result<(), RuntimeError> {
        let stopped = finish_stopped_source(lease);
        self.registry.expire_job_ticket(id)?;
        match stopped {
            Ok(()) => match failure {
                Some(message) => self.fail_before_launch(id, message),
                None => self.replace_state(
                    id,
                    JobState::Cancelled {
                        finished_at_unix_ms: now_ms()?,
                        reason: CancellationReason::Requested,
                    },
                    None,
                ),
            },
            Err(error) => self.replace_state(
                id,
                JobState::OwnershipUnknown {
                    observed_at_unix_ms: now_ms()?,
                    worker_pid: None,
                    reason: format!(
                        "launch setup ended: {}; source intent clearance failed: {error}",
                        failure.unwrap_or_else(|| "cancelled before command release".into())
                    ),
                },
                None,
            ),
        }
    }
    fn fail_spawned_setup(
        &mut self,
        id: &JobId,
        mut child: izu_process::OwnedProcess,
        lease: izu_engine::WorkspaceWriterLease,
        message: String,
    ) -> Result<(), RuntimeError> {
        let mut stopped = crate::os::kill_owned_group(&mut child);
        if stopped.is_ok() {
            stopped = finish_stopped_source(lease);
        }
        self.registry.expire_job_ticket(id)?;
        match stopped {
            Ok(()) => self.fail_before_launch(id, message),
            Err(error) => self.replace_state(
                id,
                JobState::OwnershipUnknown {
                    observed_at_unix_ms: now_ms()?,
                    worker_pid: Some(child.id()),
                    reason: format!("setup failed: {message}; cleanup uncertain: {error}"),
                },
                None,
            ),
        }
    }

    fn fail_before_launch(&mut self, id: &JobId, message: String) -> Result<(), RuntimeError> {
        self.registry.expire_job_ticket(id)?;
        let now = now_ms()?;
        self.replace_state(
            id,
            JobState::Finished {
                started_at_unix_ms: now,
                finished_at_unix_ms: now,
                outcome: CommandOutcome::SpawnFailed { message },
            },
            None,
        )
    }
    fn set_blocked(&mut self, id: &JobId, block: AdmissionBlock) -> Result<(), RuntimeError> {
        let state = JobState::Queued {
            blocked: Some(block),
        };
        if self.status(id)?.state != state {
            self.replace_state(id, state, None)?;
        }
        Ok(())
    }
    fn replace_state(
        &mut self,
        id: &JobId,
        state: JobState,
        output: Option<JobOutput>,
    ) -> Result<(), RuntimeError> {
        self.transact(|runtime| {
            let terminal = state.is_terminal();
            let mut next = runtime.state.clone();
            let job = next
                .jobs
                .get_mut(id)
                .ok_or_else(|| RuntimeError::UnknownJob(id.clone()))?;
            job.state = state;
            if let Some(output) = output {
                job.output = output;
            }
            runtime.commit(next)?;
            if terminal {
                runtime.pending.remove(id);
                runtime.controllers.remove(id);
            }
            Ok(())
        })
    }
    pub(crate) fn commit(&mut self, next: RegistryState) -> Result<(), RuntimeError> {
        self.ensure_usable()?;
        if self.transaction.is_none() {
            return Err(RuntimeError::Invalid(
                "registry publication requires an admission transaction".into(),
            ));
        }
        if let Err(error) = self.registry.save(&next) {
            self.poisoned = true;
            return Err(error);
        }
        self.state = next;
        Ok(())
    }
    fn current_state(&self) -> Result<RegistryState, RuntimeError> {
        self.ensure_usable()?;
        if self.transaction.is_some() {
            return Ok(self.state.clone());
        }
        let _transaction = self.registry.transaction()?;
        self.registry.read_state(&self.config)
    }
    fn transact<T>(
        &mut self,
        action: impl FnOnce(&mut Self) -> Result<T, RuntimeError>,
    ) -> Result<T, RuntimeError> {
        if self.transaction.is_some() {
            return action(self);
        }
        self.ensure_usable()?;
        let lock = self.registry.transaction()?;
        self.state = self
            .registry
            .refresh(&self.config, &self.controllers.keys().cloned().collect())?;
        self.transaction = Some(lock);
        let result = action(self);
        self.transaction.take();
        result
    }
    pub(crate) fn record_workspace_closed(&mut self, id: WorkspaceId) -> Result<(), RuntimeError> {
        self.transact(|runtime| {
            let mut next = runtime.state.clone();
            if next.workspaces.iter().any(|binding| binding.id == id) {
                next.closed_workspaces.insert(id);
            }
            runtime.commit(next)
        })
    }
    fn ensure_usable(&self) -> Result<(), RuntimeError> {
        if self.poisoned {
            Err(RuntimeError::DurabilityUncertain(
                "registry publication failed; reopen to reconcile before further actions".into(),
            ))
        } else {
            self.registry.check_locator()
        }
    }
}

enum Finish {
    Result(crate::worker::WorkerResult),
    Cancel(CancellationReason),
    Failure(String),
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Only this controller's handles authorize signals. Lost registry ACKs
        // remain unknown; shutdown never changes a foreign live controller job.
        if !self.poisoned {
            let _ = self.shutdown();
        }
        for (id, mut active) in std::mem::take(&mut self.active) {
            let stopped = crate::os::kill_owned_group(&mut active.child);
            active.output.stop();
            if stopped.is_ok() {
                let _ = finish_stopped_source(active.lease);
            }
            let _ = crate::registry::expire_ticket(&active.directory, &self.registry.job_path(&id));
        }
    }
}
