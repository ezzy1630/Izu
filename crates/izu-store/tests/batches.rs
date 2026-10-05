#![cfg(feature = "fault-injection")]

use izu_model::{
    CancellationToken, FileMode, ObjectId, ObjectKind, Operation, OperationId, RepoPath,
    RepositoryView, Tree, TreeEntry, encode_metadata, hash_object,
};
use izu_store::{
    DurableBoundary, FaultHook, HeadExpectation, ObjectBatchReceipt, PublishOutcome, Store,
    StoreError, StoreOptions,
};
use std::cell::Cell;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture(
    options: &StoreOptions,
) -> Result<(tempfile::TempDir, PathBuf, Store), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().canonicalize()?.join("store");
    let store = Store::init(&path, options)?;
    Ok((directory, path, store))
}

fn object_path(path: &Path, id: ObjectId) -> PathBuf {
    let name = id.to_string();
    path.join("objects").join(&name[..2]).join(&name[2..])
}

fn publish(store: &Store, label: &str) -> Result<OperationId, StoreError> {
    let cancel = CancellationToken::new();
    let transaction = store.begin(HeadExpectation::Any, &cancel)?;
    let operation = Operation {
        parent: transaction.current_head(),
        view: RepositoryView::default(),
        description: label.into(),
        created_at_unix_ms: 1,
    };
    let bytes = encode_metadata(&operation, &store.options().limits)?;
    let id = OperationId::from_object(store.put(ObjectKind::Operation, &bytes, &cancel)?);
    assert!(matches!(
        transaction.publish(id, &cancel)?,
        PublishOutcome::Durable(_)
    ));
    Ok(id)
}

#[test]
fn staged_objects_are_provisional_then_reopen_with_exact_bytes() -> TestResult {
    let (_directory, path, store) = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let existing = store.put(ObjectKind::Blob, b"existing", &cancel)?;
    let payload = [0_u8, 255, 1, 13, 10, 128];
    let mut batch = store.begin_object_batch(&cancel)?;
    let blob = batch
        .stage_blob(&mut payload.as_slice(), payload.len() as u64, &cancel)?
        .object_id();
    assert!(store.get(blob, ObjectKind::Blob, &cancel).is_err());
    assert_eq!(
        batch
            .stage(ObjectKind::Blob, &payload, &cancel)?
            .object_id(),
        blob
    );
    assert_eq!(
        batch
            .stage(ObjectKind::Blob, b"existing", &cancel)?
            .object_id(),
        existing
    );
    let empty = batch.stage_blob(&mut io::empty(), 0, &cancel)?.object_id();
    let mut tree = Tree::default();
    tree.entries.insert(
        RepoPath::new("binary")?,
        TreeEntry::File {
            blob,
            mode: FileMode::Regular,
        },
    );
    let bytes = encode_metadata(&tree, &store.options().limits)?;
    let tree_id = batch.stage(ObjectKind::Tree, &bytes, &cancel)?.object_id();
    assert!(!object_path(&path, tree_id).exists());
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 5);
    let receipt = batch.finish(&cancel)?;
    assert_eq!(
        receipt,
        ObjectBatchReceipt {
            object_count: 5,
            payload_bytes: 2 * payload.len() as u64 + 8 + bytes.len() as u64
        }
    );
    drop(store);
    let reopened = Store::open(&path, &StoreOptions::default())?;
    assert_eq!(reopened.get(blob, ObjectKind::Blob, &cancel)?, payload);
    assert_eq!(
        reopened.get(existing, ObjectKind::Blob, &cancel)?,
        b"existing"
    );
    assert!(reopened.get(empty, ObjectKind::Blob, &cancel)?.is_empty());
    assert_eq!(reopened.get(tree_id, ObjectKind::Tree, &cancel)?, bytes);
    assert_eq!(reopened.verify(&cancel)?.object_count, 4);
    assert_eq!(reopened.current_head(&cancel)?, None);
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    let empty_batch = reopened.begin_object_batch(&cancel)?.finish(&cancel)?;
    assert_eq!(empty_batch.object_count, 0);
    Ok(())
}

