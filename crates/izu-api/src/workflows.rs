//! Human conveniences resolve to exact engine inputs before any publication.
use super::*;
use izu_engine::{
    CheckpointReceipt, LandReceipt, MergePreparation, WorkspaceForkReceipt, WorkspaceState,
};
use izu_model::RevisionId;

#[derive(Serialize)]
struct ManagedReceipt {
    workspace: WorkspaceState,
    before: CheckpointReceipt,
    after: Option<CheckpointReceipt>,
    job: Option<izu_runtime::JobSnapshot>,
}

#[derive(Serialize)]
struct CreatedWorkspaceReceipt {
    workspace: WorkspaceState,
    fork_operation: izu_model::OperationId,
}

#[derive(Serialize)]
struct LandingReceipt {
    preparation: MergePreparation,
    checks: Vec<izu_runtime::CheckExecution>,
    land: Option<LandReceipt>,
}

pub struct LandingPlan {
    pub checks: Vec<CheckDefinition>,
    pub author: Author,
    pub timeout_seconds: u64,
}

impl Api {
    /// Resolve a unique registered name once. Machine mutation requests still
    /// carry the exact workspace ID and revision/tree expectation.
    pub fn named_context(
        &self,
        context: ReadContext,
        name: Option<&str>,
        cancel: &CancellationToken,
    ) -> Result<ReadContext, ApiError> {
        let Some(name) = name else {
            return Ok(context);
        };
        let repo = repository(&context)?;
        let state = named_workspace(&repo, name, cancel)?.ok_or_else(|| ApiError {
            code: ErrorCode::NotFound,
            message: format!("No open change named {name}"),
            next_action: "Use `izu start NAME -- PROGRAM ARGS` or inspect `izu workspace list`."
                .into(),
            operation_id: None,
            recovery_operation_id: None,
            retained_paths: Box::default(),
        })?;
        Ok(ReadContext {
            workspace: Some(state.id.to_string()),
            ..context
        })
    }

    pub fn resolve_selector(
        &self,
        context: &ReadContext,
        selector: &str,
        cancel: &CancellationToken,
    ) -> Result<String, ApiError> {
        let repo = repository(context)?;
        resolve(&repo, context, selector, cancel).map(|id| id.to_string())
    }

    pub fn commit_operation(
        &self,
        context: &ReadContext,
        selection: PathSelection,
        message: String,
        author: Author,
        cancel: &CancellationToken,
    ) -> Result<Operation, ApiError> {
        let exact = self.mutation_context(context, cancel)?;
        let repo = repository(context)?;
        let state = repo
            .workspace(parse(&exact.workspace, "workspace")?, cancel)
            .map_err(engine_error)?;
        let target = primary_reference(&repo, &state, cancel)?;
        Ok(Operation::Commit {
            context: exact,
            selection,
            message,
            author,
            target,
        })
    }

    pub fn start_operation(
        &self,
        context: &ReadContext,
        name: String,
        from: Option<&str>,
        launch: LaunchArguments,
        cancel: &CancellationToken,
    ) -> Result<Operation, ApiError> {
        validate_launch(&launch)?;
        let repo = repository(context)?;
        let target = match named_workspace(&repo, &name, cancel)? {
            Some(state) => {
                require_private(&repo, &state)?;
                if let Some(selector) = from
                    && resolve(&repo, context, selector, cancel)? != state.expected.head
                {
                    return Err(ApiError {
                        code: ErrorCode::StaleExpectation,
                        message: format!(
                            "Existing change {name} has a different revision from --from"
                        ),
                        next_action:
                            "Omit --from to resume this change, or choose a new change name.".into(),
                        operation_id: None,
                        recovery_operation_id: None,
                        retained_paths: Box::default(),
                    });
                }
                ManagedStartTarget::Resume {
                    context: context_for(context, &state),
                }
            }
            None => {
                let revision = match from {
                    Some(selector) => resolve(&repo, context, selector, cancel)?,
                    None => {
                        let main = RefName::new("main").map_err(model_error)?;
                        match repo
                            .view(cancel)
                            .map_err(engine_error)?
                            .refs
                            .get(&main)
                            .copied()
                        {
                            Some(revision) => revision,
                            None => {
                                repo.workspace(selected_workspace(&repo, context)?, cancel)
                                    .map_err(engine_error)?
                                    .expected
                                    .head
                            }
                        }
                    }
                };
                ManagedStartTarget::New {
                    context: context.clone(),
                    name,
                    from: revision.to_string(),
                }
            }
        };
        Ok(Operation::ManagedStart { target, launch })
    }

