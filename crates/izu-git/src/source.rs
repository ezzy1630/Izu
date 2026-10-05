use crate::objects::{self, Commit, TreeEntry};
use crate::tool::GitTool;
use crate::{Error, GitInventory, GitLimits, GitObjectId, GitSource, Result, validate_git_ref};
use izu_model::CancellationToken;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

pub(crate) struct Cache {
    _directory: tempfile::TempDir,
    canonical: PathBuf,
}

impl Cache {
    pub fn new(tool: &GitTool, cancel: &CancellationToken) -> Result<Self> {
        let mut builder = tempfile::Builder::new();
        builder.prefix("izu-git-");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            builder.permissions(fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir().map_err(|source| Error::Io {
            action: "create Git cache",
            source,
        })?;
        let arguments = vec![
            OsString::from("init"),
            OsString::from("--bare"),
            OsString::from("--template="),
            OsString::from("--initial-branch=main"),
            directory.path().as_os_str().to_owned(),
        ];
        tool.run(
            None,
            "initialize owned cache",
            &arguments,
            &[],
            16 * 1024,
            cancel,
        )?;
        let canonical = fs::canonicalize(directory.path()).map_err(|source| Error::Io {
            action: "resolve owned cache path",
            source,
        })?;
        Ok(Self {
            _directory: directory,
            canonical,
        })
    }
    pub fn path(&self) -> &Path {
        &self.canonical
    }
}

pub(crate) struct Stage {
    pub inventory: GitInventory,
    pub commits: BTreeMap<GitObjectId, Commit>,
    pub order: Vec<GitObjectId>,
    pub trees: BTreeMap<GitObjectId, Vec<TreeEntry>>,
    pub blobs: BTreeMap<GitObjectId, Vec<u8>>,
    pub _cache: Cache,
}

pub(crate) fn read_bounded(path: &Path, limit: usize) -> Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path).map_err(|source| Error::Io {
        action: "inspect Git source file",
        source,
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(Error::InvalidSource(
            "Git source metadata must be a regular file".into(),
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(Error::Limit("Git source metadata"));
    }
    let mut bytes = Vec::new();
    #[cfg(unix)]
    let file: fs::File = rustix::fs::open(
        path,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC
            | rustix::fs::OFlags::NONBLOCK,
        rustix::fs::Mode::empty(),
    )
    .map_err(|source| Error::Io {
        action: "open Git source file without following links",
        source: source.into(),
    })?
    .into();
    #[cfg(not(unix))]
    let file = fs::File::open(path).map_err(|source| Error::Io {
        action: "open Git source file",
        source,
    })?;
    let metadata = file.metadata().map_err(|source| Error::Io {
        action: "inspect opened Git metadata",
        source,
    })?;
    if !metadata.is_file() {
        return Err(Error::InvalidSource(
            "opened Git metadata is not a regular file".into(),
        ));
    }
    if metadata.len() > limit as u64 {
        return Err(Error::Limit("Git source metadata"));
    }
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| Error::Io {
            action: "read Git source file",
            source,
        })?;
    if bytes.len() > limit {
        return Err(Error::Limit("Git source metadata"));
    }
    Ok(bytes)
}

pub(crate) fn local_git_dir(path: &Path) -> Result<PathBuf> {
    let source = fs::canonicalize(path).map_err(|source| Error::Io {
        action: "resolve explicit Git source",
        source,
    })?;
    if !source.is_dir() {
        return Err(Error::InvalidSource("source is not a directory".into()));
    }
    let candidate = source.join(".git");
    let git_directory = match fs::symlink_metadata(&candidate) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => candidate,
        Ok(_) => {
            return Err(Error::Unsupported(vec![
                "Git files, linked worktrees and symbolic .git directories".into(),
            ]));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => source,
        Err(source) => {
            return Err(Error::Io {
                action: "inspect Git metadata",
                source,
            });
        }
    };
    for directory in ["objects", "refs"] {
        let metadata =
            fs::symlink_metadata(git_directory.join(directory)).map_err(|source| Error::Io {
                action: "inspect Git directory",
                source,
            })?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(Error::InvalidSource(
                "Git object/ref directory is missing or symbolic".into(),
            ));
        }
    }
    Ok(git_directory)
}

