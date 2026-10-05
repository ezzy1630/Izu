use crate::error::io;
use crate::source;
use crate::*;
use izu_model::{
    ChangeState, Limits, ObjectKind, Operation, Validate, decode_metadata, encode_metadata,
};
use izu_platform::Directory;
use izu_store::{HeadExpectation, PublishOutcome, Store};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const WORKING: &str = "working";
const RECOVERY: &str = "recovery";
const MARKER_VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceMarker {
    version: u32,
    metadata: String,
    workspace: WorkspaceId,
}

struct SourcePublication {
    change: ChangeId,
    revision: RevisionId,
    reference: Option<RefExpectation>,
}

/// Keeps observed source attached to its registered locator until the receipt.
/// This is an identity guard, not an exclusion lease or a filesystem snapshot.
struct SourceGuard<'a> {
    repository: &'a Repository,
    workspace: WorkspaceId,
    root: PathBuf,
    directory: Directory,
}

impl<'a> SourceGuard<'a> {
    fn open(repository: &'a Repository, state: &WorkspaceState) -> Result<Self> {
        let root = Path::new(&state.record.root);
        let directory = Directory::open(root).map_err(|error| io(root, error))?;
        Self::new(repository, state, directory)
    }

    fn pinned(
        repository: &'a Repository,
        state: &WorkspaceState,
        directory: &Directory,
    ) -> Result<Self> {
        Self::new(
            repository,
            state,
            directory
                .try_clone()
                .map_err(|error| io(&state.record.root, error))?,
        )
    }

    fn new(
        repository: &'a Repository,
        state: &WorkspaceState,
        directory: Directory,
    ) -> Result<Self> {
        let guard = Self {
            repository,
            workspace: state.id,
            root: PathBuf::from(&state.record.root),
            directory,
        };
        guard.verify()?;
        Ok(guard)
    }

    fn verify(&self) -> Result<()> {
        root_identity(&self.directory, &self.root)?;
        root_identity(
            self.repository.store.root_directory(),
            &self.repository.metadata,
        )?;
        if self.repository.metadata.parent() == Some(self.root.as_path()) {
            let metadata = self
                .directory
                .open_dir(OsStr::new(".izu"))
                .map_err(|error| io(self.root.join(".izu"), error))?;
            same_directory(
                &metadata,
                self.repository.store.root_directory(),
                &self.repository.metadata,
            )?;
        } else {
            let marker = read_workspace_marker(&self.directory, &self.root)?;
            if marker.workspace != self.workspace
                || Path::new(&marker.metadata) != self.repository.metadata
            {
                return Err(EngineError::SourceChanged(self.root.join(".izu")));
            }
        }
        Ok(())
    }

    fn uncertainty(&self, tree: TreeId, reason: impl std::fmt::Display) -> String {
        format!(
            "{reason}; retained source tree {tree} for workspace {} at original source locator {}; repository metadata locator {}; inspect displaced original source and history before recovery",
            self.workspace,
            self.root.display(),
            self.repository.metadata.display(),
        )
    }
}

/// A native repository and its explicitly selected source workspace.
/// Opening and metadata operations never change source files.
pub struct Repository {
    root: PathBuf,
    metadata: PathBuf,
    selected: WorkspaceId,
    store: Store,
    options: RepositoryOptions,
}

/// A cooperative native source-writer guard and its pinned no-follow root.
/// Drop releases the permanent workspace lock. External editors are outside
/// this protocol; callers must preserve originals and check source identity.
pub struct WorkspaceGuard<'a> {
    repository: &'a Repository,
    _lock: File,
    _live: File,
    directory: Directory,
    state: WorkspaceState,
}

/// A cooperative managed-process/source-materialization exclusion lease.
/// It deliberately does not hold the metadata/capture lock, so a managed child
/// may checkpoint or commit its own source. Restore/close wait or fail busy.
pub struct WorkspaceWriterLease {
    _lock: File,
    state: WorkspaceState,
    intent: WriterIntent,
    directory: Directory,
    repository_directory: Directory,
    source_directory: Directory,
    intent_name: std::ffi::OsString,
}

impl WorkspaceWriterLease {
    pub fn state(&self) -> &WorkspaceState {
        &self.state
    }
    pub fn root_path(&self) -> &Path {
        Path::new(&self.state.record.root)
    }
    pub fn intent(&self) -> &WriterIntent {
        &self.intent
    }
    /// The actual source root pinned while the lease's source state was checked.
    /// The separate private metadata directory holds only the writer intent.
    pub fn source_directory(&self) -> &Directory {
        &self.source_directory
    }

    /// The owner calls this only after the selected process group is stopped
    /// and reaped. Dropping the lease alone intentionally leaves Unknown intent.
    pub fn finish_stopped(self) -> Result<()> {
        let current = read_intent(&self.directory, &self.intent_name, self.state.id)?
            .ok_or_else(|| EngineError::InvalidInput("managed writer intent is missing".into()))?;
        if current.token != self.intent.token {
            return Err(EngineError::WorkspaceBusy {
                workspace: self.state.id,
                kind: "different managed writer intent",
            });
        }
        self.directory
            .remove_file(&self.intent_name)
            .map_err(|error| io(self.root_path(), error))?;
        self.directory
            .sync()
            .map_err(|error| io(self.root_path(), error))
    }
}

impl WorkspaceGuard<'_> {
    pub fn directory(&self) -> &Directory {
        &self.directory
    }
    pub fn state(&self) -> &WorkspaceState {
        &self.state
    }
    pub fn root_path(&self) -> &Path {
        Path::new(&self.state.record.root)
    }
    pub fn is_primary(&self) -> bool {
        self.repository
            .metadata
            .parent()
            .is_some_and(|path| path == self.root_path())
    }
    pub fn capture(
        &self,
        selection: Selection,
        cancel: &CancellationToken,
    ) -> Result<CapturedTree> {
        let source = SourceGuard::pinned(self.repository, &self.state, &self.directory)?;
        let (tree, stats) = self.repository.capture_state_at(
            &self.state,
            &self.directory,
            &selection,
            false,
            cancel,
        )?;
        let tree = self.repository.put_tree(&tree, cancel)?;
        source.verify()?;
        Ok(CapturedTree {
            workspace: self.state.id,
            expected: self.state.expected,
            tree,
            stats,
        })
    }
}

fn timestamp() -> Result<i64> {
    let elapsed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| EngineError::InvalidInput("system clock precedes Unix epoch".into()))?;
    i64::try_from(elapsed.as_millis())
        .map_err(|_| EngineError::InvalidInput("system clock exceeds supported range".into()))
}

fn new_change() -> Result<ChangeId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| EngineError::InvalidInput(format!("change identity entropy: {error}")))?;
    Ok(ChangeId::from_bytes(bytes))
}

fn new_workspace() -> Result<WorkspaceId> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|error| {
        EngineError::InvalidInput(format!("workspace identity entropy: {error}"))
    })?;
    Ok(WorkspaceId::from_bytes(bytes))
}

fn canonical_root(root: &Path) -> Result<PathBuf> {
    let canonical = std::fs::canonicalize(root).map_err(|error| io(root, error))?;
    let directory = Directory::open(&canonical).map_err(|error| io(&canonical, error))?;
    directory
        .metadata_self()
        .map_err(|error| io(&canonical, error))?;
    Ok(canonical)
}

fn initialization_root(root: &Path) -> Result<(PathBuf, Directory)> {
    match std::fs::symlink_metadata(root) {
        Ok(_) => {
            let root = canonical_root(root)?;
            path_text(&root)?;
            let directory = Directory::open(&root).map_err(|error| io(&root, error))?;
            return Ok((root, directory));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io(root, error)),
    }
    let name = root.file_name().ok_or_else(|| {
        EngineError::InvalidInput("new repository requires an ordinary directory name".into())
    })?;
    let parent = root
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let parent = canonical_root(parent)?;
    let root = parent.join(name);
    path_text(&root)?;
    let parent_directory = Directory::open(&parent).map_err(|error| io(&parent, error))?;
    // Only the requested final component is owned. A create race is never
    // converted into initialization of another process's new directory.
    let directory = parent_directory.create_dir(name).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            EngineError::AlreadyExists(root.clone())
        } else {
            io(&root, error)
        }
    })?;
    parent_directory
        .sync()
        .map_err(|error| io(&parent, error))?;
    root_identity(&directory, &root)?;
    Ok((root, directory))
}

#[cfg(unix)]
fn same_directory(first: &Directory, second: &Directory, locator: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let first = first.metadata_self().map_err(|error| io(locator, error))?;
    let second = second.metadata_self().map_err(|error| io(locator, error))?;
    if first.dev() != second.dev() || first.ino() != second.ino() {
        return Err(EngineError::SourceChanged(locator.into()));
    }
    Ok(())
}

#[cfg(not(unix))]
fn same_directory(_first: &Directory, _second: &Directory, _locator: &Path) -> Result<()> {
    Err(EngineError::UnsupportedPlatform("pinned source identity"))
}

fn root_identity(directory: &Directory, locator: &Path) -> Result<()> {
    let current = Directory::open(locator).map_err(|error| io(locator, error))?;
    same_directory(directory, &current, locator)
}

fn path_text(path: &Path) -> Result<String> {
    path.to_str()
        .map(str::to_owned)
        .ok_or_else(|| EngineError::IncompatiblePath(path.into()))
}

fn read_workspace_marker(directory: &Directory, root: &Path) -> Result<WorkspaceMarker> {
    let locator = root.join(".izu");
    let mut file = directory
        .open_read(OsStr::new(".izu"))
        .map_err(|error| io(&locator, error))?;
    let before = file.metadata().map_err(|error| io(&locator, error))?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(16 * 1024)
        .map_err(|_| EngineError::Allocation {
            resource: "workspace marker",
        })?;
    Read::by_ref(&mut file)
        .take(16 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| io(&locator, error))?;
    if bytes.len() > 16 * 1024 {
        return Err(EngineError::Limit {
            resource: "workspace marker bytes",
            limit: 16 * 1024,
        });
    }
    let after = file.metadata().map_err(|error| io(&locator, error))?;
    let current = directory
        .metadata(OsStr::new(".izu"))
        .map_err(|error| io(&locator, error))?;
    if !source::same_metadata(&before, &after) || !source::same_metadata(&after, &current) {
        return Err(EngineError::SourceChanged(locator));
    }
    let marker: WorkspaceMarker = serde_json::from_slice(&bytes)
        .map_err(|error| EngineError::InvalidMarker(error.to_string()))?;
    if marker.version != MARKER_VERSION {
        return Err(EngineError::InvalidMarker(format!(
            "unsupported marker version {}",
            marker.version
        )));
    }
    if !Path::new(&marker.metadata).is_absolute() {
        return Err(EngineError::InvalidMarker(
            "metadata path must be absolute".into(),
        ));
    }
    Ok(marker)
}

fn working_tree(record: &WorkspaceRecord) -> Result<TreeId> {
    let source = record.sources.get(WORKING).ok_or_else(|| {
        EngineError::InvalidInput("workspace has no working source record".into())
    })?;
    if source.path.is_some() {
        return Err(EngineError::InvalidInput(
            "working source must describe the workspace root".into(),
        ));
    }
    Ok(source.tree)
}

fn expectation(record: &WorkspaceRecord) -> Result<WorkspaceExpectation> {
    Ok(WorkspaceExpectation {
        head: record.head,
        working_tree: working_tree(record)?,
    })
}

fn check_workspace(
    view: &RepositoryView,
    id: WorkspaceId,
    expected: WorkspaceExpectation,
) -> Result<&WorkspaceRecord> {
    let record = view
        .workspaces
        .get(&id)
        .ok_or(EngineError::UnknownWorkspace(id))?;
    if expectation(record)? != expected {
        return Err(EngineError::StaleWorkspace { workspace: id });
    }
    Ok(record)
}

fn check_ref(view: &RepositoryView, name: &RefName, expected: Option<RevisionId>) -> Result<()> {
    let actual = view.refs.get(name).copied();
    if actual != expected {
        return Err(EngineError::StaleRef {
            name: name.clone(),
            expected,
            actual,
        });
    }
    Ok(())
}

fn set_working(record: &mut WorkspaceRecord, tree: TreeId, revision: Option<RevisionId>) {
    record.sources.insert(
        WORKING.into(),
        SourceRecord {
            path: None,
            tree,
            revision,
        },
    );
}

impl Repository {
    /// Cold archive staging has no synthetic revision or operation. Immutable
    /// archive objects may be imported here before the guarded adoption call.
    pub fn initialize_archive(root: impl AsRef<Path>, options: RepositoryOptions) -> Result<Self> {
        let root = canonical_root(root.as_ref())?;
        let directory = Directory::open(&root).map_err(|error| io(&root, error))?;
        Self::initialize_archive_at(&directory, &root, options)
    }

    /// Initializes only the supplied, pinned empty directory. The locator is
    /// recorded for diagnostics and later publication, never reopened to write.
    pub fn initialize_archive_at(
        directory: &Directory,
        locator: &Path,
        options: RepositoryOptions,
    ) -> Result<Self> {
        validate_source_limits(&options.source_limits)?;
        if !locator.is_absolute() {
            return Err(EngineError::InvalidInput(
                "archive staging locator must be absolute".into(),
            ));
        }
        path_text(locator)?;
        if directory
            .entries()
            .map_err(|error| io(locator, error))?
            .next()
            .transpose()
            .map_err(|error| io(locator, error))?
            .is_some()
        {
            return Err(EngineError::DirectoryNotEmpty(locator.into()));
        }
        let metadata = locator.join(".izu");
        let store = Store::init_at(directory, OsStr::new(".izu"), &options.store)?;
        let repository = Self {
            root: locator.into(),
            metadata,
            selected: new_workspace()?,
            store,
            options,
        };
        let root = repository.store.root_directory();
        root.create_dir(OsStr::new("workspace-locks"))
            .map_err(|error| io(&repository.metadata, error))?
            .sync()
            .map_err(|error| io(&repository.metadata, error))?;
        root.sync()
            .map_err(|error| io(&repository.metadata, error))?;
        Ok(repository)
    }

