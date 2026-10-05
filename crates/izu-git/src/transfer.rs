use crate::objects;
use crate::source::{self, Cache, Stage};
use crate::tool::GitTool;
use crate::{
    Error, GitInventory, GitObjectId, GitSource, GitToolConfig, Result, native_error,
    validate_git_ref,
};
use izu_engine::{RefUpdate, Repository, RepositoryOptions, Selection};
use izu_model::{
    CancellationToken, ChangeId, FileMode, Identity, OperationId, RefName, RepoPath, Revision,
    RevisionId, RevisionOrigin, SymlinkTarget, Tree, TreeEntry, TreeId,
};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Cursor, Write};
use std::path::Path;

#[derive(Clone, Debug, Default)]
pub struct ImportOptions {
    /// Empty means import all ordinary source branches into identically named
    /// new native refs. Existing native refs require an explicit expected ID.
    pub refs: Vec<RefImport>,
}
#[derive(Clone, Debug)]
pub struct RefImport {
    pub git_ref: String,
    pub native_ref: RefName,
    pub expected: Option<RevisionId>,
}
#[derive(Clone, Debug, Default)]
pub struct ExportOptions {
    /// Empty means export all native refs under refs/heads/.
    pub refs: Vec<RefExport>,
    pub committer: Option<GitSignature>,
}
#[derive(Clone, Debug)]
pub struct RefExport {
    pub native_ref: RefName,
    pub git_ref: String,
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitSignature {
    pub name: String,
    pub email: String,
    pub timestamp_unix_seconds: i64,
    pub timezone_minutes: i16,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NonFastForwardPolicy {
    Reject,
    ExplicitAllow,
}
#[derive(Clone, Debug)]
pub struct PushRequest {
    pub remote: GitSource,
    pub native_ref: RefName,
    pub git_ref: String,
    /// None explicitly leases an absent remote ref. No tracking ref is inferred.
    pub expected_old: Option<GitObjectId>,
    pub non_fast_forward: NonFastForwardPolicy,
    pub committer: Option<GitSignature>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ImportReport {
    pub operation: OperationId,
    pub inventory: GitInventory,
    pub revision_mapping: BTreeMap<GitObjectId, RevisionId>,
    pub imported_refs: BTreeMap<String, RevisionId>,
}
#[derive(Clone, Debug, Serialize)]
pub struct ExportReport {
    pub refs: BTreeMap<String, GitObjectId>,
    pub revision_mapping: BTreeMap<RevisionId, GitObjectId>,
    pub warnings: Vec<String>,
}
#[derive(Clone, Debug, Serialize)]
pub struct PushReport {
    pub git_ref: String,
    pub previous: Option<GitObjectId>,
    pub published: GitObjectId,
    pub verified: GitObjectId,
    pub warnings: Vec<String>,
}

pub struct GitAdapter {
    tool: GitTool,
}

impl GitAdapter {
    pub fn new(config: GitToolConfig) -> Result<Self> {
        Ok(Self {
            tool: GitTool::new(config)?,
        })
    }
    pub fn available(&self, cancel: &CancellationToken) -> Result<String> {
        let bytes = self
            .tool
            .run(
                None,
                "query installed Git",
                &GitTool::args(&["--version"]),
                &[],
                4096,
                cancel,
            )?
            .stdout;
        let version = String::from_utf8(bytes)
            .map_err(|_| Error::InvalidSource("non-UTF8 Git version".into()))?;
        if !version.starts_with("git version ") {
            return Err(Error::InvalidSource(
                "configured executable is not an installed Git tool".into(),
            ));
        }
        Ok(version.trim().into())
    }
    pub fn inventory(
        &self,
        source: &GitSource,
        cancel: &CancellationToken,
    ) -> Result<GitInventory> {
        Ok(source::stage(&self.tool, source, cancel)?.inventory)
    }
    pub fn inventory_with_options(
        &self,
        source: &GitSource,
        options: &ImportOptions,
        cancel: &CancellationToken,
    ) -> Result<GitInventory> {
        Ok(self.stage_source(source, options, cancel)?.inventory)
    }
    fn stage_source(
        &self,
        source: &GitSource,
        options: &ImportOptions,
        cancel: &CancellationToken,
    ) -> Result<Stage> {
        let selected = if options.refs.is_empty() {
            None
        } else {
            Some(
                options
                    .refs
                    .iter()
                    .map(|selection| selection.git_ref.clone())
                    .collect::<BTreeSet<_>>(),
            )
        };
        source::stage_selected(&self.tool, source, selected.as_ref(), cancel)
    }
    pub fn import_into(
        &self,
        repository: &Repository,
        source: &GitSource,
        options: &ImportOptions,
        cancel: &CancellationToken,
    ) -> Result<ImportReport> {
        let stage = self.stage_source(source, options, cancel)?;
        self.import_stage(repository, stage, options, &[], cancel)
    }
    fn import_stage(
        &self,
        repository: &Repository,
        stage: Stage,
        options: &ImportOptions,
        extra_refs: &[RefUpdate],
        cancel: &CancellationToken,
    ) -> Result<ImportReport> {
        if !stage.inventory.unsupported.is_empty() {
            return Err(Error::Unsupported(stage.inventory.unsupported));
        }
        let selections = if options.refs.is_empty() {
            stage
                .inventory
                .branches
                .keys()
                .map(|git_ref| {
                    let name = git_ref
                        .strip_prefix("refs/heads/")
                        .ok_or_else(|| Error::InvalidRef(git_ref.clone()))?;
                    Ok(RefImport {
                        git_ref: git_ref.clone(),
                        native_ref: RefName::new(name)
                            .map_err(|error| native_error("validate imported reference", error))?,
                        expected: None,
                    })
                })
                .collect::<Result<Vec<_>>>()?
        } else {
            options.refs.clone()
        };
        let mut names = BTreeSet::new();
        for selection in &selections {
            validate_git_ref(&selection.git_ref)?;
            if !stage.inventory.branches.contains_key(&selection.git_ref) {
                return Err(Error::InvalidRef(selection.git_ref.clone()));
            }
            if !names.insert(selection.native_ref.clone()) {
                return Err(Error::InvalidRef("duplicate native import target".into()));
            }
        }
        let mut blobs = BTreeMap::new();
        for (id, bytes) in &stage.blobs {
            let blob = repository
                .put_blob(&mut Cursor::new(bytes), bytes.len() as u64, cancel)
                .map_err(|error| native_error("import Git blob", error))?;
            blobs.insert(*id, blob);
        }
        let mut trees = BTreeMap::<GitObjectId, TreeId>::new();
        let mut revisions = BTreeMap::new();
        for id in &stage.order {
            let commit = stage
                .commits
                .get(id)
                .ok_or_else(|| Error::InvalidSource("staged commit missing".into()))?;
            let tree = if let Some(tree) = trees.get(&commit.tree) {
                *tree
            } else {
                let mut entries = BTreeMap::new();
                flatten_tree(
                    &stage,
                    commit.tree,
                    "",
                    &blobs,
                    &mut entries,
                    &self.tool.config.limits,
                    0,
                )?;
                let tree = repository
                    .put_tree(&Tree { entries }, cancel)
                    .map_err(|error| native_error("import Git tree", error))?;
                trees.insert(commit.tree, tree);
                tree
            };
            let parents = commit
                .parents
                .iter()
                .map(|parent| {
                    revisions
                        .get(parent)
                        .copied()
                        .ok_or_else(|| Error::InvalidSource("parent has not been imported".into()))
                })
                .collect::<Result<Vec<_>>>()?;
            let raw = repository
                .put_blob(
                    &mut Cursor::new(&commit.raw),
                    commit.raw.len() as u64,
                    cancel,
                )
                .map_err(|error| native_error("preserve original Git commit", error))?;
            let mut digest = Sha256::new();
            digest.update(b"izu.git.change.v1\0");
            digest.update(id.as_bytes());
            let full_digest = digest.finalize();
            let mut change_bytes = [0_u8; 16];
            change_bytes.copy_from_slice(&full_digest[..16]);
            let change = ChangeId::from_bytes(change_bytes);
            let revision = Revision {
                change,
                tree,
                parents,
                description: commit.message.clone(),
                author: Identity {
                    name: commit.author_name.clone(),
                    email: commit.author_email.clone(),
                },
                created_at_unix_ms: commit
                    .author_seconds
                    .checked_mul(1000)
                    .ok_or(Error::Limit("Git author timestamp"))?,
                origin: Some(RevisionOrigin::Git {
                    object_id: id.to_string(),
                    raw_commit: raw,
                }),
            };
            let revision = repository
                .put_revision(&revision, cancel)
                .map_err(|error| native_error("import Git revision", error))?;
            revisions.insert(*id, revision);
        }
        let mut updates = selections
            .iter()
            .map(|selection| {
                let git_id = stage
                    .inventory
                    .branches
                    .get(&selection.git_ref)
                    .ok_or_else(|| Error::InvalidRef(selection.git_ref.clone()))?;
                let native = revisions.get(git_id).copied().ok_or_else(|| {
                    Error::InvalidSource("source ref has no imported revision".into())
                })?;
                Ok(RefUpdate {
                    name: selection.native_ref.clone(),
                    expected: selection.expected,
                    new: Some(native),
                })
            })
            .collect::<Result<Vec<_>>>()?;
        updates.extend_from_slice(extra_refs);
        let imported_refs = updates
            .iter()
            .filter_map(|update| update.new.map(|id| (update.name.as_str().to_owned(), id)))
            .collect();
        let imported: Vec<_> = revisions.values().copied().collect();
        let operation = repository
            .import_revisions(&imported, &updates, cancel)
            .map_err(|error| native_error("publish imported native history", error))?;
        Ok(ImportReport {
            operation,
            inventory: stage.inventory,
            revision_mapping: revisions,
            imported_refs,
        })
    }
    pub fn fetch_into(
        &self,
        repository: &Repository,
        source: &GitSource,
        options: &ImportOptions,
        cancel: &CancellationToken,
    ) -> Result<ImportReport> {
        self.import_into(repository, source, options, cancel)
    }
    pub fn clone_into(
        &self,
        destination: &Path,
        source: &GitSource,
        options: &ImportOptions,
        cancel: &CancellationToken,
    ) -> Result<(Repository, ImportReport)> {
        // Complete capability inventory before creating any native repository;
        // use the same verified snapshot when publishing its native references.
        let stage = self.stage_source(source, options, cancel)?;
        if !stage.inventory.unsupported.is_empty() {
            return Err(Error::Unsupported(stage.inventory.unsupported));
        }
        if !stage.inventory.branches.is_empty() {
            let head = stage.inventory.symbolic_head.as_ref().ok_or_else(|| {
                Error::Unsupported(vec!["cloning a detached or unadvertised Git HEAD".into()])
            })?;
            if !stage.inventory.branches.contains_key(head) {
                return Err(Error::InvalidSource(
                    "source default branch is unborn while other branches exist".into(),
                ));
            }
            if !options.refs.is_empty()
                && !options
                    .refs
                    .iter()
                    .any(|selection| &selection.git_ref == head)
            {
                return Err(Error::InvalidSource(
                    "clone selections do not include the source default branch".into(),
                ));
            }
        }
        if destination.exists()
            && fs::read_dir(destination)
                .map_err(|source| Error::Io {
                    action: "inspect clone destination",
                    source,
                })?
                .next()
                .is_some()
        {
            return Err(Error::InvalidSource(
                "clone destination must be new or empty".into(),
            ));
        }
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        if !destination.exists() {
            fs::create_dir(destination).map_err(|source| Error::Io {
                action: "create explicit clone destination",
                source,
            })?;
        }
        let repository = Repository::init(destination, RepositoryOptions::default())
            .map_err(|error| native_error("initialize clone destination", error))?;
        let main = RefName::new("main")
            .map_err(|error| native_error("validate clone anchor name", error))?;
        let view = repository
            .view(cancel)
            .map_err(|error| native_error("read owned clone initialization", error))?;
        let anchor = view.refs.get(&main).copied().ok_or_else(|| {
            Error::InvalidSource("owned clone initialization has no main anchor".into())
        })?;
        let revision = repository
            .revision(anchor, cancel)
            .map_err(|error| native_error("read owned clone anchor", error))?;
        if !matches!(revision.origin, Some(RevisionOrigin::Bootstrap)) {
            return Err(Error::InvalidSource(
                "owned clone anchor is not internal initialization".into(),
            ));
        }
        let mut clone_options = if options.refs.is_empty() {
            ImportOptions {
                refs: stage
                    .inventory
                    .branches
                    .keys()
                    .map(|name| {
                        let native = name
                            .strip_prefix("refs/heads/")
                            .ok_or_else(|| Error::InvalidRef(name.clone()))?;
                        Ok(RefImport {
                            git_ref: name.clone(),
                            native_ref: RefName::new(native)
                                .map_err(|error| native_error("validate clone branch", error))?,
                            expected: None,
                        })
                    })
                    .collect::<Result<Vec<_>>>()?,
            }
        } else {
            options.clone()
        };
        let mut replaced_main = false;
        for selection in &mut clone_options.refs {
            if selection.native_ref == main {
                if selection.expected.is_some() {
                    return Err(Error::InvalidSource(
                        "new clone cannot lease pre-existing native source history".into(),
                    ));
                }
                selection.expected = Some(anchor);
                replaced_main = true;
            }
        }
        let extra = if replaced_main {
            Vec::new()
        } else {
            vec![RefUpdate {
                name: main,
                expected: Some(anchor),
                new: None,
            }]
        };
        let checkout_git = if stage.inventory.branches.is_empty() {
            None
        } else {
            let head = stage.inventory.symbolic_head.as_ref().ok_or_else(|| {
                Error::Unsupported(vec!["cloning a detached or unadvertised Git HEAD".into()])
            })?;
            if !clone_options
                .refs
                .iter()
                .any(|selection| &selection.git_ref == head)
            {
                return Err(Error::InvalidSource(
                    "clone selections do not include the source default branch".into(),
                ));
            }
            Some(*stage.inventory.branches.get(head).ok_or_else(|| {
                Error::InvalidSource(
                    "source default branch is unborn while other source branches exist".into(),
                )
            })?)
        };
        let report = self.import_stage(&repository, stage, &clone_options, &extra, cancel)?;
        if let Some(git_id) = checkout_git {
            let revision = report
                .revision_mapping
                .get(&git_id)
                .copied()
                .ok_or_else(|| {
                    Error::InvalidSource("clone default branch has no native revision".into())
                })?;
            let workspace = repository
                .workspace(repository.workspace_id(), cancel)
                .map_err(|error| native_error("read clone workspace expectation", error))?;
            repository
                .restore(
                    workspace.id,
                    workspace.expected,
                    revision,
                    Selection::All,
                    cancel,
                )
                .map_err(|error| native_error("materialize cloned source", error))?;
        }
        Ok((repository, report))
    }
    pub fn export_to(
        &self,
        repository: &Repository,
        target: &Path,
        options: &ExportOptions,
        cancel: &CancellationToken,
    ) -> Result<ExportReport> {
        let prepared = self.prepare_export(repository, options, cancel)?;
        if target.exists()
            && fs::read_dir(target)
                .map_err(|source| Error::Io {
                    action: "inspect export target",
                    source,
                })?
                .next()
                .is_some()
        {
            return Err(Error::InvalidSource(
                "export target must be new or empty".into(),
            ));
        }
        let arguments = vec![
            OsString::from("init"),
            OsString::from("--bare"),
            OsString::from("--template="),
            OsString::from("--initial-branch=main"),
            target.as_os_str().to_owned(),
        ];
        self.tool.run(
            None,
            "initialize explicit export target",
            &arguments,
            &[],
            16 * 1024,
            cancel,
        )?;
        let target = source::local_git_dir(target)?;
        copy_objects(
            prepared.cache.path(),
            &target,
            &self.tool.config.limits,
            cancel,
        )?;
        verify_git_closure(
            &self.tool,
            &target,
            prepared.report.refs.values().copied(),
            cancel,
        )?;
        publish_local_refs(
            &self.tool,
            &target,
            &prepared.report.refs,
            &BTreeMap::new(),
            cancel,
        )?;
        // HEAD is cosmetic and only set inside this newly created target.
        if let Some(name) = prepared.report.refs.keys().next() {
            self.tool.run(
                Some(&target),
                "set exported symbolic HEAD",
                &GitTool::args(&["symbolic-ref", "HEAD", name]),
                &[],
                4096,
                cancel,
            )?;
        }
        let (actual, _, unsupported) = source::local_refs(&target, &self.tool.config.limits)?;
        if !unsupported.is_empty() || actual != prepared.report.refs {
            return Err(Error::PublicationUncertain(
                "exported refs differ from the intended refs".into(),
            ));
        }
        Ok(prepared.report)
    }
    pub fn push_ref(
        &self,
        repository: &Repository,
        request: &PushRequest,
        cancel: &CancellationToken,
    ) -> Result<PushReport> {
        self.push_ref_inner(repository, request, cancel, || {})
    }
    fn push_ref_inner(
        &self,
        repository: &Repository,
        request: &PushRequest,
        cancel: &CancellationToken,
        before_publication: impl FnOnce(),
    ) -> Result<PushReport> {
        validate_git_ref(&request.git_ref)?;
        if !request.git_ref.starts_with("refs/heads/") {
            return Err(Error::Unsupported(vec![
                "publication only supports ordinary branch refs".into(),
            ]));
        }
        let observed = source::target_ref(&self.tool, &request.remote, &request.git_ref, cancel)?;
        if observed != request.expected_old {
            return Err(Error::LeaseMismatch {
                expected: request.expected_old,
                observed,
            });
        }
        let options = ExportOptions {
            refs: vec![RefExport {
                native_ref: request.native_ref.clone(),
                git_ref: request.git_ref.clone(),
            }],
            committer: request.committer.clone(),
        };
        let prepared = self.prepare_export(repository, &options, cancel)?;
        let published = prepared
            .report
            .refs
            .get(&request.git_ref)
            .copied()
            .ok_or_else(|| Error::InvalidRef(request.native_ref.as_str().into()))?;
        if let Some(old) = observed {
            ensure_remote_object(
                &self.tool,
                prepared.cache.path(),
                &request.remote,
                &request.git_ref,
                old,
                cancel,
            )?;
            if request.non_fast_forward == NonFastForwardPolicy::Reject
                && !is_ancestor(&self.tool, prepared.cache.path(), old, published, cancel)?
            {
                return Err(Error::NonFastForward);
            }
        }
        before_publication();
        match &request.remote {
            GitSource::Local(path) => {
                let target = source::bare_target(path, &self.tool.config.limits)?;
                let (_, _, unsupported) = source::local_refs(&target, &self.tool.config.limits)?;
                if !unsupported.is_empty() {
                    return Err(Error::Unsupported(unsupported));
                }
                verify_existing_target_objects(&self.tool, &target, &prepared.objects, cancel)?;
            }
            GitSource::Https(_) => {}
        }
        let lease = format!(
            "--force-with-lease={}:{}",
            request.git_ref,
            observed.map(|id| id.to_string()).unwrap_or_default()
        );
        let refspec = format!("{published}:{}", request.git_ref);
        let arguments = vec![
            OsString::from("push"),
            OsString::from("--porcelain"),
            OsString::from("--no-verify"),
            OsString::from(lease),
            OsString::from("--"),
            source::source_argument(&request.remote)?,
            OsString::from(refspec),
        ];
        let publication = match &request.remote {
            GitSource::Local(_) => self
                .tool
                .run_publication(prepared.cache.path(), &arguments, cancel)
                .map_err(|error| (Box::new(error), false))
                .and_then(|output| {
                    if output.status.success() {
                        return Ok(());
                    }
                    let definite = definite_push_rejection(&output.stdout, &request.git_ref);
                    Err((
                        Box::new(Error::CommandFailed {
                            operation: "publish explicit leased remote ref".into(),
                            code: output.status.code(),
                            stderr: output.stderr,
                        }),
                        definite,
                    ))
                }),
            GitSource::Https(url) => crate::wire::push(
                &self.tool,
                url,
                prepared.cache.path(),
                &request.git_ref,
                observed,
                published,
                cancel,
            ),
        };
        if let Err((error, definite_rejection)) = publication {
            let error = *error;
            let readback = source::target_ref(
                &self.tool,
                &request.remote,
                &request.git_ref,
                &CancellationToken::new(),
            );
            if !definite_rejection {
                return Err(Error::PublicationUncertain(format!(
                    "target {:?}, ref {}, expected {observed:?}, attempted {published}, readback {readback:?}: {error}",
                    request.remote, request.git_ref
                )));
            }
            return match readback {
                Ok(Some(actual)) if actual == published => {
                    Err(Error::PublicationUncertain(format!(
                        "rejected target {:?}, ref {}, expected {observed:?}, attempted {published}, readback {actual}: {error}",
                        request.remote, request.git_ref
                    )))
                }
                Ok(actual) if actual != observed => Err(Error::LeaseMismatch {
                    expected: observed,
                    observed: actual,
                }),
                Ok(_) => Err(error),
                Err(read_error) => Err(Error::PublicationUncertain(format!(
                    "target {:?}, ref {}, expected {observed:?}, attempted {published}: rejection {error}; readback {read_error}",
                    request.remote, request.git_ref
                ))),
            };
        }
        let actual = source::target_ref(&self.tool, &request.remote, &request.git_ref, cancel)
            .map_err(|error| Error::PublicationUncertain(error.to_string()))?;
        if actual != Some(published) {
            return Err(Error::PublicationUncertain(format!(
                "target is now {actual:?}, intended {published}"
            )));
        }
        if let GitSource::Local(path) = &request.remote {
            let target = source::bare_target(path, &self.tool.config.limits)
                .map_err(|error| Error::PublicationUncertain(error.to_string()))?;
            verify_local_target_closure(&self.tool, &target, [published], cancel)
                .map_err(|error| Error::PublicationUncertain(error.to_string()))?;
        }
        Ok(PushReport {
            git_ref: request.git_ref.clone(),
            previous: observed,
            published,
            verified: published,
            warnings: prepared.report.warnings,
        })
    }
    fn prepare_export(
        &self,
        repository: &Repository,
        options: &ExportOptions,
        cancel: &CancellationToken,
    ) -> Result<Prepared> {
        let view = repository
            .view(cancel)
            .map_err(|error| native_error("read native references", error))?;
        let selections = if options.refs.is_empty() {
            view.refs
                .keys()
                .map(|name| RefExport {
                    native_ref: name.clone(),
                    git_ref: format!("refs/heads/{}", name.as_str()),
                })
                .collect()
        } else {
            options.refs.clone()
        };
        let mut targets = BTreeSet::new();
        let mut roots = Vec::new();
        for selection in &selections {
            validate_git_ref(&selection.git_ref)?;
            if !selection.git_ref.starts_with("refs/heads/") {
                return Err(Error::Unsupported(vec![
                    "export only supports ordinary branch refs".into(),
                ]));
            }
            if !targets.insert(selection.git_ref.clone()) {
                return Err(Error::InvalidRef("duplicate Git export target".into()));
            }
            let revision = view
                .refs
                .get(&selection.native_ref)
                .copied()
                .ok_or_else(|| Error::InvalidRef(selection.native_ref.as_str().into()))?;
            roots.push((selection.git_ref.clone(), revision));
        }
        let cache = Cache::new(&self.tool, cancel)?;
        let mut builder = ExportBuilder {
            adapter: self,
            repository,
            cache: cache.path(),
            cancel,
            options,
            revisions: BTreeMap::new(),
            trees: BTreeMap::new(),
            warnings: BTreeSet::new(),
            total_bytes: 0,
            objects: 0,
            object_kinds: BTreeMap::new(),
        };
        for (_, root) in &roots {
            builder.revision(*root)?;
        }
        let refs = roots
            .iter()
            .map(|(name, root)| {
                builder
                    .revisions
                    .get(root)
                    .copied()
                    .map(|id| (name.clone(), id))
                    .ok_or_else(|| Error::InvalidSource("exported revision missing".into()))
            })
            .collect::<Result<_>>()?;
        let report = ExportReport {
            refs,
            revision_mapping: builder.revisions.clone(),
            warnings: builder.warnings.iter().cloned().collect(),
        };
        let objects = builder.object_kinds.clone();
        Ok(Prepared {
            cache,
            report,
            objects,
        })
    }
}

struct Prepared {
    cache: Cache,
    report: ExportReport,
    objects: BTreeMap<GitObjectId, String>,
}

fn flatten_tree(
    stage: &Stage,
    id: GitObjectId,
    prefix: &str,
    blobs: &BTreeMap<GitObjectId, izu_model::ObjectId>,
    result: &mut BTreeMap<RepoPath, TreeEntry>,
    limits: &crate::GitLimits,
    depth: usize,
) -> Result<()> {
    if depth > limits.max_tree_depth {
        return Err(Error::Limit("flattened tree depth"));
    }
    let entries = stage
        .trees
        .get(&id)
        .ok_or_else(|| Error::InvalidSource("staged source tree missing".into()))?;
    if entries.is_empty() && !prefix.is_empty() {
        let path = RepoPath::new(prefix)
            .map_err(|error| native_error("validate imported directory", error))?;
        result.insert(
            path,
            TreeEntry::Directory {
                mode: FileMode::from_unix_permissions(0o755)
                    .map_err(|error| native_error("map directory mode", error))?,
            },
        );
    }
    for entry in entries {
        if result.len() >= limits.max_tree_entries {
            return Err(Error::Limit("flattened source entries"));
        }
        let name = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        let path =
            RepoPath::new(&name).map_err(|error| native_error("validate imported path", error))?;
        let value = match entry.mode {
            0o40000 => {
                // Git records a directory kind, not POSIX directory permissions.
                // Materialize the conventional 0755 mode explicitly so native
                // recapture does not invent the private creation mode as source.
                result.insert(
                    path,
                    TreeEntry::Directory {
                        mode: FileMode::Executable,
                    },
                );
                flatten_tree(stage, entry.object, &name, blobs, result, limits, depth + 1)?;
                continue;
            }
            0o100644 | 0o100755 => TreeEntry::File {
                blob: blobs
                    .get(&entry.object)
                    .copied()
                    .ok_or_else(|| Error::InvalidSource("imported blob missing".into()))?,
                mode: if entry.mode == 0o100755 {
                    FileMode::Executable
                } else {
                    FileMode::Regular
                },
            },
            0o120000 => TreeEntry::Symlink {
                target: SymlinkTarget::new(
                    stage
                        .blobs
                        .get(&entry.object)
                        .cloned()
                        .ok_or_else(|| Error::InvalidSource("symlink blob missing".into()))?,
                )
                .map_err(|error| native_error("map symbolic link", error))?,
            },
            _ => {
                return Err(Error::Unsupported(vec![format!(
                    "source tree mode {:o}",
                    entry.mode
                )]));
            }
        };
        if result.insert(path, value).is_some() {
            return Err(Error::InvalidSource(
                "duplicate flattened source path".into(),
            ));
        }
    }
    Ok(())
}

struct ExportBuilder<'a> {
    adapter: &'a GitAdapter,
    repository: &'a Repository,
    cache: &'a Path,
    cancel: &'a CancellationToken,
    options: &'a ExportOptions,
    revisions: BTreeMap<RevisionId, GitObjectId>,
    trees: BTreeMap<TreeId, GitObjectId>,
    warnings: BTreeSet<String>,
    total_bytes: usize,
    objects: usize,
    object_kinds: BTreeMap<GitObjectId, String>,
}
impl ExportBuilder<'_> {
    fn write(&mut self, kind: &str, bytes: &[u8]) -> Result<GitObjectId> {
        if bytes.len() > self.adapter.tool.config.limits.max_object_bytes {
            return Err(Error::Limit("native Git exchange object"));
        }
        self.objects = self
            .objects
            .checked_add(1)
            .ok_or(Error::Limit("export object count"))?;
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes.len())
            .ok_or(Error::Limit("export bytes"))?;
        if self.objects > self.adapter.tool.config.limits.max_objects
            || self.total_bytes > self.adapter.tool.config.limits.max_total_object_bytes
        {
            return Err(Error::Limit("native Git export"));
        }
        let arguments = GitTool::args(&["hash-object", "-w", "-t", kind, "--stdin"]);
        let output = self
            .adapter
            .tool
            .run(
                Some(self.cache),
                "write original exchange object",
                &arguments,
                bytes,
                128,
                self.cancel,
            )?
            .stdout;
        let id: GitObjectId = std::str::from_utf8(&output)
            .map_err(|_| Error::InvalidSource("non-UTF8 Git object ID".into()))?
            .trim()
            .parse()?;
        if id != objects::object_id(kind, bytes) {
            return Err(Error::InvalidObject {
                object: id.to_string(),
                reason: "Git export object hash mismatch".into(),
            });
        }
        self.object_kinds.insert(id, kind.into());
        Ok(id)
    }
    fn revision(&mut self, root: RevisionId) -> Result<GitObjectId> {
        let mut pending = vec![(root, false)];
        let mut visiting = BTreeSet::new();
        while let Some((id, ready)) = pending.pop() {
            if self.revisions.contains_key(&id) {
                continue;
            }
            if pending.len() > self.adapter.tool.config.limits.max_objects
                || visiting.len() > self.adapter.tool.config.limits.max_objects
            {
                return Err(Error::Limit("native revision graph"));
            }
            let revision = self
                .repository
                .revision(id, self.cancel)
                .map_err(|error| native_error("read export revision", error))?;
            if matches!(revision.origin, Some(RevisionOrigin::Bootstrap)) {
                return Err(Error::Unsupported(vec![
                    "native initialization anchor is not a source commit".into(),
                ]));
            }
            if !ready {
                if !visiting.insert(id) {
                    return Err(Error::InvalidSource("native revision parent cycle".into()));
                }
                pending.push((id, true));
                for parent in revision.parents.iter().rev() {
                    let parent_revision = self
                        .repository
                        .revision(*parent, self.cancel)
                        .map_err(|error| native_error("read initialization parent", error))?;
                    if matches!(parent_revision.origin, Some(RevisionOrigin::Bootstrap)) {
                        self.warnings.insert(
                            "Native initialization anchor is omitted from Git source history"
                                .into(),
                        );
                    } else {
                        pending.push((*parent, false));
                    }
                }
                continue;
            }
            visiting.remove(&id);
            let tree = self.tree(revision.tree)?;
            let mut parents = Vec::new();
            for parent in &revision.parents {
                let parent_revision = self
                    .repository
                    .revision(*parent, self.cancel)
                    .map_err(|error| native_error("read initialization parent", error))?;
                if matches!(parent_revision.origin, Some(RevisionOrigin::Bootstrap)) {
                    continue;
                }
                parents.push(
                    self.revisions
                        .get(parent)
                        .copied()
                        .ok_or_else(|| Error::InvalidSource("export parent missing".into()))?,
                );
            }
            let preserved = if let Some(RevisionOrigin::Git {
                object_id,
                raw_commit,
            }) = &revision.origin
            {
                let original: GitObjectId = object_id.parse()?;
                let raw = self.read_blob(*raw_commit)?;
                if objects::object_id("commit", &raw) != original {
                    return Err(Error::InvalidObject {
                        object: object_id.clone(),
                        reason: "stored original commit hash mismatch".into(),
                    });
                }
                let parsed = objects::commit(original, raw.clone())?;
                if !parsed.unsupported.is_empty() {
                    return Err(Error::Unsupported(parsed.unsupported));
                }
                if parsed.tree == tree
                    && parsed.parents == parents
                    && parsed.message == revision.description
                    && parsed.author_name == revision.author.name
                    && parsed.author_email == revision.author.email
                    && parsed.author_seconds.checked_mul(1000) == Some(revision.created_at_unix_ms)
                {
                    Some(self.write("commit", &raw)?)
                } else {
                    None
                }
            } else {
                None
            };
            let git_id = if let Some(preserved) = preserved {
                preserved
            } else {
                let signature = self
                    .options
                    .committer
                    .as_ref()
                    .ok_or(Error::MissingCommitter)?;
                validate_signature(signature)?;
                validate_identity(&revision.author.name, &revision.author.email)?;
                let seconds = revision.created_at_unix_ms.div_euclid(1000);
                if seconds < 0 {
                    return Err(Error::Unsupported(vec![
                        "negative native author timestamp".into(),
                    ]));
                }
                if revision.created_at_unix_ms.rem_euclid(1000) != 0 {
                    self.warnings.insert("Git author timestamps have second precision; native milliseconds are truncated".into());
                }
                let mut raw = format!("tree {tree}\n");
                for parent in &parents {
                    raw.push_str(&format!("parent {parent}\n"));
                }
                raw.push_str(&format!(
                    "author {} <{}> {seconds} +0000\ncommitter {} <{}> {} {}\n\n",
                    revision.author.name,
                    revision.author.email,
                    signature.name,
                    signature.email,
                    signature.timestamp_unix_seconds,
                    timezone(signature.timezone_minutes)
                ));
                if raw
                    .len()
                    .checked_add(revision.description.len())
                    .is_none_or(|length| length > self.adapter.tool.config.limits.max_object_bytes)
                {
                    return Err(Error::Limit("native Git commit payload"));
                }
                raw.try_reserve(revision.description.len())
                    .map_err(|_| Error::Limit("native Git commit allocation"))?;
                raw.push_str(&revision.description);
                self.write("commit", raw.as_bytes())?
            };
            self.revisions.insert(id, git_id);
        }
        self.revisions
            .get(&root)
            .copied()
            .ok_or_else(|| Error::InvalidSource("export root missing".into()))
    }
    fn tree(&mut self, id: TreeId) -> Result<GitObjectId> {
        if let Some(value) = self.trees.get(&id) {
            return Ok(*value);
        }
        let tree = self
            .repository
            .tree(id, self.cancel)
            .map_err(|error| native_error("read export tree", error))?;
        if tree.entries.len() > self.adapter.tool.config.limits.max_tree_entries {
            return Err(Error::Limit("native export tree entries"));
        }
        let mut root = Directory::default();
        for (path, entry) in &tree.entries {
            if path
                .as_str()
                .split('/')
                .any(|part| part.eq_ignore_ascii_case(".git") || part.eq_ignore_ascii_case(".izu"))
            {
                return Err(Error::InvalidSource("unsafe native export path".into()));
            }
            let (mode, blob) = match entry {
                TreeEntry::File { blob, mode } => {
                    let mode = match mode.unix_permissions() {
                        0o644 => 0o100644,
                        0o755 => 0o100755,
                        value => {
                            return Err(Error::Unsupported(vec![format!(
                                "native file mode {value:o} cannot map exactly to Git"
                            )]));
                        }
                    };
                    let bytes = self.read_blob(*blob)?;
                    if path
                        .as_str()
                        .split('/')
                        .next_back()
                        .is_some_and(|name| name == ".gitattributes" || name == ".gitmodules")
                        || bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n")
                    {
                        return Err(Error::Unsupported(vec![
                            "attributes, submodule config or LFS source cannot be exported safely"
                                .into(),
                        ]));
                    }
                    (mode, Some(self.write("blob", &bytes)?))
                }
                TreeEntry::Symlink { target } => {
                    (0o120000, Some(self.write("blob", target.as_bytes())?))
                }
                TreeEntry::Directory { mode } => {
                    if mode.unix_permissions() != 0o755 {
                        return Err(Error::Unsupported(vec![
                            "native directory permissions cannot map exactly to Git".into(),
                        ]));
                    }
                    (0o40000, None)
                }
                TreeEntry::Conflict { .. } => {
                    return Err(Error::Unsupported(vec![
                        "unresolved native tree conflict".into(),
                    ]));
                }
            };
            root.insert(path.as_str(), mode, blob)?;
        }
        let result = self.write_directory(&root, 0)?;
        self.trees.insert(id, result);
        Ok(result)
    }
    fn write_directory(&mut self, directory: &Directory, depth: usize) -> Result<GitObjectId> {
        if depth > self.adapter.tool.config.limits.max_tree_depth {
            return Err(Error::Limit("native export path depth"));
        }
        let mut entries = Vec::new();
        for (name, value) in &directory.entries {
            let (mode, id, is_directory) = match value {
                Node::Leaf { mode, object } => (*mode, *object, false),
                Node::Directory(child) => {
                    if child.entries.is_empty() {
                        self.warnings
                            .insert("Git working-tree checkout omits empty directories".into());
                    }
                    (0o40000, self.write_directory(child, depth + 1)?, true)
                }
            };
            let mut sorting = name.as_bytes().to_vec();
            if is_directory {
                sorting.push(b'/');
            }
            entries.push((sorting, name, mode, id));
        }
        entries.sort_by(|first, second| first.0.cmp(&second.0));
        let mut bytes = Vec::new();
        for (_, name, mode, id) in entries {
            let entry = format!("{mode:o} {name}\0");
            let length = entry
                .len()
                .checked_add(20)
                .ok_or(Error::Limit("native Git tree payload"))?;
            if bytes
                .len()
                .checked_add(length)
                .is_none_or(|length| length > self.adapter.tool.config.limits.max_object_bytes)
            {
                return Err(Error::Limit("native Git tree payload"));
            }
            bytes
                .try_reserve(length)
                .map_err(|_| Error::Limit("native Git tree allocation"))?;
            bytes.extend_from_slice(entry.as_bytes());
            bytes.extend_from_slice(id.as_bytes());
        }
        self.write("tree", &bytes)
    }
    fn read_blob(&self, id: izu_model::ObjectId) -> Result<Vec<u8>> {
        let mut output = LimitedBuffer {
            bytes: Vec::new(),
            limit: self.adapter.tool.config.limits.max_object_bytes,
            exceeded: false,
        };
        let read = self.repository.read_blob(id, &mut output, self.cancel);
        if output.exceeded {
            return Err(Error::Limit("native blob for Git export"));
        }
        read.map_err(|error| native_error("stream bounded native Git export blob", error))?;
        Ok(output.bytes)
    }
}

