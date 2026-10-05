#![forbid(unsafe_code)]
//! Thin human and JSON interfaces over the shared izu API.

pub mod mcp;

use clap::{Args, Parser, Subcommand};
use izu_api::{
    Api, ApiError, Author, CancellationToken, CheckDefinition, ConflictResolution,
    EnvironmentSharing, GitAuthentication, GitCommitter, GitImportRef, GitLocation, GitTransport,
    HttpsLocation, LaunchArguments, MAX_REQUEST_BYTES, MutationContext, Operation, Outcome,
    PathSelection, ReadContext, Request, Response, parse_request,
};
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::PathBuf;

#[derive(Debug, Parser)]
#[command(
    name = "izu",
    version,
    about = "Original native version control for human and agent work",
    long_about = "Original native version control. Local checkpoints are acknowledged only after required filesystem barriers; they are not remote backups. Use `capabilities` to inspect this build's supported operations and limitations."
)]
pub struct Cli {
    #[arg(
        long,
        global = true,
        default_value = ".",
        help = "Repository or registered workspace directory"
    )]
    pub repo: PathBuf,
    #[arg(
        long,
        global = true,
        help = "Advanced exact workspace ID; normal use selects --change NAME or the current directory"
    )]
    pub workspace: Option<String>,
    #[arg(
        long,
        global = true,
        conflicts_with = "workspace",
        help = "Select one open private change by name"
    )]
    pub change: Option<String>,
    #[arg(long, global = true, help = "Print one schema-versioned JSON response")]
    pub json: bool,
    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Create a native repository in a new or existing project directory.
    Init {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
    /// Inspect supported operations and limitations of this build.
    Capabilities,
    /// Print the JSON request and response schemas.
    Schema,
    /// Compare files with the selected workspace revision.
    Status,
    /// Inspect the selected workspace path and exact revision/tree expectation.
    Inspect,
    /// Compare working files, or two explicit native revisions.
    Diff {
        #[arg(long, requires = "after")]
        before: Option<String>,
        #[arg(long, requires = "before")]
        after: Option<String>,
    },
    /// Show ancestry of an explicit revision or selected workspace head.
    Log {
        revision: Option<String>,
        #[arg(long, conflicts_with = "revision")]
        from: Option<String>,
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Show an exact native revision record.
    Show { revision: String },
    /// Save a durable local snapshot, without creating an intentional commit.
    Checkpoint {
        #[command(flatten)]
        selection: SelectionArgs,
    },
    /// Create an intentional commit; unselected working edits are preserved.
    Commit {
        #[arg(short = 'm', long)]
        message: String,
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        author: AuthorArgs,
    },
    /// Start or resume a named private change and launch an explicitly chosen program there.
    Start {
        name: String,
        #[arg(long, help = "Reference or revision; default is the current main")]
        from: Option<String>,
        #[command(flatten)]
        launch: LaunchArgs,
    },
    /// Launch in the selected private change, checkpointing before and after execution.
    Run {
        #[command(flatten)]
        launch: LaunchArgs,
    },
    /// Manage private workspaces for independent changes.
    Change {
        #[command(subcommand)]
        command: ChangeCommand,
    },
    /// Inspect, select, and close registered workspaces.
    Workspace {
        #[command(subcommand)]
        command: WorkspaceCommand,
    },
    /// List or conditionally update native references.
    Refs {
        #[command(subcommand)]
        command: RefCommand,
    },
    /// Restore selected files from an exact revision, preserving the prior source.
    Restore {
        revision: String,
        #[command(flatten)]
        selection: SelectionArgs,
    },
    /// Move the selected clean workspace to main (or another explicit reference/revision).
    Update {
        #[arg(default_value = "main")]
        revision: String,
    },
    /// Undo selected source files from one operation; never rewind the whole repository.
    Undo {
        operation: String,
        #[command(flatten)]
        selection: SelectionArgs,
    },
    /// Apply the selected inverse of one single-parent revision, preserving unselected edits.
    Revert {
        revision: String,
        #[arg(short = 'm', long)]
        message: String,
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        author: AuthorArgs,
    },
    /// Inspect the durable operation lineage.
    Operations {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    /// Prepare a merge candidate with an explicitly required argv check.
    Candidate {
        source: String,
        #[arg(long)]
        target: String,
        #[arg(long, group = "target_expectation", required = true)]
        expect: Option<String>,
        #[arg(long, group = "target_expectation", required = true)]
        expect_absent: bool,
        #[arg(long)]
        check: String,
        #[arg(
            long,
            help = "Native object ID returned by environment bind, not its cache key"
        )]
        environment: Option<String>,
        #[command(flatten)]
        author: AuthorArgs,
        #[arg(last=true,required=true,num_args=1..)]
        argv: Vec<String>,
    },
    /// Show a candidate and its exact required checks.
    CandidateShow { candidate: String },
    /// Resolve one durable conflict path by an explicit side or supplied text file.
    Resolve {
        candidate: String,
        #[arg(long)]
        path: String,
        #[arg(long, group = "resolution", required = true, value_enum)]
        take: Option<ResolutionSide>,
        #[arg(long, group = "resolution", required = true)]
        content_file: Option<PathBuf>,
        #[arg(long, requires = "content_file")]
        executable: bool,
        #[command(flatten)]
        author: AuthorArgs,
    },
    /// Resolve a durable native revision conflict without automatically changing source.
    ResolveRevision {
        revision: String,
        #[arg(long)]
        path: String,
        #[arg(long, group = "resolution", required = true, value_enum)]
        take: Option<ResolutionSide>,
        #[arg(long, group = "resolution", required = true)]
        content_file: Option<PathBuf>,
        #[arg(long, requires = "content_file")]
        executable: bool,
        #[command(flatten)]
        author: AuthorArgs,
    },
    /// Run one candidate's declared check and attach evidence for its exact inputs.
    Check {
        candidate: String,
        #[arg(long)]
        check: String,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[arg(
            long,
            help = "Publish a pending attempt without launching; immediately invalidates an earlier pass"
        )]
        start_only: bool,
    },
    /// Integrate a candidate only when all exact checks pass and the ref matches.
    Land {
        candidate: Option<String>,
        #[arg(long, conflicts_with_all = ["candidate", "source"])]
        current: bool,
        #[arg(long, conflicts_with = "candidate")]
        source: Option<String>,
        #[arg(long, default_value = "main")]
        target: String,
        #[arg(long)]
        check: Option<String>,
        #[arg(
            long,
            help = "Verified starting environment binding for the declared check"
        )]
        environment: Option<String>,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[command(flatten)]
        author: OptionalAuthorArgs,
        #[arg(last = true, num_args = 1..)]
        argv: Vec<String>,
    },
    /// Verify referenced native objects; this does not run project tests.
    Verify,
    /// Inspect recoverable interrupted native publication state.
    Recover,
    /// Inspect, cancel, or explicitly acknowledge stopped managed jobs.
    Jobs {
        #[command(subcommand)]
        command: JobCommand,
    },
    /// Inspect native writer intent or record an exact-token stopped assertion.
    Writer {
        #[command(subcommand)]
        command: WriterCommand,
    },
    /// Explicit trusted prepared-environment cache operations; discovery runs no recipes.
    Environment {
        #[command(subcommand)]
        command: EnvironmentCommand,
    },
    /// Create, verify, and restore a portable complete native snapshot.
    Bundle {
        #[command(subcommand)]
        command: BundleCommand,
    },
    /// Explicit interoperability with local Git repositories or HTTPS URLs.
    Git {
        #[command(subcommand)]
        command: GitCommand,
    },
    /// Agent protocol discovery or an explicitly bound managed process.
    Agent {
        #[command(subcommand)]
        command: AgentCommand,
    },
    /// Read one bounded typed request from stdin and print its response.
    Json,
    #[command(name = "__runtime-worker", hide = true)]
    RuntimeWorker {
        #[arg(long)]
        ticket: PathBuf,
    },
    #[command(name = "__process-worker", hide = true)]
    ProcessWorker {
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        internal: Vec<OsString>,
    },
}

