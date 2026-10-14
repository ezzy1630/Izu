//! Run this boundary experiment on native Windows; cross-checking its build
//! does not prove runtime behavior or filesystem persistence.
#![cfg(windows)]

#[path = "../src/windows/fs.rs"]
pub mod fs;
#[path = "../src/windows/nt.rs"]
mod nt;

use fs::Directory;
use izu_platform::FileIdentity;
use std::ffi::OsStr;
use std::io::{Read, Write};
use std::path::Path;

#[test]
fn reparse_points_never_anchor_access_or_follow_final_reads()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(fixture.path())?;
    let target = root.create_dir(OsStr::new("target"))?;
    target
        .create_new_file(OsStr::new("file"))?
        .write_all(b"original")?;
    std::os::windows::fs::symlink_dir(fixture.path().join("target"), fixture.path().join("link"))?;
    std::os::windows::fs::symlink_file(
        fixture.path().join("target/file"),
        fixture.path().join("file-link"),
    )?;
    assert!(root.open_dir(OsStr::new("link")).is_err());
    assert!(Directory::open(&fixture.path().join("link")).is_err());
    assert!(root.open_read(OsStr::new("file-link")).is_err());
    assert!(root.open_lock(OsStr::new("file-link")).is_err());
    assert!(root.remove_dir(OsStr::new("link")).is_err());
    assert!(fixture.path().join("target/file").is_file());
    Ok(())
}

#[test]
fn path_components_reject_escape_streams_aliases_and_overlong_names()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(fixture.path())?;
    for name in [
        "",
        ".",
        "..",
        "../outside",
        "a\\b",
        "file:stream",
        "CON",
        "nul.txt",
        "COM1",
        "LPT9.txt",
        "trailing.",
        "trailing ",
        "a?b",
        "a\0b",
    ] {
        assert_eq!(
            root.create_new_file(OsStr::new(name))
                .expect_err(name)
                .kind(),
            std::io::ErrorKind::InvalidInput
        );
    }
    assert!(root.create_new_file(OsStr::new(&"x".repeat(256))).is_err());
    assert!(Directory::open(&fixture.path().join("..")).is_err());
    assert!(Directory::open(Path::new("C:relative")).is_err());
    Ok(())
}

#[test]
fn directory_handle_stays_anchored_after_rename_and_replacement()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(fixture.path())?;
    let held = root.create_dir(OsStr::new("held"))?;
    let clone = held.try_clone()?;
    let identity = FileIdentity::of(held.file())?;
    root.rename_noreplace(OsStr::new("held"), &root, OsStr::new("moved"))?;
    root.create_dir(OsStr::new("held"))?;
    clone
        .create_new_file(OsStr::new("inside"))?
        .write_all(b"anchored")?;
    assert_eq!(identity, FileIdentity::of(clone.file())?);
    assert!(fixture.path().join("moved/inside").is_file());
    assert!(!fixture.path().join("held/inside").exists());
    assert!(held.metadata_self()?.is_dir());
    let entries = held.entries()?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].name, OsStr::new("inside"));
    assert_eq!(
        identity,
        FileIdentity::of(&nt::reopen_directory(held.file(), false)?)?
    );
    Ok(())
}