#[test]
fn drop_and_failed_streaming_never_acknowledge_a_partial_batch() -> TestResult {
    for (bytes, declared) in [(b"short".as_slice(), 6), (b"extra".as_slice(), 4)] {
        let (_directory, path, store) = fixture(&StoreOptions::default())?;
        let cancel = CancellationToken::new();
        let mut batch = store.begin_object_batch(&cancel)?;
        batch.stage(ObjectKind::Blob, b"first", &cancel)?;
        let mut reader = bytes;
        assert!(matches!(
            batch.stage_blob(&mut reader, declared, &cancel),
            Err(StoreError::InputLengthMismatch)
        ));
        assert!(matches!(
            batch.finish(&cancel),
            Err(StoreError::BatchAborted)
        ));
        assert_eq!(store.verify(&cancel)?.object_count, 0);
        assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    }
    let (_directory, path, store) = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let mut batch = store.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"drop me", &cancel)?;
    drop(batch);
    assert_eq!(store.verify(&cancel)?.object_count, 0);
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    let calls = Cell::new(0);
    let mut batch = store.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"good before reader error", &cancel)?;
    assert!(matches!(
        batch.stage_blob(&mut ObservedReader(&calls), 1, &cancel),
        Err(StoreError::Io { .. })
    ));
    assert_eq!(calls.get(), 1);
    assert!(matches!(
        batch.finish(&cancel),
        Err(StoreError::BatchAborted)
    ));
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    Ok(())
}

#[test]
fn streamed_blob_crosses_chunks_and_obeys_the_configured_maximum() -> TestResult {
    let maximum = 2 * 64 * 1024 + 3;
    let mut options = StoreOptions::default();
    options.limits.max_blob_bytes = maximum;
    let (_directory, path, store) = fixture(&options)?;
    let cancel = CancellationToken::new();
    let bytes: Vec<u8> = (0..maximum).map(|index| (index % 251) as u8).collect();
    let mut batch = store.begin_object_batch(&cancel)?;
    let id = batch
        .stage_blob(&mut bytes.as_slice(), maximum, &cancel)?
        .object_id();
    let receipt = batch.finish(&cancel)?;
    assert_eq!(receipt.payload_bytes, maximum);
    assert_eq!(store.get(id, ObjectKind::Blob, &cancel)?, bytes);
    let calls = Cell::new(0);
    let mut refused = store.begin_object_batch(&cancel)?;
    assert!(matches!(
        refused.stage_blob(&mut ObservedReader(&calls), maximum + 1, &cancel),
        Err(StoreError::Model(_))
    ));
    assert_eq!(calls.get(), 0);
    assert!(matches!(
        refused.finish(&cancel),
        Err(StoreError::BatchAborted)
    ));
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    Ok(())
}

struct ObservedReader<'a>(&'a Cell<usize>);

impl Read for ObservedReader<'_> {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        self.0.set(self.0.get() + 1);
        Err(io::Error::other("reader was invoked"))
    }
}

#[test]
fn batch_budgets_are_checked_before_reading_or_creating_next_temp() -> TestResult {
    for (options, first) in [
        (
            StoreOptions {
                max_scan_objects: 1,
                ..StoreOptions::default()
            },
            Some(b"a".as_slice()),
        ),
        (
            StoreOptions {
                max_scan_bytes: 1,
                ..StoreOptions::default()
            },
            Some(b"a".as_slice()),
        ),
        (
            StoreOptions {
                max_in_memory_bytes: 1,
                ..StoreOptions::default()
            },
            None,
        ),
    ] {
        let (_directory, path, store) = fixture(&options)?;
        let cancel = CancellationToken::new();
        let calls = Cell::new(0);
        let mut batch = store.begin_object_batch(&cancel)?;
        if let Some(first) = first {
            batch.stage(ObjectKind::Blob, first, &cancel)?;
        }
        let previous = fs::read_dir(path.join("tmp"))?.count();
        assert!(matches!(
            batch.stage_blob(&mut ObservedReader(&calls), 1, &cancel),
            Err(StoreError::LimitExceeded(_))
        ));
        assert_eq!(calls.get(), 0);
        assert_eq!(fs::read_dir(path.join("tmp"))?.count(), previous);
        assert!(matches!(
            batch.finish(&cancel),
            Err(StoreError::BatchAborted)
        ));
        assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    }
    let (_directory, _, store) = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let calls = Cell::new(0);
    let mut batch = store.begin_object_batch(&cancel)?;
    assert!(matches!(
        batch.stage_blob(&mut ObservedReader(&calls), u64::MAX, &cancel),
        Err(StoreError::Model(_))
    ));
    assert_eq!(calls.get(), 0);
    assert!(matches!(
        batch.finish(&cancel),
        Err(StoreError::BatchAborted)
    ));
    Ok(())
}