#[derive(Debug, Args)]
pub struct SelectionArgs {
    #[arg(
        long = "path",
        help = "Select a repository-relative file or directory; repeat for multiple paths"
    )]
    pub paths: Vec<String>,
}

impl SelectionArgs {
    fn into_selection(self) -> PathSelection {
        if self.paths.is_empty() {
            PathSelection::All {}
        } else {
            PathSelection::Paths { paths: self.paths }
        }
    }
}

#[derive(Debug, Args)]
pub struct AuthorArgs {
    #[arg(
        long,
        env = "IZU_AUTHOR_NAME",
        help = "Explicit author name; may be configured in IZU_AUTHOR_NAME"
    )]
    pub author_name: String,
    #[arg(
        long,
        env = "IZU_AUTHOR_EMAIL",
        help = "Explicit author email; may be configured in IZU_AUTHOR_EMAIL"
    )]
    pub author_email: String,
}

impl From<AuthorArgs> for Author {
    fn from(value: AuthorArgs) -> Self {
        Self {
            name: value.author_name,
            email: value.author_email,
        }
    }
}

#[derive(Debug, Args)]
pub struct OptionalAuthorArgs {
    #[arg(long, env = "IZU_AUTHOR_NAME", requires = "author_email")]
    author_name: Option<String>,
    #[arg(long, env = "IZU_AUTHOR_EMAIL", requires = "author_name")]
    author_email: Option<String>,
}

impl OptionalAuthorArgs {
    fn into_author(self) -> Result<Author, ApiError> {
        match (self.author_name, self.author_email) {
            (Some(name), Some(email)) => Ok(Author { name, email }),
            _ => Err(ApiError::invalid(
                "Supply --author-name and --author-email, or explicitly configure IZU_AUTHOR_NAME and IZU_AUTHOR_EMAIL",
            )),
        }
    }
}

#[derive(Debug, Args)]
pub struct LaunchArgs {
    #[arg(long, default_value_t = 300)]
    timeout_seconds: u64,
    #[arg(long = "env", value_parser = parse_env)]
    env: Vec<(String, String)>,
    #[arg(last = true, required = true, num_args = 1..)]
    argv: Vec<String>,
}