struct LimitedBuffer {
    bytes: Vec<u8>,
    limit: usize,
    exceeded: bool,
}
impl Write for LimitedBuffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self
            .bytes
            .len()
            .checked_add(bytes.len())
            .is_none_or(|length| length > self.limit)
        {
            self.exceeded = true;
            return Err(std::io::Error::other(
                "native blob exceeds Git export bound",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| std::io::Error::other("bounded Git export allocation failed"))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Default)]
struct Directory {
    entries: BTreeMap<String, Node>,
}
enum Node {
    Leaf { mode: u32, object: GitObjectId },
    Directory(Directory),
}
impl Directory {
    fn insert(&mut self, path: &str, mode: u32, object: Option<GitObjectId>) -> Result<()> {
        if let Some((head, rest)) = path.split_once('/') {
            let entry = self
                .entries
                .entry(head.to_owned())
                .or_insert_with(|| Node::Directory(Directory::default()));
            return match entry {
                Node::Directory(directory) => directory.insert(rest, mode, object),
                Node::Leaf { .. } => Err(Error::InvalidSource(
                    "native tree has file/directory prefix collision".into(),
                )),
            };
        }
        if mode == 0o40000 {
            if matches!(self.entries.get(path), Some(Node::Leaf { .. })) {
                return Err(Error::InvalidSource("native tree prefix collision".into()));
            }
            self.entries
                .entry(path.into())
                .or_insert_with(|| Node::Directory(Directory::default()));
        } else {
            let object =
                object.ok_or_else(|| Error::InvalidSource("native leaf missing object".into()))?;
            if self
                .entries
                .insert(path.into(), Node::Leaf { mode, object })
                .is_some()
            {
                return Err(Error::InvalidSource("native tree prefix collision".into()));
            }
        }
        Ok(())
    }
}

