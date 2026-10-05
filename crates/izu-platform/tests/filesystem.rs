#![cfg(unix)]

use izu_platform::{Directory, EntryKind, sync_file};
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::Barrier;

#[test]
fn anchored_files_reject_symlinks_and_path_escape() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root_path = fixture.path().canonicalize()?;
    let root = Directory::open(&root_path)?;
    root.require_persistent_filesystem()?;
    let mut file = root.create_new_file(OsStr::new("original"))?;
    file.write_all(b"keep")?;
    sync_file(&file)?;
    root.symlink(Path::new("original"), OsStr::new("link"))?;
    assert!(root.open_read(OsStr::new("link")).is_err());
    assert!(root.metadata(OsStr::new("link"))?.file_type().is_symlink());
    assert_eq!(
        root.read_link(OsStr::new("link"), 4096)?,
        Path::new("original")
    );
    assert!(root.open_read(OsStr::new("../original")).is_err());
    assert!(root.open_dir(OsStr::new("..")).is_err());
    root.create_dir(OsStr::new("child"))?;
    root.symlink(Path::new("child"), OsStr::new("directory-link"))?;
    assert!(root.open_dir(OsStr::new("directory-link")).is_err());
    assert!(Directory::open(&root_path.join("directory-link")).is_err());
    let entries: Vec<_> = root.entries()?.collect::<std::io::Result<Vec<_>>>()?;
    assert!(
        entries
            .iter()
            .any(|entry| entry.name == "link" && entry.kind == EntryKind::Symlink)
    );
    root.sync()?;
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn known_tmpfs_is_rejected_even_when_sync_succeeds() -> Result<(), Box<dyn std::error::Error>> {
    // The integrator's Linux container supplies /dev/shm as bounded tmpfs.
    // A host without that mount cannot execute this filesystem-specific case.
    let Ok(path) = std::fs::canonicalize("/dev/shm") else {
        return Ok(());
    };
    let directory = Directory::open(&path)?;
    let error = directory
        .require_persistent_filesystem()
        .expect_err("shared-memory filesystem is volatile");
    assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
    directory.sync()?;
    Ok(())
}

#[test]
fn hard_link_and_directory_rename_never_overwrite() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(&fixture.path().canonicalize()?)?;
    let mut first = root.create_new_file(OsStr::new("first"))?;
    first.write_all(b"first")?;
    let mut second = root.create_new_file(OsStr::new("second"))?;
    second.write_all(b"second")?;
    assert_eq!(
        root.hard_link(OsStr::new("first"), &root, OsStr::new("second"))
            .expect_err("must not replace")
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    root.create_dir(OsStr::new("from"))?;
    root.create_dir(OsStr::new("exists"))?;
    assert!(
        root.rename_noreplace(OsStr::new("from"), &root, OsStr::new("exists"))
            .is_err()
    );
    root.open_dir(OsStr::new("from"))?;
    root.rename_noreplace(OsStr::new("from"), &root, OsStr::new("new"))?;
    let new = root.open_dir(OsStr::new("new"))?;
    new.create_new_file(OsStr::new("held"))?;
    assert!(root.remove_dir(OsStr::new("new")).is_err());
    new.remove_file(OsStr::new("held"))?;
    root.remove_dir(OsStr::new("new"))?;
    let mut original = String::new();
    root.open_read(OsStr::new("second"))?
        .read_to_string(&mut original)?;
    assert_eq!(original, "second");
    root.sync()?;
    Ok(())
}

#[test]
fn anchored_directory_survives_namespace_replacement() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root_path = fixture.path().canonicalize()?;
    let root = Directory::open(&root_path)?;
    let child = root.create_dir(OsStr::new("held"))?;
    let duplicate = child.try_clone()?;
    drop(child);
    root.rename_noreplace(OsStr::new("held"), &root, OsStr::new("original"))?;
    root.symlink(Path::new("somewhere-else"), OsStr::new("held"))?;
    let mut file = duplicate.create_new_file(OsStr::new("inside"))?;
    file.write_all(b"anchored")?;
    assert!(root_path.join("original/inside").exists());
    assert!(!root_path.join("somewhere-else/inside").exists());
    sync_file(&file)?;
    duplicate.sync()?;
    root.sync()?;
    Ok(())
}

#[test]
fn concurrent_lock_creation_opens_one_permanent_inode() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(&fixture.path().canonicalize()?)?;
    let parent_identity = root.metadata_self()?;
    // Each fresh name exercises creation, not just reopening an existing lock.
    for round in 0..128 {
        let name = format!("same-key-{round}.lock");
        let barrier = Barrier::new(2);
        let (left, right) = std::thread::scope(|scope| {
            let left = scope.spawn(|| {
                barrier.wait();
                root.open_lock(OsStr::new(&name))
            });
            let right = scope.spawn(|| {
                barrier.wait();
                root.open_lock(OsStr::new(&name))
            });
            (left.join(), right.join())
        });
        let left = left.expect("lock opener thread panicked");
        let right = right.expect("lock opener thread panicked");
        let still_anchored = root.metadata_self()?;
        assert_eq!(parent_identity.dev(), still_anchored.dev());
        assert_eq!(parent_identity.ino(), still_anchored.ino());
        assert!(still_anchored.nlink() > 0);
        let entry = root.metadata(OsStr::new(&name))?;
        assert!(entry.is_file());
        if left.is_err() || right.is_err() {
            eprintln!(
                "round {round}: left={left:?}, right={right:?}, linked parent dev/ino={}/{}, entry inode={}",
                still_anchored.dev(),
                still_anchored.ino(),
                entry.ino()
            );
        }
        let left = left?;
        let right = right?;
        let left_identity = left.metadata()?;
        let right_identity = right.metadata()?;
        assert_eq!(left_identity.dev(), right_identity.dev());
        assert_eq!(left_identity.ino(), right_identity.ino());
        assert_eq!(left_identity.ino(), entry.ino());
    }
    Ok(())
}

#[test]
fn lock_open_preserves_existing_data_and_rejects_invalid_entries()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(&fixture.path().canonicalize()?)?;
    let mut original = root.open_lock(OsStr::new("held.lock"))?;
    original.write_all(b"retained")?;
    let identity = original.metadata()?;
    let mut existing = root.open_lock(OsStr::new("held.lock"))?;
    assert_eq!(identity.ino(), existing.metadata()?.ino());
    let mut bytes = Vec::new();
    existing.read_to_end(&mut bytes)?;
    assert_eq!(bytes, b"retained");
    root.symlink(Path::new("held.lock"), OsStr::new("link.lock"))?;
    assert!(root.open_lock(OsStr::new("link.lock")).is_err());
    root.create_dir(OsStr::new("directory.lock"))?;
    assert!(root.open_lock(OsStr::new("directory.lock")).is_err());
    let removed = root.create_dir(OsStr::new("removed"))?;
    root.remove_dir(OsStr::new("removed"))?;
    assert_eq!(
        removed
            .open_lock(OsStr::new("never-created.lock"))
            .expect_err("removed pinned parents remain an error")
            .kind(),
        std::io::ErrorKind::NotFound
    );
    assert!(root.metadata(OsStr::new("removed")).is_err());
    Ok(())
}
