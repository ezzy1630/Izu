//! Directory-only submission for a verified empty source target. File data keeps
//! its strict barrier; the target namespace is provisional until `finish`.
use super::{check, io};
use crate::{EngineError, Result};
use izu_model::{CancellationToken, RepoPath, Tree, TreeEntry};
use izu_platform::{Directory, sync_kernel};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io as std_io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct EntryIdentity {
    device: u64,
    inode: u64,
}

impl EntryIdentity {
    pub(super) fn metadata(metadata: &Metadata, path: &Path) -> Result<Self> {
        #[cfg(unix)]
        {
            if metadata.nlink() == 0 {
                return Err(EngineError::SourceChanged(path.into()));
            }
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = (metadata, path);
            Err(EngineError::UnsupportedPlatform("source entry identity"))
        }
    }

    fn directory(directory: &Directory, path: &Path) -> Result<Self> {
        Self::metadata(
            &directory.metadata_self().map_err(|error| io(path, error))?,
            path,
        )
    }

    fn require_device(self, participant: Self, path: &Path) -> Result<()> {
        if self.device != participant.device {
            return Err(io(
                path,
                std_io::Error::new(
                    std_io::ErrorKind::Unsupported,
                    "batched source publication requires one filesystem device",
                ),
            ));
        }
        Ok(())
    }
}

struct Binding<'a> {
    path: &'a RepoPath,
    entry: &'a TreeEntry,
    // None is a reserved, not-yet-published entry. finish requires all bindings.
    identity: Option<EntryIdentity>,
}

/// Constructed only after checking the actual pinned target and its locator.
pub(super) struct FreshTarget<'a> {
    root: &'a Directory,
    path: &'a Path,
    identity: EntryIdentity,
    bindings: Vec<Binding<'a>>,
}