impl From<LaunchArgs> for LaunchArguments {
    fn from(value: LaunchArgs) -> Self {
        Self {
            argv: value.argv,
            env: value.env.into_iter().collect(),
            timeout_seconds: value.timeout_seconds,
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum ChangeCommand {
    List,
    /// Inspect an open private change by name.
    Open {
        name: String,
    },
    /// Preserve source and deregister the named private change; its directory remains.
    Close {
        name: String,
    },
    /// Create a private workspace at an explicit path from one revision.
    Start {
        #[arg(long)]
        name: String,
        #[arg(long)]
        path: PathBuf,
        #[arg(long)]
        from: Option<String>,
    },
    /// Revise the selected logical change while preserving its earlier immutable revision.
    Revise {
        #[arg(short = 'm', long)]
        message: String,
        #[command(flatten)]
        selection: SelectionArgs,
        #[command(flatten)]
        author: AuthorArgs,
    },
}

#[derive(Debug, Subcommand)]
pub enum EnvironmentCommand {
    /// Print the actual captured source identity for an explicitly selected private change.
    SourceIdentity,
    /// Persist a verified binding as a local native Blob; a candidate roots it for bundles.
    Bind {
        recipe: PathBuf,
        #[arg(
            long,
            required = true,
            help = "Explicitly trust the supplied recipe and prepared code"
        )]
        trust_recipe: bool,
    },
    Status {
        #[command(flatten)]
        recipe: RecipeArgs,
    },
    /// Import caller-prepared dependency/output paths after explicitly confirming no writers.
    Import {
        prepared: PathBuf,
        #[command(flatten)]
        recipe: RecipeArgs,
        #[arg(long, required = true)]
        quiescent: bool,
        #[arg(long)]
        copy: bool,
    },
    Materialize {
        #[command(flatten)]
        recipe: RecipeArgs,
        #[arg(long)]
        copy: bool,
    },
}

#[derive(Debug, Subcommand)]
pub enum JobCommand {
    List {
        #[arg(long, default_value_t = 30)]
        limit: usize,
    },
    Status {
        id: String,
    },
    Cancel {
        id: String,
    },
    AcknowledgeStopped {
        id: String,
        #[arg(
            long,
            help = "Explicit assertion after verifying all writers stopped; never inferred from a saved PID"
        )]
        note: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum WriterCommand {
    Status,
    AcknowledgeStopped {
        token: String,
        #[arg(
            long,
            help = "Explicit assertion after verifying the process group and all external writers stopped"
        )]
        note: String,
    },
}

#[derive(Debug, Args)]
pub struct RecipeArgs {
    #[arg(long)]
    recipe: PathBuf,
    #[arg(
        long,
        required = true,
        help = "Explicitly trust the supplied recipe and prepared code"
    )]
    trust_recipe: bool,
    #[arg(
        long,
        help = "Optional cache path; default is native repository metadata environments/v1"
    )]
    cache: Option<PathBuf>,
}

impl RecipeArgs {
    fn read(self) -> Result<(String, bool, Option<PathBuf>), ApiError> {
        Ok((
            read_recipe_json(&self.recipe)?,
            self.trust_recipe,
            self.cache,
        ))
    }
}

fn read_recipe_json(path: &std::path::Path) -> Result<String, ApiError> {
    let bytes = izu_api::read_input_file(path, MAX_REQUEST_BYTES)?;
    String::from_utf8(bytes).map_err(|error| ApiError::invalid(error.to_string()))
}

#[derive(Clone, Debug, clap::ValueEnum)]
pub enum ResolutionSide {
    Base,
    Ours,
    Theirs,
    Delete,
}

#[derive(Debug, Subcommand)]
pub enum WorkspaceCommand {
    List,
    /// Show an exact workspace's path and expected revision token.
    Open {
        id: String,
    },
    /// Deregister one workspace after preserving its source; directory is retained.
    Close {
        id: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum RefCommand {
    List,
    Set {
        name: String,
        revision: String,
        #[arg(long, group = "ref_expectation", required = true)]
        expect: Option<String>,
        #[arg(long, group = "ref_expectation", required = true)]
        expect_absent: bool,
    },
    Delete {
        name: String,
        #[arg(long)]
        expect: String,
    },
}

#[derive(Debug, Subcommand)]
pub enum GitCommand {
    Import {
        source: PathBuf,
        #[command(flatten)]
        reference: GitImportArgs,
        #[command(flatten)]
        transport: GitTransportArgs,
    },
    Export {
        target: PathBuf,
        #[command(flatten)]
        committer: GitCommitterArgs,
    },
    Fetch {
        source: PathBuf,
        #[command(flatten)]
        reference: GitImportArgs,
        #[command(flatten)]
        transport: GitTransportArgs,
    },
    Push {
        target: PathBuf,
        #[arg(long)]
        native_ref: String,
        #[arg(long)]
        git_ref: String,
        #[arg(long, group = "git_expectation", required = true)]
        expect: Option<String>,
        #[arg(long, group = "git_expectation", required = true)]
        expect_absent: bool,
        #[arg(long)]
        allow_non_fast_forward: bool,
        #[command(flatten)]
        committer: GitCommitterArgs,
        #[command(flatten)]
        transport: GitTransportArgs,
    },
}

#[derive(Debug, Args)]
pub struct GitTransportArgs {
    #[arg(
        long,
        requires = "https_token_env",
        help = "Explicit HTTPS basic-auth username, such as x-access-token"
    )]
    https_user: Option<String>,
    #[arg(
        long,
        requires = "https_user",
        help = "Read this explicitly chosen secret environment variable for this repository URL only"
    )]
    https_token_env: Option<String>,
    #[arg(
        long,
        help = "Additional explicit DER TLS trust anchor; never disables certificate verification"
    )]
    tls_root_cert: Vec<PathBuf>,
}