    /// Adopts a verified archive only into a cold, owned staging repository.
    /// Historical operations remain the parent chain; historical absolute source
    /// locations are never touched. The caller publishes the stage at final_root.
    pub fn adopt_archive(
        &mut self,
        archived_root: OperationId,
        selected: WorkspaceId,
        staging_root: &Path,
        final_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        let staging = canonical_root(staging_root)?;
        if staging != self.root {
            return Err(EngineError::InvalidInput(
                "archive stage differs from initialized repository".into(),
            ));
        }
        let directory = Directory::open(&staging).map_err(|error| io(&staging, error))?;
        self.adopt_archive_at(archived_root, selected, &directory, final_root, cancel)
    }

    /// Materializes through the original staging descriptor and preserves the
    /// archived operation chain. A substituted staging pathname is never used.
    pub fn adopt_archive_at(
        &mut self,
        archived_root: OperationId,
        selected: WorkspaceId,
        staging: &Directory,
        final_root: &Path,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        let metadata = staging
            .open_dir(OsStr::new(".izu"))
            .map_err(|error| io(&self.metadata, error))?;
        same_directory(&metadata, self.store.root_directory(), &self.metadata)?;
        if self.store.current_head(cancel)?.is_some() {
            return Err(EngineError::InvalidInput(
                "archive adoption requires absent native HEAD".into(),
            ));
        }
        if !final_root.is_absolute() {
            return Err(EngineError::InvalidInput(
                "archive final source path must be absolute".into(),
            ));
        }
        match std::fs::symlink_metadata(final_root) {
            Ok(_) => return Err(EngineError::AlreadyExists(final_root.into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(io(final_root, error)),
        }
        let parent = final_root
            .parent()
            .ok_or_else(|| EngineError::InvalidInput("archive final path has no parent".into()))?;
        let canonical_parent = canonical_root(parent)?;
        let final_name = final_root
            .file_name()
            .ok_or_else(|| EngineError::InvalidInput("archive final path has no name".into()))?;
        let final_path = canonical_parent.join(final_name);
        let archived = self.operation(archived_root, cancel)?;
        self.verify_graph(
            vec![izu_model::ObjectReference {
                id: archived_root.object_id(),
                kind: ObjectKind::Operation,
            }],
            cancel,
        )?;
        self.validate_view_links(&archived.view, cancel)?;
        let mut workspace = archived
            .view
            .workspaces
            .get(&selected)
            .cloned()
            .ok_or(EngineError::UnknownWorkspace(selected))?;
        let tree_id = working_tree(&workspace)?;
        let tree = self.tree(tree_id, cancel)?;
        reject_conflicts(&tree)?;
        publish(self.store.bootstrap_archive(archived_root, cancel)?)?;
        self.prepare_workspace_locks(selected)?;
        source::materialize_at(
            &self.store,
            staging,
            &self.root,
            &Tree::default(),
            &tree,
            &format!("archive-{archived_root}"),
            &self.options.source_limits,
            cancel,
        )
        .map_err(|error| EngineError::RestorationFailed {
            recovery: archived_root,
            reason: error.to_string(),
        })?;
        self.verify_source_at(staging, &self.root, &tree, cancel)
            .map_err(|error| EngineError::RestorationFailed {
                recovery: archived_root,
                reason: error.to_string(),
            })?;
        workspace.root = path_text(&final_path)?;
        let head = workspace.head;
        let operation = self
            .mutate(
                "Restore archive into explicit new source workspace".into(),
                cancel,
                |view| {
                    view.workspaces.clear();
                    view.workspaces.insert(selected, workspace);
                    Ok(())
                },
            )
            .map_err(|error| EngineError::RestorationUncertain {
                recovery: archived_root,
                operation: error.uncertain_operation(),
                reason: error.to_string(),
            })?;
        staging
            .sync()
            .map_err(|error| EngineError::RestorationUncertain {
                recovery: archived_root,
                operation: Some(operation),
                reason: error.to_string(),
            })?;
        self.selected = selected;
        Ok(RestoreReceipt {
            operation,
            recovery_operation: archived_root,
            workspace: selected,
            head,
            tree: tree_id,
        })
    }

    pub fn init(root: impl AsRef<Path>, options: RepositoryOptions) -> Result<Self> {
        validate_source_limits(&options.source_limits)?;
        let (root, directory) = initialization_root(root.as_ref())?;
        let metadata = root.join(".izu");
        for name in [".ezy", ".ezy-recovery", ".izu"] {
            match directory.metadata(OsStr::new(name)) {
                Ok(_) => return Err(EngineError::AlreadyExists(root.join(name))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(root.join(name), error)),
            }
        }
        let store = Store::init_at(&directory, OsStr::new(".izu"), &options.store)?;
        let selected = new_workspace()?;
        let repository = Self {
            root,
            metadata,
            selected,
            store,
            options,
        };
        repository.prepare_workspace_locks(selected)?;
        let cancel = CancellationToken::new();
        let empty = repository.put_tree(
            &Tree {
                entries: BTreeMap::new(),
            },
            &cancel,
        )?;
        let change = new_change()?;
        let initial = Revision {
            change,
            tree: empty,
            parents: Vec::new(),
            description: "Repository initialized".into(),
            author: Identity {
                name: "izu".into(),
                email: "izu@localhost".into(),
            },
            created_at_unix_ms: timestamp()?,
            origin: Some(izu_model::RevisionOrigin::Bootstrap),
        };
        let revision = repository.put_revision(&initial, &cancel)?;
        let workspace = WorkspaceRecord {
            name: "default".into(),
            root: path_text(&repository.root)?,
            head: revision,
            sources: BTreeMap::from([(
                WORKING.into(),
                SourceRecord {
                    path: None,
                    tree: empty,
                    revision: Some(revision),
                },
            )]),
        };
        let view = RepositoryView {
            changes: BTreeMap::from([(change, ChangeState::resolved(revision))]),
            refs: BTreeMap::from([(RefName::new("main")?, revision)]),
            workspaces: BTreeMap::from([(selected, workspace)]),
            ..RepositoryView::default()
        };
        root_identity(&directory, &repository.root)?;
        let transaction = repository.store.begin(HeadExpectation::Absent, &cancel)?;
        let operation = Operation {
            parent: None,
            view,
            description: "Initialize repository".into(),
            created_at_unix_ms: timestamp()?,
        };
        let id = repository.write_operation(&operation, &cancel)?;
        publish(transaction.publish(id, &cancel)?)?;
        Ok(repository)
    }

    pub fn open(root: impl AsRef<Path>, options: RepositoryOptions) -> Result<Self> {
        validate_source_limits(&options.source_limits)?;
        let root = canonical_root(root.as_ref())?;
        let directory = Directory::open(&root).map_err(|error| io(&root, error))?;
        let internal = directory.metadata(OsStr::new(".izu")).map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                EngineError::NotRepository(root.clone())
            } else {
                io(root.join(".izu"), error)
            }
        })?;
        let (metadata, selected_marker) = if internal.is_dir() {
            (root.join(".izu"), None)
        } else if internal.is_file() {
            let marker = read_workspace_marker(&directory, &root)?;
            let metadata = PathBuf::from(marker.metadata);
            (metadata, Some(marker.workspace))
        } else {
            return Err(EngineError::InvalidMarker(
                ".izu must be a native directory or regular workspace marker".into(),
            ));
        };
        let store = Store::open(&metadata, &options.store)?;
        let head = store
            .current_head(&CancellationToken::new())?
            .ok_or(EngineError::Uninitialized)?;
        let bytes = store.get(
            head.object_id(),
            ObjectKind::Operation,
            &CancellationToken::new(),
        )?;
        let operation: Operation = decode_metadata(&bytes, &options.store.limits)?;
        let root_text = path_text(&root)?;
        let selected = match selected_marker {
            Some(id) => {
                let record = operation
                    .view
                    .workspaces
                    .get(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                if record.root != root_text {
                    return Err(EngineError::InvalidMarker(
                        "workspace root differs from registered root".into(),
                    ));
                }
                id
            }
            None => operation
                .view
                .workspaces
                .iter()
                .find(|(_, record)| record.root == root_text)
                .map(|(id, _)| *id)
                .ok_or_else(|| {
                    EngineError::InvalidMarker("repository source root is not registered".into())
                })?,
        };
        Ok(Self {
            root,
            metadata,
            selected,
            store,
            options,
        })
    }

    pub fn discover(path: impl AsRef<Path>, options: RepositoryOptions) -> Result<Self> {
        let mut current =
            std::fs::canonicalize(path.as_ref()).map_err(|error| io(path.as_ref(), error))?;
        if current.is_file() {
            current.pop();
        }
        loop {
            match std::fs::symlink_metadata(current.join(".izu")) {
                Ok(_) => return Self::open(current, options),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(io(current.join(".izu"), error)),
            }
            if !current.pop() {
                return Err(EngineError::NotRepository(path.as_ref().into()));
            }
        }
    }

    pub fn root_path(&self) -> &Path {
        &self.root
    }
    pub fn metadata_path(&self) -> &Path {
        &self.metadata
    }
    pub fn workspace_id(&self) -> WorkspaceId {
        self.selected
    }
    pub fn limits(&self) -> &Limits {
        &self.options.store.limits
    }