impl<'a> FreshTarget<'a> {
    pub(super) fn new(
        root: &'a Directory,
        path: &'a Path,
        tree: &'a Tree,
        memory_limit: u64,
        cancel: &CancellationToken,
    ) -> Result<Self> {
        check(cancel)?;
        let identity = EntryIdentity::directory(root, path)?;
        root.require_persistent_filesystem()
            .map_err(|error| io(path, error))?;
        let bytes = tree
            .entries
            .len()
            .checked_mul(std::mem::size_of::<Binding<'_>>())
            .and_then(|bytes| bytes.checked_add(std::mem::size_of::<DirectoryBatch<'_>>()))
            .and_then(|bytes| u64::try_from(bytes).ok())
            .ok_or(EngineError::Limit {
                resource: "fresh source bookkeeping bytes",
                limit: memory_limit,
            })?;
        if bytes > memory_limit {
            return Err(EngineError::Limit {
                resource: "fresh source bookkeeping bytes",
                limit: memory_limit,
            });
        }
        let mut bindings = Vec::new();
        bindings
            .try_reserve_exact(tree.entries.len())
            .map_err(|_| EngineError::Allocation {
                resource: "fresh source identity bindings",
            })?;
        for (path, entry) in &tree.entries {
            check(cancel)?;
            bindings.push(Binding {
                path,
                entry,
                identity: None,
            });
        }
        let target = Self {
            root,
            path,
            identity,
            bindings,
        };
        target.verify_root()?;
        target.require_empty()?;
        target.verify_root()?;
        Ok(target)
    }

    fn require_empty(&self) -> Result<()> {
        if self
            .root
            .entries()
            .map_err(|error| io(self.path, error))?
            .next()
            .transpose()
            .map_err(|error| io(self.path, error))?
            .is_some()
        {
            return Err(EngineError::DirectoryNotEmpty(self.path.into()));
        }
        Ok(())
    }

    fn verify_root(&self) -> Result<()> {
        let current = Directory::open(self.path).map_err(|error| io(self.path, error))?;
        if EntryIdentity::directory(self.root, self.path)? != self.identity
            || EntryIdentity::directory(&current, self.path)? != self.identity
        {
            return Err(EngineError::SourceChanged(self.path.into()));
        }
        Ok(())
    }

    pub(super) fn begin(self, cancel: &CancellationToken) -> Result<DirectoryBatch<'a>> {
        check(cancel)?;
        self.verify_root()?;
        self.require_empty()?;
        #[cfg(test)]
        test_support::at(Boundary::TargetPrepared, self.path)?;
        Ok(DirectoryBatch { target: self })
    }
}

/// Retains observed device/inode records, not one descriptor per entry. This
/// cannot detect inode reuse or substitution before initial identity capture.
/// Traversal holds a constant number of descriptors independent of path depth.
pub(super) struct DirectoryBatch<'a> {
    target: FreshTarget<'a>,
}

impl DirectoryBatch<'_> {
    fn binding(&self, path: &str) -> Result<&Binding<'_>> {
        self.target
            .bindings
            .binary_search_by(|binding| binding.path.as_str().cmp(path))
            .ok()
            .and_then(|index| self.target.bindings.get(index))
            .ok_or_else(|| EngineError::SourceChanged(self.target.path.join(path)))
    }

    fn expected(&self, path: &str) -> Result<EntryIdentity> {
        self.binding(path)?
            .identity
            .ok_or_else(|| EngineError::SourceChanged(self.target.path.join(path)))
    }

    pub(super) fn parent(&self, path: &RepoPath) -> Result<(Directory, OsString)> {
        let (parent, name) = path
            .as_str()
            .rsplit_once('/')
            .unwrap_or(("", path.as_str()));
        Ok((self.open_directory(parent)?, OsString::from(name)))
    }

    fn open_directory(&self, relative: &str) -> Result<Directory> {
        let mut directory = self
            .target
            .root
            .try_clone()
            .map_err(|error| io(self.target.path, error))?;
        if EntryIdentity::directory(&directory, self.target.path)? != self.target.identity {
            return Err(EngineError::SourceChanged(self.target.path.into()));
        }
        // Prefixes borrow the validated path, avoiding retained ancestor strings.
        let mut end = 0_usize;
        for component in relative
            .split('/')
            .filter(|component| !component.is_empty())
        {
            end = end
                .checked_add(component.len())
                .ok_or_else(|| EngineError::SourceChanged(self.target.path.into()))?;
            let prefix = relative
                .get(..end)
                .ok_or_else(|| EngineError::SourceChanged(self.target.path.into()))?;
            let child = directory
                .open_dir(OsStr::new(component))
                .map_err(|error| io(self.target.path.join(prefix), error))?;
            self.check_directory_identity(prefix, &child)?;
            directory = child;
            if end < relative.len() {
                end = end
                    .checked_add(1)
                    .ok_or_else(|| EngineError::SourceChanged(self.target.path.into()))?;
            }
        }
        Ok(directory)
    }

    fn check_directory_identity(&self, path: &str, directory: &Directory) -> Result<()> {
        let identity = EntryIdentity::directory(directory, &self.target.path.join(path))?;
        self.target
            .identity
            .require_device(identity, self.target.path)?;
        if !matches!(self.binding(path)?.entry, TreeEntry::Directory { .. })
            || identity != self.expected(path)?
        {
            return Err(EngineError::SourceChanged(self.target.path.join(path)));
        }
        Ok(())
    }

    pub(super) fn check_directory(&self, path: &RepoPath, directory: &Directory) -> Result<()> {
        self.check_directory_identity(path.as_str(), directory)
    }

    pub(super) fn record_directory(
        &mut self,
        path: &RepoPath,
        directory: &Directory,
    ) -> Result<()> {
        self.record(
            path,
            &directory
                .metadata_self()
                .map_err(|error| io(self.target.path, error))?,
            None,
        )
    }

    pub(super) fn file_created(
        &self,
        path: &RepoPath,
        parent: &Directory,
        name: &OsStr,
        file: &File,
    ) -> Result<EntryIdentity> {
        let original = EntryIdentity::metadata(
            &file
                .metadata()
                .map_err(|error| io(self.target.path, error))?,
            self.target.path,
        )?;
        self.target
            .identity
            .require_device(original, self.target.path)?;
        self.check_file_name(path, parent, name, original)?;
        #[cfg(test)]
        test_support::at(Boundary::FileCreated, self.target.path)?;
        Ok(original)
    }

    fn check_file_name(
        &self,
        path: &RepoPath,
        parent: &Directory,
        name: &OsStr,
        original: EntryIdentity,
    ) -> Result<()> {
        let metadata = parent
            .metadata(name)
            .map_err(|error| io(self.target.path.join(path.as_str()), error))?;
        let actual = EntryIdentity::metadata(&metadata, self.target.path)?;
        self.target
            .identity
            .require_device(actual, self.target.path)?;
        if !metadata.is_file() || actual != original {
            return Err(EngineError::SourceChanged(
                self.target.path.join(path.as_str()),
            ));
        }
        Ok(())
    }

    pub(super) fn record_file(
        &mut self,
        path: &RepoPath,
        parent: &Directory,
        name: &OsStr,
        file: &File,
        original: EntryIdentity,
    ) -> Result<()> {
        // The original file remains open through its strongest sync and this
        // named-entry comparison. No partial or displaced fresh file is deleted.
        let held = EntryIdentity::metadata(
            &file
                .metadata()
                .map_err(|error| io(self.target.path, error))?,
            self.target.path,
        )?;
        self.target
            .identity
            .require_device(held, self.target.path)?;
        if held != original {
            return Err(EngineError::SourceChanged(
                self.target.path.join(path.as_str()),
            ));
        }
        self.check_file_name(path, parent, name, original)?;
        self.record(
            path,
            &parent
                .metadata(name)
                .map_err(|error| io(self.target.path, error))?,
            Some(original),
        )?;
        #[cfg(test)]
        test_support::at(Boundary::EntryPublished, self.target.path)?;
        Ok(())
    }

    pub(super) fn record_symlink(
        &mut self,
        path: &RepoPath,
        parent: &Directory,
        name: &OsStr,
    ) -> Result<()> {
        self.record(
            path,
            &parent
                .metadata(name)
                .map_err(|error| io(self.target.path, error))?,
            None,
        )?;
        #[cfg(test)]
        test_support::at(Boundary::EntryPublished, self.target.path)?;
        Ok(())
    }

    fn record(
        &mut self,
        path: &RepoPath,
        metadata: &Metadata,
        original: Option<EntryIdentity>,
    ) -> Result<()> {
        let identity = EntryIdentity::metadata(metadata, &self.target.path.join(path.as_str()))?;
        self.target
            .identity
            .require_device(identity, self.target.path)?;
        let index = self
            .target
            .bindings
            .binary_search_by(|binding| binding.path.cmp(path))
            .map_err(|_| EngineError::SourceChanged(self.target.path.join(path.as_str())))?;
        let binding = self
            .target
            .bindings
            .get_mut(index)
            .ok_or_else(|| EngineError::SourceChanged(self.target.path.join(path.as_str())))?;
        if !kind_matches(binding.entry, metadata)
            || binding.identity.is_some()
            || original.is_some_and(|original| original != identity)
        {
            return Err(EngineError::SourceChanged(
                self.target.path.join(path.as_str()),
            ));
        }
        binding.identity = Some(identity);
        Ok(())
    }

    fn check_layout(&self, cancel: &CancellationToken) -> Result<()> {
        self.target.verify_root()?;
        for binding in &self.target.bindings {
            check(cancel)?;
            let (directory, name) = self.parent(binding.path)?;
            let metadata = directory
                .metadata(&name)
                .map_err(|error| io(self.target.path.join(binding.path.as_str()), error))?;
            let identity = EntryIdentity::metadata(&metadata, self.target.path)?;
            self.target
                .identity
                .require_device(identity, self.target.path)?;
            if !kind_matches(binding.entry, &metadata) || Some(identity) != binding.identity {
                return Err(EngineError::SourceChanged(
                    self.target.path.join(binding.path.as_str()),
                ));
            }
        }
        Ok(())
    }

    fn submit(&self, directory: &Directory) -> Result<()> {
        let identity = EntryIdentity::directory(directory, self.target.path)?;
        self.target
            .identity
            .require_device(identity, self.target.path)?;
        directory
            .require_persistent_filesystem()
            .map_err(|error| io(self.target.path, error))?;
        sync_kernel(directory.file()).map_err(|error| io(self.target.path, error))
    }

    pub(super) fn finish(self, cancel: &CancellationToken) -> Result<()> {
        check(cancel)?;
        self.check_layout(cancel)?;
        for binding in &self.target.bindings {
            if matches!(binding.entry, TreeEntry::Directory { .. }) {
                check(cancel)?;
                self.submit(&self.open_directory(binding.path.as_str())?)?;
            }
        }
        self.submit(self.target.root)?;
        #[cfg(test)]
        test_support::at(Boundary::DirectoriesSubmitted, self.target.path)?;
        check(cancel)?;
        self.check_layout(cancel)?;
        self.target
            .root
            .sync()
            .map_err(|error| io(self.target.path, error))?;
        #[cfg(test)]
        test_support::at(Boundary::DirectorySynced, self.target.path)?;
        check(cancel)?;
        self.check_layout(cancel)
    }
}

