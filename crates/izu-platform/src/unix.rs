use rustix::fs::{self, AtFlags, Mode, OFlags};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::mem::MaybeUninit;
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

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

pub struct DirEntries {
    dir: fs::Dir,
}

fn component(name: &OsStr) -> io::Result<()> {
    let bytes = name.as_bytes();
    if bytes.is_empty()
        || bytes == b"."
        || bytes == b".."
        || bytes.contains(&b'/')
        || bytes.contains(&0)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected one ordinary path component",
        ));
    }
    Ok(())
}

fn regular(file: File) -> io::Result<File> {
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected regular file",
        ))
    }
}

impl Directory {
    /// Opens each component separately with `O_NOFOLLOW`. Explicit `..` is rejected.
    pub fn open(path: &Path) -> io::Result<Self> {
        let anchor = if path.is_absolute() {
            Path::new("/")
        } else {
            Path::new(".")
        };
        let fd = fs::open(
            anchor,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        let mut dir = Self {
            file: File::from(fd),
        };
        for part in path.components() {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => dir = dir.open_dir(name)?,
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "parent or platform prefix component is unsupported",
                    ));
                }
            }
        }
        Ok(dir)
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    /// Duplicates the anchored descriptor and never resolves the path again.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }

    pub fn metadata_self(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    pub fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
        component(name)?;
        let fd = fs::openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::empty(),
        )?;
        Ok(Self {
            file: File::from(fd),
        })
    }

    /// Creates only this entry. The caller must sync the parent before durable ACK.
    pub fn create_dir(&self, name: &OsStr) -> io::Result<Self> {
        component(name)?;
        fs::mkdirat(&self.file, name, Mode::from_raw_mode(0o700))?;
        self.open_dir(name)
    }

    pub fn ensure_dir(&self, name: &OsStr) -> io::Result<Self> {
        match self.create_dir(name) {
            Ok(dir) => Ok(dir),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self.open_dir(name),
            Err(error) => Err(error),
        }
    }

    pub fn open_read(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        let fd = fs::openat(
            &self.file,
            name,
            OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        regular(File::from(fd))
    }

    pub fn create_new_file(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        let fd = fs::openat(
            &self.file,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::from_raw_mode(0o600),
        )?;
        regular(File::from(fd))
    }

    /// Existing lock files are never truncated, replaced, or removed.
    pub fn open_lock(&self, name: &OsStr) -> io::Result<File> {
        match self.open_existing_lock(name) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // APFS concurrent non-exclusive O_CREAT opens reproduced ENOENT
                // despite a still-linked parent and existing resulting entry.
                // Separate opening from exclusive creation; EEXIST alone proves
                // that a competing creator requires one more existing-file open.
                match self.create_new_file(name) {
                    Ok(file) => Ok(file),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                        self.open_existing_lock(name)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    pub fn open_existing_lock(&self, name: &OsStr) -> io::Result<File> {
        component(name)?;
        let fd = fs::openat(
            &self.file,
            name,
            OFlags::RDWR | OFlags::CLOEXEC | OFlags::NOFOLLOW | OFlags::NONBLOCK,
            Mode::empty(),
        )?;
        regular(File::from(fd))
    }

    /// Does not follow a symlink even for the final entry.
    pub fn metadata(&self, name: &OsStr) -> io::Result<Metadata> {
        component(name)?;
        #[cfg(target_os = "linux")]
        let flags = OFlags::PATH | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        #[cfg(target_vendor = "apple")]
        let flags = OFlags::RDONLY | OFlags::SYMLINK | OFlags::CLOEXEC | OFlags::NONBLOCK;
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
        let fd = fs::openat(&self.file, name, flags, Mode::empty())?;
        File::from(fd).metadata()
    }

    pub fn entries(&self) -> io::Result<DirEntries> {
        Ok(DirEntries {
            dir: fs::Dir::read_from(&self.file)?,
        })
    }

    /// Atomic replacement is for mutable state, never immutable objects.
    pub fn rename_replace(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
    ) -> io::Result<()> {
        component(source)?;
        component(target)?;
        fs::renameat(&self.file, source, &destination.file, target)?;
        Ok(())
    }

    /// Atomically moves an entry only when the destination does not exist.
    /// Platforms without the kernel primitive return Unsupported, never fall back
    /// to a separate existence check followed by a replacing rename.
    pub fn rename_noreplace(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
    ) -> io::Result<()> {
        component(source)?;
        component(target)?;
        #[cfg(any(target_os = "linux", target_vendor = "apple"))]
        {
            fs::renameat_with(
                &self.file,
                source,
                &destination.file,
                target,
                fs::RenameFlags::NOREPLACE,
            )?;
            Ok(())
        }
        #[cfg(not(any(target_os = "linux", target_vendor = "apple")))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "atomic no-replacement rename is unsupported",
        ))
    }

    /// Atomic creation which fails if any destination entry already exists.
    pub fn hard_link(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
    ) -> io::Result<()> {
        component(source)?;
        component(target)?;
        fs::linkat(
            &self.file,
            source,
            &destination.file,
            target,
            AtFlags::empty(),
        )?;
        Ok(())
    }

    /// Removes a single entry and never recursively deletes a directory.
    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        component(name)?;
        fs::unlinkat(&self.file, name, AtFlags::empty())?;
        Ok(())
    }

    /// Removes an empty directory only. It never follows symlinks or recurses.
    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        component(name)?;
        fs::unlinkat(&self.file, name, AtFlags::REMOVEDIR)?;
        Ok(())
    }

    pub fn read_link(&self, name: &OsStr, max_bytes: usize) -> io::Result<PathBuf> {
        component(name)?;
        const CAPACITY: usize = 65_536;
        if max_bytes == 0 || max_bytes >= CAPACITY {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "symlink limit must be between 1 and 65535 bytes",
            ));
        }
        let mut buffer = [MaybeUninit::new(0_u8); CAPACITY];
        let (bytes, _) = fs::readlinkat_raw(&self.file, name, &mut buffer[..max_bytes + 1])?;
        if bytes.len() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "symlink target exceeds limit",
            ));
        }
        let mut target = OsString::new();
        target
            .try_reserve(bytes.len())
            .map_err(|error| io::Error::other(error.to_string()))?;
        target.push(OsStr::from_bytes(bytes));
        Ok(PathBuf::from(target))
    }

    pub fn symlink(&self, target: &Path, name: &OsStr) -> io::Result<()> {
        component(name)?;
        fs::symlinkat(target, &self.file, name)?;
        Ok(())
    }

    pub fn sync(&self) -> io::Result<()> {
        sync_file(&self.file)
    }

    pub fn require_persistent_filesystem(&self) -> io::Result<()> {
        require_persistent_file(&self.file)
    }
}