    pub fn revert_operation(
        &self,
        context: &ReadContext,
        revision: String,
        selection: PathSelection,
        message: String,
        author: Author,
        cancel: &CancellationToken,
    ) -> Result<Operation, ApiError> {
        let exact = self.mutation_context(context, cancel)?;
        let repo = repository(context)?;
        let state = repo
            .workspace(parse(&exact.workspace, "workspace")?, cancel)
            .map_err(engine_error)?;
        let target = primary_reference(&repo, &state, cancel)?;
        Ok(Operation::Revert {
            context: exact,
            revision: resolve(&repo, context, &revision, cancel)?.to_string(),
            selection,
            message,
            author,
            target,
        })
    }

    pub fn run_operation(
        &self,
        context: &ReadContext,
        launch: LaunchArguments,
        cancel: &CancellationToken,
    ) -> Result<Operation, ApiError> {
        validate_launch(&launch)?;
        let repo = repository(context)?;
        let state = repo
            .workspace(selected_workspace(&repo, context)?, cancel)
            .map_err(engine_error)?;
        require_private(&repo, &state)?;
        Ok(Operation::ManagedRun {
            context: context_for(context, &state),
            launch,
        })
    }

    pub fn land_operation(
        &self,
        context: &ReadContext,
        source: Option<&str>,
        target: String,
        plan: LandingPlan,
        cancel: &CancellationToken,
    ) -> Result<Operation, ApiError> {
        let LandingPlan {
            checks,
            author,
            timeout_seconds,
        } = plan;
        let repo = repository(context)?;
        let source = match source {
            Some(selector) => resolve(&repo, context, selector, cancel)?,
            None => {
                let workspace = selected_workspace(&repo, context)?;
                if !repo
                    .status(workspace, cancel)
                    .map_err(engine_error)?
                    .entries
                    .is_empty()
                {
                    return Err(ApiError::invalid(
                        "Current change has uncommitted edits. Commit the selected change before landing its intentional revision.",
                    ));
                }
                repo.workspace(workspace, cancel)
                    .map_err(engine_error)?
                    .expected
                    .head
            }
        };
        let name = RefName::new(target).map_err(model_error)?;
        let expected = repo
            .view(cancel)
            .map_err(engine_error)?
            .refs
            .get(&name)
            .copied();
        Ok(Operation::LandCurrent {
            context: context.clone(),
            source: source.to_string(),
            target: ReferenceExpectation {
                name: name.to_string(),
                expected: expected.map(|id| id.to_string()),
            },
            checks,
            author,
            timeout_seconds,
        })
    }

    pub(super) fn managed_start(
        &self,
        target: ManagedStartTarget,
        launch: LaunchArguments,
        cancel: &CancellationToken,
    ) -> Result<DispatchData, ApiError> {
        validate_launch(&launch)?;
        match target {
            ManagedStartTarget::New {
                context,
                name,
                from,
            } => {
                let repo = repository(&context)?;
                let from = parse(&from, "from revision")?;
                let runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                let source_operation = runtime
                    .source_operation_permit(cancel)
                    .map_err(runtime_error)?;
                let fork = repo
                    .fork_managed_workspace_receipt(name, from, cancel)
                    .map_err(engine_error)?;
                let started = self.managed_run_admitted(
                    context_for(&context, &fork.workspace),
                    launch,
                    cancel,
                    runtime,
                    source_operation,
                );
                managed_start_result(fork, started)
            }
            ManagedStartTarget::Resume { context } => self.managed_run(context, launch, cancel),
        }
    }

    pub(super) fn managed_run(
        &self,
        context: MutationContext,
        launch: LaunchArguments,
        cancel: &CancellationToken,
    ) -> Result<DispatchData, ApiError> {
        validate_launch(&launch)?;
        let (repo, _, _) = mutation(&context)?;
        let runtime =
            izu_runtime::Runtime::open(repo.metadata_path().join("runtime"), self.runtime_config())
                .map_err(runtime_error)?;
        let source_operation = runtime
            .source_operation_permit(cancel)
            .map_err(runtime_error)?;
        self.managed_run_admitted(context, launch, cancel, runtime, source_operation)
    }