#[test]
fn batch_preserves_write_hooks_and_orders_full_barriers_without_strict_file_hook() -> TestResult {
    let (_directory, path, baseline) = fixture(&StoreOptions::default())?;
    let events = Arc::new(Mutex::new(Vec::new()));
    let observed = events.clone();
    let hook_path = path.clone();
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if matches!(
                boundary,
                DurableBoundary::BatchTemporaryKernelSynced | DurableBoundary::BatchDataSynced
            ) && fs::read_dir(hook_path.join("objects"))?.count() != 0
            {
                return Err(io::Error::other("object linked before first full barrier"));
            }
            observed
                .lock()
                .map_err(|_| io::Error::other("poisoned events"))?
                .push(boundary);
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    drop(baseline);
    let store = Store::open(&path, &options)?;
    let cancel = CancellationToken::new();
    let mut batch = store.begin_object_batch(&cancel)?;
    for bytes in [b"one".as_slice(), b"two".as_slice()] {
        batch.stage(ObjectKind::Blob, bytes, &cancel)?;
    }
    batch.finish(&cancel)?;
    let observed = events
        .lock()
        .map_err(|_| io::Error::other("poisoned events"))?;
    for boundary in [
        DurableBoundary::ObjectTempCreated,
        DurableBoundary::ObjectDataWritten,
        DurableBoundary::BatchObjectKernelSynced,
        DurableBoundary::BatchFinalObjectKernelSynced,
    ] {
        assert_eq!(
            observed.iter().filter(|event| **event == boundary).count(),
            2
        );
    }
    assert!(!observed.contains(&DurableBoundary::ObjectFileSynced));
    let data = observed
        .iter()
        .position(|event| *event == DurableBoundary::BatchDataSynced)
        .expect("data barrier");
    let linked = observed
        .iter()
        .position(|event| *event == DurableBoundary::BatchObjectLinked)
        .expect("link boundary");
    let directories = observed
        .iter()
        .position(|event| *event == DurableBoundary::BatchDirectoriesKernelSynced)
        .expect("directory submissions");
    let full = observed
        .iter()
        .position(|event| *event == DurableBoundary::BatchDirectorySynced)
        .expect("namespace full barrier");
    assert!(data < linked && linked < directories && directories < full);
    Ok(())
}

const BATCH_BOUNDARIES: &[DurableBoundary] = &[
    DurableBoundary::ObjectTempCreated,
    DurableBoundary::ObjectDataWritten,
    DurableBoundary::BatchObjectKernelSynced,
    DurableBoundary::BatchTemporaryKernelSynced,
    DurableBoundary::BatchDataSynced,
    DurableBoundary::BatchObjectLinked,
    DurableBoundary::BatchFinalObjectKernelSynced,
    DurableBoundary::ClosureDirectorySubmitted,
    DurableBoundary::BatchDirectoriesKernelSynced,
    DurableBoundary::BatchDirectorySynced,
];