impl GitTransportArgs {
    fn read(self) -> Result<GitTransport, ApiError> {
        let authentication = match (self.https_user, self.https_token_env) {
            (Some(username), Some(variable)) => {
                if variable.is_empty()
                    || variable.len() > 256
                    || !variable
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                {
                    return Err(ApiError::invalid(
                        "Secret environment variable must be a nonempty identifier of at most 256 bytes",
                    ));
                }
                let password = std::env::var(&variable).map_err(|_| {
                    ApiError::invalid(format!(
                        "Explicit credential variable {variable} is absent or is not Unicode"
                    ))
                })?;
                if password.is_empty() || password.len() > 8192 {
                    return Err(ApiError::invalid(
                        "Explicit HTTPS credential must contain 1..=8192 bytes",
                    ));
                }
                Some(GitAuthentication::Basic { username, password })
            }
            (None, None) => None,
            _ => {
                return Err(ApiError::invalid(
                    "Provide --https-user and --https-token-env together",
                ));
            }
        };
        if self.tls_root_cert.len() > 8 {
            return Err(ApiError::invalid(
                "At most eight explicit TLS trust anchors are supported",
            ));
        }
        let mut root_certificates = Vec::new();
        for path in self.tls_root_cert {
            let bytes = izu_api::read_input_file(&path, 64 * 1024)?;
            root_certificates.push(bytes);
        }
        Ok(GitTransport {
            authentication,
            root_certificates,
        })
    }
}

fn git_location(path: PathBuf) -> Result<GitLocation, ApiError> {
    match path.to_str() {
        Some(url) if url.starts_with("https://") => Ok(GitLocation::Https(HttpsLocation {
            url: url.to_owned(),
        })),
        Some(url) if url.contains("://") => Err(ApiError::unsupported(
            "Git transports other than explicit local paths and HTTPS",
        )),
        _ => Ok(GitLocation::Local(path)),
    }
}

#[derive(Debug, Subcommand)]
pub enum BundleCommand {
    /// Archive already captured native history. Checkpoint first to include newest edits.
    Create {
        path: PathBuf,
    },
    Inspect {
        path: PathBuf,
    },
    Verify {
        path: PathBuf,
    },
    /// Restore into a new nonexistent directory, remapping historical workspace paths.
    Restore {
        path: PathBuf,
        destination: PathBuf,
        #[arg(long)]
        selected_workspace: Option<String>,
    },
}

#[derive(Debug, Args)]
pub struct GitImportArgs {
    #[arg(long, requires = "git_ref")]
    native_ref: Option<String>,
    #[arg(long, requires = "native_ref")]
    git_ref: Option<String>,
    #[arg(long, requires = "native_ref", conflicts_with = "expect_absent")]
    expect: Option<String>,
    #[arg(long, requires = "native_ref", conflicts_with = "expect")]
    expect_absent: bool,
}

impl GitImportArgs {
    fn into_refs(self) -> Result<Vec<GitImportRef>, ApiError> {
        match (self.native_ref, self.git_ref) {
            (Some(native_ref), Some(git_ref)) if self.expect.is_some() || self.expect_absent => {
                Ok(vec![GitImportRef {
                    native_ref,
                    git_ref,
                    expected: self.expect,
                }])
            }
            (None, None) => Ok(Vec::new()),
            _ => Err(ApiError::invalid(
                "Explicit Git import mapping requires --native-ref, --git-ref and --expect or --expect-absent",
            )),
        }
    }
}

#[derive(Debug, Args)]
pub struct GitCommitterArgs {
    #[arg(long, env = "IZU_AUTHOR_NAME", requires = "committer_email")]
    committer_name: Option<String>,
    #[arg(long, env = "IZU_AUTHOR_EMAIL", requires = "committer_name")]
    committer_email: Option<String>,
    #[arg(
        long,
        requires = "committer_name",
        help = "Committer timestamp; default is the current observed UTC time"
    )]
    timestamp: Option<i64>,
    #[arg(
        long,
        default_value_t = 0,
        allow_hyphen_values = true,
        help = "Committer offset in minutes; default is explicitly UTC"
    )]
    timezone_minutes: i16,
}

impl GitCommitterArgs {
    fn into_committer(self) -> Result<Option<GitCommitter>, ApiError> {
        match (self.committer_name, self.committer_email) {
            (Some(name), Some(email)) => {
                let timestamp_unix_seconds = match self.timestamp {
                    Some(timestamp) => timestamp,
                    None => i64::try_from(
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map_err(|error| ApiError::invalid(error.to_string()))?
                            .as_secs(),
                    )
                    .map_err(|error| ApiError::invalid(error.to_string()))?,
                };
                Ok(Some(GitCommitter {
                    name,
                    email,
                    timestamp_unix_seconds,
                    timezone_minutes: self.timezone_minutes,
                }))
            }
            (None, None) => Ok(None),
            _ => Err(ApiError::invalid(
                "Git export requires both committer name and email",
            )),
        }
    }
}

#[derive(Debug, Subcommand)]
pub enum AgentCommand {
    /// Serve bounded newline-delimited MCP JSON-RPC on stdin/stdout.
    Serve,
    /// Run a selected program in a workspace, guarded by its exact revision token.
    Run {
        #[arg(long)]
        cwd: PathBuf,
        #[arg(long)]
        expected_head: String,
        #[arg(long)]
        expected_tree: String,
        #[arg(long, default_value_t = 300)]
        timeout_seconds: u64,
        #[arg(long="env",value_parser=parse_env)]
        env: Vec<(String, String)>,
        #[arg(last=true,required=true,num_args=1..)]
        argv: Vec<String>,
    },
}

fn parse_env(value: &str) -> Result<(String, String), String> {
    let (name, value) = value
        .split_once('=')
        .ok_or_else(|| "Use NAME=VALUE".to_string())?;
    if name.is_empty() || name.contains('\0') || value.contains('\0') {
        return Err("Environment names must be nonempty and values must contain no NUL".into());
    }
    Ok((name.into(), value.into()))
}

impl Cli {
    fn read_context(&self) -> ReadContext {
        ReadContext {
            repository: self.repo.clone(),
            workspace: self.workspace.clone(),
        }
    }