impl Iterator for DirEntries {
    type Item = io::Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            let entry = match self.dir.next()? {
                Ok(entry) => entry,
                Err(error) => return Some(Err(error.into())),
            };
            let bytes = entry.file_name().to_bytes();
            if bytes == b"." || bytes == b".." {
                continue;
            }
            let mut name = OsString::new();
            if let Err(error) = name.try_reserve(bytes.len()) {
                return Some(Err(io::Error::other(error.to_string())));
            }
            name.push(OsStr::from_bytes(bytes));
            let kind = match entry.file_type() {
                fs::FileType::RegularFile => EntryKind::File,
                fs::FileType::Directory => EntryKind::Directory,
                fs::FileType::Symlink => EntryKind::Symlink,
                _ => EntryKind::Other,
            };
            return Some(Ok(DirectoryEntry { name, kind }));
        }
    }
}

/// Requests the strongest implemented operating-system persistence barrier.
/// Successful calls rely on the filesystem and storage device honoring them.
pub fn sync_file(file: &File) -> io::Result<()> {
    require_durable_platform()?;
    // F_FULLFSYNC includes the file's fsync and then requests a device flush.
    // Rust 1.99's File::sync_all already invokes F_FULLFSYNC on Apple; composing
    // it with another full flush would issue the expensive request twice.
    #[cfg(target_os = "macos")]
    fs::fcntl_fullfsync(file)?;
    #[cfg(target_os = "linux")]
    fs::fsync(file)?;
    Ok(())
}

