//! Handle-relative Windows filesystem operations. Durable ACK stays disabled
//! until a directory-entry persistence contract has been established and reviewed.
use super::nt::{self, Access};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata};
use std::io;
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path, PathBuf, Prefix};
use windows_sys::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT,
};

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
    file: File,
    restart: bool,
    finished: bool,
}

fn invalid_component() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "expected one ordinary Windows path component",
    )
}

fn component(name: &OsStr) -> io::Result<Vec<u16>> {
    let mut units = Vec::new();
    units
        .try_reserve_exact(255)
        .map_err(|error| io::Error::other(error.to_string()))?;
    for unit in name.encode_wide() {
        if units.len() == 255
            || unit < 32
            || matches!(unit, 34 | 42 | 47 | 58 | 60 | 62 | 63 | 92 | 124)
        {
            return Err(invalid_component());
        }
        units.push(unit);
    }
    if units.is_empty()
        || units == [46]
        || units == [46, 46]
        || matches!(units.last(), Some(32 | 46))
    {
        return Err(invalid_component());
    }
    // Deny Win32 device aliases even though an NT relative open can address
    // some of these as ordinary files. Every adapter must use the same domain.
    let stem = units.split(|unit| *unit == 46).next().unwrap_or(&[]);
    let mut ascii = [0_u8; 6];
    if stem.len() <= ascii.len() && stem.iter().all(|unit| *unit < 128) {
        for (to, from) in ascii.iter_mut().zip(stem) {
            *to = (*from as u8).to_ascii_uppercase();
        }
        let stem = &ascii[..stem.len()];
        if matches!(stem, b"CON" | b"PRN" | b"AUX" | b"NUL" | b"CLOCK$")
            || (stem.len() == 4
                && matches!(&stem[..3], b"COM" | b"LPT")
                && matches!(stem[3], b'1'..=b'9'))
        {
            return Err(invalid_component());
        }
    }
    // Win32 also reserves COM/LPT with superscript 1, 2 and 3 suffixes.
    if stem.len() == 4
        && matches!(stem[3], 0x00b9 | 0x00b2 | 0x00b3)
        && ((matches!(stem[0], 67 | 99)
            && matches!(stem[1], 79 | 111)
            && matches!(stem[2], 77 | 109))
            || (matches!(stem[0], 76 | 108)
                && matches!(stem[1], 80 | 112)
                && matches!(stem[2], 84 | 116)))
    {
        return Err(invalid_component());
    }
    Ok(units)
}

fn reject_reparse(file: File) -> io::Result<File> {
    if nt::attributes(&file)? & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "reparse point cannot anchor filesystem access",
        ))
    } else {
        Ok(file)
    }
}

fn regular(file: File) -> io::Result<File> {
    let file = reject_reparse(file)?;
    if file.metadata()?.is_file() {
        Ok(file)
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "expected regular file",
        ))
    }
}

fn root_name(prefix: Prefix<'_>) -> io::Result<Vec<u16>> {
    let mut name = Vec::new();
    name.try_reserve_exact(1024)
        .map_err(|error| io::Error::other(error.to_string()))?;
    name.extend(r"\??\".encode_utf16());
    match prefix {
        Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
            name.extend([u16::from(drive), 58, 92]);
        }
        Prefix::UNC(server, share) | Prefix::VerbatimUNC(server, share) => {
            name.extend(r"UNC\".encode_utf16());
            name.extend(component(server)?);
            name.push(92);
            name.extend(component(share)?);
            name.push(92);
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "device namespace is unsupported",
            ));
        }
    }
    Ok(name)
}

