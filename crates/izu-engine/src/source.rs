//! Anchored source operations. Every parent is an opened, no-follow directory.
use crate::error::io;
use crate::{CaptureStats, EngineError, Result, Selection, SourceLimits};
use ignore::gitignore::{Gitignore, GitignoreBuilder};
use izu_model::{
    CancellationToken, FileMode, ObjectId, ObjectKind, RepoPath, SymlinkTarget, Tree, TreeEntry,
    encode_metadata,
};
use izu_platform::{Directory, sync_file};
use izu_store::{ObjectBatch, StagedObjectId, Store};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, Permissions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

mod fresh;

#[cfg(unix)]
use std::os::unix::{
    ffi::{OsStrExt, OsStringExt},
    fs::{MetadataExt, PermissionsExt},
};

pub(crate) fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(EngineError::Cancelled)
    } else {
        Ok(())
    }
}

pub(crate) fn protected(path: &RepoPath) -> bool {
    path.as_str().split('/').any(is_internal)
}

fn is_internal(component: &str) -> bool {
    component.eq_ignore_ascii_case(".izu")
        || component.eq_ignore_ascii_case(".git")
        || component.eq_ignore_ascii_case(".izu-recovery")
        || component.eq_ignore_ascii_case(".ezy")
        || component.eq_ignore_ascii_case(".ezy-recovery")
}

pub(crate) fn validate_selection(selection: &Selection) -> Result<()> {
    match selection {
        Selection::All => Ok(()),
        Selection::Paths(paths) => {
            if paths.is_empty() {
                return Err(EngineError::InvalidInput("empty path selection".into()));
            }
            for path in paths {
                if protected(path) {
                    return Err(EngineError::ProtectedPath(path.as_str().into()));
                }
            }
            Ok(())
        }
    }
}