#[test]
fn enospc_and_cancellation_at_every_batch_boundary_keep_the_acknowledged_head() -> TestResult {
    for boundary in BATCH_BOUNDARIES {
        for cancelled in [false, true] {
            let (_directory, path, baseline) = fixture(&StoreOptions::default())?;
            let head = publish(&baseline, "prior acknowledged operation")?;
            let head_bytes = fs::read(path.join("HEAD"))?;
            let cancel = CancellationToken::new();
            let hook_cancel = cancel.clone();
            let target = *boundary;
            let options = StoreOptions {
                fault_hook: Some(FaultHook(Arc::new(move |current| {
                    if current == target {
                        if cancelled {
                            hook_cancel.cancel();
                        } else {
                            return Err(io::Error::from_raw_os_error(28));
                        }
                    }
                    Ok(())
                }))),
                ..StoreOptions::default()
            };
            let store = Store::open(&path, &options)?;
            let mut batch = store.begin_object_batch(&cancel)?;
            let staged = batch.stage(ObjectKind::Blob, b"batch candidate", &cancel);
            let outcome = batch.finish(&cancel);
            assert!(
                outcome.is_err(),
                "{boundary:?}, cancelled={cancelled}: {outcome:?}"
            );
            if staged.is_err() {
                assert!(matches!(outcome, Err(StoreError::BatchAborted)));
            } else if !cancelled {
                assert!(
                    matches!(outcome, Err(StoreError::Io { source, .. }) if source.raw_os_error() == Some(28))
                );
            }
            assert_eq!(fs::read(path.join("HEAD"))?, head_bytes);
            let reopened = Store::open(&path, &StoreOptions::default())?;
            assert_eq!(
                reopened.recover(&CancellationToken::new())?.verified.head,
                Some(head)
            );
            assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
        }
    }
    Ok(())
}

#[test]
fn corrupted_duplicate_is_retained_and_refuses_batch_receipt() -> TestResult {
    let (_directory, path, store) = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let id = store.put(ObjectKind::Blob, b"duplicate", &cancel)?;
    let target = object_path(&path, id);
    fs::write(&target, b"damaged existing inode")?;
    let mut batch = store.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"duplicate", &cancel)?;
    assert!(batch.finish(&cancel).is_err());
    assert_eq!(fs::read(target)?, b"damaged existing inode");
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    Ok(())
}

#[test]
fn substituted_temp_names_are_never_deleted_by_stage_error_or_drop() -> TestResult {
    for boundary in [
        DurableBoundary::ObjectTempCreated,
        DurableBoundary::ObjectDataWritten,
        DurableBoundary::BatchObjectKernelSynced,
        DurableBoundary::BatchDataSynced,
    ] {
        let (directory, path, _) = fixture(&StoreOptions::default())?;
        let captured_name = Arc::new(Mutex::new(None));
        let observed = captured_name.clone();
        let tmp = path.join("tmp");
        let held = directory.path().join("original-prepared-file");
        let options = StoreOptions {
            fault_hook: Some(FaultHook(Arc::new(move |current| {
                if current == boundary {
                    let name = fs::read_dir(&tmp)?
                        .next()
                        .ok_or_else(|| io::Error::other("missing temp"))??
                        .path();
                    fs::rename(&name, &held)?;
                    fs::write(&name, b"unknown replacement must survive")?;
                    *observed
                        .lock()
                        .map_err(|_| io::Error::other("poisoned name"))? = Some(name);
                }
                Ok(())
            }))),
            ..StoreOptions::default()
        };
        let store = Store::open(&path, &options)?;
        let cancel = CancellationToken::new();
        let mut batch = store.begin_object_batch(&cancel)?;
        let _staged = batch.stage(ObjectKind::Blob, b"source bytes", &cancel);
        assert!(batch.finish(&cancel).is_err());
        let name = captured_name
            .lock()
            .map_err(|_| io::Error::other("poisoned name"))?
            .clone()
            .expect("substituted name");
        assert_eq!(fs::read(name)?, b"unknown replacement must survive");
    }
    Ok(())
}