#[test]
fn atomic_name_changes_links_and_exact_object_deletion() -> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(fixture.path())?;
    root.create_new_file(OsStr::new("first"))?
        .write_all(b"first")?;
    root.create_new_file(OsStr::new("second"))?
        .write_all(b"second")?;
    assert_eq!(
        root.rename_noreplace(OsStr::new("first"), &root, OsStr::new("second"))
            .expect_err("must not replace")
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    assert_eq!(
        root.hard_link(OsStr::new("first"), &root, OsStr::new("second"))
            .expect_err("must not replace")
            .kind(),
        std::io::ErrorKind::AlreadyExists
    );
    eprintln!("atomic changes: collisions passed; creating hard link");
    root.hard_link(OsStr::new("first"), &root, OsStr::new("linked"))?;
    assert_eq!(
        FileIdentity::of(&root.open_read(OsStr::new("first"))?)?,
        FileIdentity::of(&root.open_read(OsStr::new("linked"))?)?
    );
    eprintln!("atomic changes: hard-link identity passed; replacing name");
    root.rename_replace(OsStr::new("first"), &root, OsStr::new("second"))?;
    let mut content = String::new();
    root.open_read(OsStr::new("second"))?
        .read_to_string(&mut content)?;
    assert_eq!(content, "first");
    let child = root.ensure_dir(OsStr::new("child"))?;
    root.ensure_dir(OsStr::new("child"))?;
    child.create_new_file(OsStr::new("file"))?;
    assert!(root.remove_dir(OsStr::new("child")).is_err());
    child.remove_file(OsStr::new("file"))?;
    root.remove_dir(OsStr::new("child"))?;
    assert!(root.metadata(OsStr::new("child")).is_err());
    eprintln!(
        "atomic changes: replacement and exact deletion passed; reopening enumeration handle"
    );
    let entries = root.entries()?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(entries.len(), 2);
    assert!(
        entries
            .iter()
            .all(|entry| entry.kind == fs::EntryKind::File)
    );
    let mut first = root.entries()?;
    let first_entry = first.next().transpose()?.ok_or("missing first entry")?;
    let other = root.entries()?.collect::<std::io::Result<Vec<_>>>()?;
    assert_eq!(other.len(), 2);
    let last = first.next().transpose()?.ok_or("scan cursor was shared")?;
    assert_ne!(first_entry.name, last.name);
    assert!(first.next().is_none());
    Ok(())
}

#[test]
fn permanent_lock_creation_converges_on_same_object_without_truncation()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(fixture.path())?;
    for round in 0..32 {
        let barrier = std::sync::Barrier::new(2);
        let name = format!("lock-{round}");
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
        let mut left = left.map_err(|_| "left thread panicked")??;
        let right = right.map_err(|_| "right thread panicked")??;
        assert_eq!(FileIdentity::of(&left)?, FileIdentity::of(&right)?);
        left.write_all(b"retained")?;
        let mut reopened = root.open_existing_lock(OsStr::new(&name))?;
        let mut data = String::new();
        reopened.read_to_string(&mut data)?;
        assert_eq!(data, "retained");
    }
    Ok(())
}

#[test]
fn directory_flush_probe_reports_actual_results_without_enabling_durable_ack()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let root = Directory::open(fixture.path())?;
    let mut file = root.create_new_file(OsStr::new("flush-file"))?;
    file.write_all(b"persistence probe")?;
    nt::flush(&file)?;
    nt::flush_win32(&file)?;
    println!("file NT/Win32 flush: success");
    println!(
        "directory read-handle NT flush: {:?}",
        nt::flush(root.file())
    );
    println!(
        "directory read-handle Win32 flush: {:?}",
        nt::flush_win32(root.file())
    );
    let writable = nt::reopen_directory(root.file(), true);
    match writable {
        Ok(directory) => {
            println!(
                "directory write-handle NT flush: {:?}",
                nt::flush(&directory)
            );
            println!(
                "directory write-handle Win32 flush: {:?}",
                nt::flush_win32(&directory)
            );
        }
        Err(error) => println!("directory write-handle open: {error:?}"),
    }
    for result in [
        root.sync(),
        root.require_persistent_filesystem(),
        fs::sync_file(&file),
        fs::sync_kernel(&file),
        fs::require_persistent_file(&file),
        fs::require_durable_platform(),
    ] {
        assert_eq!(
            result
                .expect_err("no persistence guarantee established")
                .kind(),
            std::io::ErrorKind::Unsupported
        );
    }
    assert_eq!(
        root.read_link(OsStr::new("link"), 4096)
            .expect_err("not implemented")
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    assert_eq!(
        root.symlink(Path::new("target"), OsStr::new("link"))
            .expect_err("not implemented")
            .kind(),
        std::io::ErrorKind::Unsupported
    );
    Ok(())
}
