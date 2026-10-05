use izu_platform::Directory;
use std::ffi::OsStr;
use std::fs::{File, Metadata};
use std::io;
use std::path::Path;

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

/// The original descriptor remains live while this identity is used, preventing
/// inode reuse. Names are never sufficient authority for disposing of staging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    #[cfg(unix)]
    fn metadata(metadata: &Metadata) -> io::Result<Self> {
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    #[cfg(not(unix))]
    fn metadata(_: &Metadata) -> io::Result<Self> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "owned entry identity is unsupported",
        ))
    }

    pub fn file(file: &File) -> io::Result<Self> {
        Self::metadata(&file.metadata()?)
    }

    pub fn directory(directory: &Directory) -> io::Result<Self> {
        Self::metadata(&directory.metadata_self()?)
    }

    pub fn matches_name(self, parent: &Directory, name: &OsStr) -> io::Result<bool> {
        Ok(self == Self::metadata(&parent.metadata(name)?)?)
    }

    pub fn require_name(self, parent: &Directory, name: &OsStr) -> io::Result<()> {
        if self.matches_name(parent, name)? {
            Ok(())
        } else {
            Err(io::Error::other(
                "owned staging identity changed; foreign entry preserved",
            ))
        }
    }

    pub fn require_directory_path(self, path: &Path) -> io::Result<()> {
        let observed = Directory::open(path)?;
        if self == Self::directory(&observed)? {
            Ok(())
        } else {
            Err(io::Error::other(
                "parent path no longer names its pinned directory",
            ))
        }
    }
}