fn validate_identity(name: &str, email: &str) -> Result<()> {
    if name.len() > 4096 || email.len() > 4096 {
        return Err(Error::Limit("explicit Git identity"));
    }
    if name.is_empty()
        || email.is_empty()
        || name.contains(['<', '>', '\0', '\n', '\r'])
        || email.contains(['<', '>', '\0', '\n', '\r'])
    {
        return Err(Error::InvalidSource(
            "explicit Git identity contains unsupported header characters".into(),
        ));
    }
    Ok(())
}
fn validate_signature(signature: &GitSignature) -> Result<()> {
    validate_identity(&signature.name, &signature.email)?;
    if signature.timestamp_unix_seconds < 0 || signature.timezone_minutes.unsigned_abs() >= 24 * 60
    {
        return Err(Error::InvalidSource(
            "invalid explicit Git committer date".into(),
        ));
    }
    Ok(())
}
fn timezone(minutes: i16) -> String {
    let sign = if minutes < 0 { '-' } else { '+' };
    let value = minutes.unsigned_abs();
    format!("{sign}{:02}{:02}", value / 60, value % 60)
}

fn copy_objects(
    from: &Path,
    target: &Path,
    limits: &crate::GitLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    // Loose immutable objects are copied before any target ref is published.
    // Existing files are not replaced; installed Git verifies reads afterward.
    use izu_platform::{Directory, EntryKind, sync_file};
    let source_root = Directory::open(from).map_err(|source| Error::Io {
        action: "anchor owned export cache",
        source,
    })?;
    let objects = source_root
        .open_dir(OsStr::new("objects"))
        .map_err(|source| Error::Io {
            action: "anchor owned object directory",
            source,
        })?;
    let target_root = Directory::open(target).map_err(|source| Error::Io {
        action: "anchor explicit Git publication target",
        source,
    })?;
    let target_objects = target_root
        .open_dir(OsStr::new("objects"))
        .map_err(|source| Error::Io {
            action: "anchor target object directory",
            source,
        })?;
    for prefix in objects.entries().map_err(|source| Error::Io {
        action: "list export objects",
        source,
    })? {
        if cancel.is_cancelled() {
            return Err(Error::Cancelled);
        }
        let prefix = prefix.map_err(|source| Error::Io {
            action: "read export object directory",
            source,
        })?;
        let name = prefix.name;
        let name_text = name
            .to_str()
            .ok_or_else(|| Error::InvalidSource("invalid owned object directory".into()))?;
        if name_text.len() != 2 || !name_text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            continue;
        }
        if prefix.kind != EntryKind::Directory {
            return Err(Error::InvalidSource(
                "owned object shard is not a directory".into(),
            ));
        }
        let source_directory = objects.open_dir(&name).map_err(|source| Error::Io {
            action: "anchor owned object shard",
            source,
        })?;
        let destination = target_objects
            .ensure_dir(&name)
            .map_err(|source| Error::Io {
                action: "anchor target object shard",
                source,
            })?;
        for object in source_directory.entries().map_err(|source| Error::Io {
            action: "list loose export objects",
            source,
        })? {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let object = object.map_err(|source| Error::Io {
                action: "read loose export object",
                source,
            })?;
            let name_text = object
                .name
                .to_str()
                .ok_or_else(|| Error::InvalidSource("invalid owned object filename".into()))?;
            if object.kind != EntryKind::File
                || name_text.len() != 38
                || !name_text.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(Error::InvalidSource(
                    "invalid owned loose object entry".into(),
                ));
            }
            let mut entropy = [0_u8; 16];
            getrandom::fill(&mut entropy).map_err(|error| {
                Error::InvalidSource(format!("exchange temporary identity: {error}"))
            })?;
            let temporary_name = format!(
                "izu-tmp-{}",
                entropy
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            );
            let temporary_name = OsStr::new(&temporary_name);
            let mut temporary = destination
                .create_new_file(temporary_name)
                .map_err(|source| Error::Io {
                    action: "prepare anchored target object",
                    source,
                })?;
            let result = (|| {
                let mut input =
                    source_directory
                        .open_read(&object.name)
                        .map_err(|source| Error::Io {
                            action: "open owned export object",
                            source,
                        })?;
                let bound = limits
                    .max_object_bytes
                    .checked_mul(2)
                    .and_then(|size| size.checked_add(4096))
                    .ok_or(Error::Limit("compressed exchange object"))?;
                if input
                    .metadata()
                    .map_err(|source| Error::Io {
                        action: "inspect owned export object",
                        source,
                    })?
                    .len()
                    > bound as u64
                {
                    return Err(Error::Limit("compressed exchange object"));
                }
                std::io::copy(&mut input, &mut temporary).map_err(|source| Error::Io {
                    action: "copy immutable exchange object",
                    source,
                })?;
                sync_file(&temporary).map_err(|source| Error::Io {
                    action: "persist exchange object",
                    source,
                })?;
                match destination.rename_noreplace(temporary_name, &destination, &object.name) {
                    Ok(()) => Ok(()),
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                        // Do not overwrite an existing immutable object. Its
                        // complete logical closure is verified before refs move.
                        destination
                            .open_read(&object.name)
                            .map_err(|source| Error::Io {
                                action: "inspect existing immutable object",
                                source,
                            })?;
                        Ok(())
                    }
                    Err(source) => Err(Error::Io {
                        action: "install immutable exchange object",
                        source,
                    }),
                }
            })();
            let _ = destination.remove_file(temporary_name);
            result?;
        }
        destination.sync().map_err(|source| Error::Io {
            action: "persist target object directory",
            source,
        })?;
    }
    target_objects.sync().map_err(|source| Error::Io {
        action: "persist target object root",
        source,
    })?;
    Ok(())
}

