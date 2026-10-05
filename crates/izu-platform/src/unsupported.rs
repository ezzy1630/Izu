use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct Directory {
    file: File,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Symlink,
    Other,
}

#[derive(Debug)]
pub struct DirectoryEntry {
    pub name: OsString,
    pub kind: EntryKind,
}

pub type DirEntries = std::iter::Empty<io::Result<DirectoryEntry>>;

fn unsupported<T>() -> io::Result<T> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "descriptor-relative durable filesystem access is unsupported on this platform",
    ))
}

impl Directory {
    pub fn open(_: &Path) -> io::Result<Self> {
        unsupported()
    }
    pub fn file(&self) -> &File {
        &self.file
    }
    pub fn try_clone(&self) -> io::Result<Self> {
        unsupported()
    }
    pub fn metadata_self(&self) -> io::Result<Metadata> {
        unsupported()
    }
    pub fn open_dir(&self, _: &OsStr) -> io::Result<Self> {
        unsupported()
    }
    pub fn create_dir(&self, _: &OsStr) -> io::Result<Self> {
        unsupported()
    }
    pub fn ensure_dir(&self, _: &OsStr) -> io::Result<Self> {
        unsupported()
    }
    pub fn open_read(&self, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub fn create_new_file(&self, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub fn open_lock(&self, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub fn open_existing_lock(&self, _: &OsStr) -> io::Result<File> {
        unsupported()
    }
    pub fn metadata(&self, _: &OsStr) -> io::Result<Metadata> {
        unsupported()
    }
    pub fn entries(&self) -> io::Result<DirEntries> {
        unsupported()
    }
    pub fn rename_replace(&self, _: &OsStr, _: &Directory, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub fn rename_noreplace(&self, _: &OsStr, _: &Directory, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub fn hard_link(&self, _: &OsStr, _: &Directory, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub fn remove_file(&self, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub fn remove_dir(&self, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub fn read_link(&self, _: &OsStr, _: usize) -> io::Result<PathBuf> {
        unsupported()
    }
    pub fn symlink(&self, _: &Path, _: &OsStr) -> io::Result<()> {
        unsupported()
    }
    pub fn sync(&self) -> io::Result<()> {
        unsupported()
    }
    pub fn require_persistent_filesystem(&self) -> io::Result<()> {
        unsupported()
    }
}

pub fn sync_file(_: &File) -> io::Result<()> {
    unsupported()
}
pub fn sync_kernel(_: &File) -> io::Result<()> {
    unsupported()
}
pub fn require_durable_platform() -> io::Result<()> {
    unsupported()
}
pub fn require_persistent_file(_: &File) -> io::Result<()> {
    unsupported()
}