/// Reject case aliases proactively on all hosts, so a saved tree remains portable
/// to the supported case-insensitive host without silently replacing one entry.
pub(crate) fn validate_tree(tree: &Tree) -> Result<()> {
    let mut folded = BTreeMap::<String, &RepoPath>::new();
    for (path, entry) in &tree.entries {
        if protected(path) {
            return Err(EngineError::ProtectedPath(path.as_str().into()));
        }
        let lower = path.namespace_key()?;
        if let Some(previous) = folded.insert(lower, path)
            && previous != path
        {
            return Err(EngineError::PathCollision(format!(
                "{} and {}",
                previous.as_str(),
                path.as_str()
            )));
        }
        let mut prefix = String::new();
        let components: Vec<_> = path.as_str().split('/').collect();
        for component in components.iter().take(components.len().saturating_sub(1)) {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(component);
            let parent = RepoPath::new(prefix.clone())?;
            if !matches!(
                tree.entries.get(&parent),
                Some(TreeEntry::Directory { .. } | TreeEntry::Conflict { .. })
            ) {
                return Err(EngineError::PathCollision(path.as_str().into()));
            }
        }
        if let TreeEntry::Symlink { target } = entry
            && target.as_bytes().len() > 65_535
        {
            return Err(EngineError::Limit {
                resource: "symlink target bytes",
                limit: 65_535,
            });
        }
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn same_metadata(before: &Metadata, after: &Metadata) -> bool {
    before.dev() == after.dev()
        && before.ino() == after.ino()
        && before.nlink() == after.nlink()
        && before.len() == after.len()
        && before.mode() == after.mode()
        && before.mtime() == after.mtime()
        && before.mtime_nsec() == after.mtime_nsec()
        && before.ctime() == after.ctime()
        && before.ctime_nsec() == after.ctime_nsec()
}

#[cfg(not(unix))]
pub(crate) fn same_metadata(_before: &Metadata, _after: &Metadata) -> bool {
    false
}

#[cfg(unix)]
fn mode(metadata: &Metadata) -> Result<FileMode> {
    Ok(FileMode::from_unix_permissions(metadata.mode() & 0o7777)?)
}
#[cfg(not(unix))]
fn mode(_metadata: &Metadata) -> Result<FileMode> {
    Err(EngineError::UnsupportedPlatform("POSIX source modes"))
}

struct Walker<'a, 'store> {
    batch: &'a mut ObjectBatch<'store>,
    root: &'a Path,
    selection: &'a Selection,
    tracked: &'a BTreeSet<RepoPath>,
    limits: &'a SourceLimits,
    cancel: &'a CancellationToken,
    include_ignored: bool,
    tree: Tree,
    stats: CaptureStats,
    visited: usize,
    ignores: Vec<Gitignore>,
}

impl Walker<'_, '_> {
    fn bump_entry(&mut self) -> Result<()> {
        self.visited = self.visited.checked_add(1).ok_or(EngineError::Limit {
            resource: "source entries",
            limit: self.limits.max_entries as u64,
        })?;
        if self.visited > self.limits.max_entries {
            return Err(EngineError::Limit {
                resource: "source entries",
                limit: self.limits.max_entries as u64,
            });
        }
        check(self.cancel)
    }

    fn ignored(&self, path: &RepoPath, directory: bool) -> bool {
        if self.include_ignored {
            return false;
        }
        let absolute = self.root.join(path.as_str());
        let mut ignored = false;
        for matcher in &self.ignores {
            match matcher.matched_path_or_any_parents(&absolute, directory) {
                ignore::Match::Ignore(_) => ignored = true,
                ignore::Match::Whitelist(_) => ignored = false,
                ignore::Match::None => {}
            }
        }
        ignored
    }

    fn load_ignore(&mut self, directory: &Directory, relative: &str) -> Result<usize> {
        let previous = self.ignores.len();
        if self.include_ignored {
            return Ok(previous);
        }
        let mut builder = GitignoreBuilder::new(self.root.join(relative));
        let mut any = false;
        for filename in [".gitignore", ".izuignore"] {
            let path = self.root.join(relative).join(filename);
            let metadata = match directory.metadata(OsStr::new(filename)) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(io(&path, error)),
            };
            // A symlink named .gitignore is versioned data, never a rule source.
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > self.limits.max_ignore_bytes {
                return Err(EngineError::Limit {
                    resource: "ignore bytes",
                    limit: self.limits.max_ignore_bytes,
                });
            }
            let mut file = directory
                .open_read(OsStr::new(filename))
                .map_err(|error| io(&path, error))?;
            let mut bytes = Vec::new();
            let length = usize::try_from(metadata.len()).map_err(|_| EngineError::Limit {
                resource: "ignore bytes",
                limit: self.limits.max_ignore_bytes,
            })?;
            bytes
                .try_reserve_exact(length)
                .map_err(|_| EngineError::Allocation {
                    resource: "ignore file",
                })?;
            Read::by_ref(&mut file)
                .take(self.limits.max_ignore_bytes.saturating_add(1))
                .read_to_end(&mut bytes)
                .map_err(|error| io(&path, error))?;
            if bytes.len() as u64 > self.limits.max_ignore_bytes {
                return Err(EngineError::Limit {
                    resource: "ignore bytes",
                    limit: self.limits.max_ignore_bytes,
                });
            }
            let after = file.metadata().map_err(|error| io(&path, error))?;
            let path_after = directory
                .metadata(OsStr::new(filename))
                .map_err(|error| io(&path, error))?;
            if !same_metadata(&metadata, &after) || !same_metadata(&metadata, &path_after) {
                return Err(EngineError::SourceChanged(path));
            }
            let text = std::str::from_utf8(&bytes).map_err(|_| {
                EngineError::InvalidInput(format!("ignore file is not UTF-8: {}", path.display()))
            })?;
            for line in text.lines() {
                builder
                    .add_line(Some(path.clone()), line)
                    .map_err(|error| {
                        EngineError::InvalidInput(format!("{}: {error}", path.display()))
                    })?;
            }
            any = true;
        }
        if any {
            self.ignores
                .try_reserve(1)
                .map_err(|_| EngineError::Allocation {
                    resource: "ignore matchers",
                })?;
            self.ignores.push(
                builder
                    .build()
                    .map_err(|error| EngineError::InvalidInput(error.to_string()))?,
            );
        }
        Ok(previous)
    }

    fn walk(&mut self, directory: &Directory, relative: &str, depth: usize) -> Result<()> {
        if depth > self.limits.max_depth {
            return Err(EngineError::Limit {
                resource: "source depth",
                limit: self.limits.max_depth as u64,
            });
        }
        check(self.cancel)?;
        let before = directory
            .metadata_self()
            .map_err(|error| io(self.root.join(relative), error))?;
        let previous_ignores = self.load_ignore(directory, relative)?;
        let entries = directory
            .entries()
            .map_err(|error| io(self.root.join(relative), error))?;
        for entry in entries {
            self.bump_entry()?;
            let entry = entry.map_err(|error| io(self.root.join(relative), error))?;
            let name = entry.name.to_str().ok_or_else(|| {
                EngineError::IncompatiblePath(self.root.join(relative).join(&entry.name))
            })?;
            if is_internal(name) {
                continue;
            }
            let joined = if relative.is_empty() {
                name.to_owned()
            } else {
                format!("{relative}/{name}")
            };
            let path = RepoPath::new(joined.clone())?;
            let absolute = self.root.join(&joined);
            let metadata = directory
                .metadata(&entry.name)
                .map_err(|error| io(&absolute, error))?;
            if metadata.is_dir() {
                if !self.selection.may_descend(&path) {
                    continue;
                }
                let tracked_below = self.tracked.iter().any(|tracked| {
                    tracked == &path
                        || tracked
                            .as_str()
                            .strip_prefix(path.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                });
                if self.ignored(&path, true) && !tracked_below {
                    continue;
                }
                let child = directory
                    .open_dir(&entry.name)
                    .map_err(|error| io(&absolute, error))?;
                if self.selection.includes(&path)
                    && (!self.ignored(&path, true) || self.tracked.contains(&path))
                {
                    self.tree.entries.insert(
                        path.clone(),
                        TreeEntry::Directory {
                            mode: mode(&metadata)?,
                        },
                    );
                    self.stats.entries =
                        self.stats
                            .entries
                            .checked_add(1)
                            .ok_or(EngineError::Limit {
                                resource: "source entries",
                                limit: self.limits.max_entries as u64,
                            })?;
                }
                self.walk(&child, &joined, depth + 1)?;
                // A selected child needs a recorded parent mode. Existing
                // unselected parent metadata remains the captured baseline.
                if !self.tree.entries.contains_key(&path)
                    && self.tree.entries.keys().any(|child| {
                        child
                            .as_str()
                            .strip_prefix(path.as_str())
                            .is_some_and(|rest| rest.starts_with('/'))
                    })
                {
                    self.tree.entries.insert(
                        path.clone(),
                        TreeEntry::Directory {
                            mode: mode(&metadata)?,
                        },
                    );
                }
                let path_after = directory
                    .metadata(&entry.name)
                    .map_err(|error| io(&absolute, error))?;
                let child_after = child
                    .metadata_self()
                    .map_err(|error| io(&absolute, error))?;
                if !same_metadata(&metadata, &path_after)
                    || !same_metadata(&path_after, &child_after)
                {
                    return Err(EngineError::SourceChanged(absolute));
                }
            } else if self.selection.includes(&path)
                && (!self.ignored(&path, false) || self.tracked.contains(&path))
            {
                let bytes = if metadata.is_file() {
                    metadata.len()
                } else {
                    0
                };
                let total = self
                    .stats
                    .bytes
                    .checked_add(bytes)
                    .ok_or(EngineError::Limit {
                        resource: "total source bytes",
                        limit: self.limits.max_total_bytes,
                    })?;
                if total > self.limits.max_total_bytes {
                    return Err(EngineError::Limit {
                        resource: "total source bytes",
                        limit: self.limits.max_total_bytes,
                    });
                }
                let value = read_entry(
                    directory,
                    &entry.name,
                    &absolute,
                    &metadata,
                    self.limits,
                    self.cancel,
                    |file, length| {
                        self.batch
                            .stage_blob(file, length, self.cancel)
                            .map(StagedObjectId::object_id)
                    },
                )?;
                self.stats.bytes = total;
                self.stats.entries =
                    self.stats
                        .entries
                        .checked_add(1)
                        .ok_or(EngineError::Limit {
                            resource: "source entries",
                            limit: self.limits.max_entries as u64,
                        })?;
                self.tree.entries.insert(path, value);
            }
        }
        self.ignores.truncate(previous_ignores);
        let after = directory
            .metadata_self()
            .map_err(|error| io(self.root.join(relative), error))?;
        if !same_metadata(&before, &after) {
            return Err(EngineError::SourceChanged(self.root.join(relative)));
        }
        Ok(())
    }
}

fn read_entry(
    directory: &Directory,
    name: &OsStr,
    absolute: &Path,
    metadata: &Metadata,
    limits: &SourceLimits,
    cancel: &CancellationToken,
    write_blob: impl FnOnce(&mut File, u64) -> izu_store::Result<ObjectId>,
) -> Result<TreeEntry> {
    check(cancel)?;
    let entry = if metadata.is_file() {
        #[cfg(unix)]
        if metadata.nlink() != 1 {
            return Err(EngineError::UnsupportedEntry(absolute.into()));
        }
        if metadata.len() > limits.max_blob_bytes {
            return Err(EngineError::Limit {
                resource: "source blob bytes",
                limit: limits.max_blob_bytes,
            });
        }
        let mut file = directory
            .open_read(name)
            .map_err(|error| io(absolute, error))?;
        let opened = file.metadata().map_err(|error| io(absolute, error))?;
        #[cfg(unix)]
        if opened.nlink() != 1 {
            return Err(EngineError::UnsupportedEntry(absolute.into()));
        }
        if !same_metadata(metadata, &opened) {
            return Err(EngineError::SourceChanged(absolute.into()));
        }
        let result = write_blob(&mut file, metadata.len());
        let after = file.metadata().map_err(|error| io(absolute, error))?;
        let path_after = directory
            .metadata(name)
            .map_err(|error| io(absolute, error))?;
        if !same_metadata(metadata, &after) || !same_metadata(metadata, &path_after) {
            return Err(EngineError::SourceChanged(absolute.into()));
        }
        TreeEntry::File {
            blob: result?,
            mode: mode(metadata)?,
        }
    } else if metadata.file_type().is_symlink() {
        let target = directory
            .read_link(name, 65_535)
            .map_err(|error| io(absolute, error))?;
        #[cfg(unix)]
        let target = SymlinkTarget::new(target.as_os_str().as_bytes().to_vec())?;
        #[cfg(not(unix))]
        return Err(EngineError::UnsupportedPlatform("POSIX symlink bytes"));
        #[cfg(unix)]
        {
            TreeEntry::Symlink { target }
        }
    } else {
        return Err(EngineError::UnsupportedEntry(absolute.into()));
    };
    let after = directory
        .metadata(name)
        .map_err(|error| io(absolute, error))?;
    if !same_metadata(metadata, &after) {
        return Err(EngineError::SourceChanged(absolute.into()));
    }
    Ok(entry)
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn capture_at(
    store: &Store,
    directory: &Directory,
    root: &Path,
    baseline: &Tree,
    tracked: &BTreeSet<RepoPath>,
    selection: &Selection,
    limits: &SourceLimits,
    cancel: &CancellationToken,
    include_ignored: bool,
) -> Result<(Tree, CaptureStats)> {
    validate_selection(selection)?;
    validate_tree(baseline)?;
    check(cancel)?;
    if baseline.entries.len() > limits.max_entries {
        return Err(EngineError::Limit {
            resource: "baseline source entries",
            limit: limits.max_entries as u64,
        });
    }
    let mut tree = baseline.clone();
    tree.entries.retain(|path, _| !selection.includes(path));
    let mut batch = store.begin_object_batch(cancel)?;
    let mut walker = Walker {
        batch: &mut batch,
        root,
        selection,
        tracked,
        limits,
        cancel,
        include_ignored,
        tree,
        stats: CaptureStats::default(),
        visited: 0,
        ignores: Vec::new(),
    };
    walker.walk(directory, "", 0)?;
    validate_tree(&walker.tree)?;
    let tree = walker.tree;
    let stats = walker.stats;
    let encoded_tree = encode_metadata(&tree, &store.options().limits)?;
    batch.stage(ObjectKind::Tree, &encoded_tree, cancel)?;
    // Addresses inside the tree remain provisional until all captured objects
    // and their directory entries have completed the batch's durable barrier.
    batch.finish(cancel)?;
    Ok((tree, stats))
}

pub(crate) fn overlay(baseline: &Tree, captured: &Tree, selection: &Selection) -> Result<Tree> {
    let mut tree = baseline.clone();
    tree.entries.retain(|path, _| !selection.includes(path));
    for (path, entry) in &captured.entries {
        if selection.includes(path) {
            tree.entries.insert(path.clone(), entry.clone());
        }
    }
    let paths: Vec<_> = tree.entries.keys().cloned().collect();
    for path in paths {
        for (index, byte) in path.as_str().bytes().enumerate() {
            if byte != b'/' {
                continue;
            }
            let ancestor = RepoPath::new(&path.as_str()[..index])?;
            if let std::collections::btree_map::Entry::Vacant(vacant) =
                tree.entries.entry(ancestor.clone())
            {
                let entry = captured
                    .entries
                    .get(&ancestor)
                    .ok_or_else(|| EngineError::PathCollision(path.as_str().into()))?;
                vacant.insert(entry.clone());
            }
        }
    }
    validate_tree(&tree)?;
    Ok(tree)
}

/// Only selected target entries require new ancestors. Existing unselected
/// directories keep their observed modes, including ignored recovery source.
pub(crate) fn complete_restore_ancestors(
    result: &mut Tree,
    target: &Tree,
    before: &Tree,
    selection: &Selection,
) -> Result<()> {
    for path in target
        .entries
        .keys()
        .filter(|path| selection.includes(path))
    {
        for (index, byte) in path.as_str().bytes().enumerate() {
            if byte != b'/' {
                continue;
            }
            let ancestor = RepoPath::new(&path.as_str()[..index])?;
            match result.entries.get(&ancestor) {
                Some(TreeEntry::Directory { .. }) => continue,
                Some(_) => return Err(EngineError::PathCollision(path.as_str().into())),
                None => {}
            }
            let entry = before
                .entries
                .get(&ancestor)
                .or_else(|| target.entries.get(&ancestor));
            let Some(entry @ TreeEntry::Directory { .. }) = entry else {
                return Err(EngineError::PathCollision(path.as_str().into()));
            };
            result.entries.insert(ancestor, entry.clone());
        }
    }
    validate_tree(result)
}

fn parent(
    root: &Directory,
    path: &RepoPath,
    create: bool,
    root_path: &Path,
) -> Result<(Directory, OsString)> {
    let mut components = path.as_str().split('/').peekable();
    let mut directory = root.try_clone().map_err(|error| io(root_path, error))?;
    while let Some(component) = components.next() {
        if components.peek().is_none() {
            return Ok((directory, OsString::from(component)));
        }
        let next = if create {
            directory.ensure_dir(OsStr::new(component))
        } else {
            directory.open_dir(OsStr::new(component))
        };
        let child = next.map_err(|error| io(root_path.join(path.as_str()), error))?;
        if create {
            directory.sync().map_err(|error| io(root_path, error))?;
        }
        directory = child;
    }
    Err(EngineError::InvalidInput("empty source path".into()))
}

fn random_name(prefix: &str) -> Result<OsString> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| EngineError::InvalidInput(format!("random source identifier: {error}")))?;
    let mut name = prefix.to_owned();
    for byte in bytes {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}").map_err(|_| EngineError::Allocation {
            resource: "temporary path",
        })?;
    }
    Ok(name.into())
}