fn verify_git_closure(
    tool: &GitTool,
    directory: &Path,
    roots: impl IntoIterator<Item = GitObjectId>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut arguments = GitTool::args(&["fsck", "--strict", "--no-reflogs", "--no-dangling"]);
    arguments.extend(roots.into_iter().map(|id| OsString::from(id.to_string())));
    tool.run(
        Some(directory),
        "verify intended Git object closure",
        &arguments,
        &[],
        64 * 1024,
        cancel,
    )?;
    Ok(())
}

fn target_only_cache(tool: &GitTool, target: &Path, cancel: &CancellationToken) -> Result<Cache> {
    let cache = Cache::new(tool, cancel)?;
    let objects = target.join("objects");
    let objects = objects
        .to_str()
        .ok_or_else(|| Error::Unsupported(vec!["non-UTF8 target object directory".into()]))?;
    if objects.contains(['\n', '\r']) {
        return Err(Error::InvalidSource(
            "target object directory contains a newline".into(),
        ));
    }
    fs::write(
        cache.path().join("objects/info/alternates"),
        format!("{objects}\n"),
    )
    .map_err(|source| Error::Io {
        action: "prepare isolated target verification cache",
        source,
    })?;
    Ok(cache)
}

fn verify_existing_target_objects(
    tool: &GitTool,
    target: &Path,
    objects: &BTreeMap<GitObjectId, String>,
    cancel: &CancellationToken,
) -> Result<()> {
    // Only target objects are visible here. Export cache objects must not mask a
    // corrupt preexisting object that claims the same legacy Git ID.
    let cache = target_only_cache(tool, target, cancel)?;
    let target_root = izu_platform::Directory::open(target).map_err(|source| Error::Io {
        action: "anchor target object verification",
        source,
    })?;
    let loose = target_root
        .open_dir(OsStr::new("objects"))
        .map_err(|source| Error::Io {
            action: "anchor target loose objects",
            source,
        })?;
    let ids: Vec<_> = objects.keys().copied().collect();
    let batch_size = (tool.config.limits.max_object_bytes / 41).clamp(1, 4096);
    let mut total = 0_usize;
    for chunk in ids.chunks(batch_size) {
        let input = chunk.iter().map(|id| format!("{id}\n")).collect::<String>();
        let output_limit = chunk
            .len()
            .checked_mul(128)
            .ok_or(Error::Limit("target object advertisement"))?;
        let output = tool
            .run(
                Some(cache.path()),
                "inspect existing target objects",
                &GitTool::args(&[
                    "cat-file",
                    "--batch-check=%(objectname) %(objecttype) %(objectsize)",
                ]),
                input.as_bytes(),
                output_limit,
                cancel,
            )?
            .stdout;
        let text = std::str::from_utf8(&output)
            .map_err(|_| Error::InvalidSource("invalid target object advertisement".into()))?;
        let lines: Vec<_> = text.lines().collect();
        if lines.len() != chunk.len() {
            return Err(Error::InvalidSource(
                "incomplete target object advertisement".into(),
            ));
        }
        for (line, expected) in lines.iter().zip(chunk) {
            let fields: Vec<_> = line.split(' ').collect();
            if fields.len() == 2 && fields[0] == expected.to_string() && fields[1] == "missing" {
                // Git may report an unreadable corrupt loose object as missing.
                // Its physical presence forbids publishing that claimed ID.
                if loose_object_present(&loose, *expected, &tool.config.limits)? {
                    return Err(Error::InvalidObject {
                        object: expected.to_string(),
                        reason: "target contains an unreadable loose object under the intended ID"
                            .into(),
                    });
                }
                continue;
            }
            if fields.len() != 3
                || fields[0] != expected.to_string()
                || objects.get(expected).is_none_or(|kind| kind != fields[1])
            {
                return Err(Error::InvalidObject {
                    object: expected.to_string(),
                    reason: "target object has an unexpected kind or identity".into(),
                });
            }
            let length: usize = fields[2].parse().map_err(|_| Error::InvalidObject {
                object: expected.to_string(),
                reason: "invalid target object length".into(),
            })?;
            total = total
                .checked_add(length)
                .ok_or(Error::Limit("existing target object payload"))?;
            if length > tool.config.limits.max_object_bytes
                || total > tool.config.limits.max_total_object_bytes
            {
                return Err(Error::Limit("existing target object payload"));
            }
            let bytes = tool
                .run(
                    Some(cache.path()),
                    "verify existing target object",
                    &GitTool::args(&["cat-file", fields[1], &expected.to_string()]),
                    &[],
                    tool.config.limits.max_object_bytes,
                    cancel,
                )?
                .stdout;
            if bytes.len() != length || objects::object_id(fields[1], &bytes) != *expected {
                return Err(Error::InvalidObject {
                    object: expected.to_string(),
                    reason: "target object is corrupt".into(),
                });
            }
            let hash = tool
                .run(
                    Some(cache.path()),
                    "validate target object with installed Git",
                    &GitTool::args(&["hash-object", "-t", fields[1], "--stdin"]),
                    &bytes,
                    128,
                    cancel,
                )?
                .stdout;
            if std::str::from_utf8(&hash)
                .map_err(|_| Error::InvalidSource("invalid target hash response".into()))?
                .trim()
                .parse::<GitObjectId>()?
                != *expected
            {
                return Err(Error::InvalidObject {
                    object: expected.to_string(),
                    reason: "installed Git rejected target object identity".into(),
                });
            }
        }
    }
    Ok(())
}

