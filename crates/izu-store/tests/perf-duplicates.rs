#![cfg(all(feature = "fault-injection", unix))]

use izu_model::{CancellationToken, ObjectKind};
use izu_store::{DurableBoundary, FaultHook, Store, StoreOptions};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::sync::{Arc, Mutex};

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn existing_duplicates_keep_the_published_inode_and_complete_each_barrier() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?.join("store");
    let cancel = CancellationToken::new();
    let baseline = Store::init(&root, &StoreOptions::default())?;
    let id = baseline.put(ObjectKind::Blob, b"retained duplicate", &cancel)?;
    let name = id.to_string();
    let target = root.join("objects").join(&name[..2]).join(&name[2..]);
    let original = fs::metadata(&target)?;
    let bytes = fs::read(&target)?;
    drop(baseline);
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            observed
                .lock()
                .map_err(|_| io::Error::other("poisoned events"))?
                .push(boundary);
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(&root, &options)?;
    let mut batch = store.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"retained duplicate", &cancel)?;
    batch.stage(ObjectKind::Blob, b"retained duplicate", &cancel)?;
    let receipt = batch.finish(&cancel)?;
    assert_eq!(receipt.object_count, 2);
    let current = fs::metadata(&target)?;
    assert_eq!(
        (current.dev(), current.ino()),
        (original.dev(), original.ino())
    );
    assert_eq!(fs::read(&target)?, bytes);
    assert_eq!(
        store.get(id, ObjectKind::Blob, &cancel)?,
        b"retained duplicate"
    );
    assert_eq!(fs::read_dir(root.join("tmp"))?.count(), 0);
    let events = events
        .lock()
        .map_err(|_| io::Error::other("poisoned events"))?;
    for boundary in [
        DurableBoundary::BatchObjectKernelSynced,
        DurableBoundary::BatchObjectLinked,
        DurableBoundary::BatchFinalObjectKernelSynced,
    ] {
        assert_eq!(events.iter().filter(|event| **event == boundary).count(), 2);
    }
    for boundary in [
        DurableBoundary::BatchDataSynced,
        DurableBoundary::BatchDirectoriesKernelSynced,
        DurableBoundary::BatchDirectorySynced,
    ] {
        assert!(events.contains(&boundary));
    }
    Ok(())
}

#[test]
fn byte_identical_duplicate_replacement_after_identity_recording_refuses_receipt() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?.join("store");
    let cancel = CancellationToken::new();
    let baseline = Store::init(&root, &StoreOptions::default())?;
    let id = baseline.put(ObjectKind::Blob, b"identity matters", &cancel)?;
    let name = id.to_string();
    let target = root.join("objects").join(&name[..2]).join(&name[2..]);
    let held = directory.path().join("held-original");
    let original = fs::read(&target)?;
    drop(baseline);
    let hook_target = target.clone();
    let hook_held = held.clone();
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::BatchObjectLinked {
                fs::rename(&hook_target, &hook_held)?;
                fs::copy(&hook_held, &hook_target)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(&root, &options)?;
    let mut batch = store.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"identity matters", &cancel)?;
    assert!(batch.finish(&cancel).is_err());
    assert_eq!(fs::read(&target)?, original);
    assert_eq!(fs::read(&held)?, original);
    assert_eq!(fs::read_dir(root.join("tmp"))?.count(), 0);
    Ok(())
}

#[test]
fn simultaneous_duplicate_batches_preserve_one_verified_object() -> TestResult {
    let directory = tempfile::tempdir()?;
    let root = directory.path().canonicalize()?.join("store");
    Store::init(&root, &StoreOptions::default())?;
    let barrier = Arc::new(std::sync::Barrier::new(4));
    let mut writers = Vec::new();
    for _ in 0..4 {
        let root = root.clone();
        let barrier = barrier.clone();
        writers.push(std::thread::spawn(move || -> Result<_, String> {
            let cancel = CancellationToken::new();
            let store = Store::open(&root, &StoreOptions::default()).map_err(|e| e.to_string())?;
            let mut batch = store
                .begin_object_batch(&cancel)
                .map_err(|e| e.to_string())?;
            let id = batch
                .stage(ObjectKind::Blob, b"simultaneous duplicate", &cancel)
                .map_err(|e| e.to_string())?;
            barrier.wait();
            batch.finish(&cancel).map_err(|e| e.to_string())?;
            Ok(id.object_id())
        }));
    }
    let cancel = CancellationToken::new();
    let store = Store::open(&root, &StoreOptions::default())?;
    for writer in writers {
        let id = writer
            .join()
            .map_err(|_| io::Error::other("writer panicked"))?
            .map_err(io::Error::other)?;
        assert_eq!(
            store.get(id, ObjectKind::Blob, &cancel)?,
            b"simultaneous duplicate"
        );
    }
    assert_eq!(fs::read_dir(root.join("tmp"))?.count(), 0);
    Ok(())
}

#[test]
fn reused_blob_replacement_before_finish_is_not_acknowledged() -> TestResult {
    let d = tempfile::tempdir()?;
    let root = d.path().join("store");
    let c = CancellationToken::new();
    let store = Store::init(&root, &StoreOptions::default())?;
    let id = store.put(ObjectKind::Blob, b"reuse identity", &c)?;
    let name = id.to_string();
    let path = root.join("objects").join(&name[..2]).join(&name[2..]);
    let mut batch = store.begin_object_batch(&c)?;
    batch.stage_blob_reusing(&mut std::io::Cursor::new(b"reuse identity"), 14, id, &c)?;
    let kept = d.path().join("original");
    fs::rename(&path, &kept)?;
    fs::copy(&kept, &path)?;
    assert!(batch.finish(&c).is_err());
    assert_eq!(store.get(id, ObjectKind::Blob, &c)?, b"reuse identity");
    Ok(())
}