    pub fn into_operation(
        self,
        api: &Api,
        cancel: &CancellationToken,
    ) -> Result<Operation, ApiError> {
        let read = api.named_context(self.read_context(), self.change.as_deref(), cancel)?;
        let mutation = |context: &ReadContext| api.mutation_context(context, cancel);
        match self.command {
            Command::Init { path } => Ok(Operation::Init { path }),
            Command::Capabilities => Ok(Operation::Capabilities {}),
            Command::Schema => Ok(Operation::Schema {}),
            Command::Status => Ok(Operation::Status { context: read }),
            Command::Inspect => Ok(Operation::Inspect { context: read }),
            Command::Diff { before, after } => Ok(Operation::Diff {
                before: before
                    .map(|selector| api.resolve_selector(&read, &selector, cancel))
                    .transpose()?,
                after: after
                    .map(|selector| api.resolve_selector(&read, &selector, cancel))
                    .transpose()?,
                context: read,
            }),
            Command::Log {
                revision,
                from,
                limit,
            } => Ok(Operation::Log {
                from: from
                    .or(revision)
                    .map(|selector| api.resolve_selector(&read, &selector, cancel))
                    .transpose()?,
                context: read,
                limit,
            }),
            Command::Show { revision } => Ok(Operation::Show {
                revision: api.resolve_selector(&read, &revision, cancel)?,
                context: read,
            }),
            Command::Checkpoint { selection } => Ok(Operation::Checkpoint {
                context: mutation(&read)?,
                selection: selection.into_selection(),
            }),
            Command::Commit {
                message,
                selection,
                author,
            } => api.commit_operation(
                &read,
                selection.into_selection(),
                message,
                author.into(),
                cancel,
            ),
            Command::Start { name, from, launch } => {
                api.start_operation(&read, name, from.as_deref(), launch.into(), cancel)
            }
            Command::Run { launch } => api.run_operation(&read, launch.into(), cancel),
            Command::Change {
                command: ChangeCommand::List,
            } => Ok(Operation::WorkspaceList { context: read }),
            Command::Change {
                command: ChangeCommand::Open { name },
            } => Ok(Operation::Inspect {
                context: api.named_context(read, Some(&name), cancel)?,
            }),
            Command::Change {
                command: ChangeCommand::Close { name },
            } => Ok(Operation::WorkspaceClose {
                context: mutation(&api.named_context(read, Some(&name), cancel)?)?,
            }),
            Command::Change {
                command: ChangeCommand::Start { name, path, from },
            } => {
                let from = match from {
                    Some(id) => api.resolve_selector(&read, &id, cancel)?,
                    None => mutation(&read)?.expected.head,
                };
                Ok(Operation::WorkspaceStart {
                    context: read,
                    name,
                    path,
                    from,
                })
            }
            Command::Change {
                command:
                    ChangeCommand::Revise {
                        message,
                        selection,
                        author,
                    },
            } => Ok(Operation::Revise {
                context: mutation(&read)?,
                selection: selection.into_selection(),
                message,
                author: author.into(),
            }),
            Command::Workspace {
                command: WorkspaceCommand::List,
            } => Ok(Operation::WorkspaceList { context: read }),
            Command::Workspace {
                command: WorkspaceCommand::Open { id },
            } => Ok(Operation::Inspect {
                context: ReadContext {
                    workspace: Some(id),
                    ..read
                },
            }),
            Command::Workspace {
                command: WorkspaceCommand::Close { id },
            } => Ok(Operation::WorkspaceClose {
                context: mutation(&ReadContext {
                    workspace: Some(id),
                    ..read
                })?,
            }),
            Command::Refs {
                command: RefCommand::List,
            } => Ok(Operation::RefList { context: read }),
            Command::Refs {
                command:
                    RefCommand::Set {
                        name,
                        revision,
                        expect,
                        ..
                    },
            } => Ok(Operation::RefSet {
                context: read,
                name,
                expected: expect,
                revision: Some(revision),
            }),
            Command::Refs {
                command: RefCommand::Delete { name, expect },
            } => Ok(Operation::RefSet {
                context: read,
                name,
                expected: Some(expect),
                revision: None,
            }),
            Command::Restore {
                revision,
                selection,
            } => Ok(Operation::Restore {
                revision: api.resolve_selector(&read, &revision, cancel)?,
                context: mutation(&read)?,
                selection: selection.into_selection(),
            }),
            Command::Update { revision } => Ok(Operation::Update {
                revision: api.resolve_selector(&read, &revision, cancel)?,
                context: mutation(&read)?,
            }),
            Command::Undo {
                operation,
                selection,
            } => Ok(Operation::Undo {
                context: mutation(&read)?,
                operation,
                selection: selection.into_selection(),
            }),
            Command::Revert {
                revision,
                message,
                selection,
                author,
            } => api.revert_operation(
                &read,
                revision,
                selection.into_selection(),
                message,
                author.into(),
                cancel,
            ),
            Command::Operations { limit } => Ok(Operation::Operations {
                context: read,
                limit,
            }),
            Command::Candidate {
                source,
                target,
                expect,
                check,
                environment,
                author,
                argv,
                ..
            } => Ok(Operation::Candidate {
                source: api.resolve_selector(&read, &source, cancel)?,
                context: read,
                target,
                expected_target: expect,
                checks: vec![CheckDefinition {
                    name: check,
                    argv,
                    environment,
                }],
                author: author.into(),
            }),
            Command::CandidateShow { candidate } => Ok(Operation::CandidateShow {
                context: read,
                candidate,
            }),
            Command::ResolveRevision {
                revision,
                path,
                take,
                content_file,
                executable,
                author,
            } => Ok(Operation::ResolveRevision {
                revision: api.resolve_selector(&read, &revision, cancel)?,
                context: read,
                path,
                resolution: resolution_input(take, content_file, executable)?,
                author: author.into(),
            }),
            Command::Resolve {
                candidate,
                path,
                take,
                content_file,
                executable,
                author,
            } => {
                let resolution = resolution_input(take, content_file, executable)?;
                Ok(Operation::Resolve {
                    context: read,
                    candidate,
                    path,
                    resolution,
                    author: author.into(),
                })
            }
            Command::Check {
                candidate,
                check,
                timeout_seconds,
                start_only,
            } => {
                if start_only {
                    Ok(Operation::CheckStart {
                        context: read,
                        candidate,
                        check,
                    })
                } else {
                    Ok(Operation::Check {
                        context: read,
                        candidate,
                        check,
                        timeout_seconds,
                    })
                }
            }
            Command::Land {
                candidate,
                current,
                source,
                target,
                check,
                environment,
                timeout_seconds,
                author,
                argv,
            } => match candidate {
                Some(candidate)
                    if !current
                        && source.is_none()
                        && check.is_none()
                        && environment.is_none()
                        && argv.is_empty() =>
                {
                    Ok(Operation::Land {
                        context: read,
                        candidate,
                    })
                }
                None if current || source.is_some() => {
                    let check = check.ok_or_else(|| {
                        ApiError::invalid(
                            "Checked landing requires --check NAME and -- PROGRAM ARGS",
                        )
                    })?;
                    if argv.is_empty() {
                        return Err(ApiError::invalid(
                            "Checked landing requires explicit -- PROGRAM ARGS",
                        ));
                    }
                    api.land_operation(
                        &read,
                        source.as_deref(),
                        target,
                        izu_api::LandingPlan {
                            checks: vec![CheckDefinition {
                                name: check,
                                argv,
                                environment,
                            }],
                            author: author.into_author()?,
                            timeout_seconds,
                        },
                        cancel,
                    )
                }
                _ => Err(ApiError::invalid(
                    "Choose a candidate, or --current/--source with an explicit check program",
                )),
            },
            Command::Verify => Ok(Operation::Verify { context: read }),
            Command::Recover => Ok(Operation::Recover { context: read }),
            Command::Jobs { command } => match command {
                JobCommand::List { limit } => Ok(Operation::JobsList {
                    context: read,
                    limit,
                }),
                JobCommand::Status { id } => Ok(Operation::JobStatus {
                    context: read,
                    job: id,
                }),
                JobCommand::Cancel { id } => Ok(Operation::JobCancel {
                    context: read,
                    job: id,
                }),
                JobCommand::AcknowledgeStopped { id, note } => {
                    Ok(Operation::JobAcknowledgeStopped {
                        context: read,
                        job: id,
                        operator_note: note,
                    })
                }
            },
            Command::Writer { command } => match command {
                WriterCommand::Status => Ok(Operation::WriterStatus { context: read }),
                WriterCommand::AcknowledgeStopped { token, note } => {
                    Ok(Operation::WriterAcknowledgeStopped {
                        context: read,
                        token,
                        operator_note: note,
                    })
                }
            },
            Command::Environment { command } => match command {
                EnvironmentCommand::Bind {
                    recipe,
                    trust_recipe,
                } => Ok(Operation::EnvironmentBind {
                    context: read,
                    recipe_json: read_recipe_json(&recipe)?,
                    trusted: trust_recipe,
                }),
                EnvironmentCommand::SourceIdentity => Ok(Operation::EnvironmentSourceIdentity {
                    context: mutation(&read)?,
                }),
                EnvironmentCommand::Status { recipe } => {
                    let (recipe_json, trusted, cache) = recipe.read()?;
                    Ok(Operation::EnvironmentStatus {
                        context: read,
                        cache,
                        recipe_json,
                        trusted,
                    })
                }
                EnvironmentCommand::Import {
                    prepared,
                    recipe,
                    quiescent,
                    copy,
                } => {
                    let (recipe_json, trusted, cache) = recipe.read()?;
                    Ok(Operation::EnvironmentImport {
                        context: read,
                        cache,
                        recipe_json,
                        prepared,
                        trusted,
                        quiescent,
                        sharing: if copy {
                            EnvironmentSharing::Copy
                        } else {
                            EnvironmentSharing::PreferClone
                        },
                    })
                }
                EnvironmentCommand::Materialize { recipe, copy } => {
                    let (recipe_json, trusted, cache) = recipe.read()?;
                    Ok(Operation::EnvironmentMaterialize {
                        context: mutation(&read)?,
                        cache,
                        recipe_json,
                        trusted,
                        sharing: if copy {
                            EnvironmentSharing::Copy
                        } else {
                            EnvironmentSharing::PreferClone
                        },
                    })
                }
            },
            Command::Bundle {
                command: BundleCommand::Create { path },
            } => Ok(Operation::BundleCreate {
                context: read,
                path,
            }),
            Command::Bundle {
                command: BundleCommand::Inspect { path },
            } => Ok(Operation::BundleInspect { path }),
            Command::Bundle {
                command: BundleCommand::Verify { path },
            } => Ok(Operation::BundleVerify { path }),
            Command::Bundle {
                command:
                    BundleCommand::Restore {
                        path,
                        destination,
                        selected_workspace,
                    },
            } => Ok(Operation::BundleRestore {
                path,
                destination,
                workspace: selected_workspace,
            }),
            Command::Git {
                command:
                    GitCommand::Import {
                        source,
                        reference,
                        transport,
                    },
            } => Ok(Operation::GitImport {
                context: read,
                source: git_location(source)?,
                refs: reference.into_refs()?,
                transport: transport.read()?,
            }),
            Command::Git {
                command: GitCommand::Export { target, committer },
            } => Ok(Operation::GitExport {
                context: read,
                target,
                committer: committer.into_committer()?,
            }),
            Command::Git {
                command:
                    GitCommand::Fetch {
                        source,
                        reference,
                        transport,
                    },
            } => Ok(Operation::GitFetch {
                context: read,
                source: git_location(source)?,
                refs: reference.into_refs()?,
                transport: transport.read()?,
            }),
            Command::Git {
                command:
                    GitCommand::Push {
                        target,
                        native_ref,
                        git_ref,
                        expect,
                        allow_non_fast_forward,
                        committer,
                        transport,
                        ..
                    },
            } => Ok(Operation::GitPush {
                context: read,
                target: git_location(target)?,
                native_ref,
                git_ref,
                expected_old: expect,
                allow_non_fast_forward,
                committer: committer.into_committer()?,
                transport: transport.read()?,
            }),
            Command::Agent {
                command:
                    AgentCommand::Run {
                        cwd,
                        expected_head,
                        expected_tree,
                        timeout_seconds,
                        env,
                        argv,
                    },
            } => {
                let workspace = read.workspace.ok_or_else(|| {
                    ApiError::invalid(
                        "`agent run` requires --workspace with the exact workspace ID",
                    )
                })?;
                Ok(Operation::AgentRun {
                    context: MutationContext {
                        repository: read.repository,
                        workspace,
                        expected: izu_api::Expectation {
                            head: expected_head,
                            working_tree: expected_tree,
                        },
                    },
                    cwd,
                    argv,
                    env: env.into_iter().collect::<BTreeMap<_, _>>(),
                    timeout_seconds,
                })
            }
            Command::Json
            | Command::Agent {
                command: AgentCommand::Serve,
            }
            | Command::RuntimeWorker { .. }
            | Command::ProcessWorker { .. } => Err(ApiError::invalid(
                "Transport commands must be dispatched at the process boundary",
            )),
        }
    }
}