fn loose_object_present(
    objects: &izu_platform::Directory,
    id: GitObjectId,
    limits: &crate::GitLimits,
) -> Result<bool> {
    let text = id.to_string();
    let shard = match objects.open_dir(OsStr::new(&text[..2])) {
        Ok(shard) => shard,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::Io {
                action: "anchor preexisting target object shard",
                source,
            });
        }
    };
    let file = match shard.open_read(OsStr::new(&text[2..])) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(source) => {
            return Err(Error::Io {
                action: "open preexisting target object",
                source,
            });
        }
    };
    let bound = limits
        .max_object_bytes
        .checked_mul(2)
        .and_then(|size| size.checked_add(4096))
        .ok_or(Error::Limit("compressed target object"))?;
    if file
        .metadata()
        .map_err(|source| Error::Io {
            action: "inspect anchored target object",
            source,
        })?
        .len()
        > bound as u64
    {
        return Err(Error::Limit("compressed target object"));
    }
    Ok(true)
}

fn verify_local_target_closure(
    tool: &GitTool,
    target: &Path,
    roots: impl IntoIterator<Item = GitObjectId>,
    cancel: &CancellationToken,
) -> Result<()> {
    let cache = target_only_cache(tool, target, cancel)?;
    verify_git_closure(tool, cache.path(), roots, cancel)
}