/// Restores only the computed differences. Original regular files and symlinks
/// are moved into a durable recovery directory, never unlinked. This also keeps
/// bytes written through an already-open external file handle recoverable.
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_at(
    store: &Store,
    root: &Directory,
    root_path: &Path,
    before: &Tree,
    after: &Tree,
    recovery_name: &str,
    limits: &SourceLimits,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    validate_tree(before)?;
    validate_materialization(store, after, limits, cancel)?;
    let recovery_root = root
        .ensure_dir(OsStr::new(".izu-recovery"))
        .map_err(|error| io(root_path, error))?;
    root.sync().map_err(|error| io(root_path, error))?;
    let recovery = recovery_root
        .create_dir(OsStr::new(recovery_name))
        .map_err(|error| io(root_path, error))?;
    recovery_root.sync().map_err(|error| io(root_path, error))?;
    let recovery_path = root_path.join(".izu-recovery").join(recovery_name);
    let mut changed: Vec<RepoPath> = Vec::new();
    changed
        .try_reserve(before.entries.len().saturating_add(after.entries.len()))
        .map_err(|_| EngineError::Allocation {
            resource: "restore paths",
        })?;
    for path in before.entries.keys().chain(after.entries.keys()) {
        if before.entries.get(path) != after.entries.get(path) {
            changed.push(path.clone());
        }
    }
    changed.sort();
    changed.dedup();
    // Remove leaf entries first so file/directory replacements never traverse a
    // symlink and cannot recursively delete unselected children.
    let mut removals = changed.clone();
    removals.sort_by_key(|path| std::cmp::Reverse(path.as_str().split('/').count()));
    for path in &removals {
        check(cancel)?;
        let Some(old) = before.entries.get(path) else {
            continue;
        };
        let (directory, name) = parent(root, path, false, root_path)?;
        if matches!(old, TreeEntry::Directory { .. }) {
            if !matches!(after.entries.get(path), Some(TreeEntry::Directory { .. })) {
                directory
                    .remove_dir(&name)
                    .map_err(|error| io(root_path.join(path.as_str()), error))?;
                directory.sync().map_err(|error| io(root_path, error))?;
            }
        } else {
            let mut recovery_parent = recovery
                .try_clone()
                .map_err(|error| io(&recovery_path, error))?;
            let parts: Vec<_> = path.as_str().split('/').collect();
            for part in parts.iter().take(parts.len().saturating_sub(1)) {
                let next = recovery_parent
                    .ensure_dir(OsStr::new(part))
                    .map_err(|error| io(&recovery_path, error))?;
                recovery_parent
                    .sync()
                    .map_err(|error| io(&recovery_path, error))?;
                recovery_parent = next;
            }
            directory
                .rename_noreplace(&name, &recovery_parent, &name)
                .map_err(|error| io(root_path.join(path.as_str()), error))?;
            directory.sync().map_err(|error| io(root_path, error))?;
            recovery_parent
                .sync()
                .map_err(|error| io(&recovery_path, error))?;
            let moved_metadata = recovery_parent
                .metadata(&name)
                .map_err(|error| io(&recovery_path, error))?;
            let moved = read_entry(
                &recovery_parent,
                &name,
                &recovery_path.join(path.as_str()),
                &moved_metadata,
                limits,
                cancel,
                |file, length| store.put_blob(file, length, cancel),
            )?;
            if &moved != old {
                return Err(EngineError::SourceChanged(root_path.join(path.as_str())));
            }
        }
    }
    install_entries(
        store,
        root,
        root_path,
        after,
        changed,
        Destination::Recovery {
            directory: &recovery,
            path: &recovery_path,
        },
        limits,
        cancel,
    )?;
    recovery.sync().map_err(|error| io(&recovery_path, error))?;
    Ok(recovery_path)
}