fn kind_matches(entry: &TreeEntry, metadata: &Metadata) -> bool {
    match entry {
        TreeEntry::Directory { .. } => metadata.is_dir(),
        TreeEntry::File { .. } => metadata.is_file(),
        TreeEntry::Symlink { .. } => metadata.file_type().is_symlink(),
        TreeEntry::Conflict { .. } => false,
    }
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Boundary {
    TargetPrepared,
    FileCreated,
    FileDataSynced,
    EntryPublished,
    DirectoriesSubmitted,
    DirectorySynced,
}

#[cfg(test)]
pub(super) mod test_support {
    use super::*;
    use std::cell::RefCell;
    type Hook = Box<dyn FnMut(Boundary, &Path) -> std_io::Result<()>>;
    thread_local! { static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) }; }
    pub(in crate::source) struct HookGuard;
    impl Drop for HookGuard {
        fn drop(&mut self) {
            HOOK.with(|hook| *hook.borrow_mut() = None);
        }
    }
    pub(in crate::source) fn install(
        hook: impl FnMut(Boundary, &Path) -> std_io::Result<()> + 'static,
    ) -> HookGuard {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
        HookGuard
    }
    pub(in crate::source) fn at(boundary: Boundary, path: &Path) -> Result<()> {
        HOOK.with(|hook| match hook.borrow_mut().as_mut() {
            Some(hook) => hook(boundary, path).map_err(|error| io(path, error)),
            None => Ok(()),
        })
    }
}

#[cfg(all(test, unix))]
mod tests;