fn inspect_config(directory: &Path, limits: &GitLimits) -> Result<Vec<String>> {
    let mut unsupported = Vec::new();
    let path = directory.join("config");
    if path.exists() {
        let bytes = read_bounded(&path, limits.max_config_bytes)?;
        let source = std::str::from_utf8(&bytes)
            .map_err(|_| Error::InvalidSource("non-UTF8 Git configuration".into()))?;
        let mut section = String::new();
        for line in source.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(['#', ';']) {
                continue;
            }
            if line.starts_with('[') {
                let end = line.find(']').ok_or_else(|| {
                    Error::InvalidSource("malformed Git configuration section".into())
                })?;
                section = line[1..end]
                    .split_whitespace()
                    .next()
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if matches!(section.as_str(), "filter" | "include" | "includeif" | "lfs") {
                    unsupported.push(format!("source config section {section}"));
                }
                continue;
            }
            let (key, value) = line
                .split_once('=')
                .or_else(|| line.split_once(char::is_whitespace))
                .unwrap_or((line, "true"));
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().trim_matches('"');
            if section == "extensions"
                && !(key == "objectformat" && value.eq_ignore_ascii_case("sha1"))
            {
                unsupported.push(format!("Git repository extension {key}={value}"));
            }
            if section == "core" && key == "repositoryformatversion" && value != "0" && value != "1"
            {
                unsupported.push(format!("Git repository format {value}"));
            }
        }
    }
    for path in [
        "objects/info/alternates",
        "objects/info/http-alternates",
        "shallow",
        "info/grafts",
        "commondir",
    ] {
        if directory.join(path).exists() {
            unsupported.push(format!("source metadata {path}"));
        }
    }
    Ok(unsupported)
}

pub(crate) fn bare_target(path: &Path, limits: &GitLimits) -> Result<PathBuf> {
    let directory = local_git_dir(path)?;
    let bytes = read_bounded(&directory.join("config"), limits.max_config_bytes)?;
    let config = std::str::from_utf8(&bytes)
        .map_err(|_| Error::InvalidSource("non-UTF8 target Git config".into()))?;
    let mut section = String::new();
    let mut bare = false;
    let mut worktree = false;
    for line in config.lines().map(str::trim) {
        if line.starts_with('[') {
            let end = line
                .find(']')
                .ok_or_else(|| Error::InvalidSource("malformed target Git configuration".into()))?;
            section = line[1..end].trim().to_ascii_lowercase();
        } else if section == "core" && !line.is_empty() && !line.starts_with(['#', ';']) {
            let (key, value) = line
                .split_once('=')
                .or_else(|| line.split_once(char::is_whitespace))
                .unwrap_or((line, "true"));
            let key = key.trim();
            let value = value.trim().trim_matches('"');
            if key.eq_ignore_ascii_case("bare") {
                bare = value.eq_ignore_ascii_case("true")
                    || value.eq_ignore_ascii_case("yes")
                    || value == "1"
                    || value.eq_ignore_ascii_case("on");
            }
            if key.eq_ignore_ascii_case("worktree") {
                worktree = true;
            }
        }
    }
    if !bare || worktree {
        return Err(Error::Unsupported(vec![
            "local publication requires an explicit bare Git target without an attached worktree"
                .into(),
        ]));
    }
    if directory.join("worktrees").exists() {
        return Err(Error::Unsupported(vec![
            "local publication target has linked worktree registrations".into(),
        ]));
    }
    let unsupported = inspect_config(&directory, limits)?;
    if !unsupported.is_empty() {
        return Err(Error::Unsupported(unsupported));
    }
    Ok(directory)
}