    pub fn lock_workspace(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceGuard<'_>> {
        let live = self.live_lock(id, cancel)?;
        let lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        if state.expected != expected {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let root = Path::new(&state.record.root);
        let directory = Directory::open(root).map_err(|error| io(root, error))?;
        Ok(WorkspaceGuard {
            repository: self,
            _lock: lock,
            _live: live,
            directory,
            state,
        })
    }

    pub fn lease_writer(
        &self,
        id: WorkspaceId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceWriterLease> {
        let lock = self.live_lock(id, cancel)?;
        let _metadata = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        let source_directory = Directory::open(Path::new(&state.record.root))
            .map_err(|error| io(&state.record.root, error))?;
        let root = self
            .store
            .root_directory()
            .try_clone()
            .map_err(|error| io(&self.metadata, error))?;
        let directory = root
            .open_dir(OsStr::new("workspace-locks"))
            .map_err(|error| io(&self.metadata, error))?;
        let intent_name = std::ffi::OsString::from(format!("live-{id}.intent"));
        let token = WriterToken::new(new_workspace()?.to_string())?;
        let intent = WriterIntent {
            workspace: id,
            token,
            created_at_unix_ms: timestamp()?,
        };
        let bytes = serde_json::to_vec(&intent)
            .map_err(|error| EngineError::InvalidInput(error.to_string()))?;
        let mut marker = directory
            .create_new_file(&intent_name)
            .map_err(|error| io(&self.metadata, error))?;
        marker
            .write_all(&bytes)
            .map_err(|error| io(&self.metadata, error))?;
        izu_platform::sync_file(&marker).map_err(|error| io(&self.metadata, error))?;
        directory
            .sync()
            .map_err(|error| io(&self.metadata, error))?;
        Ok(WorkspaceWriterLease {
            _lock: lock,
            state,
            intent,
            directory,
            repository_directory: root,
            source_directory,
            intent_name,
        })
    }

    pub fn lease_workspace(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceWriterLease> {
        if self.workspace(id, cancel)?.expected != expected {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let lease = self.lease_writer(id, cancel)?;
        if lease.state.expected != expected {
            lease.finish_stopped()?;
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        Ok(lease)
    }

    pub fn writer_intent(
        &self,
        id: WorkspaceId,
        cancel: &CancellationToken,
    ) -> Result<Option<WriterIntent>> {
        source::check(cancel)?;
        let root = self
            .store
            .root_directory()
            .try_clone()
            .map_err(|error| io(&self.metadata, error))?;
        let directory = match root.open_dir(OsStr::new("workspace-locks")) {
            Ok(directory) => directory,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(io(&self.metadata, error)),
        };
        read_intent(&directory, OsStr::new(&format!("live-{id}.intent")), id)
    }

    /// Explicit recovery assertion for a crashed controller. The caller must
    /// prove its process group stopped; this records the assertion and clears
    /// only the exact named intent. Ordinary recovery never clears writer intent.
    pub fn acknowledge_writer_stopped(
        &self,
        id: WorkspaceId,
        expected: &WriterToken,
        cancel: &CancellationToken,
    ) -> Result<OperationId> {
        self.acknowledge_writer_stopped_with_note(
            id,
            expected,
            "Caller asserts the selected writer group is stopped and reaped".into(),
            cancel,
        )
    }

    /// Persists the caller's explicit stopped/reaped assertion with the exact
    /// token. The engine does not infer process ownership from a PID.
    pub fn acknowledge_writer_stopped_with_note(
        &self,
        id: WorkspaceId,
        expected: &WriterToken,
        note: String,
        cancel: &CancellationToken,
    ) -> Result<OperationId> {
        if note.trim().is_empty() {
            return Err(EngineError::InvalidInput(
                "stopped writer acknowledgement requires an assertion".into(),
            ));
        }
        if note.len() > self.limits().max_text_bytes {
            return Err(EngineError::Limit {
                resource: "writer acknowledgement note",
                limit: self.limits().max_text_bytes as u64,
            });
        }
        let _live = self.source_lock(id, "managed writer", &format!("live-{id}.lock"), cancel)?;
        let _writer = self.workspace_lock(id, cancel)?;
        let intent = self
            .writer_intent(id, cancel)?
            .ok_or_else(|| EngineError::InvalidInput("managed writer intent is absent".into()))?;
        if &intent.token != expected {
            return Err(EngineError::WorkspaceBusy {
                workspace: id,
                kind: "different managed writer intent",
            });
        }
        let operation = self.mutate(
            format!(
                "Explicitly acknowledge stopped writer {} for workspace {id}: {note}",
                expected.as_str()
            ),
            cancel,
            |_| Ok(()),
        )?;
        let root = self
            .store
            .root_directory()
            .try_clone()
            .map_err(|error| io(&self.metadata, error))?;
        let directory = root
            .open_dir(OsStr::new("workspace-locks"))
            .map_err(|error| io(&self.metadata, error))?;
        directory
            .remove_file(OsStr::new(&format!("live-{id}.intent")))
            .map_err(|error| EngineError::PublicationUncertain {
                operation,
                reason: format!(
                    "stopped assertion is durable; removing writer intent failed: {error}"
                ),
            })?;
        directory
            .sync()
            .map_err(|error| EngineError::PublicationUncertain { operation, reason: format!("stopped assertion is durable; writer intent removal persistence is uncertain: {error}") })?;
        Ok(operation)
    }

    pub fn current_operation(&self, cancel: &CancellationToken) -> Result<OperationId> {
        source::check(cancel)?;
        self.store
            .current_head(cancel)?
            .ok_or(EngineError::Uninitialized)
    }

    pub fn operation(&self, id: OperationId, cancel: &CancellationToken) -> Result<Operation> {
        self.decode(id.object_id(), ObjectKind::Operation, cancel)
    }

    pub fn view(&self, cancel: &CancellationToken) -> Result<RepositoryView> {
        Ok(self
            .operation(self.current_operation(cancel)?, cancel)?
            .view)
    }

    pub fn workspace(&self, id: WorkspaceId, cancel: &CancellationToken) -> Result<WorkspaceState> {
        let mut view = self.view(cancel)?;
        let record = view
            .workspaces
            .remove(&id)
            .ok_or(EngineError::UnknownWorkspace(id))?;
        let expected = expectation(&record)?;
        Ok(WorkspaceState {
            id,
            record,
            expected,
        })
    }

    pub fn revision(&self, id: RevisionId, cancel: &CancellationToken) -> Result<Revision> {
        let revision: Revision = self.decode(id.object_id(), ObjectKind::Revision, cancel)?;
        if matches!(revision.origin, Some(izu_model::RevisionOrigin::Bootstrap))
            && !self.tree(revision.tree, cancel)?.entries.is_empty()
        {
            return Err(EngineError::InvalidInput(
                "bootstrap revision must have an empty tree".into(),
            ));
        }
        Ok(revision)
    }

    pub fn tree(&self, id: TreeId, cancel: &CancellationToken) -> Result<Tree> {
        let tree: Tree = self.decode(id.object_id(), ObjectKind::Tree, cancel)?;
        source::validate_tree(&tree)?;
        Ok(tree)
    }

    pub fn candidate(
        &self,
        id: CandidateId,
        cancel: &CancellationToken,
    ) -> Result<IntegrationCandidate> {
        if !self.view(cancel)?.candidates.contains(&id) {
            return Err(EngineError::UnknownCandidate(id.to_string()));
        }
        self.decode(id.object_id(), ObjectKind::Candidate, cancel)
    }

    pub fn evidence(&self, id: EvidenceId, cancel: &CancellationToken) -> Result<CheckEvidence> {
        self.decode(id.object_id(), ObjectKind::Evidence, cancel)
    }

    pub fn object(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        cancel: &CancellationToken,
    ) -> Result<Vec<u8>> {
        source::check(cancel)?;
        Ok(self.store.get(id, kind, cancel)?)
    }

    pub fn blob(&self, id: ObjectId, cancel: &CancellationToken) -> Result<Vec<u8>> {
        self.object(id, ObjectKind::Blob, cancel)
    }
    pub fn read_blob<W: Write>(
        &self,
        id: ObjectId,
        writer: &mut W,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        source::check(cancel)?;
        Ok(self.store.read_blob(id, writer, cancel)?)
    }
    pub fn put_blob<R: Read>(
        &self,
        reader: &mut R,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        source::check(cancel)?;
        Ok(self.store.put_blob(reader, length, cancel)?)
    }
    pub fn put_tree(&self, tree: &Tree, cancel: &CancellationToken) -> Result<TreeId> {
        source::validate_tree(tree)?;
        let bytes = encode_metadata(tree, self.limits())?;
        Ok(TreeId::from_object(self.store.put(
            ObjectKind::Tree,
            &bytes,
            cancel,
        )?))
    }
    pub fn put_revision(
        &self,
        revision: &Revision,
        cancel: &CancellationToken,
    ) -> Result<RevisionId> {
        let tree = self.tree(revision.tree, cancel)?;
        if matches!(revision.origin, Some(izu_model::RevisionOrigin::Bootstrap))
            && !tree.entries.is_empty()
        {
            return Err(EngineError::InvalidInput(
                "bootstrap revision must have an empty tree".into(),
            ));
        }
        for parent in &revision.parents {
            self.revision(*parent, cancel)?;
        }
        let bytes = encode_metadata(revision, self.limits())?;
        Ok(RevisionId::from_object(self.store.put(
            ObjectKind::Revision,
            &bytes,
            cancel,
        )?))
    }

    /// Imports an immutable native object, including unreferenced historical
    /// operations. This never changes HEAD or adopts foreign workspace paths.
    pub fn put_object(
        &self,
        kind: ObjectKind,
        bytes: &[u8],
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        source::check(cancel)?;
        match kind {
            ObjectKind::Blob => {}
            ObjectKind::Tree => {
                let tree: Tree = decode_metadata(bytes, self.limits())?;
                source::validate_tree(&tree)?;
            }
            ObjectKind::Revision => {
                let _: Revision = decode_metadata(bytes, self.limits())?;
            }
            ObjectKind::Operation => {
                let _: Operation = decode_metadata(bytes, self.limits())?;
            }
            ObjectKind::Candidate => {
                let _: IntegrationCandidate = decode_metadata(bytes, self.limits())?;
            }
            ObjectKind::Evidence => {
                let _: CheckEvidence = decode_metadata(bytes, self.limits())?;
            }
        }
        Ok(self.store.put(kind, bytes, cancel)?)
    }

    fn decode<T: serde::de::DeserializeOwned + serde::Serialize + Validate>(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        cancel: &CancellationToken,
    ) -> Result<T> {
        source::check(cancel)?;
        Ok(decode_metadata(
            &self.store.get(id, kind, cancel)?,
            self.limits(),
        )?)
    }

    fn write_operation(
        &self,
        operation: &Operation,
        cancel: &CancellationToken,
    ) -> Result<OperationId> {
        let bytes = encode_metadata(operation, self.limits())?;
        Ok(OperationId::from_object(self.store.put(
            ObjectKind::Operation,
            &bytes,
            cancel,
        )?))
    }

    /// Read the latest view while holding the store lock, then validate only the
    /// touched workspace/reference. Unrelated concurrent updates remain intact.
    fn mutate<F>(
        &self,
        description: String,
        cancel: &CancellationToken,
        update: F,
    ) -> Result<OperationId>
    where
        F: FnOnce(&mut RepositoryView) -> Result<()>,
    {
        self.mutate_guarded(description, cancel, None, update)
    }

    fn mutate_source<F>(
        &self,
        source: &SourceGuard<'_>,
        tree: TreeId,
        description: String,
        cancel: &CancellationToken,
        update: F,
    ) -> Result<OperationId>
    where
        F: FnOnce(&mut RepositoryView) -> Result<()>,
    {
        self.mutate_guarded(description, cancel, Some((source, tree)), update)
    }

    fn mutate_guarded<F>(
        &self,
        description: String,
        cancel: &CancellationToken,
        source: Option<(&SourceGuard<'_>, TreeId)>,
        update: F,
    ) -> Result<OperationId>
    where
        F: FnOnce(&mut RepositoryView) -> Result<()>,
    {
        source::check(cancel)?;
        if let Some((source, _)) = source {
            source.verify()?;
        }
        let transaction = self.store.begin(HeadExpectation::Any, cancel)?;
        let parent = transaction
            .current_head()
            .ok_or(EngineError::Uninitialized)?;
        let mut view = self.operation(parent, cancel)?.view;
        update(&mut view)?;
        let operation = Operation {
            parent: Some(parent),
            view,
            description,
            created_at_unix_ms: timestamp()?,
        };
        let id = self.write_operation(&operation, cancel)?;
        if let Some((source, _)) = source {
            source.verify()?;
        }
        let result = publish(transaction.publish(id, cancel)?);
        let Some((source, tree)) = source else {
            return result;
        };
        // A visible HEAD cannot be reported as a prepublication rejection,
        // even if the store's own persistence checks also returned uncertainty.
        let identity = source.verify();
        match (result, identity) {
            (Ok(operation), Ok(())) => Ok(operation),
            (Ok(operation), Err(error)) => Err(EngineError::PublicationUncertain {
                operation,
                reason: source.uncertainty(tree, error),
            }),
            (Err(EngineError::PublicationUncertain { operation, reason }), identity) => {
                let reason = match identity {
                    Ok(()) => reason,
                    Err(error) => format!("{reason}; {error}"),
                };
                Err(EngineError::PublicationUncertain {
                    operation,
                    reason: source.uncertainty(tree, reason),
                })
            }
            (Err(error), _) => Err(error),
        }
    }

    fn tracked(
        &self,
        state: &WorkspaceState,
        cancel: &CancellationToken,
    ) -> Result<BTreeSet<RepoPath>> {
        let baseline = self.tree(self.revision(state.record.head, cancel)?.tree, cancel)?;
        let working = self.tree(state.expected.working_tree, cancel)?;
        Ok(baseline
            .entries
            .into_keys()
            .chain(working.entries.into_keys())
            .collect())
    }

    fn capture_state(
        &self,
        state: &WorkspaceState,
        selection: &Selection,
        include_ignored: bool,
        cancel: &CancellationToken,
    ) -> Result<(SourceGuard<'_>, Tree, CaptureStats)> {
        let source = SourceGuard::open(self, state)?;
        let (tree, stats) =
            self.capture_state_at(state, &source.directory, selection, include_ignored, cancel)?;
        source.verify()?;
        Ok((source, tree, stats))
    }

    fn capture_state_at(
        &self,
        state: &WorkspaceState,
        directory: &Directory,
        selection: &Selection,
        include_ignored: bool,
        cancel: &CancellationToken,
    ) -> Result<(Tree, CaptureStats)> {
        let base = self.tree(state.expected.working_tree, cancel)?;
        reject_conflicts(&base)?;
        let tracked = self.tracked(state, cancel)?;
        source::capture_at(
            &self.store,
            directory,
            Path::new(&state.record.root),
            &base,
            &tracked,
            selection,
            &self.options.source_limits,
            cancel,
            include_ignored,
        )
    }

    fn verify_source_at(
        &self,
        directory: &Directory,
        root: &Path,
        expected: &Tree,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let tracked = expected.entries.keys().cloned().collect();
        let (actual, _) = source::capture_at(
            &self.store,
            directory,
            root,
            expected,
            &tracked,
            &Selection::All,
            &self.options.source_limits,
            cancel,
            true,
        )?;
        if &actual != expected {
            return Err(EngineError::SourceChanged(root.into()));
        }
        Ok(())
    }

    pub fn capture(
        &self,
        id: WorkspaceId,
        selection: Selection,
        cancel: &CancellationToken,
    ) -> Result<CapturedTree> {
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        let (source, tree, stats) = self.capture_state(&state, &selection, false, cancel)?;
        let tree = self.put_tree(&tree, cancel)?;
        source.verify()?;
        Ok(CapturedTree {
            workspace: id,
            expected: state.expected,
            tree,
            stats,
        })
    }

    /// Includes ignored entries for recovery/owned-environment checks. Native
    /// metadata and recovery originals remain protected from traversal.
    pub fn capture_all(&self, id: WorkspaceId, cancel: &CancellationToken) -> Result<CapturedTree> {
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        let (source, tree, stats) = self.capture_state(&state, &Selection::All, true, cancel)?;
        let tree = self.put_tree(&tree, cancel)?;
        source.verify()?;
        Ok(CapturedTree {
            workspace: id,
            expected: state.expected,
            tree,
            stats,
        })
    }

    /// Captures the lease's pinned source while retaining its live writer lease.
    /// Only the native source lock is acquired, so this cannot recursively wait
    /// on the live lock already held by the caller.
    pub fn capture_leased(
        &self,
        lease: &WorkspaceWriterLease,
        selection: Selection,
        cancel: &CancellationToken,
    ) -> Result<CapturedTree> {
        source::check(cancel)?;
        match same_directory(
            &lease.repository_directory,
            self.store.root_directory(),
            &self.metadata,
        ) {
            Ok(()) => {}
            Err(EngineError::SourceChanged(_)) => {
                return Err(EngineError::InvalidInput(
                    "writer lease belongs to another repository".into(),
                ));
            }
            Err(error) => return Err(error),
        }
        let id = lease.state.id;
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        if state.expected != lease.state.expected || state.record.root != lease.state.record.root {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let source = SourceGuard::pinned(self, &state, &lease.source_directory)?;
        let (tree, stats) =
            self.capture_state_at(&state, &lease.source_directory, &selection, false, cancel)?;
        let tree = self.put_tree(&tree, cancel)?;
        source.verify()?;
        Ok(CapturedTree {
            workspace: id,
            expected: state.expected,
            tree,
            stats,
        })
    }

    pub fn checkpoint(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        selection: Selection,
        cancel: &CancellationToken,
    ) -> Result<CheckpointReceipt> {
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        if state.expected != expected {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let (source, tree, _) = self.capture_state(&state, &selection, false, cancel)?;
        let tree_id = self.put_tree(&tree, cancel)?;
        let head_tree = self.revision(expected.head, cancel)?.tree;
        let operation = self.mutate_source(
            &source,
            tree_id,
            format!("Checkpoint workspace {}", state.record.name),
            cancel,
            |view| {
                check_workspace(view, id, expected)?;
                let record = view
                    .workspaces
                    .get_mut(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                set_working(
                    record,
                    tree_id,
                    (head_tree == tree_id).then_some(expected.head),
                );
                Ok(())
            },
        )?;
        Ok(CheckpointReceipt {
            operation,
            workspace: id,
            head: expected.head,
            tree: tree_id,
        })
    }

    pub fn commit(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        selection: Selection,
        description: String,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<CommitReceipt> {
        self.commit_internal(id, expected, selection, description, author, None, cancel)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn commit_to_ref(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        selection: Selection,
        description: String,
        author: Identity,
        reference: RefExpectation,
        cancel: &CancellationToken,
    ) -> Result<CommitReceipt> {
        if reference
            .expected
            .is_some_and(|revision| revision != expected.head)
        {
            return Err(EngineError::InvalidInput("reference and workspace heads diverged; reconcile before committing to that reference".into()));
        }
        self.commit_internal(
            id,
            expected,
            selection,
            description,
            author,
            Some(reference),
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn commit_internal(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        selection: Selection,
        description: String,
        author: Identity,
        reference: Option<RefExpectation>,
        cancel: &CancellationToken,
    ) -> Result<CommitReceipt> {
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        if state.expected != expected {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let previous = self.revision(expected.head, cancel)?;
        let (source, working, _) = self.capture_state(&state, &selection, false, cancel)?;
        let committed = source::overlay(&self.tree(previous.tree, cancel)?, &working, &selection)?;
        let tree = self.put_tree(&committed, cancel)?;
        let working_tree = self.put_tree(&working, cancel)?;
        let change = new_change()?;
        let revision = Revision {
            change,
            tree,
            parents: vec![expected.head],
            description,
            author,
            created_at_unix_ms: timestamp()?,
            origin: None,
        };
        let revision_id = self.put_revision(&revision, cancel)?;
        let operation = self.mutate_source(
            &source,
            working_tree,
            format!("Commit workspace {}", state.record.name),
            cancel,
            |view| {
                check_workspace(view, id, expected)?;
                if let Some(reference) = &reference {
                    check_ref(view, &reference.name, reference.expected)?;
                }
                view.changes
                    .insert(change, ChangeState::resolved(revision_id));
                let record = view
                    .workspaces
                    .get_mut(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                record.head = revision_id;
                set_working(
                    record,
                    working_tree,
                    (tree == working_tree).then_some(revision_id),
                );
                if let Some(reference) = &reference {
                    view.refs.insert(reference.name.clone(), revision_id);
                }
                Ok(())
            },
        )?;
        Ok(CommitReceipt {
            operation,
            workspace: id,
            revision: revision_id,
            change,
            tree,
            working_tree,
        })
    }

    /// Rewrites a stable change into a new immutable revision. An expected head
    /// prevents a concurrent rewrite from being silently overwritten.
    pub fn revise(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        selection: Selection,
        description: String,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<CommitReceipt> {
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        if state.expected != expected {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let previous = self.revision(expected.head, cancel)?;
        let (source, working, _) = self.capture_state(&state, &selection, false, cancel)?;
        let committed = source::overlay(&self.tree(previous.tree, cancel)?, &working, &selection)?;
        let tree = self.put_tree(&committed, cancel)?;
        let working_tree = self.put_tree(&working, cancel)?;
        let change = previous.change;
        let revision = Revision {
            change,
            tree,
            parents: previous.parents,
            description,
            author,
            created_at_unix_ms: timestamp()?,
            origin: None,
        };
        let revision_id = self.put_revision(&revision, cancel)?;
        let operation = self.mutate_source(
            &source,
            working_tree,
            format!("Revise workspace {}", state.record.name),
            cancel,
            |view| {
                check_workspace(view, id, expected)?;
                let change_state = view
                    .changes
                    .get_mut(&change)
                    .ok_or_else(|| EngineError::UnknownRevision(change.to_string()))?;
                if !change_state.heads.remove(&expected.head) {
                    return Err(EngineError::DivergentChange(change.to_string()));
                }
                change_state.heads.insert(revision_id);
                let record = view
                    .workspaces
                    .get_mut(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                record.head = revision_id;
                set_working(
                    record,
                    working_tree,
                    (tree == working_tree).then_some(revision_id),
                );
                Ok(())
            },
        )?;
        Ok(CommitReceipt {
            operation,
            workspace: id,
            revision: revision_id,
            change,
            tree,
            working_tree,
        })
    }

    pub fn status(&self, id: WorkspaceId, cancel: &CancellationToken) -> Result<Status> {
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        let baseline = self.tree(self.revision(state.record.head, cancel)?.tree, cancel)?;
        let (source, captured, stats) =
            self.capture_state(&state, &Selection::All, false, cancel)?;
        let entries = tree_diff(&baseline, &captured)?;
        let captured_tree = self.put_tree(&captured, cancel)?;
        source.verify()?;
        Ok(Status {
            workspace: id,
            head: state.record.head,
            checkpoint_tree: state.expected.working_tree,
            captured_tree,
            entries,
            stats,
        })
    }

    pub fn diff(
        &self,
        before: RevisionId,
        after: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathChange>> {
        tree_diff(
            &self.tree(self.revision(before, cancel)?.tree, cancel)?,
            &self.tree(self.revision(after, cancel)?.tree, cancel)?,
        )
    }

    pub fn working_diff(
        &self,
        id: WorkspaceId,
        cancel: &CancellationToken,
    ) -> Result<Vec<PathChange>> {
        Ok(self.status(id, cancel)?.entries)
    }

    pub fn log(
        &self,
        start: RevisionId,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<RevisionSummary>> {
        self.history_limit(limit)?;
        let mut seen = BTreeSet::new();
        let mut queue = VecDeque::from([start]);
        let mut result = Vec::new();
        let mut bytes = 0_u64;
        result
            .try_reserve(limit.min(1024))
            .map_err(|_| EngineError::Allocation {
                resource: "revision history",
            })?;
        while let Some(id) = queue.pop_front() {
            source::check(cancel)?;
            if !seen.insert(id) {
                continue;
            }
            if result.len() >= limit {
                break;
            }
            bytes = self.add_history_bytes(bytes, id.object_id(), cancel)?;
            let revision = self.revision(id, cancel)?;
            queue
                .try_reserve(revision.parents.len())
                .map_err(|_| EngineError::Allocation {
                    resource: "revision traversal",
                })?;
            queue.extend(revision.parents.iter().copied());
            result.try_reserve(1).map_err(|_| EngineError::Allocation {
                resource: "revision history",
            })?;
            result.push(RevisionSummary { id, revision });
        }
        Ok(result)
    }

    pub fn operations(
        &self,
        limit: usize,
        cancel: &CancellationToken,
    ) -> Result<Vec<OperationSummary>> {
        self.history_limit(limit)?;
        let mut current = Some(self.current_operation(cancel)?);
        let mut seen = BTreeSet::new();
        let mut result = Vec::new();
        let mut bytes = 0_u64;
        while let Some(id) = current {
            source::check(cancel)?;
            if result.len() >= limit {
                break;
            }
            if !seen.insert(id) {
                return Err(EngineError::InvalidInput("operation cycle".into()));
            }
            bytes = self.add_history_bytes(bytes, id.object_id(), cancel)?;
            let operation = self.operation(id, cancel)?;
            current = operation.parent;
            result.try_reserve(1).map_err(|_| EngineError::Allocation {
                resource: "operation history",
            })?;
            result.push(OperationSummary { id, operation });
        }
        Ok(result)
    }

    fn history_limit(&self, limit: usize) -> Result<()> {
        if limit == 0 || limit > self.options.source_limits.max_history {
            return Err(EngineError::Limit {
                resource: "history traversal",
                limit: self.options.source_limits.max_history as u64,
            });
        }
        Ok(())
    }

    fn add_history_bytes(
        &self,
        previous: u64,
        id: ObjectId,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        let bytes = previous
            .checked_add(self.store.object_info(id, cancel)?.payload_len)
            .ok_or(EngineError::Limit {
                resource: "history bytes",
                limit: self.options.source_limits.max_history_bytes,
            })?;
        if bytes > self.options.source_limits.max_history_bytes {
            return Err(EngineError::Limit {
                resource: "history bytes",
                limit: self.options.source_limits.max_history_bytes,
            });
        }
        Ok(bytes)
    }

    fn workspace_lock(&self, id: WorkspaceId, cancel: &CancellationToken) -> Result<File> {
        self.source_lock(id, "native source", &format!("{id}.lock"), cancel)
    }

    fn live_lock(&self, id: WorkspaceId, cancel: &CancellationToken) -> Result<File> {
        let lock = self.source_lock(
            id,
            "managed writer or materialization",
            &format!("live-{id}.lock"),
            cancel,
        )?;
        if self.writer_intent(id, cancel)?.is_some() {
            return Err(EngineError::WorkspaceBusy {
                workspace: id,
                kind: "unknown managed writer (explicit stopped acknowledgement required)",
            });
        }
        Ok(lock)
    }

    fn source_lock(
        &self,
        id: WorkspaceId,
        kind: &'static str,
        filename: &str,
        cancel: &CancellationToken,
    ) -> Result<File> {
        source::check(cancel)?;
        let context = |action: &'static str, error: std::io::Error| {
            io(
                &self.metadata,
                std::io::Error::new(error.kind(), format!("{action}: {error}")),
            )
        };
        let root = self
            .store
            .root_directory()
            .try_clone()
            .map_err(|error| context("open workspace lock root", error))?;
        let locks = root
            .open_dir(OsStr::new("workspace-locks"))
            .map_err(|error| context("open workspace locks directory", error))?;
        let file = locks
            .open_existing_lock(OsStr::new(filename))
            .map_err(|error| context("open workspace lock", error))?;
        let started = Instant::now();
        loop {
            source::check(cancel)?;
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(std::fs::TryLockError::WouldBlock) => {
                    let remaining = self
                        .options
                        .store
                        .lock_timeout
                        .checked_sub(started.elapsed())
                        .ok_or(EngineError::WorkspaceBusy {
                            workspace: id,
                            kind,
                        })?;
                    if remaining.is_zero() {
                        return Err(EngineError::WorkspaceBusy {
                            workspace: id,
                            kind,
                        });
                    }
                    std::thread::sleep(remaining.min(Duration::from_millis(5)));
                }
                Err(std::fs::TryLockError::Error(error)) => {
                    return Err(context("acquire workspace lock", error));
                }
            }
        }
    }

    fn prepare_workspace_locks(&self, id: WorkspaceId) -> Result<()> {
        let root = self
            .store
            .root_directory()
            .try_clone()
            .map_err(|error| io(&self.metadata, error))?;
        let directory = root
            .ensure_dir(OsStr::new("workspace-locks"))
            .map_err(|error| io(&self.metadata, error))?;
        root.sync().map_err(|error| io(&self.metadata, error))?;
        for filename in [format!("{id}.lock"), format!("live-{id}.lock")] {
            let file = match directory.create_new_file(OsStr::new(&filename)) {
                Ok(file) => file,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => directory
                    .open_existing_lock(OsStr::new(&filename))
                    .map_err(|error| io(&self.metadata, error))?,
                Err(error) => return Err(io(&self.metadata, error)),
            };
            izu_platform::sync_file(&file).map_err(|error| io(&self.metadata, error))?;
        }
        directory.sync().map_err(|error| io(&self.metadata, error))
    }

    pub fn resolve_revision(
        &self,
        selector: &str,
        cancel: &CancellationToken,
    ) -> Result<RevisionId> {
        if selector == "@" {
            return Ok(self.workspace(self.selected, cancel)?.record.head);
        }
        let view = self.view(cancel)?;
        if let Some(change) = selector.strip_prefix("change:") {
            let id: ChangeId = change.parse()?;
            let state = view
                .changes
                .get(&id)
                .ok_or_else(|| EngineError::UnknownRevision(selector.into()))?;
            return state
                .single_head()
                .ok_or_else(|| EngineError::DivergentChange(selector.into()));
        }
        if let Ok(name) = RefName::new(selector)
            && let Some(id) = view.refs.get(&name)
        {
            return Ok(*id);
        }
        if let Ok(id) = selector.parse::<RevisionId>() {
            self.revision(id, cancel)?;
            return Ok(id);
        }
        // Prefix queries only inspect authoritative revision heads/ancestors.
        if selector.len() < 8 || !selector.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(EngineError::UnknownRevision(selector.into()));
        }
        let roots: BTreeSet<_> = view
            .changes
            .values()
            .flat_map(|state| state.heads.iter().copied())
            .collect();
        let mut matches = BTreeSet::new();
        let mut visited = BTreeSet::new();
        let mut queue: VecDeque<_> = roots.into_iter().collect();
        while let Some(id) = queue.pop_front() {
            source::check(cancel)?;
            if !visited.insert(id) {
                continue;
            }
            if visited.len() > self.options.source_limits.max_history {
                return Err(EngineError::Limit {
                    resource: "revision search",
                    limit: self.options.source_limits.max_history as u64,
                });
            }
            if id.to_string().starts_with(selector) {
                matches.insert(id);
            }
            queue.extend(self.revision(id, cancel)?.parents);
        }
        match matches.len() {
            1 => matches
                .into_iter()
                .next()
                .ok_or_else(|| EngineError::UnknownRevision(selector.into())),
            0 => Err(EngineError::UnknownRevision(selector.into())),
            _ => Err(EngineError::InvalidInput(format!(
                "ambiguous revision prefix {selector}"
            ))),
        }
    }

    pub fn update_ref(
        &self,
        name: &RefName,
        expected: Option<RevisionId>,
        new: Option<RevisionId>,
        cancel: &CancellationToken,
    ) -> Result<OperationId> {
        self.import_revisions(
            &[],
            &[RefUpdate {
                name: name.clone(),
                expected,
                new,
            }],
            cancel,
        )
    }

    pub fn import_revisions(
        &self,
        revisions: &[RevisionId],
        refs: &[RefUpdate],
        cancel: &CancellationToken,
    ) -> Result<OperationId> {
        if revisions.len() > self.options.source_limits.max_history {
            return Err(EngineError::Limit {
                resource: "import revisions",
                limit: self.options.source_limits.max_history as u64,
            });
        }
        let mut imported = Vec::new();
        imported
            .try_reserve(revisions.len())
            .map_err(|_| EngineError::Allocation {
                resource: "import revisions",
            })?;
        for id in revisions {
            let revision = self.revision(*id, cancel)?;
            self.verify_revision_closure(*id, cancel)?;
            imported.push((*id, revision.change));
        }
        let mut unique = BTreeSet::new();
        for update in refs {
            if !unique.insert(update.name.clone()) {
                return Err(EngineError::InvalidInput("duplicate ref update".into()));
            }
            if let Some(id) = update.new {
                self.verify_revision_closure(id, cancel)?;
            }
        }
        self.mutate(
            "Import revisions and update references".into(),
            cancel,
            |view| {
                for update in refs {
                    check_ref(view, &update.name, update.expected)?;
                }
                for (id, change) in imported {
                    match view.changes.get_mut(&change) {
                        Some(state) => {
                            state.heads.insert(id);
                        }
                        None => {
                            view.changes.insert(change, ChangeState::resolved(id));
                        }
                    }
                }
                for update in refs {
                    match update.new {
                        Some(id) => {
                            view.refs.insert(update.name.clone(), id);
                        }
                        None => {
                            view.refs.remove(&update.name);
                        }
                    }
                }
                Ok(())
            },
        )
    }

    pub fn fork_workspace(
        &self,
        name: String,
        path: impl AsRef<Path>,
        from: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceState> {
        let requested = path.as_ref();
        let (root, directory) = match std::fs::symlink_metadata(requested) {
            Ok(metadata) if metadata.is_dir() => {
                let root = canonical_root(requested)?;
                let directory = Directory::open(&root).map_err(|error| io(&root, error))?;
                (root, directory)
            }
            Ok(_) => return Err(EngineError::AlreadyExists(requested.into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let parent_path = requested
                    .parent()
                    .filter(|path| !path.as_os_str().is_empty())
                    .unwrap_or_else(|| Path::new("."));
                let parent_path = canonical_root(parent_path)?;
                let filename = requested.file_name().ok_or_else(|| {
                    EngineError::InvalidInput("workspace path has no final component".into())
                })?;
                let parent =
                    Directory::open(&parent_path).map_err(|error| io(&parent_path, error))?;
                let directory = parent
                    .create_dir(filename)
                    .map_err(|error| io(requested, error))?;
                parent.sync().map_err(|error| io(&parent_path, error))?;
                (parent_path.join(filename), directory)
            }
            Err(error) => return Err(io(requested, error)),
        };
        self.fork_workspace_at(name, root, &directory, from, cancel)
            .map(|receipt| receipt.workspace)
    }

    fn fork_workspace_at(
        &self,
        name: String,
        root: PathBuf,
        directory: &Directory,
        from: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceForkReceipt> {
        if name.trim().is_empty() {
            return Err(EngineError::InvalidInput("workspace name is empty".into()));
        }
        let revision = self.revision(from, cancel)?;
        let tree = self.tree(revision.tree, cancel)?;
        reject_conflicts(&tree)?;
        if directory
            .entries()
            .map_err(|error| io(&root, error))?
            .next()
            .transpose()
            .map_err(|error| io(&root, error))?
            .is_some()
        {
            return Err(EngineError::DirectoryNotEmpty(root));
        }
        let id = new_workspace()?;
        self.prepare_workspace_locks(id)?;
        let root_text = path_text(&root)?;
        // The explicit empty target is owned by this operation; originals do not
        // exist. Failures leave it available rather than recursively deleting.
        source::materialize_empty_at(
            &self.store,
            directory,
            &root,
            &tree,
            &self.options.source_limits,
            cancel,
        )?;
        let marker = WorkspaceMarker {
            version: MARKER_VERSION,
            metadata: path_text(&self.metadata)?,
            workspace: id,
        };
        let bytes = serde_json::to_vec(&marker)
            .map_err(|error| EngineError::InvalidMarker(error.to_string()))?;
        source::write_marker_at(directory, &root, &bytes)?;
        self.verify_source_at(directory, &root, &tree, cancel)?;
        root_identity(directory, &root)?;
        let record = WorkspaceRecord {
            name: name.clone(),
            root: root_text,
            head: from,
            sources: BTreeMap::from([(
                WORKING.into(),
                SourceRecord {
                    path: None,
                    tree: revision.tree,
                    revision: Some(from),
                },
            )]),
        };
        let operation = self.mutate(format!("Fork workspace {name}"), cancel, |view| {
            if view
                .workspaces
                .values()
                .any(|workspace| workspace.name == name)
            {
                return Err(EngineError::WorkspaceNameExists(name.clone()));
            }
            if view
                .workspaces
                .values()
                .any(|workspace| workspace.root == record.root)
            {
                return Err(EngineError::AlreadyExists(root.clone()));
            }
            view.workspaces.insert(id, record.clone());
            Ok(())
        })?;
        Ok(WorkspaceForkReceipt {
            workspace: WorkspaceState {
                id,
                record,
                expected: WorkspaceExpectation {
                    head: from,
                    working_tree: revision.tree,
                },
            },
            operation,
        })
    }

    pub fn fork_managed_workspace(
        &self,
        name: String,
        from: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceState> {
        self.fork_managed_workspace_receipt(name, from, cancel)
            .map(|receipt| receipt.workspace)
    }

    /// Returns the exact acknowledged fork operation, without rereading a HEAD
    /// that another controller may already have advanced.
    pub fn fork_managed_workspace_receipt(
        &self,
        name: String,
        from: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<WorkspaceForkReceipt> {
        source::check(cancel)?;
        if self
            .view(cancel)?
            .workspaces
            .values()
            .any(|workspace| workspace.name == name)
        {
            return Err(EngineError::WorkspaceNameExists(name));
        }
        let root = self
            .store
            .root_directory()
            .try_clone()
            .map_err(|error| io(&self.metadata, error))?;
        let workspaces = root
            .ensure_dir(OsStr::new("workspaces"))
            .map_err(|error| io(&self.metadata, error))?;
        root.sync().map_err(|error| io(&self.metadata, error))?;
        let leaf = format!("workspace-{}", new_workspace()?);
        let directory = workspaces
            .create_dir(OsStr::new(&leaf))
            .map_err(|error| io(&self.metadata, error))?;
        workspaces
            .sync()
            .map_err(|error| io(&self.metadata, error))?;
        self.fork_workspace_at(
            name,
            self.metadata.join("workspaces").join(leaf),
            &directory,
            from,
            cancel,
        )
    }

    pub fn restore(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        source_revision: RevisionId,
        selection: Selection,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        let revision = self.revision(source_revision, cancel)?;
        let target = self.tree(revision.tree, cancel)?;
        reject_conflicts(&target)?;
        self.restore_tree(
            id,
            expected,
            &target,
            source_revision,
            selection,
            "Restore workspace source",
            None,
            cancel,
        )
    }

    /// Updates a clean workspace to an exact source revision. The cleanliness
    /// guard and restoration share the source lock, including eligible new files.
    pub fn update_workspace(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        source_revision: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        let revision = self.revision(source_revision, cancel)?;
        let target = self.tree(revision.tree, cancel)?;
        reject_conflicts(&target)?;
        let baseline = self.tree(self.revision(expected.head, cancel)?.tree, cancel)?;
        self.restore_tree(
            id,
            expected,
            &target,
            source_revision,
            Selection::All,
            "Update clean workspace source",
            Some(&baseline),
            cancel,
        )
    }

    /// Applies the inverse of a single-parent source revision to selected clean
    /// paths. Later independent edits merge normally; conflicting alternatives
    /// are registered durably without changing workspace files or its head.
    #[allow(clippy::too_many_arguments)]
    pub fn revert(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        reverting: RevisionId,
        selection: Selection,
        description: String,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<RevertPreparation> {
        self.revert_internal(
            id,
            expected,
            reverting,
            selection,
            description,
            author,
            None,
            cancel,
        )
    }

    /// Publishes a clean intentional revert and its owned reference together.
    #[allow(clippy::too_many_arguments)]
    pub fn revert_to_ref(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        reverting: RevisionId,
        selection: Selection,
        description: String,
        author: Identity,
        reference: RefExpectation,
        cancel: &CancellationToken,
    ) -> Result<RevertPreparation> {
        if reference
            .expected
            .is_some_and(|revision| revision != expected.head)
        {
            return Err(EngineError::InvalidInput("reference and workspace heads diverged; reconcile before reverting to that reference".into()));
        }
        self.revert_internal(
            id,
            expected,
            reverting,
            selection,
            description,
            author,
            Some(reference),
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn revert_internal(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        reverting: RevisionId,
        selection: Selection,
        description: String,
        author: Identity,
        reference: Option<RefExpectation>,
        cancel: &CancellationToken,
    ) -> Result<RevertPreparation> {
        source::validate_selection(&selection)?;
        let guard = self.lock_workspace(id, expected, cancel)?;
        let source_revision = self.revision(reverting, cancel)?;
        if matches!(
            source_revision.origin,
            Some(izu_model::RevisionOrigin::Bootstrap)
        ) {
            return Err(EngineError::InvalidInput(
                "the native bootstrap has no source change to revert".into(),
            ));
        }
        if source_revision.parents.len() > 1 {
            return Err(EngineError::InvalidInput(
                "reverting a merge requires an explicit mainline parent".into(),
            ));
        }
        let base = self.tree(source_revision.tree, cancel)?;
        reject_conflicts(&base)?;
        let theirs = match source_revision.parents.first() {
            Some(parent) => self.tree(self.revision(*parent, cancel)?.tree, cancel)?,
            None => Tree::default(),
        };
        reject_conflicts(&theirs)?;
        let current = self.tree(self.revision(expected.head, cancel)?.tree, cancel)?;
        reject_conflicts(&current)?;
        let source = SourceGuard::pinned(self, guard.state(), guard.directory())?;
        let (captured, _) = self.capture_state_at(
            guard.state(),
            guard.directory(),
            &Selection::All,
            false,
            cancel,
        )?;
        for change in tree_diff(&current, &captured)? {
            if selection.includes(&change.path) {
                return Err(EngineError::UncommittedSource {
                    workspace: guard.state().id,
                    path: change.path,
                });
            }
        }
        let (merged, _) = self.merge_trees(&base, &current, &theirs, cancel)?;
        let result = source::overlay(&current, &merged, &selection)?;
        let tree = self.put_tree(&result, cancel)?;
        let revision = Revision {
            change: new_change()?,
            tree,
            parents: vec![expected.head],
            description,
            author,
            created_at_unix_ms: timestamp()?,
            origin: None,
        };
        let revision_id = self.put_revision(&revision, cancel)?;
        let conflicts = conflicts_in(&result);
        if !conflicts.is_empty() {
            let operation = self.mutate_source(
                &source,
                tree,
                "Preserve unresolved source revert".into(),
                cancel,
                |view| {
                    check_workspace(view, id, expected)?;
                    if let Some(reference) = &reference {
                        check_ref(view, &reference.name, reference.expected)?;
                    }
                    view.changes
                        .insert(revision.change, ChangeState::resolved(revision_id));
                    Ok(())
                },
            )?;
            return Ok(RevertPreparation::Conflicted {
                operation,
                revision: revision_id,
                tree,
                conflicts,
            });
        }
        let receipt = self.restore_tree_locked(
            &guard,
            &result,
            revision_id,
            selection,
            "Revert source change",
            Some(&current),
            Some(SourcePublication {
                change: revision.change,
                revision: revision_id,
                reference,
            }),
            cancel,
        )?;
        Ok(RevertPreparation::Applied {
            receipt,
            revision: revision_id,
            tree,
        })
    }

    /// Explicitly replaces conflict alternatives with resolved entries in a new
    /// revision of the same stable change. This never materializes source.
    pub fn resolve_conflicts(
        &self,
        id: RevisionId,
        resolutions: BTreeMap<RepoPath, Option<ResolvedTreeEntry>>,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<RevisionSummary> {
        if resolutions.is_empty() {
            return Err(EngineError::InvalidInput(
                "no conflict resolutions supplied".into(),
            ));
        }
        let previous = self.revision(id, cancel)?;
        let mut tree = self.tree(previous.tree, cancel)?;
        apply_resolutions(&mut tree, resolutions, self.limits().max_tree_entries)?;
        let tree_id = self.put_tree(&tree, cancel)?;
        let revision = Revision {
            tree: tree_id,
            author,
            origin: None,
            created_at_unix_ms: timestamp()?,
            ..previous
        };
        let resolved = self.put_revision(&revision, cancel)?;
        self.mutate("Resolve source revision conflicts".into(), cancel, |view| {
            let state = view
                .changes
                .get_mut(&revision.change)
                .ok_or_else(|| EngineError::UnknownRevision(revision.change.to_string()))?;
            if state.single_head() != Some(id) {
                return Err(EngineError::DivergentChange(revision.change.to_string()));
            }
            *state = ChangeState::resolved(resolved);
            Ok(())
        })?;
        Ok(RevisionSummary {
            id: resolved,
            revision,
        })
    }

    /// Undoes the selected workspace source effect of one operation. References
    /// and other workspaces are never rewound. Its parent is the exact source.
    pub fn undo(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        operation: &OperationId,
        selection: Selection,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        let undoing = self.operation(*operation, cancel)?;
        let parent = undoing.parent.ok_or_else(|| {
            EngineError::InvalidInput("initialization has no earlier workspace source".into())
        })?;
        let previous = self.operation(parent, cancel)?;
        let record = previous
            .view
            .workspaces
            .get(&id)
            .ok_or(EngineError::UnknownWorkspace(id))?;
        let after_record = undoing
            .view
            .workspaces
            .get(&id)
            .ok_or(EngineError::UnknownWorkspace(id))?;
        if expectation(record)? == expectation(after_record)? {
            return Err(EngineError::InvalidInput(
                "operation has no source effect on this workspace".into(),
            ));
        }
        if expected != expectation(after_record)? {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        let after_tree = self.tree(working_tree(after_record)?, cancel)?;
        let tree = self.tree(working_tree(record)?, cancel)?;
        reject_conflicts(&tree)?;
        self.restore_tree(
            id,
            expected,
            &tree,
            record.head,
            selection,
            "Undo workspace source",
            Some(&after_tree),
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn restore_tree(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        target: &Tree,
        head: RevisionId,
        selection: Selection,
        description: &str,
        undo_guard: Option<&Tree>,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        let guard = self.lock_workspace(id, expected, cancel)?;
        let head = if selection == Selection::All {
            head
        } else {
            expected.head
        };
        self.restore_tree_locked(
            &guard,
            target,
            head,
            selection,
            description,
            undo_guard,
            None,
            cancel,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn restore_tree_locked(
        &self,
        guard: &WorkspaceGuard<'_>,
        target: &Tree,
        head: RevisionId,
        selection: Selection,
        description: &str,
        undo_guard: Option<&Tree>,
        register: Option<SourcePublication>,
        cancel: &CancellationToken,
    ) -> Result<RestoreReceipt> {
        source::validate_selection(&selection)?;
        let state = guard.state();
        let id = state.id;
        let expected = state.expected;
        let root = Path::new(&state.record.root);
        let directory = guard.directory();
        let source = SourceGuard::pinned(self, state, directory)?;
        let (eligible_before, _) =
            self.capture_state_at(state, directory, &Selection::All, false, cancel)?;
        let (before, _) = self.capture_state_at(state, directory, &Selection::All, true, cancel)?;
        for (path, entry) in &eligible_before.entries {
            if before.entries.get(path) != Some(entry) {
                return Err(EngineError::SourceChanged(
                    Path::new(&state.record.root).join(path.as_str()),
                ));
            }
        }
        if let Some(guard) = undo_guard {
            for change in tree_diff(guard, &eligible_before)? {
                if selection.includes(&change.path) {
                    return Err(EngineError::UncommittedSource {
                        workspace: id,
                        path: change.path,
                    });
                }
            }
        }
        let recovery_tree = self.put_tree(&before, cancel)?;
        let known = self.tracked(state, cancel)?;
        let mut after = before.clone();
        after
            .entries
            .retain(|path, _| !(known.contains(path) && selection.includes(path)));
        for (path, entry) in &target.entries {
            if selection.includes(path) {
                if after
                    .entries
                    .get(path)
                    .is_some_and(|existing| existing != entry)
                    && !known.contains(path)
                {
                    return Err(EngineError::UnselectedCollision(path.as_str().into()));
                }
                after.entries.insert(path.clone(), entry.clone());
            }
        }
        source::complete_restore_ancestors(&mut after, target, &before, &selection)?;
        let mut eligible_after = eligible_before;
        eligible_after
            .entries
            .retain(|path, _| !(known.contains(path) && selection.includes(path)));
        for (path, entry) in &target.entries {
            if selection.includes(path) {
                eligible_after.entries.insert(path.clone(), entry.clone());
            }
        }
        source::complete_restore_ancestors(&mut eligible_after, target, &before, &selection)?;
        let after_id = self.put_tree(&eligible_after, cancel)?;
        let new_head = head;
        let head_tree = self.revision(new_head, cancel)?.tree;
        // The recovery operation serializes the expected workspace state before
        // any source change. Keep its exact tree reachable in operation history.
        let recovery = self.mutate_source(
            &source,
            recovery_tree,
            format!("Save recovery source for {description}"),
            cancel,
            |view| {
                check_workspace(view, id, expected)?;
                if let Some(reference) = register
                    .as_ref()
                    .and_then(|registration| registration.reference.as_ref())
                {
                    check_ref(view, &reference.name, reference.expected)?;
                }
                let record = view
                    .workspaces
                    .get_mut(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                record.sources.insert(
                    RECOVERY.into(),
                    SourceRecord {
                        path: None,
                        tree: recovery_tree,
                        revision: None,
                    },
                );
                Ok(())
            },
        )?;
        let recovery_expected = expected;
        let result = source::materialize_at(
            &self.store,
            directory,
            root,
            &before,
            &after,
            &recovery.to_string(),
            &self.options.source_limits,
            cancel,
        );
        if let Err(error) = result {
            return Err(EngineError::RestorationFailed {
                recovery,
                reason: error.to_string(),
            });
        }
        self.verify_source_at(directory, root, &after, cancel)
            .map_err(|error| EngineError::RestorationUncertain {
                recovery,
                operation: None,
                reason: error.to_string(),
            })?;
        root_identity(directory, root).map_err(|error| EngineError::RestorationUncertain {
            recovery,
            operation: None,
            reason: error.to_string(),
        })?;
        let operation = self
            .mutate_source(&source, after_id, description.into(), cancel, |view| {
                check_workspace(view, id, recovery_expected)?;
                if let Some(registration) = &register {
                    if let Some(reference) = &registration.reference {
                        check_ref(view, &reference.name, reference.expected)?;
                    }
                    view.changes.insert(
                        registration.change,
                        ChangeState::resolved(registration.revision),
                    );
                }
                let record = view
                    .workspaces
                    .get_mut(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                record.head = new_head;
                set_working(
                    record,
                    after_id,
                    (head_tree == after_id).then_some(new_head),
                );
                if let Some(reference) = register
                    .as_ref()
                    .and_then(|registration| registration.reference.as_ref())
                {
                    view.refs.insert(reference.name.clone(), new_head);
                }
                Ok(())
            })
            .map_err(|error| {
                let operation = error.uncertain_operation();
                EngineError::RestorationUncertain {
                    recovery,
                    operation,
                    reason: error.to_string(),
                }
            })?;
        Ok(RestoreReceipt {
            operation,
            recovery_operation: recovery,
            workspace: id,
            head: new_head,
            tree: after_id,
        })
    }

    /// Closing only removes registration. Its source directory and all immutable
    /// source/history objects remain available; there is no recursive delete.
    pub fn close_workspace(
        &self,
        id: WorkspaceId,
        expected: WorkspaceExpectation,
        cancel: &CancellationToken,
    ) -> Result<OperationId> {
        let _live = self.live_lock(id, cancel)?;
        let _lock = self.workspace_lock(id, cancel)?;
        let state = self.workspace(id, cancel)?;
        if state.expected != expected {
            return Err(EngineError::StaleWorkspace { workspace: id });
        }
        if Path::new(&state.record.root) == self.metadata.parent().unwrap_or(Path::new("")) {
            return Err(EngineError::InvalidInput(
                "the repository source workspace cannot be closed".into(),
            ));
        }
        let (source, current, _) = self.capture_state(&state, &Selection::All, true, cancel)?;
        let tree = self.put_tree(&current, cancel)?;
        let saved = self.mutate_source(
            &source,
            tree,
            format!("Save final source for workspace {}", state.record.name),
            cancel,
            |view| {
                check_workspace(view, id, expected)?;
                let record = view
                    .workspaces
                    .get_mut(&id)
                    .ok_or(EngineError::UnknownWorkspace(id))?;
                record.sources.insert(
                    RECOVERY.into(),
                    SourceRecord {
                        path: None,
                        tree,
                        revision: None,
                    },
                );
                Ok(())
            },
        )?;
        let final_expected = expected;
        self.mutate_source(
            &source,
            tree,
            format!(
                "Close workspace {} (source retained, checkpoint {saved})",
                state.record.name
            ),
            cancel,
            |view| {
                check_workspace(view, id, final_expected)?;
                view.workspaces.remove(&id);
                Ok(())
            },
        )
        .map_err(|error| EngineError::PublicationUncertain {
            operation: error.uncertain_operation().unwrap_or(saved),
            reason: format!(
                "close is incomplete; retained recovery operation {saved}; {}",
                source.uncertainty(tree, error),
            ),
        })
    }

    pub fn object_info(&self, id: ObjectId, cancel: &CancellationToken) -> Result<ObjectInfo> {
        let info = self.store.object_info(id, cancel)?;
        Ok(ObjectInfo {
            id: info.id,
            kind: info.kind,
            payload_len: info.payload_len,
        })
    }

    pub fn read_object_to(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        writer: &mut dyn Write,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        let mut writer = writer;
        Ok(self.store.read_object_to(id, kind, &mut writer, cancel)?)
    }

    pub fn import_object(
        &self,
        kind: ObjectKind,
        expected_id: ObjectId,
        reader: &mut dyn Read,
        length: u64,
        cancel: &CancellationToken,
    ) -> Result<ObjectId> {
        let mut reader = reader;
        let id = self
            .store
            .import_object(kind, expected_id, &mut reader, length, cancel)?;
        if kind == ObjectKind::Tree {
            self.tree(TreeId::from_object(id), cancel)?;
        }
        Ok(id)
    }

    fn verify_revision_closure(&self, id: RevisionId, cancel: &CancellationToken) -> Result<()> {
        self.verify_graph(
            vec![izu_model::ObjectReference {
                id: id.object_id(),
                kind: ObjectKind::Revision,
            }],
            cancel,
        )?;
        Ok(())
    }

    fn verify_graph(
        &self,
        mut pending: Vec<izu_model::ObjectReference>,
        cancel: &CancellationToken,
    ) -> Result<VerificationReport> {
        use izu_model::ReferencedObjects;
        let mut seen = BTreeMap::new();
        let mut report = VerificationReport::default();
        let mut bytes = 0_u64;
        while let Some(reference) = pending.pop() {
            source::check(cancel)?;
            if let Some(kind) = seen.get(&reference.id) {
                if *kind != reference.kind {
                    return Err(EngineError::InvalidInput(
                        "one object has conflicting reference kinds".into(),
                    ));
                }
                continue;
            }
            if seen.len() as u64 >= self.options.store.max_scan_objects {
                return Err(EngineError::Limit {
                    resource: "object closure",
                    limit: self.options.store.max_scan_objects,
                });
            }
            seen.insert(reference.id, reference.kind);
            let info = self.store.object_info(reference.id, cancel)?;
            if info.kind != reference.kind {
                return Err(EngineError::InvalidInput(
                    "reference object kind mismatch".into(),
                ));
            }
            bytes = bytes
                .checked_add(info.payload_len)
                .ok_or(EngineError::Limit {
                    resource: "closure bytes",
                    limit: self.options.store.max_scan_bytes,
                })?;
            if bytes > self.options.store.max_scan_bytes {
                return Err(EngineError::Limit {
                    resource: "closure bytes",
                    limit: self.options.store.max_scan_bytes,
                });
            }
            match reference.kind {
                ObjectKind::Blob => {
                    report.blobs += 1;
                    continue;
                }
                ObjectKind::Tree => report.trees += 1,
                ObjectKind::Revision => report.revisions += 1,
                ObjectKind::Operation => report.operations += 1,
                ObjectKind::Candidate => report.candidates += 1,
                ObjectKind::Evidence => report.evidence += 1,
            }
            let object = izu_model::decode_object_metadata(
                reference.kind,
                &self.object(reference.id, reference.kind, cancel)?,
                self.limits(),
            )?;
            if let izu_model::MetadataObject::Tree(tree) = &object {
                source::validate_tree(tree)?;
            }
            if let izu_model::MetadataObject::Revision(revision) = &object
                && matches!(revision.origin, Some(izu_model::RevisionOrigin::Bootstrap))
                && !self.tree(revision.tree, cancel)?.entries.is_empty()
            {
                return Err(EngineError::InvalidInput(
                    "bootstrap revision must have an empty tree".into(),
                ));
            }
            let mut allocation = Ok(());
            object.visit_references(&mut |next| {
                if allocation.is_ok() {
                    allocation = pending.try_reserve(1).map_err(|_| EngineError::Allocation {
                        resource: "object closure",
                    });
                    if allocation.is_ok() {
                        pending.push(next);
                    }
                }
            });
            allocation?;
        }
        Ok(report)
    }

    pub fn verify(&self, cancel: &CancellationToken) -> Result<VerificationReport> {
        let root = self.current_operation(cancel)?;
        self.store.verify(cancel)?;
        let report = self.verify_graph(
            vec![izu_model::ObjectReference {
                id: root.object_id(),
                kind: ObjectKind::Operation,
            }],
            cancel,
        )?;
        self.validate_view_links(&self.view(cancel)?, cancel)?;
        Ok(report)
    }

    fn validate_view_links(&self, view: &RepositoryView, cancel: &CancellationToken) -> Result<()> {
        for (change, state) in &view.changes {
            for id in &state.heads {
                if self.revision(*id, cancel)?.change != *change {
                    return Err(EngineError::InvalidInput(
                        "change head belongs to another change".into(),
                    ));
                }
            }
        }
        for workspace in view.workspaces.values() {
            working_tree(workspace)?;
            self.revision(workspace.head, cancel)?;
            for record in workspace.sources.values() {
                if let Some(revision) = record.revision
                    && self.revision(revision, cancel)?.tree != record.tree
                {
                    return Err(EngineError::InvalidInput(
                        "source revision/tree mismatch".into(),
                    ));
                }
            }
        }
        for id in &view.candidates {
            let candidate: IntegrationCandidate =
                self.decode(id.object_id(), ObjectKind::Candidate, cancel)?;
            if self.revision(candidate.result, cancel)?.tree != candidate.result_tree {
                return Err(EngineError::EvidenceMismatch(
                    "candidate result revision/tree mismatch".into(),
                ));
            }
        }
        Ok(())
    }

    pub fn recover(&self, cancel: &CancellationToken) -> Result<RecoveryReport> {
        let report = self.store.recover(cancel)?;
        Ok(RecoveryReport {
            head: report.verified.head,
            verified_objects: report.verified.object_count,
            verified_bytes: report.verified.payload_bytes,
            removed_temporary_files: report.removed_temporary_files,
            unknown_temporary_entries: report.unknown_temporary_entries,
            retained_objects: report.retained_objects,
        })
    }

    pub fn prepare_merge(
        &self,
        target: RefName,
        expected_target: Option<RevisionId>,
        source_revision: RevisionId,
        checks: Vec<CheckSpec>,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<MergePreparation> {
        check_ref(&self.view(cancel)?, &target, expected_target)?;
        let theirs = self.revision(source_revision, cancel)?;
        let theirs_tree = self.tree(theirs.tree, cancel)?;
        reject_conflicts(&theirs_tree)?;
        let ours_tree = match expected_target {
            Some(id) => self.tree(self.revision(id, cancel)?.tree, cancel)?,
            None => Tree::default(),
        };
        reject_conflicts(&ours_tree)?;
        let base = match expected_target {
            Some(ours) => self.merge_base(ours, source_revision, cancel)?,
            None => None,
        };
        let base_tree = match base {
            Some(id) => self.tree(self.revision(id, cancel)?.tree, cancel)?,
            None => Tree::default(),
        };
        reject_conflicts(&base_tree)?;
        let (merged, conflicts) = self.merge_trees(&base_tree, &ours_tree, &theirs_tree, cancel)?;
        let tree = self.put_tree(&merged, cancel)?;
        let (candidate, revision) = self.prepare_candidate_internal(
            target,
            expected_target,
            vec![source_revision],
            tree,
            checks,
            author,
            new_change()?,
            None,
            cancel,
        )?;
        if conflicts.is_empty() {
            Ok(MergePreparation::Ready {
                candidate,
                revision,
                tree,
            })
        } else {
            Ok(MergePreparation::Conflicted {
                candidate,
                revision,
                tree,
                base,
                ours: expected_target,
                theirs: source_revision,
                conflicts,
            })
        }
    }

    /// Creates a candidate from an explicitly supplied immutable result tree.
    /// Required checks bind to this result; unresolved entries remain durable
    /// and cannot be checked, materialized, or landed.
    #[allow(clippy::too_many_arguments)]
    pub fn prepare_candidate(
        &self,
        target: RefName,
        expected_target: Option<RevisionId>,
        sources: Vec<RevisionId>,
        result_tree: TreeId,
        checks: Vec<CheckSpec>,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<CandidateId> {
        Ok(self
            .prepare_candidate_internal(
                target,
                expected_target,
                sources,
                result_tree,
                checks,
                author,
                new_change()?,
                None,
                cancel,
            )?
            .0)
    }

    #[allow(clippy::too_many_arguments)]
    fn prepare_candidate_internal(
        &self,
        target: RefName,
        expected_target: Option<RevisionId>,
        sources: Vec<RevisionId>,
        result_tree: TreeId,
        checks: Vec<CheckSpec>,
        author: Identity,
        change: ChangeId,
        supersedes: Option<RevisionId>,
        cancel: &CancellationToken,
    ) -> Result<(CandidateId, RevisionId)> {
        check_ref(&self.view(cancel)?, &target, expected_target)?;
        if sources.is_empty() {
            return Err(EngineError::InvalidInput(
                "candidate requires a source revision".into(),
            ));
        }
        self.tree(result_tree, cancel)?;
        let mut parents = Vec::new();
        parents
            .try_reserve(sources.len().saturating_add(1))
            .map_err(|_| EngineError::Allocation {
                resource: "candidate parents",
            })?;
        if let Some(id) = expected_target {
            self.revision(id, cancel)?;
            parents.push(id);
        }
        for id in &sources {
            self.revision(*id, cancel)?;
            if !parents.contains(id) {
                parents.push(*id);
            }
        }
        let revision = Revision {
            change,
            tree: result_tree,
            parents,
            description: format!("Integration candidate for {}", target.as_str()),
            author,
            created_at_unix_ms: timestamp()?,
            origin: None,
        };
        let result = self.put_revision(&revision, cancel)?;
        let candidate = IntegrationCandidate {
            target: target.clone(),
            expected_target,
            sources,
            result,
            result_tree,
            checks,
            created_at_unix_ms: timestamp()?,
        };
        let bytes = encode_metadata(&candidate, self.limits())?;
        let candidate_id =
            CandidateId::from_object(self.store.put(ObjectKind::Candidate, &bytes, cancel)?);
        self.mutate(
            format!("Prepare integration candidate for {}", target.as_str()),
            cancel,
            |view| {
                check_ref(view, &target, expected_target)?;
                match supersedes {
                    Some(previous) => {
                        let state = view
                            .changes
                            .get_mut(&change)
                            .ok_or_else(|| EngineError::UnknownRevision(change.to_string()))?;
                        if !state.heads.remove(&previous) {
                            return Err(EngineError::DivergentChange(change.to_string()));
                        }
                        state.heads.insert(result);
                    }
                    None => {
                        view.changes.insert(change, ChangeState::resolved(result));
                    }
                }
                view.candidates.insert(candidate_id);
                Ok(())
            },
        )?;
        Ok((candidate_id, result))
    }

    pub fn resolve_candidate(
        &self,
        id: CandidateId,
        resolutions: BTreeMap<RepoPath, Option<ResolvedTreeEntry>>,
        author: Identity,
        cancel: &CancellationToken,
    ) -> Result<MergePreparation> {
        if resolutions.is_empty() {
            return Err(EngineError::InvalidInput(
                "conflict resolution is empty".into(),
            ));
        }
        let candidate = self.candidate(id, cancel)?;
        let previous = self.revision(candidate.result, cancel)?;
        let mut tree = self.tree(candidate.result_tree, cancel)?;
        apply_resolutions(&mut tree, resolutions, self.limits().max_tree_entries)?;
        let tree_id = self.put_tree(&tree, cancel)?;
        let remaining = conflicts_in(&tree);
        let (new_candidate, revision) = self.prepare_candidate_internal(
            candidate.target,
            candidate.expected_target,
            candidate.sources.clone(),
            tree_id,
            candidate.checks,
            author,
            previous.change,
            Some(candidate.result),
            cancel,
        )?;
        if remaining.is_empty() {
            Ok(MergePreparation::Ready {
                candidate: new_candidate,
                revision,
                tree: tree_id,
            })
        } else {
            let theirs = candidate.sources.first().copied().ok_or_else(|| {
                EngineError::InvalidInput("candidate has no source revision".into())
            })?;
            Ok(MergePreparation::Conflicted {
                candidate: new_candidate,
                revision,
                tree: tree_id,
                base: None,
                ours: candidate.expected_target,
                theirs,
                conflicts: remaining,
            })
        }
    }

    pub fn start_check(
        &self,
        id: CandidateId,
        name: &str,
        cancel: &CancellationToken,
    ) -> Result<CheckAttemptReceipt> {
        let candidate = self.candidate(id, cancel)?;
        reject_conflicts(&self.tree(candidate.result_tree, cancel)?)?;
        let spec = candidate
            .checks
            .iter()
            .find(|check| check.name == name)
            .ok_or_else(|| EngineError::EvidenceMismatch("undeclared check".into()))?;
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|error| {
            EngineError::InvalidInput(format!("check attempt entropy: {error}"))
        })?;
        let attempt = CheckAttemptId::from_bytes(bytes);
        let pending = CheckEvidence {
            attempt,
            candidate: id,
            check: name.into(),
            inputs: CheckInputs {
                revision: candidate.result,
                tree: candidate.result_tree,
                environment: spec.environment,
            },
            outcome: CheckOutcome::Pending,
            argv: spec.argv.clone(),
            started_at_unix_ms: timestamp()?,
            finished_at_unix_ms: None,
        };
        let payload = encode_metadata(&pending, self.limits())?;
        let evidence =
            EvidenceId::from_object(self.store.put(ObjectKind::Evidence, &payload, cancel)?);
        let operation = self.mutate(
            format!("Start check {name} for candidate {id}"),
            cancel,
            |view| {
                if !view.candidates.contains(&id) {
                    return Err(EngineError::UnknownCandidate(id.to_string()));
                }
                self.replace_check_pointer(view, id, name, evidence, cancel)
            },
        )?;
        Ok(CheckAttemptReceipt {
            operation,
            attempt,
            evidence,
            pending,
        })
    }

    fn replace_check_pointer(
        &self,
        view: &mut RepositoryView,
        candidate: CandidateId,
        name: &str,
        next: EvidenceId,
        cancel: &CancellationToken,
    ) -> Result<()> {
        let current = view.evidence.entry(candidate).or_default();
        let mut superseded = Vec::new();
        for previous_id in current.iter().copied() {
            let previous = self.evidence(previous_id, cancel)?;
            if previous.check == name {
                superseded
                    .try_reserve(1)
                    .map_err(|_| EngineError::Allocation {
                        resource: "check pointer replacement",
                    })?;
                superseded.push(previous_id);
            }
        }
        for previous_id in superseded {
            current.remove(&previous_id);
        }
        current.insert(next);
        Ok(())
    }

    pub fn record_evidence(
        &self,
        evidence: &CheckEvidence,
        cancel: &CancellationToken,
    ) -> Result<EvidenceId> {
        if matches!(evidence.outcome, CheckOutcome::Pending) {
            return Err(EngineError::InvalidInput(
                "use start_check to publish a pending attempt".into(),
            ));
        }
        let candidate = self.candidate(evidence.candidate, cancel)?;
        reject_conflicts(&self.tree(candidate.result_tree, cancel)?)?;
        self.validate_evidence(&candidate, evidence)?;
        let bytes = encode_metadata(evidence, self.limits())?;
        let id = EvidenceId::from_object(self.store.put(ObjectKind::Evidence, &bytes, cancel)?);
        self.mutate(
            format!(
                "Record check {} for candidate {}",
                evidence.check, evidence.candidate
            ),
            cancel,
            |view| {
                if !view.candidates.contains(&evidence.candidate) {
                    return Err(EngineError::UnknownCandidate(
                        evidence.candidate.to_string(),
                    ));
                }
                let recorded = view
                    .evidence
                    .get(&evidence.candidate)
                    .ok_or_else(|| EngineError::MissingCheck(evidence.check.clone()))?;
                let mut active = None;
                for previous_id in recorded {
                    let previous = self.evidence(*previous_id, cancel)?;
                    if previous.check == evidence.check {
                        if active.is_some() {
                            return Err(EngineError::EvidenceMismatch(
                                "ambiguous active attempt".into(),
                            ));
                        }
                        active = Some((*previous_id, previous));
                    }
                }
                let (previous_id, pending) =
                    active.ok_or_else(|| EngineError::MissingCheck(evidence.check.clone()))?;
                if previous_id == id {
                    return Ok(());
                }
                if pending.attempt != evidence.attempt {
                    return Err(EngineError::StaleCheckAttempt {
                        current: pending.attempt,
                        received: evidence.attempt,
                    });
                }
                evidence.validate_for_pending_attempt(&pending)?;
                self.replace_check_pointer(view, evidence.candidate, &evidence.check, id, cancel)
            },
        )?;
        Ok(id)
    }

    fn validate_evidence(
        &self,
        candidate: &IntegrationCandidate,
        evidence: &CheckEvidence,
    ) -> Result<()> {
        let spec = candidate
            .checks
            .iter()
            .find(|spec| spec.name == evidence.check)
            .ok_or_else(|| EngineError::EvidenceMismatch("undeclared check".into()))?;
        let expected = CheckInputs {
            revision: candidate.result,
            tree: candidate.result_tree,
            environment: spec.environment,
        };
        if evidence.inputs != expected || evidence.argv != spec.argv {
            return Err(EngineError::EvidenceMismatch(
                "revision, tree, environment, or command differs".into(),
            ));
        }
        Ok(())
    }

    pub fn land(&self, id: CandidateId, cancel: &CancellationToken) -> Result<LandReceipt> {
        let candidate = self.candidate(id, cancel)?;
        reject_conflicts(&self.tree(candidate.result_tree, cancel)?)?;
        if candidate.checks.is_empty() {
            return Err(EngineError::NoRequiredChecks);
        }
        if self.revision(candidate.result, cancel)?.tree != candidate.result_tree {
            return Err(EngineError::EvidenceMismatch(
                "candidate result tree differs from revision".into(),
            ));
        }
        let mut selected_evidence = Vec::new();
        let operation = self.mutate(format!("Land candidate {id}"), cancel, |view| {
            if !view.candidates.contains(&id) {
                return Err(EngineError::UnknownCandidate(id.to_string()));
            }
            check_ref(view, &candidate.target, candidate.expected_target)?;
            let recorded = view
                .evidence
                .get(&id)
                .ok_or_else(|| EngineError::MissingCheck(candidate.checks[0].name.clone()))?;
            for spec in &candidate.checks {
                let mut latest: Option<(EvidenceId, CheckEvidence)> = None;
                for evidence_id in recorded {
                    let evidence = self.evidence(*evidence_id, cancel)?;
                    if evidence.candidate != id {
                        return Err(EngineError::EvidenceMismatch("candidate ID differs".into()));
                    }
                    self.validate_evidence(&candidate, &evidence)?;
                    if evidence.check != spec.name {
                        continue;
                    }
                    if latest.is_some() {
                        return Err(EngineError::EvidenceMismatch(format!(
                            "ambiguous current attempt for check {}",
                            spec.name
                        )));
                    }
                    latest = Some((*evidence_id, evidence));
                }
                let (evidence_id, evidence) =
                    latest.ok_or_else(|| EngineError::MissingCheck(spec.name.clone()))?;
                if !matches!(evidence.outcome, CheckOutcome::Passed) {
                    return Err(EngineError::MissingCheck(spec.name.clone()));
                }
                selected_evidence
                    .try_reserve(1)
                    .map_err(|_| EngineError::Allocation {
                        resource: "landing evidence",
                    })?;
                selected_evidence.push(evidence_id);
            }
            view.refs.insert(candidate.target.clone(), candidate.result);
            Ok(())
        })?;
        Ok(LandReceipt {
            operation,
            candidate: id,
            target: candidate.target,
            revision: candidate.result,
            tree: candidate.result_tree,
            evidence: selected_evidence,
        })
    }

    pub fn merge_base(
        &self,
        ours: RevisionId,
        theirs: RevisionId,
        cancel: &CancellationToken,
    ) -> Result<Option<RevisionId>> {
        let mut graph = BTreeMap::new();
        let mut pending = VecDeque::from([ours, theirs]);
        let mut bytes = 0;
        while let Some(id) = pending.pop_front() {
            source::check(cancel)?;
            if graph.contains_key(&id) {
                continue;
            }
            if graph.len() >= self.options.source_limits.max_history {
                return Err(EngineError::Limit {
                    resource: "merge ancestry",
                    limit: self.options.source_limits.max_history as u64,
                });
            }
            bytes = self.add_history_bytes(bytes, id.object_id(), cancel)?;
            let parents = self.revision(id, cancel)?.parents;
            pending
                .try_reserve(parents.len())
                .map_err(|_| EngineError::Allocation {
                    resource: "merge ancestry",
                })?;
            pending.extend(parents.iter().copied());
            graph.insert(id, parents);
        }
        let ours = ancestor_ids(&graph, [ours], cancel)?;
        let theirs = ancestor_ids(&graph, [theirs], cancel)?;
        let mut common: BTreeSet<_> = ours.intersection(&theirs).copied().collect();
        if common.is_empty() {
            return Ok(None);
        }
        let mut parents = Vec::new();
        for id in &common {
            let edges = graph
                .get(id)
                .ok_or_else(|| EngineError::UnknownRevision(id.to_string()))?;
            parents
                .try_reserve(edges.len())
                .map_err(|_| EngineError::Allocation {
                    resource: "merge base parents",
                })?;
            parents.extend(edges.iter().copied());
        }
        let dominated = ancestor_ids(&graph, parents, cancel)?;
        common.retain(|id| !dominated.contains(id));
        if common.len() > 1 {
            return Err(EngineError::Merge(
                "multiple merge bases require explicit base selection".into(),
            ));
        }
        Ok(common.into_iter().next())
    }

    fn merge_trees(
        &self,
        base: &Tree,
        ours: &Tree,
        theirs: &Tree,
        cancel: &CancellationToken,
    ) -> Result<(Tree, Vec<TreeConflict>)> {
        let paths: BTreeSet<_> = base
            .entries
            .keys()
            .chain(ours.entries.keys())
            .chain(theirs.entries.keys())
            .cloned()
            .collect();
        if paths.len() > self.options.source_limits.max_entries {
            return Err(EngineError::Limit {
                resource: "merged tree entries",
                limit: self.options.source_limits.max_entries as u64,
            });
        }
        let mut tree = Tree::default();
        for path in paths {
            source::check(cancel)?;
            let b = base.entries.get(&path);
            let o = ours.entries.get(&path);
            let t = theirs.entries.get(&path);
            let merged = if o == t {
                o.cloned()
            } else if o == b {
                t.cloned()
            } else if t == b {
                o.cloned()
            } else {
                Some(self.merge_entry(b, o, t, cancel)?)
            };
            if let Some(entry) = merged {
                tree.entries.insert(path, entry);
            }
        }
        // File/directory overlaps are a native path conflict, not an invalid
        // tree that loses either input. Source revisions retain every subtree.
        let snapshot = tree.entries.clone();
        for path in snapshot.keys() {
            let mut prefix = String::new();
            let components: Vec<_> = path.as_str().split('/').collect();
            for component in components.iter().take(components.len().saturating_sub(1)) {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(component);
                let ancestor = RepoPath::new(prefix.clone())?;
                if !matches!(
                    tree.entries.get(&ancestor),
                    Some(TreeEntry::Directory { .. } | TreeEntry::Conflict { .. })
                ) {
                    tree.entries.insert(
                        ancestor.clone(),
                        conflict_entry(
                            base.entries.get(&ancestor),
                            ours.entries.get(&ancestor),
                            theirs.entries.get(&ancestor),
                            ConflictReason::Path,
                        )?,
                    );
                }
            }
        }
        source::validate_tree(&tree)?;
        let conflicts = conflicts_in(&tree);
        Ok((tree, conflicts))
    }

    fn merge_entry(
        &self,
        base: Option<&TreeEntry>,
        ours: Option<&TreeEntry>,
        theirs: Option<&TreeEntry>,
        cancel: &CancellationToken,
    ) -> Result<TreeEntry> {
        let reason = match (ours, theirs) {
            (
                Some(TreeEntry::File { blob: o, mode: om }),
                Some(TreeEntry::File { blob: t, mode: tm }),
            ) => {
                let bm = match base {
                    Some(TreeEntry::File { mode, .. }) => Some(*mode),
                    None => None,
                    _ => return conflict_entry(base, ours, theirs, ConflictReason::Type),
                };
                let merged_mode = merge_scalar(bm, Some(*om), Some(*tm));
                let Some(merged_mode) = merged_mode.flatten() else {
                    return conflict_entry(base, ours, theirs, ConflictReason::Mode);
                };
                let base_bytes = match base {
                    Some(TreeEntry::File { blob, .. }) => Some(self.merge_blob(*blob, cancel)?),
                    _ => None,
                };
                let ours_bytes = self.merge_blob(*o, cancel)?;
                let theirs_bytes = self.merge_blob(*t, cancel)?;
                let options = izu_merge::Options {
                    cancellation: Some(cancel.as_atomic()),
                    ..izu_merge::Options::default()
                };
                match izu_merge::merge(
                    base_bytes.as_deref(),
                    Some(&ours_bytes),
                    Some(&theirs_bytes),
                    &options,
                )
                .map_err(|error| {
                    if cancel.is_cancelled() {
                        EngineError::Cancelled
                    } else {
                        EngineError::Merge(error.to_string())
                    }
                })? {
                    izu_merge::MergeResult::Clean(Some(bytes)) => {
                        let blob =
                            self.put_blob(&mut bytes.as_slice(), bytes.len() as u64, cancel)?;
                        return Ok(TreeEntry::File {
                            blob,
                            mode: merged_mode,
                        });
                    }
                    izu_merge::MergeResult::Clean(None) => {
                        return conflict_entry(base, ours, theirs, ConflictReason::DeleteModify);
                    }
                    izu_merge::MergeResult::Conflicted { chunks } => {
                        if chunks.iter().any(|chunk| {
                            matches!(
                                chunk,
                                izu_merge::MergeChunk::Conflict {
                                    kind: izu_merge::ConflictKind::Binary,
                                    ..
                                }
                            )
                        }) {
                            ConflictReason::Binary
                        } else if base.is_none() {
                            ConflictReason::AddAdd
                        } else {
                            ConflictReason::Content
                        }
                    }
                }
            }
            (
                Some(TreeEntry::Directory { mode: ours_mode }),
                Some(TreeEntry::Directory { mode: theirs_mode }),
            ) => {
                let base_mode = match base {
                    Some(TreeEntry::Directory { mode }) => Some(*mode),
                    None => None,
                    _ => return conflict_entry(base, ours, theirs, ConflictReason::Type),
                };
                if let Some(Some(mode)) =
                    merge_scalar(base_mode, Some(*ours_mode), Some(*theirs_mode))
                {
                    return Ok(TreeEntry::Directory { mode });
                }
                ConflictReason::Mode
            }
            (Some(TreeEntry::Symlink { .. }), Some(TreeEntry::Symlink { .. })) => {
                ConflictReason::Content
            }
            (None, _) | (_, None) => ConflictReason::DeleteModify,
            _ => ConflictReason::Type,
        };
        conflict_entry(base, ours, theirs, reason)
    }

    fn merge_blob(&self, id: ObjectId, cancel: &CancellationToken) -> Result<Vec<u8>> {
        let info = self.object_info(id, cancel)?;
        if info.payload_len > self.options.source_limits.max_merge_bytes {
            return Err(EngineError::Limit {
                resource: "merge blob bytes",
                limit: self.options.source_limits.max_merge_bytes,
            });
        }
        self.blob(id, cancel)
    }
}

fn merge_scalar<T: Copy + PartialEq>(
    base: Option<T>,
    ours: Option<T>,
    theirs: Option<T>,
) -> Option<Option<T>> {
    if ours == theirs {
        Some(ours)
    } else if ours == base {
        Some(theirs)
    } else if theirs == base {
        Some(ours)
    } else {
        None
    }
}

fn ancestor_ids(
    graph: &BTreeMap<RevisionId, Vec<RevisionId>>,
    starts: impl IntoIterator<Item = RevisionId>,
    cancel: &CancellationToken,
) -> Result<BTreeSet<RevisionId>> {
    let mut pending = VecDeque::new();
    for id in starts {
        pending
            .try_reserve(1)
            .map_err(|_| EngineError::Allocation {
                resource: "merge ancestry frontier",
            })?;
        pending.push_back(id);
    }
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop_front() {
        source::check(cancel)?;
        if !seen.insert(id) {
            continue;
        }
        let parents = graph
            .get(&id)
            .ok_or_else(|| EngineError::UnknownRevision(id.to_string()))?;
        pending
            .try_reserve(parents.len())
            .map_err(|_| EngineError::Allocation {
                resource: "merge ancestry frontier",
            })?;
        pending.extend(parents.iter().copied());
    }
    Ok(seen)
}

fn read_intent(
    directory: &Directory,
    name: &OsStr,
    id: WorkspaceId,
) -> Result<Option<WriterIntent>> {
    let mut file = match directory.open_read(name) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io(Path::new(name), error)),
    };
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(4096)
        .map_err(|_| EngineError::Allocation {
            resource: "managed writer intent",
        })?;
    Read::by_ref(&mut file)
        .take(4097)
        .read_to_end(&mut bytes)
        .map_err(|error| io(Path::new(name), error))?;
    if bytes.len() > 4096 {
        return Err(EngineError::Limit {
            resource: "managed writer intent bytes",
            limit: 4096,
        });
    }
    let intent: WriterIntent = serde_json::from_slice(&bytes)
        .map_err(|error| EngineError::InvalidInput(format!("corrupt writer intent: {error}")))?;
    if intent.workspace != id {
        return Err(EngineError::InvalidInput(
            "writer intent workspace differs".into(),
        ));
    }
    Ok(Some(intent))
}

fn to_resolved(entry: &TreeEntry) -> Result<ResolvedTreeEntry> {
    match entry {
        TreeEntry::File { blob, mode } => Ok(ResolvedTreeEntry::File {
            blob: *blob,
            mode: *mode,
        }),
        TreeEntry::Symlink { target } => Ok(ResolvedTreeEntry::Symlink {
            target: target.clone(),
        }),
        TreeEntry::Directory { mode } => Ok(ResolvedTreeEntry::Directory { mode: *mode }),
        TreeEntry::Conflict { .. } => Err(EngineError::Merge(
            "resolve earlier conflicts before merging again".into(),
        )),
    }
}

fn from_resolved(entry: ResolvedTreeEntry) -> TreeEntry {
    match entry {
        ResolvedTreeEntry::File { blob, mode } => TreeEntry::File { blob, mode },
        ResolvedTreeEntry::Symlink { target } => TreeEntry::Symlink { target },
        ResolvedTreeEntry::Directory { mode } => TreeEntry::Directory { mode },
    }
}

fn apply_resolutions(
    tree: &mut Tree,
    resolutions: BTreeMap<RepoPath, Option<ResolvedTreeEntry>>,
    limit: usize,
) -> Result<()> {
    if resolutions.len() > limit {
        return Err(EngineError::Limit {
            resource: "conflict resolutions",
            limit: limit as u64,
        });
    }
    let mut conflicts = BTreeSet::new();
    for (path, entry) in &tree.entries {
        if matches!(entry, TreeEntry::Conflict { .. }) {
            let mut scope = String::new();
            scope
                .try_reserve_exact(path.as_str().len())
                .map_err(|_| EngineError::Allocation {
                    resource: "conflict resolution scope",
                })?;
            scope.push_str(path.as_str());
            conflicts.insert(scope);
        }
    }
    for (path, resolution) in resolutions {
        let in_scope =
            conflicts.contains(path.as_str())
                || path.as_str().bytes().enumerate().any(|(index, byte)| {
                    byte == b'/' && conflicts.contains(&path.as_str()[..index])
                });
        if !in_scope {
            return Err(EngineError::InvalidInput(format!(
                "path is not an unresolved conflict or descendant: {}",
                path.as_str()
            )));
        }
        match resolution {
            Some(entry) => {
                tree.entries.insert(path, from_resolved(entry));
            }
            None => {
                tree.entries.remove(&path);
            }
        }
    }
    Ok(())
}

fn conflict_entry(
    base: Option<&TreeEntry>,
    ours: Option<&TreeEntry>,
    theirs: Option<&TreeEntry>,
    reason: ConflictReason,
) -> Result<TreeEntry> {
    Ok(TreeEntry::Conflict {
        base: base.map(to_resolved).transpose()?,
        ours: ours.map(to_resolved).transpose()?,
        theirs: theirs.map(to_resolved).transpose()?,
        reason,
    })
}

fn conflicts_in(tree: &Tree) -> Vec<TreeConflict> {
    tree.entries
        .iter()
        .filter_map(|(path, entry)| {
            if let TreeEntry::Conflict {
                base,
                ours,
                theirs,
                reason,
            } = entry
            {
                Some(TreeConflict {
                    path: path.clone(),
                    reason: format!("{reason:?}"),
                    base: base.clone().map(from_resolved),
                    ours: ours.clone().map(from_resolved),
                    theirs: theirs.clone().map(from_resolved),
                })
            } else {
                None
            }
        })
        .collect()
}

fn publish(outcome: PublishOutcome) -> Result<OperationId> {
    match outcome {
        PublishOutcome::Durable(receipt) => Ok(receipt.current),
        PublishOutcome::VisibleButUncertain { receipt, error } => {
            Err(EngineError::PublicationUncertain {
                operation: receipt.current,
                reason: error.to_string(),
            })
        }
    }
}

fn validate_source_limits(limits: &SourceLimits) -> Result<()> {
    if limits.max_entries == 0
        || limits.max_depth == 0
        || limits.max_blob_bytes == 0
        || limits.max_total_bytes == 0
        || limits.max_ignore_bytes == 0
        || limits.max_history == 0
        || limits.max_history_bytes == 0
        || limits.max_merge_bytes == 0
    {
        return Err(EngineError::InvalidInput(
            "source limits must be positive".into(),
        ));
    }
    if limits.max_depth > 512 {
        return Err(EngineError::Limit {
            resource: "configured traversal depth",
            limit: 512,
        });
    }
    Ok(())
}

pub(crate) fn tree_diff(before: &Tree, after: &Tree) -> Result<Vec<PathChange>> {
    let paths: BTreeSet<_> = before.entries.keys().chain(after.entries.keys()).collect();
    let mut result = Vec::new();
    result
        .try_reserve(paths.len())
        .map_err(|_| EngineError::Allocation {
            resource: "tree diff",
        })?;
    for path in paths {
        let old = before.entries.get(path);
        let new = after.entries.get(path);
        if old == new {
            continue;
        }
        let kind = match (old, new) {
            (None, _) => PathChangeKind::Added,
            (_, None) => PathChangeKind::Deleted,
            (Some(old), Some(new))
                if std::mem::discriminant(old) != std::mem::discriminant(new) =>
            {
                PathChangeKind::TypeChanged
            }
            _ => PathChangeKind::Modified,
        };
        result.push(PathChange {
            path: path.clone(),
            kind,
            before: old.cloned(),
            after: new.cloned(),
        });
    }
    Ok(result)
}

fn reject_conflicts(tree: &Tree) -> Result<()> {
    if let Some((path, _)) = tree
        .entries
        .iter()
        .find(|(_, entry)| matches!(entry, TreeEntry::Conflict { .. }))
    {
        return Err(EngineError::InvalidInput(format!(
            "unresolved source conflict at {}",
            path.as_str()
        )));
    }
    Ok(())
}