/// Submits this file's data/metadata to the operating-system fsync boundary.
/// On macOS this is not a completed durable acknowledgement: an explicit full
/// device flush must follow after every participating file/directory submission.
/// It deliberately avoids Rust's Apple File::sync_all implementation.
pub fn sync_kernel(file: &File) -> io::Result<()> {
    require_durable_platform()?;
    fs::fsync(file)?;
    Ok(())
}

pub fn require_durable_platform() -> io::Result<()> {
    if cfg!(any(target_os = "macos", target_os = "linux")) {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "durable publication is unsupported on this platform",
        ))
    }
}

/// Refuses known volatile, shared and layered storage outside the first-release
/// native-storage boundary. Passing classification does not prove persistence
/// hardware or stable identity on an arbitrary mount; operation guards remain.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub fn require_persistent_file(file: &File) -> io::Result<()> {
    let stat = fs::fstatfs(file)?;
    #[cfg(target_os = "linux")]
    let refusal = linux_storage_refusal((i128::from(stat.f_type) & i128::from(u32::MAX)) as u32);
    #[cfg(target_os = "macos")]
    let refusal = {
        let mut name = [0_u8; 16];
        for (destination, source) in name.iter_mut().zip(stat.f_fstypename) {
            *destination = source as u8;
        }
        let end = name
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(name.len());
        darwin_storage_refusal(&name[..end])
    };
    match refusal {
        Some(reason) => Err(io::Error::new(io::ErrorKind::Unsupported, reason)),
        None => Ok(()),
    }
}

#[cfg(any(target_os = "linux", test))]
fn linux_storage_refusal(magic: u32) -> Option<&'static str> {
    // General-purpose Linux UAPI filesystem IDs, not VCS implementation code:
    // https://github.com/torvalds/linux/blob/master/include/uapi/linux/magic.h
    match magic {
        0x0102_1994 | 0x8584_58f6 | 0x9584_58f6 => {
            Some("known volatile filesystem cannot acknowledge persistent storage")
        }
        0x6573_5546 | 0x0102_1997 | 0x00c0_ffee | 0x794c_7630 | 0x6969 | 0x517b | 0xff53_4d42
        | 0xfe53_4d42 | 0x5346_414f | 0x6b41_4653 | 0x00c3_6400 | 0x7375_7245 => Some(
            "release storage requires a native persistent filesystem; use an owned native volume instead of shared, network or layered storage",
        ),
        _ => None,
    }
}
#[cfg(any(target_os = "macos", test))]
fn darwin_storage_refusal(name: &[u8]) -> Option<&'static str> {
    match name {
        b"tmpfs" | b"ramfs" | b"mfs" | b"devfs" => {
            Some("known volatile filesystem cannot acknowledge persistent storage")
        }
        b"nfs" | b"smbfs" | b"afpfs" | b"webdav" | b"fusefs" | b"osxfuse" | b"macfuse"
        | b"virtiofs" => Some(
            "release storage requires a native persistent filesystem; use native local storage instead of a shared or network mount",
        ),
        _ => None,
    }
}

#[cfg(test)]
mod storage_policy_tests {
    use super::*;
    #[test]
    fn release_storage_refuses_shared_and_volatile_but_keeps_native_checks() {
        for magic in [
            0x01021994, 0x858458f6, 0x958458f6, 0x65735546, 0x01021997, 0x00c0ffee, 0x794c7630,
            0x6969, 0xff534d42, 0xfe534d42,
        ] {
            assert!(linux_storage_refusal(magic).is_some());
        }
        for magic in [0xef53, 0x58465342, 0x9123683e] {
            assert!(linux_storage_refusal(magic).is_none());
        }
        for name in [b"nfs".as_slice(), b"smbfs", b"osxfuse", b"tmpfs", b"webdav"] {
            assert!(darwin_storage_refusal(name).is_some());
        }
        for name in [b"apfs".as_slice(), b"hfs"] {
            assert!(darwin_storage_refusal(name).is_none());
        }
        // Unknown classification is never a persistence or native-platform proof.
        assert!(linux_storage_refusal(0).is_none());
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn require_persistent_file(_: &File) -> io::Result<()> {
    require_durable_platform()
}