fn read_loose_refs(
    directory: &Path,
    relative: &str,
    depth: usize,
    limits: &GitLimits,
    refs: &mut BTreeMap<String, GitObjectId>,
) -> Result<()> {
    if depth > limits.max_tree_depth {
        return Err(Error::Limit("Git ref directory depth"));
    }
    let current = directory.join(relative);
    for item in fs::read_dir(&current).map_err(|source| Error::Io {
        action: "list Git refs",
        source,
    })? {
        let item = item.map_err(|source| Error::Io {
            action: "read Git ref entry",
            source,
        })?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| Error::InvalidRef("non-UTF8 reference name".into()))?;
        let full = format!("{relative}/{name}");
        let metadata = item.file_type().map_err(|source| Error::Io {
            action: "inspect Git ref entry",
            source,
        })?;
        if metadata.is_symlink() {
            return Err(Error::InvalidSource("symbolic ref metadata path".into()));
        }
        if metadata.is_dir() {
            read_loose_refs(directory, &full, depth + 1, limits, refs)?;
        } else if metadata.is_file() {
            validate_git_ref(&full)?;
            if refs.len() >= limits.max_refs && !refs.contains_key(&full) {
                return Err(Error::Limit("Git refs"));
            }
            let value = read_bounded(&item.path(), 4096)?;
            let value = std::str::from_utf8(&value)
                .map_err(|_| Error::InvalidRef(full.clone()))?
                .trim();
            if value.starts_with("ref:") {
                return Err(Error::Unsupported(vec![format!(
                    "symbolic source ref {full}"
                )]));
            }
            refs.insert(full, value.parse()?);
        } else {
            return Err(Error::InvalidSource("special file in Git refs".into()));
        }
    }
    Ok(())
}

type LocalRefSnapshot = (BTreeMap<String, GitObjectId>, Option<String>, Vec<String>);

pub(crate) fn local_refs(directory: &Path, limits: &GitLimits) -> Result<LocalRefSnapshot> {
    let mut refs = BTreeMap::new();
    let unsupported = inspect_config(directory, limits)?;
    let packed = directory.join("packed-refs");
    if packed.exists() {
        let bytes = read_bounded(&packed, limits.max_config_bytes)?;
        let value = std::str::from_utf8(&bytes)
            .map_err(|_| Error::InvalidSource("non-UTF8 packed refs".into()))?;
        for line in value
            .lines()
            .filter(|line| !line.starts_with(['#', '^']) && !line.is_empty())
        {
            let (object, name) = line
                .split_once(' ')
                .ok_or_else(|| Error::InvalidSource("malformed packed ref".into()))?;
            validate_git_ref(name)?;
            if refs.len() >= limits.max_refs {
                return Err(Error::Limit("Git refs"));
            }
            if refs.insert(name.to_owned(), object.parse()?).is_some() {
                return Err(Error::InvalidSource("duplicate packed reference".into()));
            }
        }
    }
    read_loose_refs(directory, "refs", 0, limits, &mut refs)?;
    let head = read_bounded(&directory.join("HEAD"), 4096)?;
    let head = std::str::from_utf8(&head)
        .map_err(|_| Error::InvalidSource("non-UTF8 HEAD".into()))?
        .trim();
    let symbolic = if let Some(value) = head.strip_prefix("ref: ") {
        validate_git_ref(value)?;
        Some(value.to_owned())
    } else {
        let _: GitObjectId = head.parse()?;
        None
    };
    Ok((refs, symbolic, unsupported))
}

pub(crate) fn source_argument(source: &GitSource) -> Result<OsString> {
    match source {
        GitSource::Local(path) => Ok(fs::canonicalize(path)
            .map_err(|source| Error::Io {
                action: "resolve explicit local remote",
                source,
            })?
            .into_os_string()),
        GitSource::Https(value) => match GitSource::https(value.clone())? {
            GitSource::Https(value) => Ok(value.into()),
            GitSource::Local(_) => Err(Error::InvalidSource("invalid HTTPS source".into())),
        },
    }
}