#[test]
fn late_native_shard_and_final_object_substitution_cannot_receive_a_receipt() -> TestResult {
    for (boundary, target_kind) in [
        (DurableBoundary::BatchTemporaryKernelSynced, "tmp"),
        (DurableBoundary::BatchDataSynced, "objects"),
        (DurableBoundary::BatchDirectoriesKernelSynced, "shard"),
        (DurableBoundary::BatchDirectorySynced, "shard"),
        (DurableBoundary::BatchObjectLinked, "object"),
        (DurableBoundary::BatchDirectorySynced, "object"),
    ] {
        let (directory, path, baseline) = fixture(&StoreOptions::default())?;
        publish(&baseline, "namespace baseline")?;
        let head = fs::read(path.join("HEAD"))?;
        let payload = b"batch namespace source";
        let id = hash_object(ObjectKind::Blob, payload, &baseline.options().limits)?;
        let object = object_path(&path, id);
        let target = match target_kind {
            "tmp" | "objects" => path.join(target_kind),
            "shard" => object.parent().expect("shard").to_path_buf(),
            _ => object,
        };
        let original = directory.path().join("retained-original");
        let hook_target = target.clone();
        let hook_original = original.clone();
        let options = StoreOptions {
            fault_hook: Some(FaultHook(Arc::new(move |current| {
                if current == boundary {
                    fs::rename(&hook_target, &hook_original)?;
                    if target_kind == "object" {
                        fs::write(&hook_target, b"unknown replacement")?;
                    } else {
                        fs::create_dir(&hook_target)?;
                        fs::write(hook_target.join("unknown"), b"preserve")?;
                    }
                }
                Ok(())
            }))),
            ..StoreOptions::default()
        };
        let store = Store::open(&path, &options)?;
        let cancel = CancellationToken::new();
        let mut batch = store.begin_object_batch(&cancel)?;
        batch.stage(ObjectKind::Blob, payload, &cancel)?;
        assert!(
            batch.finish(&cancel).is_err(),
            "{target_kind} at {boundary:?}"
        );
        assert_eq!(fs::read(path.join("HEAD"))?, head);
        assert!(original.exists());
        if target_kind == "object" {
            assert_eq!(fs::read(&target)?, b"unknown replacement");
        } else {
            assert_eq!(fs::read(target.join("unknown"))?, b"preserve");
        }
    }
    Ok(())
}

#[test]
fn recovery_waits_for_the_entire_batch_lifetime_and_volatile_mode_refuses_receipts() -> TestResult {
    let (_directory, path, baseline) = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let mut batch = baseline.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"in flight", &cancel)?;
    let recovery = Store::open(
        &path,
        &StoreOptions {
            lock_timeout: Duration::from_millis(25),
            ..StoreOptions::default()
        },
    )?;
    assert!(matches!(
        recovery.recover(&cancel),
        Err(StoreError::LockTimeout("temporary.lock"))
    ));
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 1);
    batch.finish(&cancel)?;
    assert_eq!(recovery.recover(&cancel)?.retained_objects, 1);
    let volatile = Store::open(
        &path,
        &StoreOptions {
            allow_volatile_for_tests: true,
            ..StoreOptions::default()
        },
    )?;
    let mut batch = volatile.begin_object_batch(&cancel)?;
    batch.stage(ObjectKind::Blob, b"explicit test mode", &cancel)?;
    assert!(matches!(
        batch.finish(&cancel),
        Err(StoreError::VolatileTestMode)
    ));
    assert!(matches!(
        volatile.begin_object_batch(&cancel)?.finish(&cancel),
        Err(StoreError::VolatileTestMode)
    ));
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    Ok(())
}

fn child_command(path: &Path, mode: &str) -> Result<Command, io::Error> {
    let mut command = Command::new(std::env::current_exe()?);
    command
        .args(["--exact", "batch_process_child", "--nocapture"])
        .env("IZU_BATCH_TEST_PATH", path)
        .env("IZU_BATCH_TEST_MODE", mode)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    Ok(command)
}

#[test]
fn independent_batch_and_strict_writers_keep_duplicates_and_operation_history() -> TestResult {
    let (_directory, path, store) = fixture(&StoreOptions::default())?;
    publish(&store, "concurrent baseline")?;
    let mut children = Vec::new();
    for index in 0..4 {
        let mut command = child_command(&path, if index % 2 == 0 { "batch" } else { "strict" })?;
        command.env("IZU_BATCH_TEST_WRITER", index.to_string());
        children.push(command.spawn()?);
    }
    for mut child in children {
        assert!(child.wait()?.success());
    }
    let cancel = CancellationToken::new();
    let mut current = store.current_head(&cancel)?;
    let mut count = 0;
    while let Some(id) = current {
        let bytes = store.get(id.object_id(), ObjectKind::Operation, &cancel)?;
        let operation: Operation = izu_model::decode_metadata(&bytes, &store.options().limits)?;
        current = operation.parent;
        count += 1;
    }
    assert_eq!(count, 9);
    let common = hash_object(
        ObjectKind::Blob,
        b"shared by independent writers",
        &store.options().limits,
    )?;
    assert_eq!(
        store.get(common, ObjectKind::Blob, &cancel)?,
        b"shared by independent writers"
    );
    store.verify(&cancel)?;
    Ok(())
}

