//! A kernel-submitted closure is not a durable publication receipt. Its files
//! and directories must share the eventual full-flush anchor's filesystem device.
use crate::{DurableBoundary, Result, Store, StoreError, io_error, kernel_file};
use izu_model::{CancellationToken, ObjectId};
use izu_platform::Directory;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    pub(crate) fn metadata(metadata: &Metadata) -> Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if metadata.nlink() == 0 {
                return Err(StoreError::Corrupt {
                    kind: "filesystem entry",
                    reason: "pinned entry was unlinked",
                });
            }
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Err(StoreError::UnsupportedDurability(io::Error::new(
                io::ErrorKind::Unsupported,
                "native device identity is unsupported",
            )))
        }
    }

    pub(crate) fn file(file: &File) -> Result<Self> {
        Self::metadata(
            &file
                .metadata()
                .map_err(|error| io_error("read pinned identity", error))?,
        )
    }
}

pub(crate) fn check_file_entry(directory: &Directory, name: &OsStr, file: &File) -> Result<()> {
    let metadata = directory
        .metadata(name)
        .map_err(|error| io_error("check published file entry", error))?;
    if !metadata.is_file() || Identity::metadata(&metadata)? != Identity::file(file)? {
        return Err(StoreError::Corrupt {
            kind: "file entry",
            reason: "entry no longer names the pinned file",
        });
    }
    Ok(())
}

pub(crate) fn check_directory_entry(
    parent: &Directory,
    name: &OsStr,
    directory: &Directory,
) -> Result<()> {
    let metadata = parent
        .metadata(name)
        .map_err(|error| io_error("check native directory entry", error))?;
    if !metadata.is_dir() || Identity::metadata(&metadata)? != Identity::file(directory.file())? {
        return Err(StoreError::Corrupt {
            kind: "store directory",
            reason: "entry no longer names the pinned directory",
        });
    }
    Ok(())
}

pub(crate) struct ClosureSubmission {
    root: Identity,
    objects: Identity,
    temporary: Identity,
    shards: [Option<Identity>; 256],
}

impl ClosureSubmission {
    pub(crate) fn new(store: &Store) -> Result<Self> {
        let submission = Self {
            root: Identity::file(store.root.file())?,
            objects: Identity::file(store.objects.file())?,
            temporary: Identity::file(store.temporary.file())?,
            shards: [None; 256],
        };
        submission.check_file(store.objects.file())?;
        submission.check_file(store.temporary.file())?;
        submission.check_layout(store)?;
        Ok(submission)
    }

    pub(crate) fn check_file(&self, file: &File) -> Result<()> {
        if Identity::file(file)?.device != self.root.device {
            return Err(StoreError::UnsupportedDurability(io::Error::new(
                io::ErrorKind::Unsupported,
                "batched publication requires one filesystem device",
            )));
        }
        Ok(())
    }

    pub(crate) fn check_layout(&self, store: &Store) -> Result<()> {
        if Identity::file(store.root.file())? != self.root {
            return Err(StoreError::Corrupt {
                kind: "store directory",
                reason: "pinned directory identity changed",
            });
        }
        for (name, expected) in [("objects", self.objects), ("tmp", self.temporary)] {
            let metadata = store
                .root
                .metadata(OsStr::new(name))
                .map_err(|error| io_error("check native directory entry", error))?;
            if !metadata.is_dir() || Identity::metadata(&metadata)? != expected {
                return Err(StoreError::Corrupt {
                    kind: "store directory",
                    reason: "entry no longer names the pinned directory",
                });
            }
        }
        for (index, expected) in self.shards.iter().enumerate() {
            let Some(expected) = expected else { continue };
            let shard = open_shard(store, index)?;
            if Identity::file(shard.file())? != *expected {
                return Err(StoreError::Corrupt {
                    kind: "object shard",
                    reason: "entry no longer names the verified directory",
                });
            }
        }
        Ok(())
    }

    pub(crate) fn submit_object(
        &mut self,
        store: &Store,
        id: ObjectId,
        file: &File,
        shard: &Directory,
    ) -> Result<()> {
        self.record_object(id, file, shard)?;
        kernel_file(file, &store.options)?;
        store.boundary(DurableBoundary::ClosureObjectSubmitted)
    }

    pub(crate) fn record_object(
        &mut self,
        id: ObjectId,
        file: &File,
        shard: &Directory,
    ) -> Result<()> {
        self.check_file(file)?;
        self.check_file(shard.file())?;
        // This descriptor is exactly the parent used to open the verified file.
        let identity = Identity::file(shard.file())?;
        let slot = &mut self.shards[usize::from(id.as_bytes()[0])];
        match slot {
            Some(previous) if *previous != identity => {
                return Err(StoreError::Corrupt {
                    kind: "object shard",
                    reason: "directory identity changed during submission",
                });
            }
            _ => *slot = Some(identity),
        }
        Ok(())
    }

    pub(crate) fn submit_directories(
        &self,
        store: &Store,
        cancel: &CancellationToken,
    ) -> Result<()> {
        for (index, expected) in self.shards.iter().enumerate() {
            let Some(expected) = expected else {
                continue;
            };
            cancel.check()?;
            let shard = open_shard(store, index)?;
            if Identity::file(shard.file())? != *expected {
                return Err(StoreError::Corrupt {
                    kind: "object shard",
                    reason: "entry no longer names the verified directory",
                });
            }
            self.check_file(shard.file())?;
            kernel_file(shard.file(), &store.options)?;
            store.boundary(DurableBoundary::ClosureDirectorySubmitted)?;
        }
        self.check_layout(store)?;
        kernel_file(store.objects.file(), &store.options)?;
        store.boundary(DurableBoundary::ClosureDirectorySubmitted)
    }
}

fn open_shard(store: &Store, index: usize) -> Result<Directory> {
    let digits = b"0123456789abcdef";
    let prefix = [
        *digits
            .get(index >> 4)
            .ok_or(StoreError::InvalidPath("invalid shard index"))?,
        *digits
            .get(index & 15)
            .ok_or(StoreError::InvalidPath("invalid shard index"))?,
    ];
    let name = std::str::from_utf8(&prefix)
        .map_err(|_| StoreError::InvalidPath("shard name is not ASCII"))?;
    store
        .objects
        .open_dir(OsStr::new(name))
        .map_err(|error| io_error("reopen submitted shard", error))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn cross_device_handle_cannot_join_a_batched_publication()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let file = tempfile::NamedTempFile::new()?;
        let actual = Identity::file(file.as_file())?;
        let other = Identity {
            device: actual.device ^ 1,
            inode: actual.inode,
        };
        let submission = ClosureSubmission {
            root: other,
            objects: other,
            temporary: other,
            shards: [None; 256],
        };
        assert!(matches!(
            submission.check_file(file.as_file()),
            Err(StoreError::UnsupportedDurability(_))
        ));
        Ok(())
    }
}
