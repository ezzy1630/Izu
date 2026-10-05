use crate::error::{check, io};
use crate::recipe::{PersistedIdentity, validate_path};
use crate::{CancellationToken, Digest, EnvironmentError, EnvironmentLimits, Recipe, Result};
use izu_platform::{Directory, sync_file};
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::ffi::{OsStr, OsString};
use std::fs::{File, Metadata, Permissions};
use std::io::{Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::Path;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum SharingPolicy {
    PreferClone,
    Copy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum MaterializationMode {
    Empty,
    Clone,
    Copy,
    Mixed,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
pub(crate) struct TransferStats {
    pub cloned_files: u64,
    pub copied_files: u64,
    pub symlinks: u64,
    pub logical_bytes: u64,
    /// This sum can double count shared extents. It is not unique physical use.
    pub allocated_file_bytes_sum: u64,
}

impl TransferStats {
    pub fn mode(&self) -> MaterializationMode {
        match (self.cloned_files != 0, self.copied_files != 0) {
            (false, false) => MaterializationMode::Empty,
            (true, false) => MaterializationMode::Clone,
            (false, true) => MaterializationMode::Copy,
            (true, true) => MaterializationMode::Mixed,
        }
    }
    pub fn add(&mut self, other: &Self) -> Result<()> {
        self.cloned_files = checked_add(self.cloned_files, other.cloned_files, "clone counts")?;
        self.copied_files = checked_add(self.copied_files, other.copied_files, "copy counts")?;
        self.symlinks = checked_add(self.symlinks, other.symlinks, "symlink counts")?;
        self.logical_bytes = checked_add(self.logical_bytes, other.logical_bytes, "logical bytes")?;
        self.allocated_file_bytes_sum = checked_add(
            self.allocated_file_bytes_sum,
            other.allocated_file_bytes_sum,
            "allocated file bytes",
        )?;
        Ok(())
    }
}

fn checked_add(a: u64, b: u64, resource: &'static str) -> Result<u64> {
    a.checked_add(b).ok_or(EnvironmentError::Limit {
        resource,
        limit: u64::MAX,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NodeIdentity {
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Stamp {
    pub identity: NodeIdentity,
    pub length: u64,
    pub mode: u32,
    pub modified_seconds: i64,
    pub modified_nanos: i64,
}

pub(crate) fn stamp(metadata: &Metadata) -> Result<Stamp> {
    #[cfg(unix)]
    {
        Ok(Stamp {
            identity: NodeIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            length: metadata.len(),
            mode: metadata.mode() & 0o777,
            modified_seconds: metadata.mtime(),
            modified_nanos: metadata.mtime_nsec(),
        })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(EnvironmentError::InvalidInput(
            "environment file identity is unsupported on this platform".into(),
        ))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum InventoryKind {
    Directory { mode: u32 },
    File { mode: u32, bytes: u64 },
    Symlink { target: String },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct InventoryEntry {
    pub root: u32,
    pub path: String,
    pub stamp: Option<Stamp>,
    pub kind: InventoryKind,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub(crate) enum ManifestKind {
    Directory {
        mode: u32,
    },
    File {
        mode: u32,
        bytes: u64,
        digest: Digest,
    },
    Symlink {
        target: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ManifestEntry {
    pub root: u32,
    pub path: String,
    pub identity: NodeIdentity,
    pub node: ManifestKind,
}

fn push<T>(values: &mut Vec<T>, value: T, resource: &'static str) -> Result<()> {
    values
        .try_reserve(1)
        .map_err(|_| EnvironmentError::Allocation(resource))?;
    values.push(value);
    Ok(())
}

pub(crate) fn open_parent(
    root: &Directory,
    path: &str,
    create: bool,
) -> Result<(Directory, OsString)> {
    validate_path(path)?;
    let mut components = path.split('/').peekable();
    let mut directory = duplicate_directory(root)?;
    while let Some(component) = components.next() {
        if create {
            directory
                .require_persistent_filesystem()
                .map_err(|error| io(path, error))?;
        }
        if components.peek().is_none() {
            return Ok((directory, OsString::from(component)));
        }
        directory = if create {
            let next = directory
                .ensure_dir(OsStr::new(component))
                .map_err(|error| io(path, error))?;
            next.require_persistent_filesystem()
                .map_err(|error| io(path, error))?;
            directory.sync().map_err(|error| io(path, error))?;
            next
        } else {
            directory
                .open_dir(OsStr::new(component))
                .map_err(|error| io(path, error))?
        };
    }
    Err(EnvironmentError::InvalidInput(
        "empty environment path".into(),
    ))
}

pub(crate) fn duplicate_directory(root: &Directory) -> Result<Directory> {
    // Directory::open_dir currently disallows dot. The platform's public handle
    // is intentionally reused by traversals that have at least one component;
    // the reviewed duplicate API avoids reopening a path after pinning it.
    root.try_clone()
        .map_err(|error| io("<directory descriptor>", error))
}

pub(crate) fn verify_directory_entry(
    parent: &Directory,
    name: &OsStr,
    pinned: &Directory,
    locator: &Path,
) -> Result<()> {
    let observed = parent
        .open_dir(name)
        .map_err(|_| EnvironmentError::NamespaceChanged(locator.to_owned()))?;
    verify_same_directory(&observed, pinned, locator)
}

pub(crate) fn verify_directory_locator(locator: &Path, pinned: &Directory) -> Result<()> {
    let observed = Directory::open(locator)
        .map_err(|_| EnvironmentError::NamespaceChanged(locator.to_owned()))?;
    verify_same_directory(&observed, pinned, locator)
}

fn verify_same_directory(observed: &Directory, pinned: &Directory, locator: &Path) -> Result<()> {
    let observed = stamp(
        &observed
            .metadata_self()
            .map_err(|error| io(locator, error))?,
    )?;
    let expected = stamp(&pinned.metadata_self().map_err(|error| io(locator, error))?)?;
    if observed.identity != expected.identity {
        return Err(EnvironmentError::NamespaceChanged(locator.to_owned()));
    }
    Ok(())
}

pub(crate) fn open_directory(root: &Directory, path: &str) -> Result<Directory> {
    let (parent, name) = open_parent(root, path, false)?;
    parent.open_dir(&name).map_err(|error| io(path, error))
}

pub(crate) fn open_regular(root: &Directory, path: &str) -> Result<File> {
    let (parent, name) = open_parent(root, path, false)?;
    parent.open_read(&name).map_err(|error| io(path, error))
}

pub(crate) fn collect_source(
    root: &Directory,
    recipe: &Recipe,
    limits: &EnvironmentLimits,
    cancel: &CancellationToken,
) -> Result<Vec<InventoryEntry>> {
    collect_declared(root, recipe.identity(), limits, cancel, true)
}

pub(crate) fn collect_declared(
    root: &Directory,
    identity: &PersistedIdentity,
    limits: &EnvironmentLimits,
    cancel: &CancellationToken,
    allow_missing_outputs: bool,
) -> Result<Vec<InventoryEntry>> {
    let mut entries = Vec::new();
    let mut budget = ScanBudget {
        total_bytes: 0,
        limits,
        cancel,
    };
    for (index, path) in identity
        .dependencies
        .iter()
        .chain(&identity.outputs)
        .enumerate()
    {
        check(cancel)?;
        if entries.len() >= limits.max_entries {
            return Err(EnvironmentError::Limit {
                resource: "entries",
                limit: limits.max_entries as u64,
            });
        }
        let root_index = u32::try_from(index)
            .map_err(|_| EnvironmentError::Allocation("declared root index"))?;
        let prepared = open_directory(root, path);
        match prepared {
            Ok(directory) => {
                scan_directory(&directory, root_index, "", 0, &mut entries, &mut budget)?
            }
            Err(EnvironmentError::Io { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound
                    && allow_missing_outputs
                    && identity.outputs.iter().any(|output| output == path) =>
            {
                push(
                    &mut entries,
                    InventoryEntry {
                        root: root_index,
                        path: String::new(),
                        stamp: None,
                        kind: InventoryKind::Directory { mode: 0o755 },
                    },
                    "inventory",
                )?;
            }
            Err(error) => return Err(error),
        }
    }
    entries.sort_by(|a, b| (a.root, &a.path).cmp(&(b.root, &b.path)));
    Ok(entries)
}

pub(crate) fn collect_payload(
    payload: &Directory,
    root_count: usize,
    limits: &EnvironmentLimits,
    cancel: &CancellationToken,
) -> Result<Vec<InventoryEntry>> {
    let mut actual_roots = 0;
    for entry in payload.entries().map_err(|error| io("payload", error))? {
        check(cancel)?;
        let entry = entry.map_err(|error| io("payload", error))?;
        let name = entry
            .name
            .to_str()
            .ok_or_else(|| EnvironmentError::Corrupt("non-UTF-8 payload name".into()))?;
        let index = name
            .parse::<usize>()
            .map_err(|_| EnvironmentError::Corrupt("unexpected payload entry".into()))?;
        if index >= root_count || name != index.to_string() {
            return Err(EnvironmentError::Corrupt("unexpected payload root".into()));
        }
        actual_roots += 1;
        if actual_roots > root_count {
            return Err(EnvironmentError::Corrupt("extra payload roots".into()));
        }
    }
    if actual_roots != root_count {
        return Err(EnvironmentError::Corrupt("missing payload root".into()));
    }
    let mut entries = Vec::new();
    let mut budget = ScanBudget {
        total_bytes: 0,
        limits,
        cancel,
    };
    for index in 0..root_count {
        let directory = payload
            .open_dir(OsStr::new(&index.to_string()))
            .map_err(|error| io("payload", error))?;
        scan_directory(
            &directory,
            u32::try_from(index).map_err(|_| EnvironmentError::Allocation("root index"))?,
            "",
            0,
            &mut entries,
            &mut budget,
        )?;
    }
    entries.sort_by(|a, b| (a.root, &a.path).cmp(&(b.root, &b.path)));
    Ok(entries)
}

struct ScanBudget<'a> {
    total_bytes: u64,
    limits: &'a EnvironmentLimits,
    cancel: &'a CancellationToken,
}

fn scan_directory(
    directory: &Directory,
    root_index: u32,
    path: &str,
    depth: usize,
    entries: &mut Vec<InventoryEntry>,
    budget: &mut ScanBudget<'_>,
) -> Result<()> {
    let limits = budget.limits;
    let cancel = budget.cancel;
    check(cancel)?;
    if depth > limits.max_depth {
        return Err(EnvironmentError::Limit {
            resource: "directory depth",
            limit: limits.max_depth as u64,
        });
    }
    if entries.len() >= limits.max_entries {
        return Err(EnvironmentError::Limit {
            resource: "entries",
            limit: limits.max_entries as u64,
        });
    }
    let root_stamp = stamp(&directory.metadata_self().map_err(|error| io(path, error))?)?;
    push(
        entries,
        InventoryEntry {
            root: root_index,
            path: path.into(),
            kind: InventoryKind::Directory {
                mode: root_stamp.mode,
            },
            stamp: Some(root_stamp.clone()),
        },
        "inventory",
    )?;
    for child in directory.entries().map_err(|error| io(path, error))? {
        check(cancel)?;
        if entries.len() >= limits.max_entries {
            return Err(EnvironmentError::Limit {
                resource: "entries",
                limit: limits.max_entries as u64,
            });
        }
        let child = child.map_err(|error| io(path, error))?;
        let name = child.name.to_str().ok_or_else(|| {
            EnvironmentError::InvalidInput("prepared filenames must be UTF-8".into())
        })?;
        let child_path = if path.is_empty() {
            name.into()
        } else {
            format!("{path}/{name}")
        };
        validate_path(&child_path)?;
        let metadata = directory
            .metadata(&child.name)
            .map_err(|error| io(&child_path, error))?;
        let child_stamp = stamp(&metadata)?;
        if metadata.is_dir() {
            let next = directory
                .open_dir(&child.name)
                .map_err(|error| io(&child_path, error))?;
            if stamp(
                &next
                    .metadata_self()
                    .map_err(|error| io(&child_path, error))?,
            )?
            .identity
                != child_stamp.identity
            {
                return Err(EnvironmentError::InputChanged(child_path));
            }
            scan_directory(&next, root_index, &child_path, depth + 1, entries, budget)?;
        } else if metadata.is_file() {
            if child_stamp.length > limits.max_file_bytes {
                return Err(EnvironmentError::Limit {
                    resource: "file bytes",
                    limit: limits.max_file_bytes,
                });
            }
            budget.total_bytes =
                checked_add(budget.total_bytes, child_stamp.length, "artifact bytes")?;
            if budget.total_bytes > limits.max_artifact_bytes {
                return Err(EnvironmentError::Limit {
                    resource: "artifact bytes",
                    limit: limits.max_artifact_bytes,
                });
            }
            push(
                entries,
                InventoryEntry {
                    root: root_index,
                    path: child_path,
                    kind: InventoryKind::File {
                        mode: child_stamp.mode,
                        bytes: child_stamp.length,
                    },
                    stamp: Some(child_stamp),
                },
                "inventory",
            )?;
        } else if metadata.is_symlink() {
            let target = directory
                .read_link(&child.name, 4096)
                .map_err(|error| io(&child_path, error))?;
            let target = target
                .to_str()
                .ok_or_else(|| {
                    EnvironmentError::InvalidInput("prepared symlink targets must be UTF-8".into())
                })?
                .to_owned();
            validate_symlink(&child_path, &target)?;
            push(
                entries,
                InventoryEntry {
                    root: root_index,
                    path: child_path,
                    kind: InventoryKind::Symlink { target },
                    stamp: Some(child_stamp),
                },
                "inventory",
            )?;
        } else {
            return Err(EnvironmentError::InvalidInput(format!(
                "special file in prepared artifact: {child_path}"
            )));
        }
    }
    if stamp(&directory.metadata_self().map_err(|error| io(path, error))?)? != root_stamp {
        return Err(EnvironmentError::InputChanged(path.into()));
    }
    Ok(())
}

pub(crate) fn validate_symlink(path: &str, target: &str) -> Result<()> {
    if target.is_empty()
        || target.len() > 4096
        || target.starts_with('/')
        || target.contains('\0')
        || target.contains('\\')
    {
        return Err(EnvironmentError::InvalidInput(
            "prepared symlink target must be bounded and relative".into(),
        ));
    }
    let mut depth = path.split('/').count().saturating_sub(1);
    for component in target.split('/') {
        match component {
            ".." => {
                depth = depth.checked_sub(1).ok_or_else(|| {
                    EnvironmentError::InvalidInput(format!(
                        "symlink escapes declared artifact root: {path}"
                    ))
                })?;
            }
            "." | "" => {}
            name => {
                if name.len() > 255 || name.eq_ignore_ascii_case(".izu") {
                    return Err(EnvironmentError::InvalidInput(
                        "symlink target has reserved or oversized component".into(),
                    ));
                }
                depth += 1;
            }
        }
    }
    Ok(())
}

pub(crate) fn hash_file(
    file: &mut File,
    limit: u64,
    cancel: &CancellationToken,
) -> Result<(Digest, u64)> {
    file.seek(SeekFrom::Start(0))
        .map_err(|error| io("<file>", error))?;
    let mut digest = Sha256::new();
    let mut bytes = 0_u64;
    let mut buffer = [0_u8; 65536];
    loop {
        check(cancel)?;
        let count = file
            .read(&mut buffer)
            .map_err(|error| io("<file>", error))?;
        if count == 0 {
            break;
        }
        bytes = checked_add(bytes, count as u64, "file bytes")?;
        if bytes > limit {
            return Err(EnvironmentError::Limit {
                resource: "file bytes",
                limit,
            });
        }
        digest.update(&buffer[..count]);
    }
    Ok((Digest::from_hasher(digest), bytes))
}

pub(crate) fn set_mode(file: &File, mode: u32) -> Result<()> {
    #[cfg(unix)]
    {
        file.set_permissions(Permissions::from_mode(mode & 0o777))
            .map_err(|error| io("<file permissions>", error))
    }
    #[cfg(not(unix))]
    {
        let _ = (file, mode);
        Err(EnvironmentError::InvalidInput(
            "environment permissions unsupported".into(),
        ))
    }
}

fn allocated_bytes(metadata: &Metadata) -> Result<u64> {
    #[cfg(unix)]
    {
        metadata
            .blocks()
            .checked_mul(512)
            .ok_or(EnvironmentError::Limit {
                resource: "allocated bytes",
                limit: u64::MAX,
            })
    }
    #[cfg(not(unix))]
    {
        let _ = metadata;
        Err(EnvironmentError::InvalidInput(
            "allocation accounting unsupported".into(),
        ))
    }
}

/// A filesystem free-space observation. Only an exclusive isolated volume can
/// turn before/after differences into a physical-allocation measurement.
pub fn filesystem_available_bytes(path: impl AsRef<Path>) -> Result<u64> {
    let directory = Directory::open(path.as_ref()).map_err(|error| io(path.as_ref(), error))?;
    available_bytes(&directory)
}

pub(crate) fn available_bytes(directory: &Directory) -> Result<u64> {
    #[cfg(unix)]
    {
        let stat = rustix::fs::fstatvfs(directory.file())
            .map_err(|error| io("<filesystem capacity>", error.into()))?;
        let fragment = if stat.f_frsize == 0 {
            stat.f_bsize
        } else {
            stat.f_frsize
        };
        stat.f_bavail
            .checked_mul(fragment)
            .ok_or(EnvironmentError::Limit {
                resource: "filesystem bytes",
                limit: u64::MAX,
            })
    }
    #[cfg(not(unix))]
    {
        let _ = directory;
        Err(EnvironmentError::InvalidInput(
            "filesystem capacity unsupported".into(),
        ))
    }
}

#[cfg(any(target_vendor = "apple", target_os = "linux"))]
fn unsupported_clone(error: rustix::io::Errno) -> bool {
    matches!(
        error,
        rustix::io::Errno::XDEV
            | rustix::io::Errno::NOTSUP
            | rustix::io::Errno::NOSYS
            | rustix::io::Errno::INVAL
            | rustix::io::Errno::NOTTY
    )
}

/// Safe syscall wrappers only. Successful return from the filesystem clone
/// primitive is the proof for Clone mode; unsupported filesystems use Copy.
fn new_destination(
    source: &File,
    destination: &Directory,
    name: &OsStr,
    policy: SharingPolicy,
) -> Result<(File, bool)> {
    #[cfg(target_vendor = "apple")]
    if policy == SharingPolicy::PreferClone {
        match rustix::fs::fclonefileat(
            source,
            destination.file(),
            name,
            rustix::fs::CloneFlags::empty(),
        ) {
            Ok(()) => {
                return Ok((
                    destination
                        .open_read(name)
                        .map_err(|error| io(name, error))?,
                    true,
                ));
            }
            Err(error) if unsupported_clone(error) => {}
            Err(error) => return Err(io(name, error.into())),
        }
    }
    let file = destination
        .create_new_file(name)
        .map_err(|error| io(name, error))?;
    #[cfg(target_os = "linux")]
    if policy == SharingPolicy::PreferClone {
        match rustix::fs::ioctl_ficlone(&file, source) {
            Ok(()) => return Ok((file, true)),
            Err(error) if unsupported_clone(error) => {
                file.set_len(0).map_err(|error| io(name, error))?;
            }
            Err(error) => return Err(io(name, error.into())),
        }
    }
    let _ = (source, policy);
    Ok((file, false))
}

pub(crate) fn transfer_root(
    source: Option<&Directory>,
    destination: &Directory,
    entries: &[InventoryEntry],
    policy: SharingPolicy,
    sealed: bool,
    limits: &EnvironmentLimits,
    cancel: &CancellationToken,
) -> Result<(Vec<ManifestEntry>, TransferStats)> {
    destination
        .require_persistent_filesystem()
        .map_err(|error| io("<environment destination>", error))?;
    let mut manifest = Vec::new();
    let mut stats = TransferStats::default();
    for entry in entries {
        check(cancel)?;
        let (node, identity) = match &entry.kind {
            InventoryKind::Directory { mode } => {
                let directory = if entry.path.is_empty() {
                    duplicate_directory(destination)?
                } else {
                    let (parent, name) = open_parent(destination, &entry.path, false)?;
                    parent
                        .create_dir(&name)
                        .map_err(|error| io(&entry.path, error))?
                };
                let identity = stamp(
                    &directory
                        .metadata_self()
                        .map_err(|error| io(&entry.path, error))?,
                )?
                .identity;
                (ManifestKind::Directory { mode: *mode }, identity)
            }
            InventoryKind::File { mode, bytes } => {
                let source = source
                    .ok_or_else(|| EnvironmentError::InvalidInput("missing file input".into()))?;
                let mut input = open_regular(source, &entry.path)?;
                let before = stamp(&input.metadata().map_err(|error| io(&entry.path, error))?)?;
                if Some(&before) != entry.stamp.as_ref() {
                    return Err(EnvironmentError::InputChanged(entry.path.clone()));
                }
                let (parent, name) = open_parent(destination, &entry.path, false)?;
                let (mut output, cloned) = new_destination(&input, &parent, &name, policy)?;
                if !cloned {
                    let mut buffer = [0_u8; 65536];
                    let mut copied = 0_u64;
                    loop {
                        check(cancel)?;
                        let count = input
                            .read(&mut buffer)
                            .map_err(|error| io(&entry.path, error))?;
                        if count == 0 {
                            break;
                        }
                        copied = checked_add(copied, count as u64, "file bytes")?;
                        if copied > *bytes || copied > limits.max_file_bytes {
                            return Err(EnvironmentError::InputChanged(entry.path.clone()));
                        }
                        output
                            .write_all(&buffer[..count])
                            .map_err(|error| io(&entry.path, error))?;
                    }
                    if copied != *bytes {
                        return Err(EnvironmentError::InputChanged(entry.path.clone()));
                    }
                }
                let (digest, actual_bytes) = hash_file(&mut output, limits.max_file_bytes, cancel)?;
                if actual_bytes != *bytes
                    || stamp(&input.metadata().map_err(|error| io(&entry.path, error))?)? != before
                {
                    return Err(EnvironmentError::InputChanged(entry.path.clone()));
                }
                let desired = if sealed {
                    0o444 | (*mode & 0o111)
                } else {
                    (*mode & 0o777) | 0o200
                };
                set_mode(&output, desired)?;
                sync_file(&output).map_err(|error| io(&entry.path, error))?;
                let metadata = output.metadata().map_err(|error| io(&entry.path, error))?;
                if stamp(&metadata)?.identity == before.identity {
                    return Err(EnvironmentError::Corrupt(
                        "destination aliases the cache/input file inode".into(),
                    ));
                }
                stats.logical_bytes =
                    checked_add(stats.logical_bytes, actual_bytes, "logical bytes")?;
                stats.allocated_file_bytes_sum = checked_add(
                    stats.allocated_file_bytes_sum,
                    allocated_bytes(&metadata)?,
                    "allocated bytes",
                )?;
                if cloned {
                    stats.cloned_files += 1;
                } else {
                    stats.copied_files += 1;
                }
                (
                    ManifestKind::File {
                        mode: *mode,
                        bytes: actual_bytes,
                        digest,
                    },
                    stamp(&metadata)?.identity,
                )
            }
            InventoryKind::Symlink { target } => {
                validate_symlink(&entry.path, target)?;
                let (parent, name) = open_parent(destination, &entry.path, false)?;
                parent
                    .symlink(Path::new(target), &name)
                    .map_err(|error| io(&entry.path, error))?;
                stats.symlinks += 1;
                (
                    ManifestKind::Symlink {
                        target: target.clone(),
                    },
                    stamp(
                        &parent
                            .metadata(&name)
                            .map_err(|error| io(&entry.path, error))?,
                    )?
                    .identity,
                )
            }
        };
        push(
            &mut manifest,
            ManifestEntry {
                root: entry.root,
                path: entry.path.clone(),
                identity,
                node,
            },
            "manifest entries",
        )?;
    }
    // Seal children before parents. Read-only metadata is a cooperation aid,
    // never the security immutability mechanism.
    for entry in entries.iter().rev() {
        if let InventoryKind::Directory { mode } = entry.kind {
            let directory = if entry.path.is_empty() {
                duplicate_directory(destination)?
            } else {
                open_directory(destination, &entry.path)?
            };
            set_mode(directory.file(), if sealed { 0o555 } else { mode | 0o700 })?;
            directory.sync().map_err(|error| io(&entry.path, error))?;
        }
    }
    Ok((manifest, stats))
}

pub(crate) fn logical_bytes(entries: &[InventoryEntry]) -> Result<u64> {
    entries.iter().try_fold(0, |total, entry| match entry.kind {
        InventoryKind::File { bytes, .. } => checked_add(total, bytes, "artifact bytes"),
        _ => Ok(total),
    })
}

pub(crate) fn entries_for_root(entries: &[InventoryEntry], root: u32) -> &[InventoryEntry] {
    let start = entries.partition_point(|entry| entry.root < root);
    let end = entries.partition_point(|entry| entry.root <= root);
    &entries[start..end]
}

pub(crate) fn unique_name(prefix: &str) -> Result<OsString> {
    let mut entropy = [0_u8; 16];
    getrandom::fill(&mut entropy).map_err(|error| {
        EnvironmentError::InvalidInput(format!("random staging identity unavailable: {error}"))
    })?;
    let mut name = String::with_capacity(prefix.len() + 32);
    name.push_str(prefix);
    for byte in entropy {
        use std::fmt::Write as _;
        write!(&mut name, "{byte:02x}")
            .map_err(|_| EnvironmentError::Allocation("staging name"))?;
    }
    Ok(name.into())
}

pub(crate) fn ensure_cache_root(path: &Path) -> Result<Directory> {
    // Absolute roots are opened component by component; no create_dir_all or
    // path-following operations are used for writable cache paths.
    let start = if path.is_absolute() {
        Path::new("/")
    } else {
        Path::new(".")
    };
    let mut directory = Directory::open(start).map_err(|error| io(start, error))?;
    directory
        .require_persistent_filesystem()
        .map_err(|error| io(path, error))?;
    for part in path.components() {
        match part {
            std::path::Component::Normal(name) => {
                let parent = directory;
                directory = parent.ensure_dir(name).map_err(|error| io(path, error))?;
                directory
                    .require_persistent_filesystem()
                    .map_err(|error| io(path, error))?;
                parent.sync().map_err(|error| io(path, error))?;
            }
            std::path::Component::RootDir | std::path::Component::CurDir => {}
            _ => {
                return Err(EnvironmentError::InvalidInput(
                    "cache path may not contain parent/prefix components".into(),
                ));
            }
        }
    }
    Ok(directory)
}

pub(crate) fn metadata_exists(directory: &Directory, name: &OsStr) -> Result<Option<Metadata>> {
    match directory.metadata(name) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(io(name, error)),
    }
}

pub(crate) fn directory_empty(directory: &Directory) -> Result<bool> {
    match directory
        .entries()
        .map_err(|error| io("<directory>", error))?
        .next()
    {
        None => Ok(true),
        Some(Ok(_)) => Ok(false),
        Some(Err(error)) => Err(io("<directory>", error)),
    }
}