/// A caller-selected target may be visible during preparation. Its source is
/// provisional until every barrier/readback and workspace registration succeeds.
#[allow(clippy::too_many_arguments)]
pub(crate) fn materialize_empty_at(
    store: &Store,
    root: &Directory,
    root_path: &Path,
    after: &Tree,
    limits: &SourceLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    validate_materialization(store, after, limits, cancel)?;
    let target = fresh::FreshTarget::new(
        root,
        root_path,
        after,
        store.options().max_in_memory_bytes,
        cancel,
    )?;
    let mut additions = Vec::new();
    additions
        .try_reserve_exact(after.entries.len())
        .map_err(|_| EngineError::Allocation {
            resource: "fresh source paths",
        })?;
    additions.extend(after.entries.keys().cloned());
    let batch = target.begin(cancel)?;
    install_entries(
        store,
        root,
        root_path,
        after,
        additions,
        Destination::Fresh(batch),
        limits,
        cancel,
    )
}

fn validate_materialization(
    store: &Store,
    after: &Tree,
    limits: &SourceLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    validate_tree(after)?;
    check(cancel)?;
    if after.entries.len() > limits.max_entries {
        return Err(EngineError::Limit {
            resource: "restored tree entries",
            limit: limits.max_entries as u64,
        });
    }
    let mut total_bytes = 0_u64;
    for (path, entry) in &after.entries {
        if path.as_str().split('/').count() > limits.max_depth {
            return Err(EngineError::Limit {
                resource: "restored path depth",
                limit: limits.max_depth as u64,
            });
        }
        if let TreeEntry::File { blob, .. } = entry {
            let info = store.object_info(*blob, cancel)?;
            if info.kind != izu_model::ObjectKind::Blob {
                return Err(EngineError::InvalidInput(
                    "source file refers to a non-blob object".into(),
                ));
            }
            if info.payload_len > limits.max_blob_bytes {
                return Err(EngineError::Limit {
                    resource: "restored blob bytes",
                    limit: limits.max_blob_bytes,
                });
            }
            total_bytes = total_bytes
                .checked_add(info.payload_len)
                .ok_or(EngineError::Limit {
                    resource: "restored total bytes",
                    limit: limits.max_total_bytes,
                })?;
            if total_bytes > limits.max_total_bytes {
                return Err(EngineError::Limit {
                    resource: "restored total bytes",
                    limit: limits.max_total_bytes,
                });
            }
        }
        match entry {
            TreeEntry::Conflict { .. } => {
                return Err(EngineError::InvalidInput(format!(
                    "unresolved source conflict at {}",
                    path.as_str()
                )));
            }
            TreeEntry::File { mode, .. } | TreeEntry::Directory { mode }
                if mode.unix_permissions() & 0o7000 != 0 =>
            {
                return Err(EngineError::InvalidInput(format!(
                    "special permission bits require separate authorization: {}",
                    path.as_str()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

enum Destination<'a> {
    Recovery {
        directory: &'a Directory,
        path: &'a Path,
    },
    Fresh(fresh::DirectoryBatch<'a>),
}

#[allow(clippy::too_many_arguments)]
fn install_entries(
    store: &Store,
    root: &Directory,
    root_path: &Path,
    after: &Tree,
    mut additions: Vec<RepoPath>,
    mut destination: Destination<'_>,
    limits: &SourceLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    // Ensure directories first, in shallow-to-deep order. Mode restoration is
    // deferred until children exist, including read-only directory modes.
    additions.sort_by_key(|path| path.as_str().split('/').count());
    for path in &additions {
        check(cancel)?;
        let Some(entry) = after.entries.get(path) else {
            continue;
        };
        let (directory, name) = match &destination {
            Destination::Recovery { .. } => parent(root, path, true, root_path)?,
            Destination::Fresh(batch) => batch.parent(path)?,
        };
        match entry {
            TreeEntry::Directory { .. } => match &mut destination {
                Destination::Recovery { .. } => {
                    directory
                        .ensure_dir(&name)
                        .map_err(|error| io(root_path.join(path.as_str()), error))?;
                    directory.sync().map_err(|error| io(root_path, error))?;
                }
                Destination::Fresh(batch) => {
                    let child = directory
                        .create_dir(&name)
                        .map_err(|error| io(root_path.join(path.as_str()), error))?;
                    batch.record_directory(path, &child)?;
                }
            },
            TreeEntry::File { blob, mode } => match &mut destination {
                Destination::Recovery {
                    directory: recovery,
                    path: recovery_path,
                } => {
                    let temp = random_name(".izu-tmp-")?;
                    let (mut file, temporary) =
                        TemporaryEntry::file(recovery, temp, recovery_path)?;
                    install_blob(store, &mut file, *blob, *mode, root_path, limits, cancel)?;
                    recovery
                        .hard_link(&temporary.name, &directory, &name)
                        .map_err(|error| io(root_path.join(path.as_str()), error))?;
                    directory.sync().map_err(|error| io(root_path, error))?;
                    temporary.cleanup()?;
                }
                Destination::Fresh(batch) => {
                    // Final names are provisional until the whole fork succeeds.
                    // Failure retains partial source; no fresh name is unlinked.
                    let mut file = directory
                        .create_new_file(&name)
                        .map_err(|error| io(root_path.join(path.as_str()), error))?;
                    let original = batch.file_created(path, &directory, &name, &file)?;
                    check(cancel)?;
                    install_blob(store, &mut file, *blob, *mode, root_path, limits, cancel)?;
                    #[cfg(test)]
                    fresh::test_support::at(fresh::Boundary::FileDataSynced, root_path)?;
                    check(cancel)?;
                    batch.record_file(path, &directory, &name, &file, original)?;
                }
            },
            TreeEntry::Symlink { target } => {
                #[cfg(unix)]
                let target = PathBuf::from(OsString::from_vec(target.as_bytes().to_vec()));
                #[cfg(not(unix))]
                return Err(EngineError::UnsupportedPlatform("source symlinks"));
                #[cfg(unix)]
                {
                    match &mut destination {
                        Destination::Recovery {
                            directory: recovery,
                            path: recovery_path,
                        } => {
                            let temp = random_name(".izu-tmp-")?;
                            let temporary =
                                TemporaryEntry::symlink(recovery, temp, &target, recovery_path)?;
                            recovery
                                .hard_link(&temporary.name, &directory, &name)
                                .map_err(|error| io(root_path.join(path.as_str()), error))?;
                            directory.sync().map_err(|error| io(root_path, error))?;
                            temporary.cleanup()?;
                        }
                        Destination::Fresh(batch) => {
                            directory
                                .symlink(&target, &name)
                                .map_err(|error| io(root_path.join(path.as_str()), error))?;
                            batch.record_symlink(path, &directory, &name)?;
                        }
                    }
                }
            }
            TreeEntry::Conflict { .. } => {
                return Err(EngineError::InvalidInput(format!(
                    "unresolved source conflict at {}",
                    path.as_str()
                )));
            }
        }
    }
    for path in additions.iter().rev() {
        if let Some(TreeEntry::Directory { mode }) = after.entries.get(path) {
            let (directory, name) = match &destination {
                Destination::Recovery { .. } => parent(root, path, false, root_path)?,
                Destination::Fresh(batch) => batch.parent(path)?,
            };
            let child = directory
                .open_dir(&name)
                .map_err(|error| io(root_path, error))?;
            if let Destination::Fresh(batch) = &destination {
                batch.check_directory(path, &child)?;
            }
            #[cfg(unix)]
            child
                .file()
                .set_permissions(Permissions::from_mode(u32::from(mode.unix_permissions())))
                .map_err(|error| io(root_path, error))?;
            if matches!(&destination, Destination::Recovery { .. }) {
                child.sync().map_err(|error| io(root_path, error))?;
            }
        }
    }
    match destination {
        Destination::Recovery { .. } => Ok(()),
        Destination::Fresh(batch) => batch.finish(cancel),
    }
}

#[allow(clippy::too_many_arguments)]
fn install_blob(
    store: &Store,
    file: &mut File,
    blob: ObjectId,
    mode: FileMode,
    root_path: &Path,
    limits: &SourceLimits,
    cancel: &CancellationToken,
) -> Result<()> {
    let mut writer = BoundedWriter {
        inner: &mut *file,
        remaining: limits.max_blob_bytes,
    };
    store.read_blob(blob, &mut writer, cancel)?;
    #[cfg(unix)]
    file.set_permissions(Permissions::from_mode(u32::from(mode.unix_permissions())))
        .map_err(|error| io(root_path, error))?;
    sync_file(file).map_err(|error| io(root_path, error))
}

pub(crate) fn write_marker_at(root: &Directory, root_path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = root
        .create_new_file(OsStr::new(".izu"))
        .map_err(|error| io(root_path, error))?;
    file.write_all(bytes)
        .map_err(|error| io(root_path, error))?;
    sync_file(&file).map_err(|error| io(root_path, error))?;
    root.sync().map_err(|error| io(root_path, error))
}

struct BoundedWriter<W> {
    inner: W,
    remaining: u64,
}
impl<W: Write> Write for BoundedWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() as u64 > self.remaining {
            return Err(std::io::Error::other(
                "restored blob exceeded configured byte limit",
            ));
        }
        let written = self.inner.write(bytes)?;
        self.remaining = self
            .remaining
            .checked_sub(written as u64)
            .ok_or_else(|| std::io::Error::other("restored write length overflow"))?;
        Ok(written)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

struct TemporaryEntry<'a> {
    directory: &'a Directory,
    name: OsString,
    path: PathBuf,
    active: bool,
}
impl<'a> TemporaryEntry<'a> {
    fn file(
        directory: &'a Directory,
        name: OsString,
        path: &Path,
    ) -> Result<(std::fs::File, Self)> {
        let file = directory
            .create_new_file(&name)
            .map_err(|error| io(path, error))?;
        Ok((
            file,
            Self {
                directory,
                name,
                path: path.into(),
                active: true,
            },
        ))
    }
    fn symlink(
        directory: &'a Directory,
        name: OsString,
        target: &Path,
        path: &Path,
    ) -> Result<Self> {
        directory
            .symlink(target, &name)
            .map_err(|error| io(path, error))?;
        Ok(Self {
            directory,
            name,
            path: path.into(),
            active: true,
        })
    }
    fn cleanup(mut self) -> Result<()> {
        self.directory
            .remove_file(&self.name)
            .map_err(|error| io(&self.path, error))?;
        self.active = false;
        self.directory.sync().map_err(|error| io(&self.path, error))
    }
}
impl Drop for TemporaryEntry<'_> {
    fn drop(&mut self) {
        if self.active {
            // Only EXCL-created copies of retained immutable source objects are
            // removed. Crash leftovers live in the protected recovery directory.
            let _ = self.directory.remove_file(&self.name);
            let _ = self.directory.sync();
        }
    }
}