pub fn bounded_json_input(reader: &mut impl Read) -> Result<Request, ApiError> {
    let mut bytes = Vec::new();
    reader
        .take((MAX_REQUEST_BYTES + 1) as u64)
        .read_to_end(&mut bytes)
        .map_err(|e| ApiError::invalid(format!("Cannot read stdin: {e}")))?;
    parse_request(&bytes)
}

pub fn write_json(writer: &mut impl Write, response: &Response) -> Result<(), ApiError> {
    let bytes = izu_api::bounded_json(response, 16 * 1024 * 1024)?;
    writer
        .write_all(&bytes)
        .map_err(|e| ApiError::invalid(format!("Cannot write response: {e}")))?;
    writer
        .write_all(b"\n")
        .map_err(|e| ApiError::invalid(format!("Cannot write response: {e}")))
}

pub fn write_human(writer: &mut impl Write, response: &Response) -> Result<(), ApiError> {
    match &response.outcome {
        Outcome::Error { error, result } => {
            writeln!(writer, "error: {error}")
                .map_err(|error| ApiError::invalid(error.to_string()))?;
            if let Some(result) = result {
                if matches!(result.kind.as_str(), "managed_start" | "managed_run") {
                    write_managed(writer, &result.data)
                } else { writeln!(
                    writer,
                    "{}",
                    serde_json::to_string_pretty(&result.data)
                        .map_err(|e| ApiError::invalid(e.to_string()))?
                ) }
            } else {
                Ok(())
            }
        }
        Outcome::Uncertain { error, result } => writeln!(
            writer,
            "error: {error}\n{}",
            serde_json::to_string_pretty(&result.data)
                .map_err(|e| ApiError::invalid(e.to_string()))?
        ),
        Outcome::Ok { result } if result.kind == "status" => {
            writeln!(
                writer,
                "Change {}\nCheckout {}",
                terminal_text(result.data["workspace_name"].as_str().unwrap_or("unknown")),
                result.data["head"].as_str().unwrap_or("unknown")
            )
            .map_err(|error| ApiError::invalid(error.to_string()))?;
            if let Some(main) = result.data["main"].as_str() {
                writeln!(writer, "Main {main}{}", if result.data["at_main"] == true { "" } else { " (different from checkout)" }).map_err(|error| ApiError::invalid(error.to_string()))?;
            }
            if let Some(entries) = result.data["entries"].as_array() {
                if entries.is_empty() {
                    writeln!(writer, "Clean")
                } else {
                    for entry in entries {
                        writeln!(
                            writer,
                            "{} {}",
                            entry["kind"].as_str().unwrap_or("Changed"),
                            entry["path"]
                        )
                        .map_err(|error| ApiError::invalid(error.to_string()))?;
                    }
                    Ok(())
                }
            } else {
                Err(std::io::Error::other("Engine status has no entries"))
            }
        }
        Outcome::Ok { result } if result.kind == "diff" => {
            if let Some(files) = result.data["files"].as_array() {
                for file in files {
                    if let Some(patch) = file
                        .pointer("/content/patch")
                        .and_then(serde_json::Value::as_str)
                    {
                        write!(writer, "{}", terminal_text(patch))
                            .map_err(|error| ApiError::invalid(error.to_string()))?;
                    } else {
                        writeln!(writer, "{}: {}", file["path"], file["content"])
                            .map_err(|error| ApiError::invalid(error.to_string()))?;
                    }
                }
                if files.is_empty() {
                    writeln!(writer, "No changes")
                } else {
                    Ok(())
                }
            } else {
                Err(std::io::Error::other("Engine diff has no files"))
            }
        }
        Outcome::Ok { result } if result.kind == "log" => {
            if let Some(entries) = result.data.as_array() {
                for entry in entries {
                    writeln!(
                        writer,
                        "{} {}",
                        entry["id"].as_str().unwrap_or("unknown"),
                        terminal_text(
                            entry
                                .pointer("/revision/description")
                                .and_then(serde_json::Value::as_str)
                                .unwrap_or("")
                        )
                    )
                    .map_err(|error| ApiError::invalid(error.to_string()))?;
                }
                Ok(())
            } else {
                Err(std::io::Error::other("Engine log is not an array"))
            }
        }
        Outcome::Ok { result } if matches!(result.kind.as_str(), "managed_start" | "managed_run") => write_managed(writer, &result.data),
        Outcome::Ok { result } if result.kind == "land_current" => {
            writeln!(writer, "Landed {} at {}.\nPrimary files are unchanged; use `izu update` when its checkout is clean.", result.data.pointer("/land/target").and_then(serde_json::Value::as_str).unwrap_or("target"), result.data.pointer("/land/revision").and_then(serde_json::Value::as_str).unwrap_or("unknown"))
        }
        Outcome::Ok { result } if result.kind == "commit" => writeln!(writer, "Committed {}{}", result.data["revision"].as_str().unwrap_or("unknown"), result.data["reference"].as_str().map(|name| format!(" to {name}")).unwrap_or_default()),
        Outcome::Ok { result } if result.kind == "checkpoint" => writeln!(writer, "Saved durable local checkpoint {}. This is not a remote backup.", result.data["operation"].as_str().unwrap_or("unknown")),
        Outcome::Ok { result } if result.kind == "workspace_list" => {
            if let Some(workspaces) = result.data.as_object() {
                for workspace in workspaces.values() {
                    writeln!(writer, "{}\t{}", terminal_text(workspace["name"].as_str().unwrap_or("unknown")), terminal_text(workspace["root"].as_str().unwrap_or("unknown"))).map_err(|error| ApiError::invalid(error.to_string()))?;
                }
                Ok(())
            } else { Err(std::io::Error::other("Engine workspace list is not a map")) }
        }
        Outcome::Ok { result } => {
            let data = serde_json::to_string_pretty(&result.data)
                .map_err(|e| ApiError::invalid(e.to_string()))?;
            writeln!(writer, "{}\n{}", result.kind.replace('_', " "), data)
        }
    }
    .map_err(|e| ApiError::invalid(format!("Cannot write output: {e}")))
}