#[test]
fn process_kill_at_each_batch_boundary_preserves_prior_head_and_reclaims_temps() -> TestResult {
    for boundary in BATCH_BOUNDARIES {
        let (directory, path, store) = fixture(&StoreOptions::default())?;
        let old = publish(&store, "pre-kill durable operation")?;
        let head = fs::read(path.join("HEAD"))?;
        let marker = directory.path().join("ready");
        let mut command = child_command(&path, "crash")?;
        command
            .env("IZU_BATCH_TEST_BOUNDARY", format!("{boundary:?}"))
            .env("IZU_BATCH_TEST_MARKER", &marker);
        let mut child = command.spawn()?;
        let start = Instant::now();
        while !marker.exists() {
            if let Some(status) = child.try_wait()? {
                panic!("child exited before {boundary:?}: {status}");
            }
            if start.elapsed() > Duration::from_secs(10) {
                child.kill()?;
                child.wait()?;
                panic!("child missed {boundary:?}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        child.kill()?;
        child.wait()?;
        let reopened = Store::open(&path, &StoreOptions::default())?;
        let cancel = CancellationToken::new();
        assert_eq!(fs::read(path.join("HEAD"))?, head);
        assert_eq!(reopened.recover(&cancel)?.verified.head, Some(old));
        assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
        publish(&reopened, "locks released after kill")?;
    }
    Ok(())
}

#[cfg(unix)]
#[test]
fn many_staged_objects_do_not_retain_one_descriptor_each() -> TestResult {
    let (_directory, path, _) = fixture(&StoreOptions::default())?;
    assert!(child_command(&path, "fds")?.status()?.success());
    Ok(())
}

#[test]
fn batch_process_child() -> TestResult {
    let Some(path) = std::env::var_os("IZU_BATCH_TEST_PATH") else {
        return Ok(());
    };
    let mode = std::env::var("IZU_BATCH_TEST_MODE")?;
    let options = if mode == "crash" {
        let boundary = std::env::var("IZU_BATCH_TEST_BOUNDARY")?;
        let marker = std::env::var_os("IZU_BATCH_TEST_MARKER").expect("owned marker");
        StoreOptions {
            fault_hook: Some(FaultHook(Arc::new(move |current| {
                if format!("{current:?}") == boundary {
                    fs::write(&marker, b"ready")?;
                    loop {
                        thread::park();
                    }
                }
                Ok(())
            }))),
            ..StoreOptions::default()
        }
    } else {
        StoreOptions::default()
    };
    let store = Store::open(Path::new(&path), &options)?;
    let cancel = CancellationToken::new();
    if mode == "crash" {
        let mut batch = store.begin_object_batch(&cancel)?;
        batch.stage(ObjectKind::Blob, b"kill candidate", &cancel)?;
        batch.finish(&cancel)?;
        panic!("expected a blocked batch boundary");
    }
    #[cfg(unix)]
    if mode == "fds" {
        let fd_path = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        let before = fs::read_dir(fd_path)?.count();
        let mut batch = store.begin_object_batch(&cancel)?;
        for index in 0_u64..64 {
            batch.stage(ObjectKind::Blob, &index.to_le_bytes(), &cancel)?;
            assert!(fs::read_dir(fd_path)?.count() <= before + 2);
        }
        batch.finish(&cancel)?;
        return Ok(());
    }
    let writer = std::env::var("IZU_BATCH_TEST_WRITER")?;
    for index in 0..2 {
        let unique = format!("writer {writer}, iteration {index}");
        if mode == "batch" {
            let mut batch = store.begin_object_batch(&cancel)?;
            batch.stage(ObjectKind::Blob, b"shared by independent writers", &cancel)?;
            batch.stage(ObjectKind::Blob, unique.as_bytes(), &cancel)?;
            batch.finish(&cancel)?;
        } else {
            store.put(ObjectKind::Blob, b"shared by independent writers", &cancel)?;
            store.put(ObjectKind::Blob, unique.as_bytes(), &cancel)?;
        }
        publish(&store, &unique)?;
    }
    Ok(())
}