pub(crate) fn stage(
    tool: &GitTool,
    source: &GitSource,
    cancel: &CancellationToken,
) -> Result<Stage> {
    stage_selected(tool, source, None, cancel)
}
pub(crate) fn stage_selected(
    tool: &GitTool,
    source: &GitSource,
    selected: Option<&BTreeSet<String>>,
    cancel: &CancellationToken,
) -> Result<Stage> {
    let cache = Cache::new(tool, cancel)?;
    let mut https_advertised = None;
    let (refs, head, mut unsupported) = match source {
        GitSource::Local(path) => {
            let directory = local_git_dir(path)?;
            let (refs, head, unsupported) = local_refs(&directory, &tool.config.limits)?;
            let path = directory.join("objects");
            let path = path
                .to_str()
                .ok_or_else(|| Error::Unsupported(vec!["non-UTF8 Git object directory".into()]))?;
            if path.contains(['\n', '\r']) {
                return Err(Error::InvalidSource(
                    "Git object directory contains a newline".into(),
                ));
            }
            fs::write(
                cache.path().join("objects/info/alternates"),
                format!("{path}\n"),
            )
            .map_err(|source| Error::Io {
                action: "prepare owned object cache",
                source,
            })?;
            (refs, head, unsupported)
        }
        GitSource::Https(url) => {
            let advertised = crate::wire::refs(tool, url, cancel)?;
            let refs = advertised.refs.clone();
            let head = advertised.head.clone();
            https_advertised = Some(advertised);
            (refs, head, Vec::new())
        }
    };
    let mut omitted_refs = BTreeMap::new();
    let refs = if let Some(selected) = selected {
        for name in selected {
            validate_git_ref(name)?;
            if !name.starts_with("refs/heads/") {
                return Err(Error::Unsupported(vec![format!(
                    "selected source reference {name} is not an ordinary branch"
                )]));
            }
            if !refs.contains_key(name) {
                return Err(Error::InvalidRef(name.clone()));
            }
        }
        refs.into_iter()
            .filter_map(|(name, id)| {
                if selected.contains(&name) {
                    Some((name, id))
                } else {
                    omitted_refs.insert(name, id);
                    None
                }
            })
            .collect()
    } else {
        refs
    };
    for name in refs.keys().filter(|name| !name.starts_with("refs/heads/")) {
        unsupported.push(format!("source reference {name}"));
    }
    if selected.is_none() {
        for name in refs
            .keys()
            .filter_map(|name| name.strip_prefix("refs/heads/"))
        {
            if izu_model::RefName::new(name).is_err() {
                unsupported.push(format!(
                    "source branch {name} needs an explicit portable native reference mapping"
                ));
            }
        }
    }
    if unsupported.is_empty()
        && !refs.is_empty()
        && let (GitSource::Https(url), Some(advertised)) = (source, https_advertised.as_ref())
    {
        let wants: Vec<_> = refs.values().copied().collect();
        crate::wire::fetch(tool, url, cache.path(), advertised, &wants, cancel)?;
        let fetched = crate::wire::refs(tool, url, cancel)?;
        if refs
            .iter()
            .any(|(name, id)| fetched.refs.get(name) != Some(id))
            || head != fetched.head
        {
            return Err(Error::InvalidSource(
                "selected remote references moved during source inventory".into(),
            ));
        }
    }
    let branches = refs
        .iter()
        .filter(|(name, _)| name.starts_with("refs/heads/"))
        .map(|(name, object)| (name.clone(), *object))
        .collect();
    let mut result = Stage {
        inventory: GitInventory {
            object_format: "sha1".into(),
            branches,
            omitted_refs,
            symbolic_head: head,
            commits: 0,
            trees: 0,
            blobs: 0,
            unsupported,
        },
        commits: BTreeMap::new(),
        order: Vec::new(),
        trees: BTreeMap::new(),
        blobs: BTreeMap::new(),
        _cache: cache,
    };
    if !result.inventory.unsupported.is_empty() {
        return Ok(result);
    }
    let mut loader = Loader {
        tool,
        total_bytes: 0,
        object_count: 0,
        cancel,
    };
    let mut pending: Vec<(GitObjectId, bool)> = result
        .inventory
        .branches
        .values()
        .map(|id| (*id, false))
        .collect();
    let mut visiting = BTreeSet::new();
    while let Some((id, ready)) = pending.pop() {
        if result.order.len() >= tool.config.limits.max_objects {
            return Err(Error::Limit("Git commit history"));
        }
        if ready {
            visiting.remove(&id);
            if !result.order.contains(&id) {
                result.order.push(id);
            }
            continue;
        }
        if result.order.contains(&id) {
            continue;
        }
        if !visiting.insert(id) {
            return Err(Error::InvalidObject {
                object: id.to_string(),
                reason: "commit parent cycle".into(),
            });
        }
        let commit = objects::commit(id, loader.read(result._cache.path(), id, "commit")?)?;
        result
            .inventory
            .unsupported
            .extend(commit.unsupported.clone());
        pending.push((id, true));
        pending.extend(commit.parents.iter().rev().map(|id| (*id, false)));
        load_tree(&mut loader, &mut result, commit.tree, 0)?;
        result.commits.insert(id, commit);
    }
    result.inventory.commits = result.commits.len();
    result.inventory.trees = result.trees.len();
    result.inventory.blobs = result.blobs.len();
    let source_trees: BTreeSet<_> = result.commits.values().map(|commit| commit.tree).collect();
    for tree in source_trees {
        validate_source_namespace(
            &result,
            tree,
            "",
            &mut BTreeMap::new(),
            &tool.config.limits,
            0,
            cancel,
        )?;
    }
    result.inventory.unsupported.sort();
    result.inventory.unsupported.dedup();
    Ok(result)
}