    fn managed_run_admitted(
        &self,
        context: MutationContext,
        launch: LaunchArguments,
        cancel: &CancellationToken,
        mut runtime: izu_runtime::Runtime,
        source_operation: izu_runtime::SourceOperationPermit,
    ) -> Result<DispatchData, ApiError> {
        let (repo, workspace, expected) = mutation(&context)?;
        let state = repo.workspace(workspace, cancel).map_err(engine_error)?;
        require_private(&repo, &state)?;
        // Refuse known active or unreconciled writers before preparation can
        // publish history. Runtime admission checks ownership again at launch.
        drop(
            repo.lock_workspace(workspace, expected, cancel)
                .map_err(engine_error)?,
        );
        let before = repo
            .checkpoint(workspace, expected, Selection::All, cancel)
            .map_err(engine_error)?;
        let mut receipt = ManagedReceipt {
            workspace: state.clone(),
            before,
            after: None,
            job: None,
        };
        let state = match repo.workspace(workspace, cancel) {
            Ok(workspace) => workspace,
            Err(error) => return managed_partial(receipt, engine_error(error)),
        };
        receipt.workspace = state.clone();
        let launched = (|| {
            let binding =
                izu_runtime::WorkspaceBinding::checked(&repo, workspace, state.expected, cancel)
                    .map_err(runtime_error)?;
            let mut request = izu_runtime::RunRequest::new(binding, launch.argv)
                .with_timeout(Duration::from_secs(launch.timeout_seconds))
                .map_err(runtime_error)?;
            request.env_overlay = launch.env;
            // Source preparation is bounded separately; commands retain the
            // ordinary shared running-job reservations throughout execution.
            drop(source_operation);
            let job = runtime.submit(request).map_err(runtime_error)?;
            runtime.wait(&job, cancel).map_err(runtime_error)
        })();
        let job = match launched {
            Ok(job) => job,
            Err(error) => return managed_partial(receipt, error),
        };
        receipt.job = Some(job.clone());
        // Cancellation ends the chosen program. Preserve its stopped source
        // with a separate recovery token; cancellation is never a rollback.
        if matches!(
            job.state,
            izu_runtime::JobState::Finished { .. } | izu_runtime::JobState::Cancelled { .. }
        ) {
            let recovery_cancel = CancellationToken::new();
            let _source_operation = match runtime.source_operation_permit(&recovery_cancel) {
                Ok(permit) => permit,
                Err(error) => return managed_partial(receipt, runtime_error(error)),
            };
            match repo.checkpoint(workspace, state.expected, Selection::All, &recovery_cancel) {
                Ok(after) => {
                    receipt.after = Some(after);
                    receipt.workspace = match repo.workspace(workspace, &recovery_cancel) {
                        Ok(workspace) => workspace,
                        Err(error) => return managed_partial(receipt, engine_error(error)),
                    };
                }
                Err(error) => return managed_partial(receipt, engine_error(error)),
            }
        }
        let mut outcome = job_result(&receipt, &job, false)?;
        if let Some(error) = &mut outcome.uncertainty {
            error.operation_id.get_or_insert_with(|| {
                receipt
                    .after
                    .as_ref()
                    .unwrap_or(&receipt.before)
                    .operation
                    .to_string()
            });
        }
        Ok(outcome)
    }

    pub(super) fn land_current(
        &self,
        context: ReadContext,
        source: String,
        target: ReferenceExpectation,
        plan: LandingPlan,
        cancel: &CancellationToken,
    ) -> Result<DispatchData, ApiError> {
        let LandingPlan {
            checks,
            author,
            timeout_seconds,
        } = plan;
        timeout(timeout_seconds)?;
        if checks.is_empty() || checks.len() > 8 {
            return Err(ApiError::invalid(
                "Checked landing requires 1..=8 explicit checks; use candidate/check commands for larger plans",
            ));
        }
        let checks = parse_checks(checks)?;
        let repo = repository(&context)?;
        let prepared = repo
            .prepare_merge(
                RefName::new(target.name).map_err(model_error)?,
                parse_optional(target.expected, "expected target")?,
                parse(&source, "source revision")?,
                checks,
                identity(author)?,
                cancel,
            )
            .map_err(engine_error)?;
        let candidate = match &prepared {
            MergePreparation::Ready { candidate, .. } => *candidate,
            MergePreparation::Conflicted { .. } => return preparation(prepared),
        };
        let declared = repo
            .candidate(candidate, cancel)
            .map_err(engine_error)?
            .checks;
        let mut receipt = LandingReceipt {
            preparation: prepared,
            checks: Vec::new(),
            land: None,
        };
        let mut runtime =
            izu_runtime::Runtime::open(repo.metadata_path().join("runtime"), self.runtime_config())
                .map_err(runtime_error)?;
        for check in declared {
            let executed = match izu_runtime::run_check(
                &mut runtime,
                &repo,
                candidate,
                &check.name,
                Duration::from_secs(timeout_seconds),
                cancel,
            ) {
                Ok(executed) => executed,
                Err(error) => return partial(receipt, runtime_error(error)),
            };
            let job = executed.job.clone();
            receipt.checks.push(executed);
            let data = job_result(&receipt, &job, true)?;
            if data.uncertainty.is_some() {
                return Ok(data);
            }
        }
        match repo.land(candidate, cancel) {
            Ok(land) => {
                receipt.land = Some(land);
                value(receipt)
            }
            Err(error) => partial(receipt, engine_error(error)),
        }
    }
}

