//! Schema-versioned interface boundary shared by CLI, JSON, and MCP.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
pub const MAX_RESULT_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub schema_version: u32,
    pub operation: Operation,
}

impl Request {
    pub fn new(operation: Operation) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            operation,
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReadContext {
    /// Repository directory or a directory within a registered workspace.
    pub repository: PathBuf,
    /// Exact workspace ID; omission selects only the workspace containing repository.
    pub workspace: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expectation {
    pub head: String,
    pub working_tree: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MutationContext {
    pub repository: PathBuf,
    pub workspace: String,
    pub expected: Expectation,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PathSelection {
    All {},
    Paths { paths: Vec<String> },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Author {
    pub name: String,
    pub email: String,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CheckDefinition {
    pub name: String,
    /// Explicit executable and arguments. Never interpreted as a shell command.
    pub argv: Vec<String>,
    /// Native Blob object ID returned by environment_bind; never a cache key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReferenceExpectation {
    pub name: String,
    /// None explicitly expects an absent native reference.
    pub expected: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LaunchArguments {
    /// Executable and arguments, without shell interpretation.
    pub argv: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub timeout_seconds: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagedStartTarget {
    New {
        context: ReadContext,
        name: String,
        from: String,
    },
    Resume {
        context: MutationContext,
    },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentSharing {
    PreferClone,
    Copy,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitCommitter {
    pub name: String,
    pub email: String,
    pub timestamp_unix_seconds: i64,
    pub timezone_minutes: i16,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitImportRef {
    pub native_ref: String,
    pub git_ref: String,
    /// None explicitly expects an absent native reference.
    pub expected: Option<String>,
}

/// Legacy string paths remain local. HTTPS requires an explicit URL object.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(untagged)]
pub enum GitLocation {
    Local(PathBuf),
    Https(HttpsLocation),
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct HttpsLocation {
    pub url: String,
}

#[derive(Clone, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum GitAuthentication {
    Basic { username: String, password: String },
}

impl std::fmt::Debug for GitAuthentication {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("GitAuthentication(redacted)")
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitTransport {
    pub authentication: Option<GitAuthentication>,
    #[serde(default)]
    pub root_certificates: Vec<Vec<u8>>,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConflictResolution {
    Base {},
    Ours {},
    Theirs {},
    Delete {},
    File { content: String, executable: bool },
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Operation {
    Capabilities {},
    Schema {},
    Init {
        path: PathBuf,
    },
    Inspect {
        context: ReadContext,
    },
    Status {
        context: ReadContext,
    },
    Diff {
        context: ReadContext,
        before: Option<String>,
        after: Option<String>,
    },
    Log {
        context: ReadContext,
        from: Option<String>,
        limit: usize,
    },
    Show {
        context: ReadContext,
        revision: String,
    },
    Checkpoint {
        context: MutationContext,
        selection: PathSelection,
    },
    Commit {
        context: MutationContext,
        selection: PathSelection,
        message: String,
        author: Author,
        /// Explicit atomic reference publication. Omission commits only the workspace.
        #[serde(default)]
        target: Option<ReferenceExpectation>,
    },
    Revise {
        context: MutationContext,
        selection: PathSelection,
        message: String,
        author: Author,
    },
    WorkspaceStart {
        context: ReadContext,
        name: String,
        path: PathBuf,
        from: String,
    },
    WorkspaceList {
        context: ReadContext,
    },
    WorkspaceClose {
        context: MutationContext,
    },
    RefList {
        context: ReadContext,
    },
    RefSet {
        context: ReadContext,
        name: String,
        expected: Option<String>,
        revision: Option<String>,
    },
    Restore {
        context: MutationContext,
        revision: String,
        selection: PathSelection,
    },
    /// Move a clean workspace to one exact revision, with recovery preservation.
    Update {
        context: MutationContext,
        revision: String,
    },
    Undo {
        context: MutationContext,
        operation: String,
        selection: PathSelection,
    },
    Revert {
        context: MutationContext,
        revision: String,
        selection: PathSelection,
        message: String,
        author: Author,
        #[serde(default)]
        target: Option<ReferenceExpectation>,
    },
    Operations {
        context: ReadContext,
        limit: usize,
    },
    Candidate {
        context: ReadContext,
        source: String,
        target: String,
        expected_target: Option<String>,
        checks: Vec<CheckDefinition>,
        author: Author,
    },
    CandidateShow {
        context: ReadContext,
        candidate: String,
    },
    Resolve {
        context: ReadContext,
        candidate: String,
        path: String,
        resolution: ConflictResolution,
        author: Author,
    },
    ResolveRevision {
        context: ReadContext,
        revision: String,
        path: String,
        resolution: ConflictResolution,
        author: Author,
    },
    Check {
        context: ReadContext,
        candidate: String,
        check: String,
        timeout_seconds: u64,
    },
    CheckStart {
        context: ReadContext,
        candidate: String,
        check: String,
    },
    Land {
        context: ReadContext,
        candidate: String,
    },
    /// Prepare, run explicitly declared checks, and conditionally publish one exact revision.
    LandCurrent {
        context: ReadContext,
        source: String,
        target: ReferenceExpectation,
        checks: Vec<CheckDefinition>,
        author: Author,
        timeout_seconds: u64,
    },
    /// Human managed launch wrapper; intentionally absent from MCP tools.
    ManagedStart {
        target: ManagedStartTarget,
        launch: LaunchArguments,
    },
    ManagedRun {
        context: MutationContext,
        launch: LaunchArguments,
    },
    Verify {
        context: ReadContext,
    },
    Recover {
        context: ReadContext,
    },
    JobsList {
        context: ReadContext,
        limit: usize,
    },
    JobStatus {
        context: ReadContext,
        job: String,
    },
    JobCancel {
        context: ReadContext,
        job: String,
    },
    JobAcknowledgeStopped {
        context: ReadContext,
        job: String,
        operator_note: String,
    },
    WriterStatus {
        context: ReadContext,
    },
    WriterAcknowledgeStopped {
        context: ReadContext,
        token: String,
        operator_note: String,
    },
    EnvironmentSourceIdentity {
        context: MutationContext,
    },
    EnvironmentBind {
        context: ReadContext,
        recipe_json: String,
        trusted: bool,
    },
    EnvironmentStatus {
        context: ReadContext,
        cache: Option<PathBuf>,
        recipe_json: String,
        trusted: bool,
    },
    EnvironmentImport {
        context: ReadContext,
        cache: Option<PathBuf>,
        recipe_json: String,
        prepared: PathBuf,
        trusted: bool,
        quiescent: bool,
        sharing: EnvironmentSharing,
    },
    EnvironmentMaterialize {
        context: MutationContext,
        cache: Option<PathBuf>,
        recipe_json: String,
        trusted: bool,
        sharing: EnvironmentSharing,
    },
    /// Archive the already durable native operation closure; uncaptured source is omitted.
    BundleCreate {
        context: ReadContext,
        path: PathBuf,
    },
    BundleInspect {
        path: PathBuf,
    },
    BundleVerify {
        path: PathBuf,
    },
    BundleRestore {
        path: PathBuf,
        destination: PathBuf,
        workspace: Option<String>,
    },
    GitImport {
        context: ReadContext,
        source: GitLocation,
        refs: Vec<GitImportRef>,
        #[serde(default)]
        transport: GitTransport,
    },
    GitExport {
        context: ReadContext,
        target: PathBuf,
        committer: Option<GitCommitter>,
    },
    GitFetch {
        context: ReadContext,
        source: GitLocation,
        refs: Vec<GitImportRef>,
        #[serde(default)]
        transport: GitTransport,
    },
    GitPush {
        context: ReadContext,
        target: GitLocation,
        native_ref: String,
        git_ref: String,
        expected_old: Option<String>,
        allow_non_fast_forward: bool,
        committer: Option<GitCommitter>,
        #[serde(default)]
        transport: GitTransport,
    },
    /// Launch an explicitly chosen process in an exact registered workspace.
    AgentRun {
        context: MutationContext,
        cwd: PathBuf,
        argv: Vec<String>,
        env: BTreeMap<String, String>,
        timeout_seconds: u64,
    },
}

impl Operation {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Capabilities {} => "capabilities",
            Self::Schema {} => "schema",
            Self::Init { .. } => "init",
            Self::Inspect { .. } => "inspect",
            Self::Status { .. } => "status",
            Self::Diff { .. } => "diff",
            Self::Log { .. } => "log",
            Self::Show { .. } => "show",
            Self::Checkpoint { .. } => "checkpoint",
            Self::Commit { .. } => "commit",
            Self::Revise { .. } => "revise",
            Self::WorkspaceStart { .. } => "workspace_start",
            Self::WorkspaceList { .. } => "workspace_list",
            Self::WorkspaceClose { .. } => "workspace_close",
            Self::RefList { .. } => "ref_list",
            Self::RefSet { .. } => "ref_set",
            Self::Restore { .. } => "restore",
            Self::Update { .. } => "update",
            Self::Undo { .. } => "undo",
            Self::Revert { .. } => "revert",
            Self::Operations { .. } => "operations",
            Self::Candidate { .. } => "candidate",
            Self::CandidateShow { .. } => "candidate_show",
            Self::Resolve { .. } => "resolve",
            Self::ResolveRevision { .. } => "resolve_revision",
            Self::Check { .. } => "check",
            Self::CheckStart { .. } => "check_start",
            Self::Land { .. } => "land",
            Self::LandCurrent { .. } => "land_current",
            Self::ManagedStart { .. } => "managed_start",
            Self::ManagedRun { .. } => "managed_run",
            Self::Verify { .. } => "verify",
            Self::Recover { .. } => "recover",
            Self::JobsList { .. } => "jobs_list",
            Self::JobStatus { .. } => "job_status",
            Self::JobCancel { .. } => "job_cancel",
            Self::JobAcknowledgeStopped { .. } => "job_acknowledge_stopped",
            Self::WriterStatus { .. } => "writer_status",
            Self::WriterAcknowledgeStopped { .. } => "writer_acknowledge_stopped",
            Self::EnvironmentSourceIdentity { .. } => "environment_source_identity",
            Self::EnvironmentBind { .. } => "environment_bind",
            Self::EnvironmentStatus { .. } => "environment_status",
            Self::EnvironmentImport { .. } => "environment_import",
            Self::EnvironmentMaterialize { .. } => "environment_materialize",
            Self::BundleCreate { .. } => "bundle_create",
            Self::BundleInspect { .. } => "bundle_inspect",
            Self::BundleVerify { .. } => "bundle_verify",
            Self::BundleRestore { .. } => "bundle_restore",
            Self::GitImport { .. } => "git_import",
            Self::GitExport { .. } => "git_export",
            Self::GitFetch { .. } => "git_fetch",
            Self::GitPush { .. } => "git_push",
            Self::AgentRun { .. } => "agent_run",
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct BuildIdentity {
    pub product: String,
    pub version: String,
    pub native_format_version: u32,
    pub build_id: Option<String>,
}

impl Default for BuildIdentity {
    fn default() -> Self {
        Self {
            product: "izu".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            native_format_version: 1,
            build_id: option_env!("IZU_BUILD_ID").map(str::to_owned),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct Response {
    pub schema_version: u32,
    pub build: BuildIdentity,
    pub outcome: Outcome,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Outcome {
    Ok {
        result: OperationResult,
    },
    Uncertain {
        error: ApiError,
        result: OperationResult,
    },
    Error {
        error: ApiError,
        #[serde(skip_serializing_if = "Option::is_none")]
        result: Option<OperationResult>,
    },
}

/// Native payloads retain the engine's serialized record types. The stable outer
/// envelope always identifies the operation and distinguishes failure/uncertainty.
#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct OperationResult {
    pub kind: String,
    pub data: serde_json::Value,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    InvalidRequest,
    UnsupportedSchema,
    UnsupportedFeature,
    NotFound,
    MissingCheck,
    StaleExpectation,
    PublicationUncertain,
    Cancelled,
    CorruptData,
    Conflict,
    Failure,
}

#[derive(Clone, Debug, Deserialize, Serialize, JsonSchema)]
pub struct ApiError {
    pub code: ErrorCode,
    pub message: String,
    pub next_action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Exact recovery source operation, distinct from a possibly visible final operation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recovery_operation_id: Option<String>,
    #[serde(default, skip_serializing_if = "retained_paths_empty")]
    pub retained_paths: Box<[PathBuf]>,
}

fn retained_paths_empty(paths: &[PathBuf]) -> bool {
    paths.is_empty()
}

impl ApiError {
    pub(crate) fn retain_paths(&mut self, paths: impl IntoIterator<Item = PathBuf>) {
        let mut retained = std::mem::take(&mut self.retained_paths).into_vec();
        retained.extend(paths);
        self.retained_paths = retained.into_boxed_slice();
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::InvalidRequest,
            message: message.into(),
            next_action: "Inspect `izu schema` and retry with a valid request.".into(),
            operation_id: None,
            recovery_operation_id: None,
            retained_paths: Box::default(),
        }
    }

    pub fn unsupported(feature: impl Into<String>) -> Self {
        Self {
            code: ErrorCode::UnsupportedFeature,
            message: format!("{} is not supported by this build", feature.into()),
            next_action: "Inspect `izu capabilities` for supported operations.".into(),
            operation_id: None,
            recovery_operation_id: None,
            retained_paths: Box::default(),
        }
    }

    pub fn exit_code(&self) -> u8 {
        match self.code {
            ErrorCode::InvalidRequest | ErrorCode::UnsupportedSchema => 2,
            ErrorCode::UnsupportedFeature => 3,
            ErrorCode::StaleExpectation | ErrorCode::Conflict => 4,
            ErrorCode::MissingCheck => 5,
            ErrorCode::PublicationUncertain => 6,
            ErrorCode::CorruptData => 7,
            ErrorCode::Cancelled => 130,
            ErrorCode::NotFound | ErrorCode::Failure => 1,
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}. {}", self.message, self.next_action)?;
        for path in &self.retained_paths {
            write!(f, "\nRetained source: {path:?}")?;
        }
        Ok(())
    }
}

impl std::error::Error for ApiError {}

impl Response {
    pub fn success(kind: impl Into<String>, data: serde_json::Value) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            build: BuildIdentity::default(),
            outcome: Outcome::Ok {
                result: OperationResult {
                    kind: kind.into(),
                    data,
                },
            },
        }
    }

    pub fn error(error: ApiError) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            build: BuildIdentity::default(),
            outcome: Outcome::Error {
                error,
                result: None,
            },
        }
    }

    pub fn uncertain(kind: impl Into<String>, data: serde_json::Value, error: ApiError) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            build: BuildIdentity::default(),
            outcome: Outcome::Uncertain {
                error,
                result: OperationResult {
                    kind: kind.into(),
                    data,
                },
            },
        }
    }

    pub fn rejected(kind: impl Into<String>, data: serde_json::Value, error: ApiError) -> Self {
        Self {
            schema_version: SCHEMA_VERSION,
            build: BuildIdentity::default(),
            outcome: Outcome::Error {
                error,
                result: Some(OperationResult {
                    kind: kind.into(),
                    data,
                }),
            },
        }
    }

    pub fn exit_code(&self) -> u8 {
        match &self.outcome {
            Outcome::Ok { .. } => 0,
            Outcome::Error { error, .. } | Outcome::Uncertain { error, .. } => error.exit_code(),
        }
    }
}

pub fn parse_request(input: &[u8]) -> Result<Request, ApiError> {
    if input.len() > MAX_REQUEST_BYTES {
        return Err(ApiError::invalid(format!(
            "Request exceeds {MAX_REQUEST_BYTES} bytes"
        )));
    }
    let request: Request = serde_json::from_slice(input)
        .map_err(|e| ApiError::invalid(format!("Invalid JSON request: {e}")))?;
    if request.schema_version != SCHEMA_VERSION {
        return Err(ApiError {
            code: ErrorCode::UnsupportedSchema,
            message: format!(
                "Unsupported schema version {} (supported: {SCHEMA_VERSION})",
                request.schema_version
            ),
            next_action: "Inspect `izu schema` and select a supported schema version.".into(),
            operation_id: None,
            recovery_operation_id: None,
            retained_paths: Box::default(),
        });
    }
    Ok(request)
}

pub fn request_schema() -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(schemars::schema_for!(Request))
        .map_err(|e| ApiError::invalid(e.to_string()))
}

pub fn operation_names() -> Result<Vec<String>, ApiError> {
    let schema = request_schema()?;
    let variants = schema
        .pointer("/$defs/Operation/oneOf")
        .or_else(|| schema.pointer("/$defs/Operation/anyOf"))
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| ApiError::invalid("Generated operation schema has no variant set"))?;
    variants
        .iter()
        .map(|variant| {
            variant
                .pointer("/properties/kind/const")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| ApiError::invalid("Generated operation schema has no kind tag"))
        })
        .collect()
}

pub fn response_schema() -> Result<serde_json::Value, ApiError> {
    serde_json::to_value(schemars::schema_for!(Response))
        .map_err(|e| ApiError::invalid(e.to_string()))
}