impl Directory {
    /// Opens the disk/share root, then every ordinary component relative to the
    /// held handle. Never resolves a descendant through the root's pathname.
    pub fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            if path
                .components()
                .any(|part| matches!(part, Component::Prefix(_) | Component::ParentDir))
            {
                return Err(invalid_component());
            }
            let mut directory = Self::open(&std::env::current_dir()?)?;
            for part in path.components() {
                match part {
                    Component::CurDir => {}
                    Component::Normal(name) => directory = directory.open_dir(name)?,
                    _ => return Err(invalid_component()),
                }
            }
            return Ok(directory);
        }
        let mut parts = path.components();
        let Some(Component::Prefix(prefix)) = parts.next() else {
            return Err(invalid_component());
        };
        let root = root_name(prefix.kind())?;
        let file = reject_reparse(nt::open(None, &root, Access::Directory, false)?)?;
        let mut directory = Self { file };
        for part in parts {
            match part {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => directory = directory.open_dir(name)?,
                _ => return Err(invalid_component()),
            }
        }
        Ok(directory)
    }

    pub fn file(&self) -> &File {
        &self.file
    }

    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            file: self.file.try_clone()?,
        })
    }

    pub fn metadata_self(&self) -> io::Result<Metadata> {
        self.file.metadata()
    }

    pub fn open_dir(&self, name: &OsStr) -> io::Result<Self> {
        let name = component(name)?;
        Ok(Self {
            file: reject_reparse(nt::open(Some(&self.file), &name, Access::Directory, false)?)?,
        })
    }

    pub fn create_dir(&self, name: &OsStr) -> io::Result<Self> {
        let name = component(name)?;
        Ok(Self {
            file: reject_reparse(nt::open(Some(&self.file), &name, Access::Directory, true)?)?,
        })
    }

    pub fn ensure_dir(&self, name: &OsStr) -> io::Result<Self> {
        match self.create_dir(name) {
            Ok(directory) => Ok(directory),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => self.open_dir(name),
            Err(error) => Err(error),
        }
    }

    pub fn open_read(&self, name: &OsStr) -> io::Result<File> {
        regular(nt::open(
            Some(&self.file),
            &component(name)?,
            Access::Read,
            false,
        )?)
    }

    pub fn create_new_file(&self, name: &OsStr) -> io::Result<File> {
        regular(nt::open(
            Some(&self.file),
            &component(name)?,
            Access::ReadWrite,
            true,
        )?)
    }

    /// The permanent lock entry is neither replaced nor truncated. Creation is
    /// exclusive; only an AlreadyExists result permits reopening a raced entry.
    pub fn open_lock(&self, name: &OsStr) -> io::Result<File> {
        match self.open_existing_lock(name) {
            Ok(file) => Ok(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
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
        regular(nt::open(
            Some(&self.file),
            &component(name)?,
            Access::ReadWrite,
            false,
        )?)
    }

    pub fn metadata(&self, name: &OsStr) -> io::Result<Metadata> {
        nt::open(Some(&self.file), &component(name)?, Access::Inspect, false)?.metadata()
    }

    pub fn entries(&self) -> io::Result<DirEntries> {
        // Reopen this exact anchored directory for an independent scan cursor.
        let file = reject_reparse(nt::reopen_directory(&self.file, false)?)?;
        Ok(DirEntries {
            file,
            restart: true,
            finished: false,
        })
    }

    pub fn rename_replace(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
    ) -> io::Result<()> {
        self.rename(source, destination, target, true)
    }

    pub fn rename_noreplace(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
    ) -> io::Result<()> {
        self.rename(source, destination, target, false)
    }

    fn rename(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
        replace: bool,
    ) -> io::Result<()> {
        let source = nt::open(Some(&self.file), &component(source)?, Access::Delete, false)?;
        nt::rename_or_link(
            &source,
            &destination.file,
            &component(target)?,
            replace,
            false,
        )
    }

    pub fn hard_link(
        &self,
        source: &OsStr,
        destination: &Directory,
        target: &OsStr,
    ) -> io::Result<()> {
        let source = regular(nt::open(
            Some(&self.file),
            &component(source)?,
            Access::Inspect,
            false,
        )?)?;
        nt::rename_or_link(&source, &destination.file, &component(target)?, false, true)
    }

    pub fn remove_file(&self, name: &OsStr) -> io::Result<()> {
        let file = nt::open(Some(&self.file), &component(name)?, Access::Delete, false)?;
        let attributes = nt::attributes(&file)?;
        if attributes & FILE_ATTRIBUTE_DIRECTORY != 0
            && attributes & FILE_ATTRIBUTE_REPARSE_POINT == 0
        {
            return Err(io::Error::new(
                io::ErrorKind::IsADirectory,
                "cannot remove directory as file",
            ));
        }
        nt::remove(&file)
    }

    pub fn remove_dir(&self, name: &OsStr) -> io::Result<()> {
        let file = reject_reparse(nt::open(
            Some(&self.file),
            &component(name)?,
            Access::Delete,
            false,
        )?)?;
        if nt::attributes(&file)? & FILE_ATTRIBUTE_DIRECTORY == 0 {
            return Err(io::Error::new(
                io::ErrorKind::NotADirectory,
                "expected directory",
            ));
        }
        nt::remove(&file)
    }

    pub fn read_link(&self, _: &OsStr, _: usize) -> io::Result<PathBuf> {
        unsupported("anchored Windows reparse target decoding is not implemented")
    }

    pub fn symlink(&self, _: &Path, _: &OsStr) -> io::Result<()> {
        unsupported("anchored Windows symlink creation is not implemented")
    }

    pub fn sync(&self) -> io::Result<()> {
        require_durable_platform()
    }

    pub fn require_persistent_filesystem(&self) -> io::Result<()> {
        require_durable_platform()
    }
}

impl Iterator for DirEntries {
    type Item = io::Result<DirectoryEntry>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            let result = nt::next_entry(&self.file, self.restart);
            self.restart = false;
            let entry = match result {
                Ok(Some(entry)) => entry,
                Ok(None) => {
                    self.finished = true;
                    return None;
                }
                Err(error) => {
                    self.finished = true;
                    return Some(Err(error));
                }
            };
            if entry.name == [46] || entry.name == [46, 46] {
                continue;
            }
            let name = OsString::from_wide(&entry.name);
            let kind = if entry.attributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                // Reparse points include junctions/filters, not just symlinks.
                EntryKind::Other
            } else if entry.attributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
                EntryKind::Directory
            } else {
                EntryKind::File
            };
            return Some(Ok(DirectoryEntry { name, kind }));
        }
        None
    }
}

fn unsupported<T>(message: &'static str) -> io::Result<T> {
    Err(io::Error::new(io::ErrorKind::Unsupported, message))
}

pub fn sync_file(_: &File) -> io::Result<()> {
    require_durable_platform()
}

pub fn sync_kernel(_: &File) -> io::Result<()> {
    require_durable_platform()
}

pub fn require_durable_platform() -> io::Result<()> {
    unsupported("Windows directory-entry persistence barrier has not been established")
}

pub fn require_persistent_file(_: &File) -> io::Result<()> {
    require_durable_platform()
}