fn validate_source_namespace(
    stage: &Stage,
    tree: GitObjectId,
    prefix: &str,
    paths: &mut BTreeMap<String, izu_model::RepoPath>,
    limits: &GitLimits,
    depth: usize,
    cancel: &CancellationToken,
) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if depth > limits.max_tree_depth {
        return Err(Error::Limit("source namespace depth"));
    }
    let entries = stage
        .trees
        .get(&tree)
        .ok_or_else(|| Error::InvalidSource("staged source directory is missing".into()))?;
    for entry in entries {
        if paths.len() >= limits.max_tree_entries {
            return Err(Error::Limit("source namespace entries"));
        }
        let name = if prefix.is_empty() {
            entry.name.clone()
        } else {
            format!("{prefix}/{}", entry.name)
        };
        let path = izu_model::RepoPath::new(&name)
            .map_err(|error| Error::InvalidSource(format!("source namespace path: {error}")))?;
        let key = path
            .namespace_key()
            .map_err(|error| Error::InvalidSource(format!("source namespace key: {error}")))?;
        if let Some(previous) = paths.insert(key, path.clone()) {
            return Err(Error::InvalidSource(format!(
                "source namespace aliases: {} and {}",
                previous.as_str(),
                path.as_str()
            )));
        }
        if entry.mode == 0o40000 {
            validate_source_namespace(
                stage,
                entry.object,
                &name,
                paths,
                limits,
                depth + 1,
                cancel,
            )?;
        }
    }
    Ok(())
}

