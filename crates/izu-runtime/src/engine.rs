use std::path::{Path, PathBuf};
use std::time::Duration;

use izu_engine::{
    CheckpointReceipt, Repository, RepositoryOptions, Selection, WorkspaceExpectation,
};
use izu_model::{
    CancellationToken, CandidateId, CheckEvidence, CheckOutcome, EvidenceId, OperationId,
    WorkspaceId,
};
use izu_platform::Directory;
use serde::{Deserialize, Serialize};

use crate::types::io_error;
use crate::*;

fn engine_error(error: izu_engine::EngineError) -> RuntimeError {
    RuntimeError::Engine(error)
}
fn close_uncertain(operation: OperationId, reason: String) -> RuntimeError {
    // Keep the visible operation typed for callers inspecting an incomplete close.
    RuntimeError::Engine(izu_engine::EngineError::PublicationUncertain { operation, reason })
}
pub(crate) fn check_cancel(cancel: &CancellationToken) -> Result<(), RuntimeError> {
    if cancel.is_cancelled() {
        Err(RuntimeError::Cancelled)
    } else {
        Ok(())
    }
}

impl WorkspaceBinding {
    pub fn checked(
        repository: &Repository,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        cancel: &CancellationToken,
    ) -> Result<Self, RuntimeError> {
        check_cancel(cancel)?;
        let workspace = repository.workspace(id, cancel).map_err(engine_error)?;
        if workspace.expected != expected {
            return Err(RuntimeError::StaleWorkspace(format!(
                "expected metadata differs for {id}"
            )));
        }
        let cwd = PathBuf::from(&workspace.record.root);
        let canonical = cwd
            .canonicalize()
            .map_err(|error| io_error("resolve registered writer cwd", &cwd, error))?;
        if canonical != cwd {
            return Err(RuntimeError::StaleWorkspace(
                "registered cwd is not canonical".into(),
            ));
        }
        let directory = crate::os::directory_identity(&cwd)?;
        let tree = repository
            .capture(id, Selection::All, cancel)
            .map_err(engine_error)?;
        if tree.tree != expected.working_tree {
            return Err(RuntimeError::StaleWorkspace("current source differs from the selected expected tree; checkpoint it before launch".into()));
        }
        let locator = repository.root_path().to_path_buf();
        Ok(Self {
            id,
            repository: locator,
            cwd,
            head: expected.head,
            tree: expected.working_tree,
            directory,
        })
    }
    pub fn load(
        repository: &Repository,
        id: WorkspaceId,
        cancel: &CancellationToken,
    ) -> Result<Self, RuntimeError> {
        let workspace = repository.workspace(id, cancel).map_err(engine_error)?;
        Self::checked(repository, id, workspace.expected, cancel)
    }
    pub fn expectation(&self) -> WorkspaceExpectation {
        WorkspaceExpectation {
            head: self.head,
            working_tree: self.tree,
        }
    }
    pub(crate) fn revalidate(&self, cancel: &CancellationToken) -> Result<(), RuntimeError> {
        let repository = Repository::open(&self.repository, RepositoryOptions::default())
            .map_err(engine_error)?;
        let current = Self::checked(&repository, self.id, self.expectation(), cancel)?;
        if &current != self {
            return Err(RuntimeError::StaleWorkspace(
                "workspace root or directory identity changed".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_directory(&self) -> Result<(), RuntimeError> {
        if crate::os::directory_identity(&self.cwd)? != self.directory {
            return Err(RuntimeError::StaleWorkspace(
                "workspace source directory was replaced; inspect displaced original source".into(),
            ));
        }
        Ok(())
    }
    pub(crate) fn validate_leased(
        &self,
        repository: &Repository,
        lease: &izu_engine::WorkspaceWriterLease,
        cancel: &CancellationToken,
    ) -> Result<(), RuntimeError> {
        check_cancel(cancel)?;
        if repository.root_path() != self.repository
            || lease.state().id != self.id
            || lease.state().expected != self.expectation()
            || lease.root_path() != self.cwd
            || crate::os::directory_identity_handle(lease.source_directory(), &self.cwd)?
                != self.directory
        {
            return Err(RuntimeError::StaleWorkspace(
                "leased source binding changed before launch".into(),
            ));
        }
        let captured = repository.capture_leased(lease, Selection::All, cancel)?;
        if captured.expected != self.expectation() || captured.tree != self.tree {
            return Err(RuntimeError::StaleWorkspace(
                "leased source differs from the selected expected tree".into(),
            ));
        }
        self.validate_directory()
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CloseReceipt {
    pub workspace: WorkspaceId,
    pub checkpoint: CheckpointReceipt,
    pub close_operation: OperationId,
    pub retained_path: PathBuf,
    /// Always false: stopping cooperating groups is not security containment.
    pub unmanaged_writers_contained: bool,
    pub source_files_deleted: bool,
}

impl Runtime {
    /// Stops/reaps owned writers and verifies engine persistence before closing.
    /// Source files, ignored/generated files and external DBs are never deleted.
    pub fn close_workspace(
        &mut self,
        repository: &Repository,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        cancel: &CancellationToken,
    ) -> Result<CloseReceipt, RuntimeError> {
        check_cancel(cancel)?;
        let _source_operation = self.source_operation_permit(cancel)?;
        let workspace = repository.workspace(id, cancel).map_err(engine_error)?;
        if workspace.expected != expected {
            return Err(RuntimeError::StaleWorkspace(format!(
                "close expectation differs for {id}"
            )));
        }
        if Path::new(&workspace.record.root) == repository.root_path() {
            return Err(RuntimeError::UnsafeClose(
                "repository source workspace cannot be closed".into(),
            ));
        }
        self.stop_workspace(id)?;
        // A completed worker no longer holds its source lease. The engine pins
        // the source present at call entry, so retain the runtime's earlier
        // directory binding when deciding which completed work may be closed.
        let binding = self.registered_binding(id)?;
        if let Some(binding) = &binding {
            if binding.repository != repository.root_path()
                || binding.cwd != Path::new(&workspace.record.root)
            {
                return Err(RuntimeError::StaleWorkspace(
                    "registered runtime source locator differs before close".into(),
                ));
            }
            binding.validate_directory()?;
        }
        let checkpoint = repository
            .checkpoint(id, expected, Selection::All, cancel)
            .map_err(engine_error)?;
        let verified = repository.workspace(id, cancel).map_err(|error| {
            close_uncertain(
                checkpoint.operation,
                format!("workspace readback after checkpoint failed; close is incomplete: {error}"),
            )
        })?;
        let checkpoint_operation =
            repository
                .operation(checkpoint.operation, cancel)
                .map_err(|error| {
                    close_uncertain(
                        checkpoint.operation,
                        format!(
                            "checkpoint operation readback failed; close is incomplete: {error}"
                        ),
                    )
                })?;
        if checkpoint.workspace != id
            || verified.expected.head != checkpoint.head
            || verified.expected.working_tree != checkpoint.tree
            || !checkpoint_operation.view.workspaces.contains_key(&id)
        {
            return Err(close_uncertain(
                checkpoint.operation,
                "engine checkpoint receipt did not match durable readback".into(),
            ));
        }
        repository.tree(checkpoint.tree, cancel).map_err(|error| {
            close_uncertain(
                checkpoint.operation,
                format!("checkpoint tree readback failed; close is incomplete: {error}"),
            )
        })?;
        if let Some(binding) = &binding {
            binding.validate_directory().map_err(|error| {
                close_uncertain(
                    checkpoint.operation,
                    format!("bound source identity changed before close: {error}; inspect displaced original source and retained registration"),
                )
            })?;
        }
        let close_operation = repository
            .close_workspace(id, verified.expected, cancel)
            .map_err(|error| {
                close_uncertain(
                    error.uncertain_operation().unwrap_or(checkpoint.operation),
                    format!(
                        "workspace close after checkpoint {} failed: {error}",
                        checkpoint.operation
                    ),
                )
            })?;
        let closed = repository
            .operation(close_operation, cancel)
            .map_err(|error| {
                close_uncertain(
                    close_operation,
                    format!("close operation readback failed: {error}"),
                )
            })?;
        if closed.view.workspaces.contains_key(&id) {
            return Err(close_uncertain(
                close_operation,
                "engine close receipt still contains workspace registration".into(),
            ));
        }
        if let Some(binding) = &binding {
            binding.validate_directory().map_err(|error| {
                close_uncertain(
                    close_operation,
                    format!("bound source identity changed before acknowledgement: {error}; inspect displaced original source and retained history"),
                )
            })?;
        }
        let receipt = CloseReceipt {
            workspace: id,
            checkpoint,
            close_operation,
            retained_path: PathBuf::from(workspace.record.root),
            unmanaged_writers_contained: false,
            source_files_deleted: false,
        };
        self.record_workspace_closed(id).map_err(|error| {
            close_uncertain(
                receipt.close_operation,
                format!("runtime close acknowledgement failed: {error}"),
            )
        })?;
        Ok(receipt)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct CheckExecution {
    pub job: JobSnapshot,
    pub evidence: EvidenceId,
    pub workspace_close: CloseReceipt,
}

/// Executes one explicitly selected immutable candidate check. The caller must
/// confirm any supplied argv against the candidate's declared command first.
pub fn run_check(
    runtime: &mut Runtime,
    repository: &Repository,
    candidate_id: CandidateId,
    check: &str,
    timeout: Duration,
    cancel: &CancellationToken,
) -> Result<CheckExecution, RuntimeError> {
    check_cancel(cancel)?;
    let candidate = repository
        .candidate(candidate_id, cancel)
        .map_err(engine_error)?;
    let spec = candidate
        .checks
        .iter()
        .find(|spec| spec.name == check)
        .ok_or_else(|| RuntimeError::Invalid(format!("candidate has no check named {check}")))?;
    // Publish the attempt before admission, environment realization or launch.
    // Every subsequent failure keeps the old pass superseded, including crashes.
    let attempt = repository
        .start_check(candidate_id, check, cancel)
        .map_err(engine_error)?;
    let evidence = attempt.pending;
    runtime.ensure_workspace_capacity()?;
    let environment = spec
        .environment
        .map(|id| crate::environment::read_binding(repository, id, cancel))
        .transpose()?;
    let parent_path = repository.metadata_path().join("check-workspaces");
    let metadata = Directory::open(repository.metadata_path())
        .map_err(|error| io_error("pin engine metadata", repository.metadata_path(), error))?;
    let parent = metadata
        .ensure_dir(std::ffi::OsStr::new("check-workspaces"))
        .map_err(|error| io_error("create private check workspace parent", &parent_path, error))?;
    crate::os::check_private_directory_handle(&parent, &parent_path)?;
    metadata.sync().map_err(|error| {
        RuntimeError::DurabilityUncertain(format!("check workspace parent: {error}"))
    })?;
    let identity = JobId::generate()?;
    let path = parent_path.join(identity.as_str());
    let workspace = repository
        .fork_workspace(format!("check-{identity}"), &path, candidate.result, cancel)
        .map_err(engine_error)?;
    if workspace.expected.head != candidate.result
        || workspace.expected.working_tree != candidate.result_tree
    {
        return Err(RuntimeError::StaleWorkspace(
            "materialized check workspace does not match candidate inputs".into(),
        ));
    }
    if let Some(environment) = &environment {
        let cache = crate::environment::open_cache(repository)?;
        let mut target = izu_environment::WorkspaceTarget::checked(
            repository,
            workspace.id,
            workspace.expected,
            cancel,
        )?;
        // Preparation is an explicit file materialization. Recipes are data and
        // are never executed by discovery, parsing or this check helper.
        cache.materialize_binding(
            environment,
            &mut target,
            izu_environment::SharingPolicy::PreferClone,
            cancel,
        )?;
    }
    let binding = WorkspaceBinding::checked(repository, workspace.id, workspace.expected, cancel)?;
    let mut request = RunRequest::new(binding, spec.argv.clone()).with_timeout(timeout)?;
    request.environment_binding = spec.environment;
    let id = runtime.submit(request)?;
    let job = match runtime.wait(&id, cancel) {
        Ok(job) => job,
        Err(error) => {
            let _ = runtime.cancel(&id);
            return Err(error);
        }
    };
    let (outcome, finished) = match &job.state {
        JobState::Finished {
            finished_at_unix_ms,
            outcome,
            ..
        } => {
            let check_outcome = if outcome.passed() {
                CheckOutcome::Passed
            } else {
                CheckOutcome::Failed {
                    exit_code: match outcome {
                        CommandOutcome::Exit { code, .. } => *code,
                        CommandOutcome::SpawnFailed { .. } => None,
                    },
                }
            };
            (check_outcome, *finished_at_unix_ms)
        }
        JobState::Cancelled {
            finished_at_unix_ms,
            ..
        } => (CheckOutcome::Cancelled, *finished_at_unix_ms),
        JobState::OwnershipUnknown {
            observed_at_unix_ms,
            reason,
            ..
        } => {
            publish_check_evidence(
                repository,
                evidence,
                CheckOutcome::Failed { exit_code: None },
                *observed_at_unix_ms,
                &CancellationToken::new(),
            )?;
            return Err(RuntimeError::UnsafeClose(format!(
                "check cleanup is uncertain: {reason}; failed attempt, writer intent and workspace files retained"
            )));
        }
        _ => {
            return Err(RuntimeError::UnsafeClose(
                "check did not reach an observed stopped state; pending attempt and workspace retained"
                    .into(),
            ));
        }
    };
    let fresh_cancel = CancellationToken::new();
    if let Err(error) = job.request.workspace.validate_directory() {
        publish_check_evidence(
            repository,
            evidence,
            CheckOutcome::Failed { exit_code: None },
            finished,
            &fresh_cancel,
        )?;
        return Err(error);
    }
    if let Some(environment) = &environment {
        let valid = job.request.environment_binding == spec.environment
            && job.starting_environment.as_ref().is_some_and(|receipt| {
                receipt.workspace == workspace.id
                    && receipt.key == environment.key()
                    && receipt.manifest_digest == environment.manifest_digest()
                    && receipt.source_identity == environment.source_identity()
                    && receipt.scope
                        == izu_environment::EnvironmentVerificationScope::StartingFileContents
                    && !receipt.security_boundary
            });
        if !valid {
            publish_check_evidence(
                repository,
                evidence,
                CheckOutcome::Failed { exit_code: None },
                finished,
                &fresh_cancel,
            )?;
            return Err(RuntimeError::Invalid(
                "bound check has no matching verified starting-file receipt; workspace retained"
                    .into(),
            ));
        }
    }
    let after = match repository.capture(workspace.id, Selection::All, &fresh_cancel) {
        Ok(after) => after,
        Err(error) => {
            publish_check_evidence(
                repository,
                evidence,
                CheckOutcome::Failed { exit_code: None },
                finished,
                &fresh_cancel,
            )?;
            return Err(engine_error(error));
        }
    };
    if let Err(error) = job.request.workspace.validate_directory() {
        publish_check_evidence(
            repository,
            evidence,
            CheckOutcome::Failed { exit_code: None },
            finished,
            &fresh_cancel,
        )?;
        return Err(error);
    }
    let source_valid = after.tree == candidate.result_tree
        && after.expected.head == candidate.result
        && after.expected.working_tree == candidate.result_tree;
    let evidence_id = publish_check_evidence(
        repository,
        evidence,
        if source_valid {
            outcome
        } else {
            CheckOutcome::Failed { exit_code: None }
        },
        finished,
        &fresh_cancel,
    )?;
    let workspace_close =
        runtime.close_workspace(repository, workspace.id, after.expected, &fresh_cancel)?;
    if !source_valid {
        return Err(RuntimeError::StaleWorkspace("check changed candidate source or revision binding; failed evidence recorded and workspace files retained".into()));
    }
    Ok(CheckExecution {
        job,
        evidence: evidence_id,
        workspace_close,
    })
}

fn publish_check_evidence(
    repository: &Repository,
    mut evidence: CheckEvidence,
    outcome: CheckOutcome,
    finished: i64,
    cancel: &CancellationToken,
) -> Result<EvidenceId, RuntimeError> {
    evidence.outcome = outcome;
    evidence.finished_at_unix_ms = Some(finished);
    let id = repository.record_evidence(&evidence, cancel)?;
    if repository.evidence(id, cancel)? != evidence {
        return Err(RuntimeError::DurabilityUncertain(
            "check evidence readback differs".into(),
        ));
    }
    Ok(id)
}