fn publish_local_refs(
    tool: &GitTool,
    target: &Path,
    refs: &BTreeMap<String, GitObjectId>,
    expected: &BTreeMap<String, Option<GitObjectId>>,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut transaction = String::from("start\n");
    for (name, object) in refs {
        validate_git_ref(name)?;
        let old = expected
            .get(name)
            .copied()
            .flatten()
            .map(|id| id.to_string())
            .unwrap_or_else(|| GitObjectId::zero_hex().into());
        transaction.push_str(&format!("update {name} {object} {old}\n"));
    }
    transaction.push_str("prepare\ncommit\n");
    tool.run(
        Some(target),
        "publish guarded local Git refs",
        &GitTool::args(&["update-ref", "--stdin"]),
        transaction.as_bytes(),
        4096,
        cancel,
    )?;
    Ok(())
}
fn ensure_remote_object(
    tool: &GitTool,
    cache: &Path,
    remote: &GitSource,
    git_ref: &str,
    expected: GitObjectId,
    cancel: &CancellationToken,
) -> Result<()> {
    match remote {
        GitSource::Local(path) => {
            let directory = source::local_git_dir(path)?.join("objects");
            let text = directory
                .to_str()
                .ok_or_else(|| Error::Unsupported(vec!["non-UTF8 local remote path".into()]))?;
            if text.contains(['\n', '\r']) {
                return Err(Error::InvalidSource(
                    "local remote object path contains newline".into(),
                ));
            }
            fs::write(cache.join("objects/info/alternates"), format!("{text}\n")).map_err(
                |source| Error::Io {
                    action: "read explicit remote objects",
                    source,
                },
            )?;
        }
        GitSource::Https(url) => {
            crate::wire::fetch_lease(tool, url, cache, git_ref, expected, cancel)?
        }
    }
    Ok(())
}
fn is_ancestor(
    tool: &GitTool,
    cache: &Path,
    old: GitObjectId,
    new: GitObjectId,
    cancel: &CancellationToken,
) -> Result<bool> {
    match tool.run(
        Some(cache),
        "check target ancestry",
        &GitTool::args(&[
            "merge-base",
            "--is-ancestor",
            &old.to_string(),
            &new.to_string(),
        ]),
        &[],
        4096,
        cancel,
    ) {
        Ok(_) => Ok(true),
        Err(Error::CommandFailed { code: Some(1), .. }) => Ok(false),
        Err(error) => Err(error),
    }
}

fn definite_push_rejection(stdout: &[u8], target: &str) -> bool {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return false;
    };
    text.lines().any(|line| {
        let mut fields = line.split('\t');
        let flag = fields.next();
        let refspec = fields.next();
        let reason = fields.next();
        flag == Some("!")
            && fields.next().is_none()
            && refspec
                .and_then(|value| value.rsplit_once(':'))
                .is_some_and(|(_, name)| name == target)
            && reason.is_some_and(|value| {
                value.starts_with("[remote rejected]") || value.starts_with("[rejected]")
            })
    })
}
