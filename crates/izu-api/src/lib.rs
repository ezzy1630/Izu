#![forbid(unsafe_code)]
//! Shared operation facade. History and source mutations belong to izu-engine.

mod protocol;
mod workflows;
pub use izu_model::CancellationToken;
pub use protocol::*;
pub use workflows::LandingPlan;

use izu_engine::{Repository, RepositoryOptions, Selection, WorkspaceExpectation};
use izu_model::{
    CheckSpec, Identity, Limits, RefName, RepoPath, ResolvedTreeEntry, Validate, WorkspaceId,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::fmt::Display;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;
use workflows::{
    environment_cache, environment_error, environment_recipe, environment_sharing, parse_checks,
};

#[derive(Clone, Debug)]
pub struct Api {
    worker_executable: PathBuf,
}

struct DispatchData {
    value: Value,
    uncertainty: Option<ApiError>,
}

#[derive(Serialize)]
struct PresentedStatus {
    #[serde(flatten)]
    status: izu_engine::Status,
    workspace_name: String,
    root: String,
    main: Option<izu_model::RevisionId>,
    at_main: bool,
}

#[derive(Serialize)]
struct EnvironmentBindingReceipt {
    object_id: izu_model::ObjectId,
    key: izu_environment::Digest,
    manifest_digest: izu_environment::Digest,
    source_identity: izu_environment::Digest,
    storage: BindingStorage,
    reachability: BindingReachability,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BindingStorage {
    DurableLocalBlob,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum BindingReachability {
    UnreferencedUntilCandidate,
}

impl Api {
    pub fn new(worker_executable: PathBuf) -> Self {
        Self { worker_executable }
    }

    pub fn for_current_executable() -> Result<Self, ApiError> {
        std::env::current_exe()
            .map(Self::new)
            .map_err(|error| failure(format!("Cannot identify izu executable: {error}")))
    }

    pub fn worker_main(&self, ticket: &Path) -> Result<(), ApiError> {
        izu_runtime::worker_main(ticket).map_err(runtime_error)
    }

    pub fn process_worker_main(&self) -> Result<(), ApiError> {
        izu_process::worker_main()
            .map_err(|error| failure(format!("Process worker failed: {error}")))
    }

    pub fn mutation_context(
        &self,
        context: &ReadContext,
        cancel: &CancellationToken,
    ) -> Result<MutationContext, ApiError> {
        let repo = repository(context)?;
        let workspace = selected_workspace(&repo, context)?;
        let state = repo.workspace(workspace, cancel).map_err(engine_error)?;
        Ok(MutationContext {
            repository: context.repository.clone(),
            workspace: workspace.to_string(),
            expected: Expectation {
                head: state.expected.head.to_string(),
                working_tree: state.expected.working_tree.to_string(),
            },
        })
    }

    pub fn execute(&self, request: Request, cancel: &CancellationToken) -> Response {
        if request.schema_version != SCHEMA_VERSION {
            return Response::error(ApiError {
                code: ErrorCode::UnsupportedSchema,
                message: format!(
                    "Unsupported schema version {} (supported: {SCHEMA_VERSION})",
                    request.schema_version
                ),
                next_action: "Use `izu schema` to select a supported version.".into(),
                operation_id: None,
                recovery_operation_id: None,
                retained_paths: Box::default(),
            });
        }
        if cancel.is_cancelled() {
            return Response::error(cancelled());
        }
        let kind = request.operation.name();
        match self.dispatch(request.operation, cancel) {
            Ok(data) => match data.uncertainty {
                Some(error) if matches!(error.code, ErrorCode::PublicationUncertain) => {
                    Response::uncertain(kind, data.value, error)
                }
                Some(error) => Response::rejected(kind, data.value, error),
                None => Response::success(kind, data.value),
            },
            Err(error) => Response::error(error),
        }
    }

    fn runtime_config(&self) -> izu_runtime::RuntimeConfig {
        let mut config = izu_runtime::RuntimeConfig::new(self.worker_executable.clone());
        config.worker_prefix = vec!["__runtime-worker".into(), "--ticket".into()];
        config
    }

    fn git_adapter(&self) -> Result<izu_git::GitAdapter, ApiError> {
        let config = izu_git::GitToolConfig {
            worker: Some(izu_git::WorkerLauncher {
                executable: self.worker_executable.clone(),
                prefix_args: vec![std::ffi::OsString::from("__process-worker")],
            }),
            ..Default::default()
        };
        izu_git::GitAdapter::new(config).map_err(git_error)
    }

    fn git_adapter_for(
        &self,
        source: &izu_git::GitSource,
        transport: GitTransport,
    ) -> Result<izu_git::GitAdapter, ApiError> {
        if transport.root_certificates.len() > 8
            || transport
                .root_certificates
                .iter()
                .any(|certificate| certificate.len() > 64 * 1024)
        {
            return Err(ApiError::invalid(
                "At most eight explicit DER trust anchors of 64 KiB each are supported",
            ));
        }
        let authentication = match (&source, transport.authentication) {
            (_, None) => None,
            (
                izu_git::GitSource::Https(url),
                Some(GitAuthentication::Basic { username, password }),
            ) => Some(
                izu_git::HttpsAuthentication::basic(url, &username, password).map_err(git_error)?,
            ),
            _ => {
                return Err(ApiError::invalid(
                    "HTTPS credentials require one explicit HTTPS repository URL",
                ));
            }
        };
        let config = izu_git::GitToolConfig {
            worker: Some(izu_git::WorkerLauncher {
                executable: self.worker_executable.clone(),
                prefix_args: vec![std::ffi::OsString::from("__process-worker")],
            }),
            authentication,
            https_root_certificates: transport.root_certificates,
            ..Default::default()
        };
        izu_git::GitAdapter::new(config).map_err(git_error)
    }

    fn dispatch(
        &self,
        operation: Operation,
        cancel: &CancellationToken,
    ) -> Result<DispatchData, ApiError> {
        match operation {
            Operation::Capabilities {} => value(json!({
                "schema_version":SCHEMA_VERSION,
                "build":BuildIdentity::default(),
                "operations":operation_names()?,
                "local_checkpoint":"acknowledged only after required filesystem barriers; not a remote backup",
                "git_transport":"explicit local Git repositories or credential-free HTTPS URLs; scoped authentication and validated TLS; no implicit remote mutation",
                "runtime":izu_runtime::RuntimeCapabilities::current(),
                "runtime_default_budget":self.runtime_config().budget,
                "mcp_versions":["2026-07-28","2025-11-25"],
                "environment_checks":{"binding":"native_blob","cache":"repository_default","scope":izu_environment::EnvironmentVerificationScope::StartingFileContents,"toolchain_identity":"caller_declared","abi":"caller_declared","security_boundary":false},
                "limitations":["No automatic host thread binding","No security sandbox or hard CPU/memory/disk containment","No automatic cloud backup","Machine mutations require exact expectations; divergent changes are never auto-selected","Environment checks verify starting files; warm outputs remain writable and execution environments are not immutable"]
            })),
            Operation::Schema {} => {
                value(json!({"request":request_schema()?,"response":response_schema()?}))
            }
            Operation::Init { path } => {
                let repo =
                    Repository::init(path, RepositoryOptions::default()).map_err(engine_error)?;
                value(
                    repo.workspace(repo.workspace_id(), cancel)
                        .map_err(engine_error)?,
                )
            }
            Operation::Inspect { context } => {
                let repo = repository(&context)?;
                value(
                    repo.workspace(selected_workspace(&repo, &context)?, cancel)
                        .map_err(engine_error)?,
                )
            }
            Operation::Status { context } => {
                let repo = repository(&context)?;
                let workspace = selected_workspace(&repo, &context)?;
                let state = repo.workspace(workspace, cancel).map_err(engine_error)?;
                let status = repo.status(workspace, cancel).map_err(engine_error)?;
                let main = repo
                    .view(cancel)
                    .map_err(engine_error)?
                    .refs
                    .get(&RefName::new("main").map_err(model_error)?)
                    .copied();
                let at_main = main == Some(status.head);
                value(PresentedStatus {
                    status,
                    workspace_name: state.record.name,
                    root: state.record.root,
                    main,
                    at_main,
                })
            }
            Operation::Diff {
                context,
                before,
                after,
            } => {
                let repo = repository(&context)?;
                match (before, after) {
                    (Some(before), Some(after)) => diff_details(
                        &repo,
                        repo.diff(
                            parse(&before, "before revision")?,
                            parse(&after, "after revision")?,
                            cancel,
                        )
                        .map_err(engine_error)?,
                        cancel,
                    ),
                    (None, None) => diff_details(
                        &repo,
                        repo.working_diff(selected_workspace(&repo, &context)?, cancel)
                            .map_err(engine_error)?,
                        cancel,
                    ),
                    _ => Err(ApiError::invalid(
                        "Diff requires both before and after revisions, or neither",
                    )),
                }
            }
            Operation::Log {
                context,
                from,
                limit,
            } => {
                history_limit(limit)?;
                let repo = repository(&context)?;
                let start = match from {
                    Some(id) => parse(&id, "from revision")?,
                    None => {
                        repo.workspace(selected_workspace(&repo, &context)?, cancel)
                            .map_err(engine_error)?
                            .expected
                            .head
                    }
                };
                value(repo.log(start, limit, cancel).map_err(engine_error)?)
            }
            Operation::Show { context, revision } => {
                let repo = repository(&context)?;
                value(
                    repo.revision(parse(&revision, "revision")?, cancel)
                        .map_err(engine_error)?,
                )
            }
            Operation::Checkpoint { context, selection } => {
                let (repo, workspace, expected) = mutation(&context)?;
                value(
                    repo.checkpoint(workspace, expected, parse_selection(selection)?, cancel)
                        .map_err(engine_error)?,
                )
            }
            Operation::Commit {
                context,
                selection,
                message,
                author,
                target,
            } => {
                if message.trim().is_empty() {
                    return Err(ApiError::invalid("Commit message must not be empty"));
                }
                let (repo, workspace, expected) = mutation(&context)?;
                let selection = parse_selection(selection)?;
                let author = identity(author)?;
                let reference = target.as_ref().map(|target| target.name.clone());
                let receipt = match target {
                    Some(target) => repo.commit_to_ref(
                        workspace,
                        expected,
                        selection,
                        message,
                        author,
                        izu_engine::RefExpectation {
                            name: RefName::new(target.name).map_err(model_error)?,
                            expected: parse_optional(
                                target.expected,
                                "expected reference revision",
                            )?,
                        },
                        cancel,
                    ),
                    None => repo.commit(workspace, expected, selection, message, author, cancel),
                }
                .map_err(engine_error)?;
                let mut data = value(receipt)?;
                if let Some(reference) = reference {
                    data.value["reference"] = json!(reference);
                }
                Ok(data)
            }
            Operation::Revise {
                context,
                selection,
                message,
                author,
            } => {
                if message.trim().is_empty() {
                    return Err(ApiError::invalid("Revision message must not be empty"));
                }
                let (repo, workspace, expected) = mutation(&context)?;
                value(
                    repo.revise(
                        workspace,
                        expected,
                        parse_selection(selection)?,
                        message,
                        identity(author)?,
                        cancel,
                    )
                    .map_err(engine_error)?,
                )
            }
            Operation::WorkspaceStart {
                context,
                name,
                path,
                from,
            } => {
                let repo = repository(&context)?;
                value(
                    repo.fork_workspace(name, path, parse(&from, "from revision")?, cancel)
                        .map_err(engine_error)?,
                )
            }
            Operation::WorkspaceList { context } => {
                let repo = repository(&context)?;
                value(repo.view(cancel).map_err(engine_error)?.workspaces)
            }
            Operation::WorkspaceClose { context } => {
                let (repo, workspace, expected) = mutation(&context)?;
                let mut runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                value(
                    runtime
                        .close_workspace(&repo, workspace, expected, cancel)
                        .map_err(runtime_error)?,
                )
            }
            Operation::RefList { context } => value(
                repository(&context)?
                    .view(cancel)
                    .map_err(engine_error)?
                    .refs,
            ),
            Operation::RefSet {
                context,
                name,
                expected,
                revision,
            } => {
                let repo = repository(&context)?;
                let name = RefName::new(name).map_err(model_error)?;
                let operation = repo
                    .update_ref(
                        &name,
                        parse_optional(expected, "expected revision")?,
                        parse_optional(revision, "new revision")?,
                        cancel,
                    )
                    .map_err(engine_error)?;
                value(json!({"operation":operation,"reference":name}))
            }
            Operation::Restore {
                context,
                revision,
                selection,
            } => {
                let (repo, workspace, expected) = mutation(&context)?;
                value(
                    repo.restore(
                        workspace,
                        expected,
                        parse(&revision, "source revision")?,
                        parse_selection(selection)?,
                        cancel,
                    )
                    .map_err(engine_error)?,
                )
            }
            Operation::Update { context, revision } => {
                let (repo, workspace, expected) = mutation(&context)?;
                value(
                    repo.update_workspace(
                        workspace,
                        expected,
                        parse(&revision, "source revision")?,
                        cancel,
                    )
                    .map_err(engine_error)?,
                )
            }
            Operation::Undo {
                context,
                operation,
                selection,
            } => {
                let (repo, workspace, expected) = mutation(&context)?;
                value(
                    repo.undo(
                        workspace,
                        expected,
                        &parse(&operation, "operation")?,
                        parse_selection(selection)?,
                        cancel,
                    )
                    .map_err(engine_error)?,
                )
            }
            Operation::Revert {
                context,
                revision,
                selection,
                message,
                author,
                target,
            } => {
                if message.trim().is_empty() {
                    return Err(ApiError::invalid("Revert message must not be empty"));
                }
                let (repo, workspace, expected) = mutation(&context)?;
                let revision = parse(&revision, "reverted revision")?;
                let selection = parse_selection(selection)?;
                let author = identity(author)?;
                let prepared = match target {
                    Some(target) => repo.revert_to_ref(
                        workspace,
                        expected,
                        revision,
                        selection,
                        message,
                        author,
                        izu_engine::RefExpectation {
                            name: RefName::new(target.name).map_err(model_error)?,
                            expected: parse_optional(target.expected, "expected target")?,
                        },
                        cancel,
                    ),
                    None => repo.revert(
                        workspace, expected, revision, selection, message, author, cancel,
                    ),
                }
                .map_err(engine_error)?;
                let mut data = value(&prepared)?;
                if let izu_engine::RevertPreparation::Conflicted {
                    revision,
                    operation,
                    ..
                } = prepared
                {
                    data.uncertainty = Some(ApiError { code: ErrorCode::Conflict, message: format!("Revert durably records unresolved conflicts in revision {revision}; source and head are retained"), next_action: "Inspect the conflict paths and use resolve-revision for explicit resolutions, then restore or integrate the resolved revision deliberately.".into(), operation_id: Some(operation.to_string()), recovery_operation_id: None, retained_paths: Box::default() });
                }
                Ok(data)
            }
            Operation::Operations { context, limit } => {
                history_limit(limit)?;
                value(
                    repository(&context)?
                        .operations(limit, cancel)
                        .map_err(engine_error)?,
                )
            }
            Operation::Candidate {
                context,
                source,
                target,
                expected_target,
                checks,
                author,
            } => {
                let repo = repository(&context)?;
                let checks = parse_checks(checks)?;
                let prepared = repo
                    .prepare_merge(
                        RefName::new(target).map_err(model_error)?,
                        parse_optional(expected_target, "expected target")?,
                        parse(&source, "source revision")?,
                        checks,
                        identity(author)?,
                        cancel,
                    )
                    .map_err(engine_error)?;
                preparation(prepared)
            }
            Operation::CandidateShow { context, candidate } => value(
                repository(&context)?
                    .candidate(parse(&candidate, "candidate")?, cancel)
                    .map_err(engine_error)?,
            ),
            Operation::Resolve {
                context,
                candidate,
                path,
                resolution,
                author,
            } => {
                let repo = repository(&context)?;
                let candidate = parse(&candidate, "candidate")?;
                let record = repo.candidate(candidate, cancel).map_err(engine_error)?;
                let (path, entry) =
                    resolve_entry(&repo, record.result_tree, path, resolution, cancel)?;
                preparation(
                    repo.resolve_candidate(
                        candidate,
                        std::collections::BTreeMap::from([(path, entry)]),
                        identity(author)?,
                        cancel,
                    )
                    .map_err(engine_error)?,
                )
            }
            Operation::ResolveRevision {
                context,
                revision,
                path,
                resolution,
                author,
            } => {
                let repo = repository(&context)?;
                let revision = parse(&revision, "revision")?;
                let record = repo.revision(revision, cancel).map_err(engine_error)?;
                let (path, entry) = resolve_entry(&repo, record.tree, path, resolution, cancel)?;
                value(
                    repo.resolve_conflicts(
                        revision,
                        std::collections::BTreeMap::from([(path, entry)]),
                        identity(author)?,
                        cancel,
                    )
                    .map_err(engine_error)?,
                )
            }
            Operation::Check {
                context,
                candidate,
                check,
                timeout_seconds,
            } => {
                timeout(timeout_seconds)?;
                let repo = repository(&context)?;
                let mut runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                let checked = izu_runtime::run_check(
                    &mut runtime,
                    &repo,
                    parse(&candidate, "candidate")?,
                    &check,
                    Duration::from_secs(timeout_seconds),
                    cancel,
                )
                .map_err(runtime_error)?;
                job_result(&checked, &checked.job, true)
            }
            Operation::CheckStart {
                context,
                candidate,
                check,
            } => value(
                repository(&context)?
                    .start_check(parse(&candidate, "candidate")?, &check, cancel)
                    .map_err(engine_error)?,
            ),
            Operation::Land { context, candidate } => value(
                repository(&context)?
                    .land(parse(&candidate, "candidate")?, cancel)
                    .map_err(engine_error)?,
            ),
            Operation::LandCurrent {
                context,
                source,
                target,
                checks,
                author,
                timeout_seconds,
            } => self.land_current(
                context,
                source,
                target,
                LandingPlan {
                    checks,
                    author,
                    timeout_seconds,
                },
                cancel,
            ),
            Operation::ManagedStart { target, launch } => {
                self.managed_start(target, launch, cancel)
            }
            Operation::ManagedRun { context, launch } => self.managed_run(context, launch, cancel),
            Operation::Verify { context } => {
                value(repository(&context)?.verify(cancel).map_err(engine_error)?)
            }
            Operation::Recover { context } => value(
                repository(&context)?
                    .recover(cancel)
                    .map_err(engine_error)?,
            ),
            Operation::JobsList { context, limit } => {
                history_limit(limit)?;
                let repo = repository(&context)?;
                let selected = context
                    .workspace
                    .as_ref()
                    .map(|id| parse::<WorkspaceId>(id, "workspace"))
                    .transpose()?;
                let runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                value(
                    runtime
                        .jobs_summary_for(selected, limit)
                        .map_err(runtime_error)?,
                )
            }
            Operation::JobStatus { context, job } => {
                let repo = repository(&context)?;
                let runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                let snapshot = runtime
                    .status(&parse(&job, "job")?)
                    .map_err(runtime_error)?;
                check_job_context(&snapshot, &context)?;
                value(snapshot)
            }
            Operation::JobCancel { context, job } => {
                let repo = repository(&context)?;
                let mut runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                let job = parse(&job, "job")?;
                check_job_context(&runtime.status(&job).map_err(runtime_error)?, &context)?;
                value(runtime.cancel(&job).map_err(runtime_error)?)
            }
            Operation::JobAcknowledgeStopped {
                context,
                job,
                operator_note,
            } => {
                let repo = repository(&context)?;
                let mut runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                let job = parse(&job, "job")?;
                check_job_context(&runtime.status(&job).map_err(runtime_error)?, &context)?;
                value(
                    runtime
                        .acknowledge_stopped(&job, operator_note)
                        .map_err(runtime_error)?,
                )
            }
            Operation::WriterStatus { context } => {
                let repo = repository(&context)?;
                let workspace = selected_workspace(&repo, &context)?;
                value(
                    json!({"workspace":workspace,"intent":repo.writer_intent(workspace, cancel).map_err(engine_error)?}),
                )
            }
            Operation::WriterAcknowledgeStopped {
                context,
                token,
                operator_note,
            } => {
                let repo = repository(&context)?;
                let workspace = selected_workspace(&repo, &context)?;
                let token = izu_engine::WriterToken::new(token).map_err(engine_error)?;
                value(
                    json!({"operation":repo.acknowledge_writer_stopped_with_note(workspace, &token, operator_note, cancel).map_err(engine_error)?,"workspace":workspace}),
                )
            }
            Operation::EnvironmentSourceIdentity { context } => {
                let (repo, workspace, expected) = mutation(&context)?;
                let target =
                    izu_environment::WorkspaceTarget::checked(&repo, workspace, expected, cancel)
                        .map_err(environment_error)?;
                value(json!({"source_identity":target.source_identity(),"workspace":workspace}))
            }
            Operation::EnvironmentBind {
                context,
                recipe_json,
                trusted,
            } => {
                let repo = repository(&context)?;
                let recipe = environment_recipe(&recipe_json, trusted)?;
                let binding = environment_cache(&repo, None)?
                    .binding(&recipe, cancel)
                    .map_err(environment_error)?;
                let bytes = binding.to_json().map_err(environment_error)?;
                let object_id = repo
                    .put_blob(&mut bytes.as_slice(), bytes.len() as u64, cancel)
                    .map_err(engine_error)?;
                value(EnvironmentBindingReceipt {
                    object_id,
                    key: binding.key(),
                    manifest_digest: binding.manifest_digest(),
                    source_identity: binding.source_identity(),
                    storage: BindingStorage::DurableLocalBlob,
                    reachability: BindingReachability::UnreferencedUntilCandidate,
                })
            }
            Operation::EnvironmentStatus {
                context,
                cache,
                recipe_json,
                trusted,
            } => {
                let repo = repository(&context)?;
                let recipe = environment_recipe(&recipe_json, trusted)?;
                let cache = environment_cache(&repo, cache)?;
                value(cache.status(&recipe, cancel).map_err(environment_error)?)
            }
            Operation::EnvironmentImport {
                context,
                cache,
                recipe_json,
                prepared,
                trusted,
                quiescent,
                sharing,
            } => {
                if !quiescent {
                    return Err(ApiError::invalid(
                        "Environment import requires an explicit assertion that prepared source has no writers",
                    ));
                }
                let repo = repository(&context)?;
                let recipe = environment_recipe(&recipe_json, trusted)?;
                value(
                    environment_cache(&repo, cache)?
                        .import_quiescent(
                            &recipe,
                            &prepared,
                            izu_environment::QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                            environment_sharing(sharing),
                            cancel,
                        )
                        .map_err(environment_error)?,
                )
            }
            Operation::EnvironmentMaterialize {
                context,
                cache,
                recipe_json,
                trusted,
                sharing,
            } => {
                let (repo, workspace, expected) = mutation(&context)?;
                let recipe = environment_recipe(&recipe_json, trusted)?;
                let mut target =
                    izu_environment::WorkspaceTarget::checked(&repo, workspace, expected, cancel)
                        .map_err(environment_error)?;
                value(
                    environment_cache(&repo, cache)?
                        .materialize(&recipe, &mut target, environment_sharing(sharing), cancel)
                        .map_err(environment_error)?,
                )
            }
            Operation::BundleCreate { context, path } => {
                let repo = repository(&context)?;
                let root = repo.current_operation(cancel).map_err(engine_error)?;
                let receipt = izu_bundle::create(
                    &repo,
                    root,
                    &path,
                    &izu_bundle::BundleOptions::default(),
                    cancel,
                )
                .map_err(bundle_error)?;
                bundle_receipt(&receipt, &receipt.publication, root.to_string())
            }
            Operation::BundleInspect { path } => value(
                izu_bundle::inspect(&path, &izu_bundle::BundleOptions::default(), cancel)
                    .map_err(bundle_error)?,
            ),
            Operation::BundleVerify { path } => value(
                izu_bundle::verify(&path, &izu_bundle::BundleOptions::default(), cancel)
                    .map_err(bundle_error)?,
            ),
            Operation::BundleRestore {
                path,
                destination,
                workspace,
            } => {
                let options = izu_bundle::RestoreOptions {
                    workspace: parse_optional(workspace, "workspace")?,
                    ..izu_bundle::RestoreOptions::default()
                };
                let receipt = izu_bundle::restore(&path, &destination, &options, cancel)
                    .map_err(bundle_error)?;
                bundle_receipt(
                    &receipt,
                    &receipt.publication,
                    receipt.recovery.operation.to_string(),
                )
            }
            Operation::GitImport {
                context,
                source,
                refs,
                transport,
            } => {
                let source = git_location(source)?;
                let adapter = self.git_adapter_for(&source, transport)?;
                value(
                    adapter
                        .import_into(
                            &repository(&context)?,
                            &source,
                            &git_import_options(refs)?,
                            cancel,
                        )
                        .map_err(git_error)?,
                )
            }
            Operation::GitExport {
                context,
                target,
                committer,
            } => value(
                self.git_adapter()?
                    .export_to(
                        &repository(&context)?,
                        &target,
                        &izu_git::ExportOptions {
                            committer: committer.map(git_signature),
                            ..Default::default()
                        },
                        cancel,
                    )
                    .map_err(git_error)?,
            ),
            Operation::GitFetch {
                context,
                source,
                refs,
                transport,
            } => {
                let source = git_location(source)?;
                value(
                    self.git_adapter_for(&source, transport)?
                        .fetch_into(
                            &repository(&context)?,
                            &source,
                            &git_import_options(refs)?,
                            cancel,
                        )
                        .map_err(git_error)?,
                )
            }
            Operation::GitPush {
                context,
                target,
                native_ref,
                git_ref,
                expected_old,
                allow_non_fast_forward,
                committer,
                transport,
            } => {
                let target = git_location(target)?;
                let adapter = self.git_adapter_for(&target, transport)?;
                let request = izu_git::PushRequest {
                    remote: target,
                    native_ref: RefName::new(native_ref).map_err(model_error)?,
                    git_ref,
                    expected_old: parse_optional(expected_old, "Git expected object")?,
                    non_fast_forward: if allow_non_fast_forward {
                        izu_git::NonFastForwardPolicy::ExplicitAllow
                    } else {
                        izu_git::NonFastForwardPolicy::Reject
                    },
                    committer: committer.map(git_signature),
                };
                value(
                    adapter
                        .push_ref(&repository(&context)?, &request, cancel)
                        .map_err(git_error)?,
                )
            }
            Operation::AgentRun {
                context,
                cwd,
                argv,
                env,
                timeout_seconds,
            } => {
                timeout(timeout_seconds)?;
                let (repo, workspace, expected) = mutation(&context)?;
                let binding =
                    izu_runtime::WorkspaceBinding::checked(&repo, workspace, expected, cancel)
                        .map_err(runtime_error)?;
                let supplied = std::fs::canonicalize(&cwd).map_err(|error| {
                    ApiError::invalid(format!(
                        "Cannot resolve supplied cwd {}: {error}",
                        cwd.display()
                    ))
                })?;
                if supplied != binding.cwd() {
                    return Err(ApiError::invalid(
                        "Supplied cwd does not match the exact registered workspace root",
                    ));
                }
                let mut request = izu_runtime::RunRequest::new(binding, argv)
                    .with_timeout(Duration::from_secs(timeout_seconds))
                    .map_err(runtime_error)?;
                request.env_overlay = env;
                let mut runtime = izu_runtime::Runtime::open(
                    repo.metadata_path().join("runtime"),
                    self.runtime_config(),
                )
                .map_err(runtime_error)?;
                let job = runtime.submit(request).map_err(runtime_error)?;
                let snapshot = runtime.wait(&job, cancel).map_err(runtime_error)?;
                job_result(&snapshot, &snapshot, false)
            }
        }
    }
}

fn repository(context: &ReadContext) -> Result<Repository, ApiError> {
    Repository::discover(&context.repository, RepositoryOptions::default()).map_err(engine_error)
}

fn selected_workspace(repo: &Repository, context: &ReadContext) -> Result<WorkspaceId, ApiError> {
    match &context.workspace {
        Some(id) => parse(id, "workspace"),
        None => Ok(repo.workspace_id()),
    }
}

fn check_job_context(
    job: &izu_runtime::JobSnapshot,
    context: &ReadContext,
) -> Result<(), ApiError> {
    if let Some(workspace) = &context.workspace
        && parse::<WorkspaceId>(workspace, "workspace")? != job.request.workspace.id()
    {
        return Err(ApiError::invalid(
            "Job does not belong to the explicitly selected workspace",
        ));
    }
    Ok(())
}

fn mutation(
    context: &MutationContext,
) -> Result<(Repository, WorkspaceId, WorkspaceExpectation), ApiError> {
    let repo = repository(&ReadContext {
        repository: context.repository.clone(),
        workspace: Some(context.workspace.clone()),
    })?;
    Ok((
        repo,
        parse(&context.workspace, "workspace")?,
        WorkspaceExpectation {
            head: parse(&context.expected.head, "expected head")?,
            working_tree: parse(&context.expected.working_tree, "expected working tree")?,
        },
    ))
}

fn parse<T: FromStr>(value: &str, label: &str) -> Result<T, ApiError>
where
    T::Err: Display,
{
    value
        .parse()
        .map_err(|error| ApiError::invalid(format!("Invalid {label}: {error}")))
}

fn parse_optional<T: FromStr>(value: Option<String>, label: &str) -> Result<Option<T>, ApiError>
where
    T::Err: Display,
{
    value.map(|value| parse(&value, label)).transpose()
}

fn parse_selection(selection: PathSelection) -> Result<Selection, ApiError> {
    match selection {
        PathSelection::All {} => Ok(Selection::All),
        PathSelection::Paths { paths } => {
            if paths.is_empty() || paths.len() > 4096 {
                return Err(ApiError::invalid("Path selection requires 1..=4096 paths"));
            }
            paths
                .into_iter()
                .map(|path| RepoPath::new(path).map_err(model_error))
                .collect::<Result<Vec<_>, _>>()
                .map(Selection::Paths)
        }
    }
}

fn identity(author: Author) -> Result<Identity, ApiError> {
    let author = Identity {
        name: author.name,
        email: author.email,
    };
    author.validate(&Limits::default()).map_err(model_error)?;
    Ok(author)
}

fn resolve_entry(
    repo: &Repository,
    tree: izu_model::TreeId,
    path: String,
    resolution: ConflictResolution,
    cancel: &CancellationToken,
) -> Result<(RepoPath, Option<ResolvedTreeEntry>), ApiError> {
    let tree = repo.tree(tree, cancel).map_err(engine_error)?;
    let path = RepoPath::new(path).map_err(model_error)?;
    let (base, ours, theirs) = match tree.entries.get(&path) {
        Some(izu_model::TreeEntry::Conflict {
            base, ours, theirs, ..
        }) => (base, ours, theirs),
        _ => {
            return Err(ApiError::invalid(
                "Selected path is not an unresolved native conflict",
            ));
        }
    };
    let entry = match resolution {
        ConflictResolution::Base {} => base.clone(),
        ConflictResolution::Ours {} => ours.clone(),
        ConflictResolution::Theirs {} => theirs.clone(),
        ConflictResolution::Delete {} => None,
        ConflictResolution::File {
            content,
            executable,
        } => {
            if content.len() > MAX_REQUEST_BYTES {
                return Err(ApiError::invalid("Supplied conflict content exceeds 1 MiB"));
            }
            let blob = repo
                .put_blob(
                    &mut std::io::Cursor::new(content.as_bytes()),
                    content.len() as u64,
                    cancel,
                )
                .map_err(engine_error)?;
            Some(ResolvedTreeEntry::File {
                blob,
                mode: if executable {
                    izu_model::FileMode::Executable
                } else {
                    izu_model::FileMode::Regular
                },
            })
        }
    };
    Ok((path, entry))
}

fn history_limit(limit: usize) -> Result<(), ApiError> {
    if !(1..=1000).contains(&limit) {
        return Err(ApiError::invalid(
            "History limit must be 1..=1000; choose a smaller scope for large records",
        ));
    }
    Ok(())
}

fn timeout(seconds: u64) -> Result<(), ApiError> {
    if !(1..=86_400).contains(&seconds) {
        return Err(ApiError::invalid("Timeout must be 1..=86400 seconds"));
    }
    Ok(())
}

fn value(value: impl Serialize) -> Result<DispatchData, ApiError> {
    let bytes = bounded_json(&value, MAX_RESULT_BYTES)?;
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| failure(format!("Cannot encode engine result: {error}")))?;
    Ok(DispatchData {
        value,
        uncertainty: None,
    })
}

/// Bound serialization before building a wire Value or nested MCP text content.
pub fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>, ApiError> {
    struct Output {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if self
                .bytes
                .len()
                .checked_add(bytes.len())
                .is_none_or(|size| size > self.limit)
            {
                return Err(std::io::Error::other(
                    "result exceeds the output byte limit",
                ));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output,value).map_err(|error|ApiError {code:ErrorCode::Failure,message:format!("Cannot encode bounded result: {error}"),next_action:"The operation may already be durable. Inspect status, operations, and exact target references before retrying; choose a smaller scope for large reads.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()})?;
    Ok(output.bytes)
}

/// Files are descriptor-opened without following aliases or blocking on FIFOs.
pub fn read_input_file(path: &Path, limit: usize) -> Result<Vec<u8>, ApiError> {
    let name = path
        .file_name()
        .ok_or_else(|| ApiError::invalid("Input must name a regular file"))?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let directory = izu_platform::Directory::open(parent).map_err(|error| {
        ApiError::invalid(format!(
            "Cannot open input parent {} (use its real directory path): {error}",
            parent.display()
        ))
    })?;
    let file = directory.open_read(name).map_err(|error| {
        ApiError::invalid(format!(
            "Cannot open regular input {}: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(|error| {
        ApiError::invalid(format!("Cannot inspect input {}: {error}", path.display()))
    })?;
    if !metadata.is_file() {
        return Err(ApiError::invalid(
            "Input must be a regular file, not a stream or device",
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(ApiError::invalid(format!(
            "Input {} exceeds {limit} bytes",
            path.display()
        )));
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve(
            usize::try_from(metadata.len())
                .map_err(|_| ApiError::invalid("Input size cannot fit memory"))?,
        )
        .map_err(|_| failure("Cannot reserve bounded file input"))?;
    let read_limit = u64::try_from(limit)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| ApiError::invalid("Input byte limit overflows"))?;
    file.take(read_limit)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            ApiError::invalid(format!(
                "Cannot read regular input {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() > limit {
        return Err(ApiError::invalid(format!(
            "Input {} exceeds {limit} bytes",
            path.display()
        )));
    }
    Ok(bytes)
}

fn failure(message: impl Into<String>) -> ApiError {
    ApiError {
        code: ErrorCode::Failure,
        message: message.into(),
        next_action: "Inspect the reported path or operation and retry after resolving the cause."
            .into(),
        operation_id: None,
        recovery_operation_id: None,
        retained_paths: Box::default(),
    }
}

fn cancelled() -> ApiError {
    ApiError {
        code: ErrorCode::Cancelled,
        message: "Operation cancelled".into(),
        next_action: "Inspect status before retrying any mutation.".into(),
        operation_id: None,
        recovery_operation_id: None,
        retained_paths: Box::default(),
    }
}

fn model_error(error: izu_model::ModelError) -> ApiError {
    if matches!(error, izu_model::ModelError::Cancelled) {
        cancelled()
    } else {
        ApiError::invalid(error.to_string())
    }
}

fn engine_error(error: izu_engine::EngineError) -> ApiError {
    let error = match error {
        izu_engine::EngineError::Store(error) => return store_error(error),
        izu_engine::EngineError::Model(error) => return model_error(error),
        error => error,
    };
    let code = match error.code() {
        "cancelled" => ErrorCode::Cancelled,
        "stale_workspace" | "stale_ref" | "source_changed" | "stale_check_attempt" => {
            ErrorCode::StaleExpectation
        }
        "missing_check" | "no_required_checks" | "evidence_mismatch" => ErrorCode::MissingCheck,
        "publication_uncertain" | "restoration_uncertain" => ErrorCode::PublicationUncertain,
        "not_repository" | "unknown_workspace" | "unknown_revision" | "unknown_candidate" => {
            ErrorCode::NotFound
        }
        "unsupported_platform" | "unsupported_entry" | "incompatible_path" => {
            ErrorCode::UnsupportedFeature
        }
        "invalid_marker" | "uninitialized" => ErrorCode::CorruptData,
        "invalid_input" | "invalid_model" | "protected_path" | "resource_limit" => {
            ErrorCode::InvalidRequest
        }
        "divergent_change"
        | "merge"
        | "unselected_collision"
        | "path_collision"
        | "already_exists"
        | "workspace_name_exists"
        | "workspace_busy"
        | "uncommitted_source"
        | "directory_not_empty" => ErrorCode::Conflict,
        _ => ErrorCode::Failure,
    };
    let next_action = if error.code() == "uncommitted_source" {
        "Preserve the selected edits. Commit or reconcile them before updating, undoing, or reverting this source."
    } else if error.code() == "restoration_failed" {
        "Restoration may have changed part of the source. Preserve the directory and recovery originals; inspect status, operations, and recover before any retry."
    } else {
        match code {
            ErrorCode::StaleExpectation => {
                "Read workspace status or references and choose the new exact expectation before retrying."
            }
            ErrorCode::MissingCheck => {
                "Inspect the candidate and run every declared check against its exact inputs before landing."
            }
            ErrorCode::PublicationUncertain => {
                "Do not blindly retry. Inspect `izu operations`, `izu status`, and `izu recover` to determine the visible operation."
            }
            ErrorCode::CorruptData => {
                "Preserve this repository and inspect `izu verify` / `izu recover`; do not delete native objects."
            }
            ErrorCode::NotFound => {
                "Select an existing repository, workspace, or exact native object ID."
            }
            ErrorCode::UnsupportedFeature => {
                "Inspect `izu capabilities` and preserve unsupported source entries."
            }
            ErrorCode::Conflict => {
                "Inspect the conflict and explicitly select or resolve the affected source, reference, or divergent revision."
            }
            ErrorCode::Cancelled => "Inspect status before retrying any mutation.",
            _ => "Inspect the reported path or operation and correct the cause before retrying.",
        }
    };
    ApiError {
        code,
        message: error.to_string(),
        next_action: next_action.into(),
        operation_id: error.uncertain_operation().map(|id| id.to_string()),
        recovery_operation_id: error.recovery_operation().map(|id| id.to_string()),
        retained_paths: Box::default(),
    }
}

fn runtime_error(error: izu_runtime::RuntimeError) -> ApiError {
    let error = match error {
        izu_runtime::RuntimeError::Engine(error) => return engine_error(error),
        izu_runtime::RuntimeError::Environment(error) => return environment_error(error),
        error => error,
    };
    let code = match error {
        izu_runtime::RuntimeError::Cancelled => ErrorCode::Cancelled,
        izu_runtime::RuntimeError::StaleWorkspace(_) => ErrorCode::StaleExpectation,
        izu_runtime::RuntimeError::UnknownJob(_) => ErrorCode::NotFound,
        izu_runtime::RuntimeError::DurabilityUncertain(_) => ErrorCode::PublicationUncertain,
        izu_runtime::RuntimeError::Unsupported(_) => ErrorCode::UnsupportedFeature,
        izu_runtime::RuntimeError::Invalid(_) => ErrorCode::InvalidRequest,
        izu_runtime::RuntimeError::Busy(_)
        | izu_runtime::RuntimeError::Capacity(_)
        | izu_runtime::RuntimeError::UnsafeClose(_) => ErrorCode::Conflict,
        _ => ErrorCode::Failure,
    };
    ApiError {code,message:error.to_string(),next_action:"Inspect the runtime job state and source status before retrying; unknown process ownership requires explicit recovery.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()}
}

fn git_signature(signature: GitCommitter) -> izu_git::GitSignature {
    izu_git::GitSignature {
        name: signature.name,
        email: signature.email,
        timestamp_unix_seconds: signature.timestamp_unix_seconds,
        timezone_minutes: signature.timezone_minutes,
    }
}

fn git_location(location: GitLocation) -> Result<izu_git::GitSource, ApiError> {
    match location {
        GitLocation::Local(path) => Ok(izu_git::GitSource::local(path)),
        GitLocation::Https(location) => izu_git::GitSource::https(location.url).map_err(git_error),
    }
}

fn git_import_options(refs: Vec<GitImportRef>) -> Result<izu_git::ImportOptions, ApiError> {
    if refs.len() > 1024 {
        return Err(ApiError::invalid(
            "At most 1024 explicit Git references are allowed",
        ));
    }
    let refs = refs
        .into_iter()
        .map(|reference| {
            Ok(izu_git::RefImport {
                native_ref: RefName::new(reference.native_ref).map_err(model_error)?,
                git_ref: reference.git_ref,
                expected: parse_optional(reference.expected, "expected native revision")?,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(izu_git::ImportOptions { refs })
}

fn git_error(error: izu_git::Error) -> ApiError {
    if let izu_git::Error::Native { source, .. } = error {
        return engine_error(source);
    }
    let code = match error {
        izu_git::Error::Cancelled => ErrorCode::Cancelled,
        izu_git::Error::PublicationUncertain(_) => ErrorCode::PublicationUncertain,
        izu_git::Error::LeaseMismatch { .. } => ErrorCode::StaleExpectation,
        izu_git::Error::Unsupported(_) | izu_git::Error::Unavailable(_) => {
            ErrorCode::UnsupportedFeature
        }
        izu_git::Error::InvalidSource(_)
        | izu_git::Error::InvalidRef(_)
        | izu_git::Error::InvalidObject { .. }
        | izu_git::Error::MissingCommitter => ErrorCode::InvalidRequest,
        izu_git::Error::NonFastForward => ErrorCode::Conflict,
        _ => ErrorCode::Failure,
    };
    ApiError {code,message:error.to_string(),next_action:"Inspect native and Git references. For publication uncertainty, read the exact target before retrying.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()}
}

fn bundle_error(error: izu_bundle::BundleError) -> ApiError {
    let error = match error {
        izu_bundle::BundleError::Engine(error) => return engine_error(error),
        izu_bundle::BundleError::Store(error) => return store_error(error),
        izu_bundle::BundleError::StagingRetained {
            stage,
            recovery_operation,
            failure,
        } => {
            let mut error = bundle_error(*failure);
            if let Some(operation) = recovery_operation {
                error.recovery_operation_id = Some(operation.to_string());
            }
            error.retain_paths([stage]);
            error.next_action = "Restore may have changed part of its owned stage. Preserve and inspect the retained paths and recovery operation before any retry; do not delete them.".into();
            return error;
        }
        izu_bundle::BundleError::StagingCleanup {
            stage,
            failure,
            cleanup,
        } => {
            let mut error = bundle_error(*failure);
            error.retain_paths([stage]);
            error.message = format!("{}; staging cleanup failed: {cleanup}", error.message);
            error.next_action =
                "Preserve and inspect the retained staging path before retrying restoration."
                    .into();
            return error;
        }
        error => error,
    };
    let code = match error.code() {
        "cancelled" => ErrorCode::Cancelled,
        "unsupported_bundle_version" => ErrorCode::UnsupportedFeature,
        "bundle_io" if matches!(&error, izu_bundle::BundleError::Io { source, .. } if source.kind() == std::io::ErrorKind::Unsupported) => {
            ErrorCode::UnsupportedFeature
        }
        "invalid_bundle"
        | "truncated_bundle"
        | "bundle_integrity"
        | "duplicate_object"
        | "missing_object"
        | "object_kind"
        | "unreachable_object"
        | "invalid_native_object" => ErrorCode::CorruptData,
        "uncertain" | "publication_uncertain" => ErrorCode::PublicationUncertain,
        "staging_changed" | "destination_exists" => ErrorCode::Conflict,
        "invalid_destination"
        | "workspace_selection_required"
        | "unknown_workspace"
        | "resource_limit" => ErrorCode::InvalidRequest,
        _ => ErrorCode::Failure,
    };
    ApiError {code,message:error.to_string(),next_action:"Preserve the bundle and destination. Inspect and verify the exact artifact before retrying any publication or restoration.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()}
}

fn store_error(error: izu_store::StoreError) -> ApiError {
    if matches!(
        &error,
        izu_store::StoreError::Model(izu_model::ModelError::Cancelled)
    ) {
        return cancelled();
    }
    let code = match error {
        izu_store::StoreError::Corrupt { .. }
        | izu_store::StoreError::UnexpectedKind { .. }
        | izu_store::StoreError::HistoryMismatch { .. }
        | izu_store::StoreError::Model(_) => ErrorCode::CorruptData,
        izu_store::StoreError::UnsupportedVersion { .. }
        | izu_store::StoreError::UnsupportedDurability(_) => ErrorCode::UnsupportedFeature,
        izu_store::StoreError::HeadConflict { .. } => ErrorCode::StaleExpectation,
        izu_store::StoreError::InvalidPath(_)
        | izu_store::StoreError::InputLengthMismatch
        | izu_store::StoreError::LimitExceeded(_) => ErrorCode::InvalidRequest,
        _ => ErrorCode::Failure,
    };
    ApiError {code,message:error.to_string(),next_action:"Preserve source and native objects; inspect `izu verify`, `izu recover`, and operation history before retrying.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()}
}

fn preparation(prepared: izu_engine::MergePreparation) -> Result<DispatchData, ApiError> {
    let mut data = value(&prepared)?;
    if let izu_engine::MergePreparation::Conflicted { candidate, .. } = prepared {
        data.uncertainty=Some(ApiError {code:ErrorCode::Conflict,message:format!("Candidate {candidate} durably records unresolved source conflicts"),next_action:"Inspect the candidate, resolve each explicit conflicting path with `izu resolve`, then check the new candidate and land it.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()});
    }
    Ok(data)
}

fn job_result(
    receipt: &impl Serialize,
    job: &izu_runtime::JobSnapshot,
    is_check: bool,
) -> Result<DispatchData, ApiError> {
    let mut data = value(receipt)?;
    let failure = match &job.state {
        izu_runtime::JobState::Finished { outcome, .. } if outcome.passed() => None,
        izu_runtime::JobState::Cancelled { .. } => Some(ErrorCode::Cancelled),
        izu_runtime::JobState::Finished { .. } => Some(if is_check {
            ErrorCode::MissingCheck
        } else {
            ErrorCode::Failure
        }),
        _ => Some(ErrorCode::Failure),
    };
    if let Some(code) = failure {
        data.uncertainty=Some(ApiError {code,message:format!("Managed job {} did not complete successfully",job.id),next_action:"Inspect the exact job state and output in this receipt. Failed or cancelled check evidence cannot authorize landing.".into(),operation_id:None,recovery_operation_id:None,retained_paths:Box::default()});
    }
    Ok(data)
}

fn diff_details(
    repo: &Repository,
    changes: Vec<izu_engine::PathChange>,
    cancel: &CancellationToken,
) -> Result<DispatchData, ApiError> {
    let mut files = Vec::new();
    let mut remaining = 4 * 1024 * 1024_usize;
    for change in &changes {
        let before = diff_content(repo, change.before.as_ref(), &mut remaining, cancel)?;
        let after = diff_content(repo, change.after.as_ref(), &mut remaining, cancel)?;
        let detail = match (before, after) {
            (Some(before), Some(after))
                if std::str::from_utf8(&before).is_ok()
                    && std::str::from_utf8(&after).is_ok()
                    && !before.contains(&0)
                    && !after.contains(&0) =>
            {
                let options = izu_merge::Options {
                    cancellation: Some(cancel.as_atomic()),
                    limits: izu_merge::Limits {
                        max_input_bytes: 2 * 1024 * 1024,
                        max_output_bytes: 2 * 1024 * 1024,
                        max_work: 5_000_000,
                        ..Default::default()
                    },
                };
                match izu_merge::diff(&before, &after, &options).and_then(|patch| {
                    izu_merge::render_unified(
                        &patch,
                        format!("a/{}", change.path.as_str()).as_bytes(),
                        format!("b/{}", change.path.as_str()).as_bytes(),
                        3,
                        &options,
                    )
                }) {
                    Ok(patch) => {
                        json!({"kind":"text","patch":String::from_utf8(patch).map_err(|error|failure(error.to_string()))?})
                    }
                    Err(izu_merge::MergeError::Cancelled) => return Err(cancelled()),
                    Err(error) => json!({"kind":"omitted","reason":error.to_string()}),
                }
            }
            (Some(_), Some(_)) => json!({"kind":"binary"}),
            _ => {
                json!({"kind":"omitted","reason":"Non-file source, unresolved conflict, or content exceeds the bounded text diff scope"})
            }
        };
        files.push(json!({"path":change.path,"change":change.kind,"content":detail}));
    }
    value(json!({"changes":changes,"files":files}))
}

fn diff_content(
    repo: &Repository,
    entry: Option<&izu_model::TreeEntry>,
    remaining: &mut usize,
    cancel: &CancellationToken,
) -> Result<Option<Vec<u8>>, ApiError> {
    let blob = match entry {
        None => return Ok(Some(Vec::new())),
        Some(izu_model::TreeEntry::File { blob, .. }) => *blob,
        _ => return Ok(None),
    };
    let info = repo.object_info(blob, cancel).map_err(engine_error)?;
    if info.payload_len > 1024 * 1024 || info.payload_len > (*remaining as u64) {
        return Ok(None);
    }
    let bytes = repo.blob(blob, cancel).map_err(engine_error)?;
    *remaining = remaining.saturating_sub(bytes.len());
    Ok(Some(bytes))
}

fn bundle_receipt(
    receipt: &impl Serialize,
    publication: &izu_bundle::Publication,
    operation: String,
) -> Result<DispatchData, ApiError> {
    let mut result = value(receipt)?;
    if let izu_bundle::Publication::VisibleButUncertain { reason } = publication {
        result.uncertainty=Some(ApiError {code:ErrorCode::PublicationUncertain,message:format!("Bundle publication is visible but its durability is uncertain: {reason}"),next_action:"Verify the exact artifact or restored destination and inspect its root operation before retrying; do not assume publication was rolled back.".into(),operation_id:Some(operation),recovery_operation_id:None,retained_paths:Box::default()});
    }
    Ok(result)
}

#[cfg(test)]
mod error_tests {
    use super::*;

    #[test]
    fn wrapped_cancellation_remains_cancellation() {
        for error in [
            izu_engine::EngineError::Model(izu_model::ModelError::Cancelled),
            izu_engine::EngineError::Store(izu_store::StoreError::Model(
                izu_model::ModelError::Cancelled,
            )),
        ] {
            assert!(matches!(engine_error(error).code, ErrorCode::Cancelled));
        }
        assert!(matches!(
            runtime_error(izu_runtime::RuntimeError::Engine(
                izu_engine::EngineError::Cancelled
            ))
            .code,
            ErrorCode::Cancelled
        ));
    }

    #[test]
    fn restoration_uncertainty_keeps_visible_and_recovery_operations_distinct() {
        let visible = "01".repeat(32).parse().expect("visible operation");
        let recovery = "02".repeat(32).parse().expect("recovery operation");
        let error = engine_error(izu_engine::EngineError::RestorationUncertain {
            recovery,
            operation: Some(visible),
            reason: "injected final sync failure".into(),
        });
        assert!(matches!(error.code, ErrorCode::PublicationUncertain));
        assert_eq!(error.operation_id, Some(visible.to_string()));
        assert_eq!(error.recovery_operation_id, Some(recovery.to_string()));
    }

    #[test]
    fn retained_bundle_stage_keeps_native_recovery_provenance() {
        let recovery = "03".repeat(32).parse().expect("recovery operation");
        let error = bundle_error(izu_bundle::BundleError::StagingRetained {
            stage: PathBuf::from("retained-owned-stage"),
            recovery_operation: Some(recovery),
            failure: Box::new(izu_bundle::BundleError::Engine(
                izu_engine::EngineError::RestorationFailed {
                    recovery,
                    reason: "injected partial source failure".into(),
                },
            )),
        });
        assert_eq!(error.recovery_operation_id, Some(recovery.to_string()));
        assert_eq!(
            error.retained_paths.as_ref(),
            [PathBuf::from("retained-owned-stage")]
        );
        assert!(error.next_action.contains("Preserve"));
    }

    #[test]
    fn late_check_attempt_is_a_stale_precondition() {
        let current = "04".repeat(16).parse().expect("current attempt");
        let received = "05".repeat(16).parse().expect("old attempt");
        let error = engine_error(izu_engine::EngineError::StaleCheckAttempt { current, received });
        assert!(matches!(error.code, ErrorCode::StaleExpectation));
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn visible_environment_namespace_change_preserves_uncertainty_and_path() {
        let destination = PathBuf::from("visible-environment-destination");
        let error = environment_error(
            izu_environment::EnvironmentError::PublicationIdentityUncertain {
                path: destination.clone(),
                source: Box::new(izu_environment::EnvironmentError::NamespaceChanged(
                    destination.clone(),
                )),
            },
        );
        assert!(matches!(error.code, ErrorCode::PublicationUncertain));
        assert_eq!(error.exit_code(), 6);
        assert_eq!(error.retained_paths.as_ref(), [destination]);
        assert!(error.next_action.contains("already be visible"));
    }

    #[test]
    fn starting_environment_mismatch_is_a_stale_precondition() {
        let error = environment_error(
            izu_environment::EnvironmentError::StartingEnvironmentMismatch(
                "actual private files differ from the bound manifest".into(),
            ),
        );
        assert!(matches!(error.code, ErrorCode::StaleExpectation));
        assert_eq!(error.exit_code(), 4);
    }

    #[test]
    fn runtime_environment_errors_keep_stale_and_cancelled_classifications() {
        let error = runtime_error(izu_runtime::RuntimeError::Environment(
            izu_environment::EnvironmentError::StartingEnvironmentMismatch(
                "actual private files differ from the bound manifest".into(),
            ),
        ));
        assert!(matches!(error.code, ErrorCode::StaleExpectation));
        assert_eq!(error.exit_code(), 4);
        let error = runtime_error(izu_runtime::RuntimeError::Environment(
            izu_environment::EnvironmentError::Cancelled,
        ));
        assert!(matches!(error.code, ErrorCode::Cancelled));
        assert_eq!(error.exit_code(), 130);
    }

    #[test]
    fn runtime_environment_publication_keeps_paths_and_native_recovery_provenance() {
        let visible = "07".repeat(32).parse().expect("visible operation");
        let recovery = "08".repeat(32).parse().expect("recovery operation");
        let destination = PathBuf::from("visible-runtime-environment-destination");
        let error = runtime_error(izu_runtime::RuntimeError::Environment(
            izu_environment::EnvironmentError::PublicationIdentityUncertain {
                path: destination.clone(),
                source: Box::new(izu_environment::EnvironmentError::Workspace(
                    izu_engine::EngineError::RestorationUncertain {
                        recovery,
                        operation: Some(visible),
                        reason: "injected final sync failure".into(),
                    },
                )),
            },
        ));
        assert!(matches!(error.code, ErrorCode::PublicationUncertain));
        assert_eq!(error.exit_code(), 6);
        assert_eq!(error.operation_id, Some(visible.to_string()));
        assert_eq!(error.recovery_operation_id, Some(recovery.to_string()));
        assert_eq!(error.retained_paths.as_ref(), [destination]);
        assert!(error.next_action.contains("already be visible"));
    }

    #[test]
    fn unsupported_bundle_filesystem_remains_an_unsupported_feature() {
        let error = bundle_error(izu_bundle::BundleError::Io {
            action: "require persistent artifact filesystem",
            source: std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "known volatile filesystem cannot acknowledge durable publication",
            ),
        });
        assert!(matches!(error.code, ErrorCode::UnsupportedFeature));
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn unsupported_environment_filesystem_remains_an_unsupported_feature() {
        let error = runtime_error(izu_runtime::RuntimeError::Environment(
            izu_environment::EnvironmentError::Io {
                path: PathBuf::from("volatile-environment-cache"),
                source: std::io::Error::new(
                    std::io::ErrorKind::Unsupported,
                    "known volatile filesystem cannot acknowledge durable publication",
                ),
            },
        ));
        assert!(matches!(error.code, ErrorCode::UnsupportedFeature));
        assert_eq!(error.exit_code(), 3);
    }

    #[test]
    fn uncommitted_source_requires_preservation_and_reconciliation() {
        let error = engine_error(izu_engine::EngineError::UncommittedSource {
            workspace: "06".repeat(16).parse().expect("workspace"),
            path: RepoPath::new("owned-dirty.txt").expect("path"),
        });
        assert!(matches!(error.code, ErrorCode::Conflict));
        assert_eq!(error.exit_code(), 4);
        assert!(error.next_action.contains("Preserve"));
        assert!(error.next_action.contains("reconcile"));
    }
}