struct Loader<'a> {
    tool: &'a GitTool,
    total_bytes: usize,
    object_count: usize,
    cancel: &'a CancellationToken,
}
impl Loader<'_> {
    fn read(&mut self, directory: &Path, id: GitObjectId, kind: &str) -> Result<Vec<u8>> {
        if self.object_count >= self.tool.config.limits.max_objects {
            return Err(Error::Limit("Git object count"));
        }
        let arguments = GitTool::args(&["cat-file", kind, &id.to_string()]);
        let bytes = self
            .tool
            .run(
                Some(directory),
                "read verified object",
                &arguments,
                &[],
                self.tool.config.limits.max_object_bytes,
                self.cancel,
            )?
            .stdout;
        self.total_bytes = self
            .total_bytes
            .checked_add(bytes.len())
            .ok_or(Error::Limit("total Git object bytes"))?;
        if self.total_bytes > self.tool.config.limits.max_total_object_bytes {
            return Err(Error::Limit("total Git object bytes"));
        }
        self.object_count += 1;
        if objects::object_id(kind, &bytes) != id {
            return Err(Error::InvalidObject {
                object: id.to_string(),
                reason: "object envelope hash does not match advertised ID".into(),
            });
        }
        // Git's own hash implementation is an additional interchange boundary.
        // Native object integrity uses SHA-256, never these legacy IDs.
        let arguments = GitTool::args(&["hash-object", "-t", kind, "--stdin"]);
        let computed = self
            .tool
            .run(
                Some(directory),
                "validate object with installed Git",
                &arguments,
                &bytes,
                128,
                self.cancel,
            )?
            .stdout;
        let computed = std::str::from_utf8(&computed)
            .map_err(|_| Error::InvalidObject {
                object: id.to_string(),
                reason: "invalid object hash response".into(),
            })?
            .trim()
            .parse::<GitObjectId>()?;
        if computed != id {
            return Err(Error::InvalidObject {
                object: id.to_string(),
                reason: "installed Git object hash mismatch".into(),
            });
        }
        Ok(bytes)
    }
}

fn load_tree(
    loader: &mut Loader<'_>,
    stage: &mut Stage,
    id: GitObjectId,
    depth: usize,
) -> Result<()> {
    if depth > loader.tool.config.limits.max_tree_depth {
        return Err(Error::Limit("Git tree depth"));
    }
    if stage.trees.contains_key(&id) {
        return Ok(());
    }
    let bytes = loader.read(stage._cache.path(), id, "tree")?;
    let entries = objects::tree(id, &bytes, loader.tool.config.limits.max_tree_entries)?;
    for entry in &entries {
        if entry.name == ".gitattributes" || entry.name == ".gitmodules" {
            stage
                .inventory
                .unsupported
                .push(format!("source feature file {}", entry.name));
        }
        match entry.mode {
            0o40000 => load_tree(loader, stage, entry.object, depth + 1)?,
            0o160000 => stage
                .inventory
                .unsupported
                .push("Git submodule (gitlink)".into()),
            0o100644 | 0o100755 | 0o120000 => {
                if !stage.blobs.contains_key(&entry.object) {
                    let bytes = loader.read(stage._cache.path(), entry.object, "blob")?;
                    if bytes.starts_with(b"version https://git-lfs.github.com/spec/v1\n") {
                        stage.inventory.unsupported.push("Git LFS pointer".into());
                    }
                    stage.blobs.insert(entry.object, bytes);
                }
            }
            mode => stage
                .inventory
                .unsupported
                .push(format!("unsupported Git tree mode {mode:o}")),
        }
    }
    stage.trees.insert(id, entries);
    Ok(())
}

pub(crate) fn target_ref(
    tool: &GitTool,
    source: &GitSource,
    name: &str,
    cancel: &CancellationToken,
) -> Result<Option<GitObjectId>> {
    validate_git_ref(name)?;
    match source {
        GitSource::Local(path) => {
            let (refs, _, unsupported) = local_refs(&local_git_dir(path)?, &tool.config.limits)?;
            if !unsupported.is_empty() {
                return Err(Error::Unsupported(unsupported));
            }
            Ok(refs.get(name).copied())
        }
        GitSource::Https(url) => crate::wire::target_ref(tool, url, name, cancel),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    #[test]
    fn metadata_fifo_is_rejected_without_blocking() {
        let fixture = tempfile::tempdir().expect("fixture");
        let path = fixture.path().join("metadata");
        #[cfg(not(target_vendor = "apple"))]
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::from_raw_mode(0o600),
        )
        .expect("FIFO fixture");
        #[cfg(target_vendor = "apple")]
        assert!(
            std::process::Command::new("/usr/bin/mkfifo")
                .arg(&path)
                .status()
                .expect("owned FIFO fixture utility")
                .success()
        );
        let started = std::time::Instant::now();
        assert!(matches!(
            read_bounded(&path, 4096),
            Err(Error::InvalidSource(_))
        ));
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }
}