fn write_managed(writer: &mut impl Write, data: &serde_json::Value) -> std::io::Result<()> {
    let name = data
        .pointer("/workspace/record/name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    let root = data
        .pointer("/workspace/record/root")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown");
    writeln!(
        writer,
        "Change {}\nDirectory {}",
        terminal_text(name),
        terminal_text(root)
    )?;
    if let Some(job) = data.get("job").filter(|job| !job.is_null()) {
        for stream in ["stdout", "stderr"] {
            if let Some(bytes) = job
                .pointer(&format!("/output/{stream}/bytes"))
                .and_then(serde_json::Value::as_array)
            {
                let bytes: Vec<u8> = bytes
                    .iter()
                    .filter_map(|byte| byte.as_u64().and_then(|byte| u8::try_from(byte).ok()))
                    .collect();
                if !bytes.is_empty() {
                    writeln!(
                        writer,
                        "{}",
                        terminal_text(&String::from_utf8_lossy(&bytes))
                    )?;
                }
            }
        }
        writeln!(writer, "Job {}", job["state"])?;
    }
    if data["after"].is_object() {
        writeln!(
            writer,
            "Stopped source saved as a durable local checkpoint."
        )?;
    }
    writeln!(
        writer,
        "Select this change with `izu --change {} status`.",
        terminal_text(name)
    )
}

fn terminal_text(text: &str) -> String {
    text.chars()
        .flat_map(|character| {
            if character.is_control() && !matches!(character, '\n' | '\t') {
                character.escape_default().collect::<Vec<_>>()
            } else {
                vec![character]
            }
        })
        .collect()
}

pub fn parse_args(args: impl IntoIterator<Item = OsString>) -> Result<Cli, clap::Error> {
    Cli::try_parse_from(args)
}

fn resolution_input(
    take: Option<ResolutionSide>,
    content_file: Option<PathBuf>,
    executable: bool,
) -> Result<ConflictResolution, ApiError> {
    Ok(match (take, content_file) {
        (Some(ResolutionSide::Base), None) => ConflictResolution::Base {},
        (Some(ResolutionSide::Ours), None) => ConflictResolution::Ours {},
        (Some(ResolutionSide::Theirs), None) => ConflictResolution::Theirs {},
        (Some(ResolutionSide::Delete), None) => ConflictResolution::Delete {},
        (None, Some(file)) => {
            let bytes = izu_api::read_input_file(&file, MAX_REQUEST_BYTES)?;
            ConflictResolution::File {
                content: String::from_utf8(bytes).map_err(|error| {
                    ApiError::invalid(format!("Supplied resolution must be UTF-8 text: {error}"))
                })?,
                executable,
            }
        }
        _ => {
            return Err(ApiError::invalid(
                "Choose exactly one --take or --content-file resolution",
            ));
        }
    })
}