fn context_for(context: &ReadContext, state: &WorkspaceState) -> MutationContext {
    MutationContext {
        repository: context.repository.clone(),
        workspace: state.id.to_string(),
        expected: Expectation {
            head: state.expected.head.to_string(),
            working_tree: state.expected.working_tree.to_string(),
        },
    }
}

fn named_workspace(
    repo: &Repository,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<WorkspaceState>, ApiError> {
    if name.is_empty() || name.len() > 1024 {
        return Err(ApiError::invalid("Change name must contain 1..=1024 bytes"));
    }
    let view = repo.view(cancel).map_err(engine_error)?;
    let mut matches = view
        .workspaces
        .iter()
        .filter(|(_, record)| record.name == name);
    let first = matches.next().map(|(id, _)| *id);
    if matches.next().is_some() {
        return Err(ApiError::invalid(
            "Change name is ambiguous; select an exact workspace ID",
        ));
    }
    first
        .map(|id| repo.workspace(id, cancel).map_err(engine_error))
        .transpose()
}

fn require_private(repo: &Repository, state: &WorkspaceState) -> Result<(), ApiError> {
    if Path::new(&state.record.root) == repo.root_path() {
        return Err(ApiError { code: ErrorCode::InvalidRequest, message: "Managed human launches require a private change".into(), next_action: "Use `izu start NAME -- PROGRAM ARGS`, or resume with `izu --change NAME run -- PROGRAM ARGS`.".into(), operation_id: None, recovery_operation_id: None, retained_paths: Box::default() });
    }
    Ok(())
}

fn primary_reference(
    repo: &Repository,
    state: &WorkspaceState,
    cancel: &CancellationToken,
) -> Result<Option<ReferenceExpectation>, ApiError> {
    if Path::new(&state.record.root) != repo.root_path() {
        return Ok(None);
    }
    let name = RefName::new("main").map_err(model_error)?;
    let expected = repo
        .view(cancel)
        .map_err(engine_error)?
        .refs
        .get(&name)
        .copied();
    if expected.is_some_and(|head| head != state.expected.head) {
        return Err(ApiError { code: ErrorCode::StaleExpectation, message: "The primary workspace is at a different revision from main".into(), next_action: "Preserve any edits with a checkpoint or private change, then use `izu update` to move a clean primary workspace to main.".into(), operation_id: None, recovery_operation_id: None, retained_paths: Box::default() });
    }
    Ok(Some(ReferenceExpectation {
        name: name.to_string(),
        expected: expected.map(|id| id.to_string()),
    }))
}

fn resolve(
    repo: &Repository,
    context: &ReadContext,
    selector: &str,
    cancel: &CancellationToken,
) -> Result<RevisionId, ApiError> {
    if selector == "@" {
        return Ok(repo
            .workspace(selected_workspace(repo, context)?, cancel)
            .map_err(engine_error)?
            .expected
            .head);
    }
    repo.resolve_revision(selector, cancel)
        .map_err(engine_error)
}

fn validate_launch(launch: &LaunchArguments) -> Result<(), ApiError> {
    timeout(launch.timeout_seconds)?;
    if launch.argv.is_empty() || launch.argv.len() > 4096 || launch.argv[0].is_empty() {
        return Err(ApiError::invalid(
            "A managed launch requires an executable and at most 4096 arguments",
        ));
    }
    let mut bytes = 0usize;
    for arg in &launch.argv {
        if arg.contains('\0') {
            return Err(ApiError::invalid("Launch argument contains NUL"));
        }
        bytes = bytes
            .checked_add(arg.len() + 1)
            .ok_or_else(|| ApiError::invalid("Launch size overflow"))?;
    }
    if bytes > 128 * 1024 || launch.env.len() > 256 {
        return Err(ApiError::invalid(
            "Launch arguments exceed 128 KiB or environment exceeds 256 entries",
        ));
    }
    let mut environment_bytes = 0usize;
    for (name, value) in &launch.env {
        if name.is_empty() || name.contains(['=', '\0']) || value.contains('\0') {
            return Err(ApiError::invalid(
                "Invalid environment overlay name or value",
            ));
        }
        environment_bytes = environment_bytes
            .checked_add(name.len())
            .and_then(|size| size.checked_add(value.len()))
            .ok_or_else(|| ApiError::invalid("Environment size overflow"))?;
    }
    if environment_bytes > 128 * 1024 {
        return Err(ApiError::invalid("Environment overlay exceeds 128 KiB"));
    }
    Ok(())
}

fn partial(receipt: impl Serialize, error: ApiError) -> Result<DispatchData, ApiError> {
    let mut data = value(receipt)?;
    data.uncertainty = Some(error);
    Ok(data)
}

fn managed_partial(receipt: ManagedReceipt, mut error: ApiError) -> Result<DispatchData, ApiError> {
    // A setup/queue failure after checkpoint is not a rollback. Retain the
    // newer engine-reported publication when present, otherwise that checkpoint.
    error.operation_id.get_or_insert_with(|| {
        receipt
            .after
            .as_ref()
            .unwrap_or(&receipt.before)
            .operation
            .to_string()
    });
    error.code = ErrorCode::PublicationUncertain;
    partial(receipt, error)
}

// The engine returns the exact fork publication. Composing the next preparation
// step cannot turn a created workspace into an ordinary prepublication error.
fn managed_start_result(
    fork: WorkspaceForkReceipt,
    started: Result<DispatchData, ApiError>,
) -> Result<DispatchData, ApiError> {
    match started {
        Ok(data) => Ok(data),
        Err(mut error) => {
            error
                .operation_id
                .get_or_insert_with(|| fork.operation.to_string());
            error.retain_paths([PathBuf::from(&fork.workspace.record.root)]);
            error.code = ErrorCode::PublicationUncertain;
            error.message = format!(
                "Start is incomplete after creating workspace {} at {}: {}",
                fork.workspace.id, fork.workspace.record.root, error.message
            );
            error.next_action = "Inspect the exact reported operation and retained workspace before resuming. A failed start does not remove the created change or roll back source history.".into();
            partial(
                CreatedWorkspaceReceipt {
                    workspace: fork.workspace,
                    fork_operation: fork.operation,
                },
                error,
            )
        }
    }
}

pub(super) fn parse_checks(checks: Vec<CheckDefinition>) -> Result<Vec<CheckSpec>, ApiError> {
    if checks.len() > 1024 {
        return Err(ApiError::invalid(
            "At most 1024 check definitions are allowed",
        ));
    }
    checks
        .into_iter()
        .map(|check| {
            let check = CheckSpec {
                name: check.name,
                argv: check.argv,
                environment: parse_optional(check.environment, "environment binding object ID")?,
            };
            check.validate(&Limits::default()).map_err(model_error)?;
            Ok(check)
        })
        .collect()
}

pub(super) fn environment_recipe(
    input: &str,
    trusted: bool,
) -> Result<izu_environment::Recipe, ApiError> {
    if !trusted {
        return Err(ApiError::invalid(
            "Environment operations require explicit trust in the recipe and prepared code",
        ));
    }
    if input.len() > MAX_REQUEST_BYTES {
        return Err(ApiError::invalid("Environment recipe exceeds 1 MiB"));
    }
    izu_environment::Recipe::from_json(
        input.as_bytes(),
        izu_environment::TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )
    .map_err(environment_error)
}

pub(super) fn environment_cache(
    repo: &Repository,
    path: Option<PathBuf>,
) -> Result<izu_environment::EnvironmentCache, ApiError> {
    izu_environment::EnvironmentCache::open(
        path.unwrap_or_else(|| repo.metadata_path().join("environments/v1")),
        izu_environment::EnvironmentLimits::default(),
    )
    .map_err(environment_error)
}

pub(super) fn environment_sharing(sharing: EnvironmentSharing) -> izu_environment::SharingPolicy {
    match sharing {
        EnvironmentSharing::PreferClone => izu_environment::SharingPolicy::PreferClone,
        EnvironmentSharing::Copy => izu_environment::SharingPolicy::Copy,
    }
}

pub(super) fn environment_error(error: izu_environment::EnvironmentError) -> ApiError {
    use izu_environment::EnvironmentError as E;
    let error = match error {
        E::Workspace(error) => return engine_error(error),
        E::PublicationIdentityUncertain { path, source } => {
            let mut error = environment_error(*source);
            error.code = ErrorCode::PublicationUncertain;
            error.message = format!(
                "Environment publication at {path:?} has uncertain identity: {}",
                error.message
            );
            error.retain_paths([path]);
            error.next_action = "Publication may already be visible. Preserve the reported destination and pinned originals; inspect the exact paths and environment cache before retrying.".into();
            return error;
        }
        E::DurabilityUncertain { path, source } => {
            let mut error = failure(format!(
                "Environment publication at {path:?} is visible but durability is uncertain: {source}"
            ));
            error.code = ErrorCode::PublicationUncertain;
            error.retain_paths([path]);
            error.next_action = "Preserve and verify the exact published path before retrying; uncertainty is not rollback.".into();
            return error;
        }
        E::PreparationIncomplete { stage, source } => {
            let mut error = environment_error(*source);
            error.retain_paths([stage]);
            return error;
        }
        E::MaterializationIncomplete {
            published,
            stages,
            source,
        } => {
            let mut error = environment_error(*source);
            error.message = format!(
                "{}; materialization already published paths {published:?}",
                error.message
            );
            error.retain_paths(stages);
            return error;
        }
        error => error,
    };
    let code = match &error {
        E::Cancelled => ErrorCode::Cancelled,
        E::Io { source, .. } | E::LockIo { source, .. }
            if source.kind() == std::io::ErrorKind::Unsupported =>
        {
            ErrorCode::UnsupportedFeature
        }
        E::InvalidRecipe(_) | E::InvalidInput(_) | E::Limit { .. } | E::Json(_) => {
            ErrorCode::InvalidRequest
        }
        E::Missing(_) => ErrorCode::NotFound,
        E::Corrupt(_) => ErrorCode::CorruptData,
        E::Busy | E::DestinationNotEmpty(_) => ErrorCode::Conflict,
        E::SourceMismatch
        | E::LockfileMismatch(_)
        | E::InputChanged(_)
        | E::StartingEnvironmentMismatch(_)
        | E::NamespaceChanged(_) => ErrorCode::StaleExpectation,
        _ => ErrorCode::Failure,
    };
    ApiError { code, message: error.to_string(), next_action: "Preserve prepared source, published paths, and retained staging paths. Inspect environment status before retrying; materialization may already have changed part of the private workspace.".into(), operation_id: None, recovery_operation_id: None, retained_paths: Box::default() }
}

#[cfg(test)]
mod source_admission_tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::mpsc;
    use std::time::Duration;

    struct CancelOnDrop(CancellationToken);
    impl Drop for CancelOnDrop {
        fn drop(&mut self) {
            self.0.cancel();
        }
    }

    struct Fixture {
        _temporary: tempfile::TempDir,
        repo: Repository,
        context: ReadContext,
        api: Api,
        cancel: CancellationToken,
    }
    impl Fixture {
        fn new() -> Self {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
            std::fs::create_dir_all(&path).unwrap();
            let temporary = tempfile::tempdir_in(path).unwrap();
            let source = temporary.path().join("source");
            std::fs::create_dir(&source).unwrap();
            let repo = Repository::init(&source, RepositoryOptions::default()).unwrap();
            Self {
                _temporary: temporary,
                repo,
                context: ReadContext {
                    repository: source,
                    workspace: None,
                },
                api: Api::new(std::env::current_exe().unwrap()),
                cancel: CancellationToken::new(),
            }
        }
        fn fork(&self) -> WorkspaceForkReceipt {
            let source = self
                .repo
                .workspace(self.repo.workspace_id(), &self.cancel)
                .unwrap();
            self.repo
                .fork_managed_workspace_receipt(
                    "created".into(),
                    source.expected.head,
                    &self.cancel,
                )
                .unwrap()
        }
        fn runtime(&self) -> izu_runtime::Runtime {
            izu_runtime::Runtime::open(
                self.repo.metadata_path().join("runtime"),
                self.api.runtime_config(),
            )
            .unwrap()
        }
    }

    fn post_fork_failure(failure: &str) {
        let fixture = Fixture::new();
        let runtime = fixture.runtime();
        let permit = runtime.source_operation_permit(&fixture.cancel).unwrap();
        let fork = fixture.fork();
        let workspace = fork.workspace.clone();
        let exact_fork = fork.operation;
        let context = context_for(&fixture.context, &workspace);
        let cancel = CancellationToken::new();
        let before = match failure {
            "cancel" => {
                cancel.cancel();
                exact_fork
            }
            "read" => {
                std::fs::rename(
                    Path::new(&workspace.record.root).join(".izu"),
                    Path::new(&workspace.record.root).join("retained-workspace-marker"),
                )
                .unwrap();
                exact_fork
            }
            "stale" => {
                std::fs::write(
                    Path::new(&workspace.record.root).join("preserved.txt"),
                    "newer human source",
                )
                .unwrap();
                fixture
                    .repo
                    .checkpoint(
                        workspace.id,
                        workspace.expected,
                        Selection::All,
                        &fixture.cancel,
                    )
                    .unwrap()
                    .operation
            }
            _ => panic!("unknown owned fixture failure"),
        };
        let failed = fixture.api.managed_run_admitted(
            context,
            LaunchArguments {
                argv: vec!["/usr/bin/true".into()],
                env: BTreeMap::new(),
                timeout_seconds: 120,
            },
            &cancel,
            runtime,
            permit,
        );
        let error = failed.err().expect("actual preparation boundary fails");
        let partial = managed_start_result(fork, Err(error)).unwrap();
        let error = partial.uncertainty.unwrap();
        assert!(matches!(error.code, ErrorCode::PublicationUncertain));
        assert_eq!(
            error.operation_id.as_deref(),
            Some(exact_fork.to_string().as_str())
        );
        assert!(
            error
                .retained_paths
                .contains(&PathBuf::from(&workspace.record.root))
        );
        assert_eq!(
            partial.value["workspace"]["id"],
            serde_json::json!(workspace.id)
        );
        assert_eq!(
            partial.value["fork_operation"],
            serde_json::json!(exact_fork)
        );
        assert!(partial.value.get("before").is_none());
        assert_eq!(
            fixture.repo.current_operation(&fixture.cancel).unwrap(),
            before,
            "failed start cannot publish another operation"
        );
        assert!(
            fixture
                .repo
                .operation(exact_fork, &fixture.cancel)
                .unwrap()
                .view
                .workspaces
                .contains_key(&workspace.id)
        );
        assert!(
            fixture
                .repo
                .workspace(workspace.id, &fixture.cancel)
                .is_ok()
        );
        if failure == "stale" {
            assert_ne!(
                before, exact_fork,
                "current HEAD must not substitute for the exact fork receipt"
            );
            assert_eq!(
                std::fs::read_to_string(Path::new(&workspace.record.root).join("preserved.txt"))
                    .unwrap(),
                "newer human source"
            );
        }
    }

    #[test]
    fn post_fork_cancellation_retains_exact_operation_workspace_and_path() {
        post_fork_failure("cancel");
    }

    #[test]
    fn post_fork_read_failure_retains_exact_operation_workspace_and_path() {
        post_fork_failure("read");
    }

    #[test]
    fn post_fork_stale_boundary_retains_fork_instead_of_guessing_current_head() {
        post_fork_failure("stale");
    }

    #[test]
    fn managed_partial_preserves_newer_engine_operation_and_latest_checkpoint_fallback() {
        let fixture = Fixture::new();
        let fork = fixture.fork();
        let before = fixture
            .repo
            .checkpoint(
                fork.workspace.id,
                fork.workspace.expected,
                Selection::All,
                &fixture.cancel,
            )
            .unwrap();
        let state = fixture
            .repo
            .workspace(fork.workspace.id, &fixture.cancel)
            .unwrap();
        std::fs::write(
            Path::new(&state.record.root).join("preserved.txt"),
            "newer source",
        )
        .unwrap();
        let after = fixture
            .repo
            .checkpoint(state.id, state.expected, Selection::All, &fixture.cancel)
            .unwrap();
        let state = fixture.repo.workspace(state.id, &fixture.cancel).unwrap();
        let newer = fixture
            .repo
            .checkpoint(state.id, state.expected, Selection::All, &fixture.cancel)
            .unwrap();
        for (after_receipt, reported, expected) in [
            (None, None, before.operation),
            (Some(after.clone()), None, after.operation),
            (None, Some(newer.operation), newer.operation),
            (Some(after.clone()), Some(newer.operation), newer.operation),
        ] {
            let receipt = ManagedReceipt {
                workspace: state.clone(),
                before: before.clone(),
                after: after_receipt,
                job: None,
            };
            let error = match reported {
                Some(operation) => engine_error(izu_engine::EngineError::PublicationUncertain {
                    operation,
                    reason: "owned error policy fixture".into(),
                }),
                None => engine_error(izu_engine::EngineError::Cancelled),
            };
            let partial = managed_partial(receipt, error).unwrap();
            let error = partial.uncertainty.unwrap();
            assert_eq!(
                error.operation_id.as_deref(),
                Some(expected.to_string().as_str())
            );
            assert!(matches!(error.code, ErrorCode::PublicationUncertain));
            assert!(
                fixture
                    .repo
                    .operation(expected, &fixture.cancel)
                    .unwrap()
                    .view
                    .workspaces
                    .contains_key(&state.id)
            );
        }
        let current = fixture.repo.current_operation(&fixture.cancel).unwrap();
        let partial = managed_start_result(
            fork,
            Err(engine_error(
                izu_engine::EngineError::PublicationUncertain {
                    operation: current,
                    reason: "newer visible engine receipt".into(),
                },
            )),
        )
        .unwrap();
        assert_eq!(
            partial.uncertainty.unwrap().operation_id,
            Some(current.to_string())
        );
    }

    #[test]
    fn managed_resume_with_unknown_writer_refuses_before_checkpoint() {
        let fixture = Fixture::new();
        let fork = fixture.fork();
        let workspace = fork.workspace;
        let source = Path::new(&workspace.record.root).join("retained.txt");
        std::fs::write(&source, "source from the lost writer").unwrap();
        let lease = fixture
            .repo
            .lease_workspace(workspace.id, workspace.expected, &fixture.cancel)
            .unwrap();
        let token = lease.intent().token.clone();
        // A dropped controller lease leaves durable intent until exact recovery.
        drop(lease);
        let before = fixture.repo.current_operation(&fixture.cancel).unwrap();
        let result = fixture.api.managed_run(
            context_for(&fixture.context, &workspace),
            LaunchArguments {
                argv: vec!["/usr/bin/true".into()],
                env: BTreeMap::new(),
                timeout_seconds: 120,
            },
            &fixture.cancel,
        );
        assert_eq!(
            fixture.repo.current_operation(&fixture.cancel).unwrap(),
            before,
            "an unreconciled writer must prevent preparation publication"
        );
        let error = result.err().expect("unreconciled writer refuses resume");
        assert!(matches!(error.code, ErrorCode::Conflict), "{error:?}");
        assert!(error.operation_id.is_none());
        assert_eq!(
            fixture
                .repo
                .writer_intent(workspace.id, &fixture.cancel)
                .unwrap()
                .unwrap()
                .token,
            token
        );
        assert_eq!(
            std::fs::read_to_string(source).unwrap(),
            "source from the lost writer"
        );
        assert!(fixture.runtime().jobs().unwrap().is_empty());
    }

    #[test]
    fn managed_new_and_resume_wait_without_publishing_and_cancel_before_preparation() {
        let temporary_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
        std::fs::create_dir_all(&temporary_root).unwrap();
        let temporary = tempfile::tempdir_in(temporary_root).unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let repo = Repository::init(&source, RepositoryOptions::default()).unwrap();
        let fresh = CancellationToken::new();
        let root = repo.workspace(repo.workspace_id(), &fresh).unwrap();
        let existing = repo
            .fork_managed_workspace("existing".into(), root.expected.head, &fresh)
            .unwrap();
        let context = ReadContext {
            repository: source,
            workspace: None,
        };
        let api = Api::new(std::env::current_exe().unwrap());
        let runtime =
            izu_runtime::Runtime::open(repo.metadata_path().join("runtime"), api.runtime_config())
                .unwrap();
        let permits: Vec<_> = (0..4)
            .map(|_| runtime.source_operation_permit(&fresh).unwrap())
            .collect();
        let targets = [
            ManagedStartTarget::New {
                context: context.clone(),
                name: "must-wait".into(),
                from: root.expected.head.to_string(),
            },
            ManagedStartTarget::Resume {
                context: context_for(&context, &existing),
            },
        ];
        for target in targets {
            let before = repo.current_operation(&fresh).unwrap();
            let expected_view = repo.view(&fresh).unwrap();
            let cancel = CancellationToken::new();
            std::thread::scope(|scope| {
                let _cancel_on_exit = CancelOnDrop(cancel.clone());
                let (entered_tx, entered_rx) = mpsc::channel();
                let (result_tx, result_rx) = mpsc::channel();
                let api = &api;
                let token = cancel.clone();
                scope.spawn(move || {
                    entered_tx.send(()).unwrap();
                    let result = api.managed_start(
                        target,
                        LaunchArguments {
                            argv: vec!["/usr/bin/true".into()],
                            env: BTreeMap::new(),
                            timeout_seconds: 120,
                        },
                        &token,
                    );
                    result_tx.send(result.err()).unwrap();
                });
                entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(matches!(
                    result_rx.recv_timeout(Duration::from_millis(200)),
                    Err(mpsc::RecvTimeoutError::Timeout)
                ));
                assert_eq!(repo.current_operation(&fresh).unwrap(), before);
                assert_eq!(repo.view(&fresh).unwrap(), expected_view);
                assert!(
                    runtime.jobs().unwrap().is_empty(),
                    "source queue must not hold the registry transaction"
                );
                cancel.cancel();
                let error = result_rx
                    .recv_timeout(Duration::from_secs(5))
                    .unwrap()
                    .expect("preparation cancelled");
                assert!(matches!(error.code, ErrorCode::Cancelled), "{error:?}");
                assert!(error.operation_id.is_none());
            });
            assert_eq!(repo.current_operation(&fresh).unwrap(), before);
            assert_eq!(repo.view(&fresh).unwrap(), expected_view);
        }
        drop(permits);
    }
}
