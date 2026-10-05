#![cfg(feature = "fault-injection")]

use izu_model::{
    CancellationToken, ObjectKind, Operation, OperationId, RepositoryView, encode_metadata,
};
use izu_store::{
    DurableBoundary, FaultHook, HeadExpectation, PublishOutcome, Store, StoreError, StoreOptions,
};
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn options_at(
    boundary: DurableBoundary,
    hook: impl Fn() -> io::Result<()> + Send + Sync + 'static,
) -> StoreOptions {
    StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |current| {
            if current == boundary { hook() } else { Ok(()) }
        }))),
        ..StoreOptions::default()
    }
}

fn operation(
    store: &Store,
    parent: Option<OperationId>,
    label: &str,
) -> Result<OperationId, StoreError> {
    let op = Operation {
        parent,
        view: RepositoryView::default(),
        description: label.into(),
        created_at_unix_ms: 1,
    };
    let payload = encode_metadata(&op, &store.options().limits)?;
    store
        .put(ObjectKind::Operation, &payload, &CancellationToken::new())
        .map(OperationId::from_object)
}

fn init() -> Result<(tempfile::TempDir, PathBuf, Store), Box<dyn std::error::Error>> {
    let fixture = tempfile::tempdir()?;
    let path = fixture.path().canonicalize()?.join("store");
    let store = Store::init(&path, &StoreOptions::default())?;
    Ok((fixture, path, store))
}

#[test]
fn injected_enospc_at_each_object_boundary_never_publishes_head() -> TestResult {
    for boundary in [
        DurableBoundary::ObjectTempCreated,
        DurableBoundary::ObjectDataWritten,
        DurableBoundary::ObjectFileSynced,
        DurableBoundary::ObjectLinked,
        DurableBoundary::ObjectDirectorySynced,
    ] {
        let (_fixture, path, _) = init()?;
        let options = options_at(boundary, || Err(io::Error::from_raw_os_error(28)));
        let store = Store::open(&path, &options)?;
        assert!(
            matches!(store.put(ObjectKind::Blob, b"prepared source", &CancellationToken::new()), Err(StoreError::Io { source, .. }) if source.raw_os_error() == Some(28))
        );
        let reopened = Store::open(&path, &StoreOptions::default())?;
        assert_eq!(reopened.current_head(&CancellationToken::new())?, None);
        reopened.recover(&CancellationToken::new())?;
        assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
    }
    Ok(())
}

#[test]
fn head_boundary_failures_report_visibility_truthfully() -> TestResult {
    for boundary in [
        DurableBoundary::ClosureObjectSubmitted,
        DurableBoundary::ClosureDirectorySubmitted,
        DurableBoundary::HeadDataWritten,
        DurableBoundary::HeadFileKernelSynced,
        DurableBoundary::HeadFileSynced,
        DurableBoundary::HeadReplaced,
        DurableBoundary::HeadDirectoriesKernelSynced,
        DurableBoundary::HeadDirectorySynced,
    ] {
        let (_fixture, path, baseline) = init()?;
        let cancel = CancellationToken::new();
        let old = operation(&baseline, None, "baseline")?;
        baseline
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(old, &cancel)?;
        let new = operation(&baseline, Some(old), "new")?;
        let options = options_at(boundary, || Err(io::Error::from_raw_os_error(28)));
        let store = Store::open(&path, &options)?;
        let outcome = store
            .begin(HeadExpectation::At(old), &cancel)?
            .publish(new, &cancel);
        let visible = matches!(
            boundary,
            DurableBoundary::HeadReplaced
                | DurableBoundary::HeadDirectoriesKernelSynced
                | DurableBoundary::HeadDirectorySynced
        );
        if visible {
            assert!(
                matches!(outcome?, PublishOutcome::VisibleButUncertain { receipt, .. } if receipt.current == new && receipt.previous == Some(old))
            );
        } else {
            assert!(outcome.is_err());
        }
        let reopened = Store::open(&path, &StoreOptions::default())?;
        assert_eq!(
            reopened.current_head(&cancel)?,
            Some(if visible { new } else { old })
        );
        reopened.recover(&cancel)?;
    }
    Ok(())
}

#[test]
fn cancellation_before_publish_keeps_head_after_publish_finishes_receipt() -> TestResult {
    for boundary in [
        DurableBoundary::ClosureObjectSubmitted,
        DurableBoundary::ClosureDirectorySubmitted,
        DurableBoundary::HeadDataWritten,
        DurableBoundary::HeadFileKernelSynced,
        DurableBoundary::HeadFileSynced,
        DurableBoundary::HeadReplaced,
        DurableBoundary::HeadDirectoriesKernelSynced,
        DurableBoundary::HeadDirectorySynced,
    ] {
        let (_fixture, path, baseline) = init()?;
        let cancel = CancellationToken::new();
        let new = operation(&baseline, None, "new")?;
        let shared = cancel.clone();
        let options = options_at(boundary, move || {
            shared.cancel();
            Ok(())
        });
        let store = Store::open(&path, &options)?;
        let result = store
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(new, &cancel);
        let visible = matches!(
            boundary,
            DurableBoundary::HeadReplaced
                | DurableBoundary::HeadDirectoriesKernelSynced
                | DurableBoundary::HeadDirectorySynced
        );
        if visible {
            assert!(matches!(result?, PublishOutcome::Durable(receipt) if receipt.current == new));
        } else {
            assert!(result.is_err());
        }
        let reopened = Store::open(&path, &StoreOptions::default())?;
        assert_eq!(
            reopened.current_head(&CancellationToken::new())?,
            if visible { Some(new) } else { None }
        );
    }
    Ok(())
}

#[test]
fn recovery_waits_for_inflight_temp_writer_lease() -> TestResult {
    let (_fixture, path, _) = init()?;
    let enter = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let shared_enter = enter.clone();
    let shared_release = release.clone();
    let options = options_at(DurableBoundary::ObjectTempCreated, move || {
        shared_enter.wait();
        shared_release.wait();
        Ok(())
    });
    let writer_store = Store::open(&path, &options)?;
    let writer = thread::spawn(move || {
        writer_store.put(ObjectKind::Blob, b"in flight", &CancellationToken::new())
    });
    enter.wait();
    let recovery_store = Store::open(
        &path,
        &StoreOptions {
            lock_timeout: Duration::from_millis(30),
            ..StoreOptions::default()
        },
    )?;
    assert!(matches!(
        recovery_store.recover(&CancellationToken::new()),
        Err(StoreError::LockTimeout("temporary.lock"))
    ));
    assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 1);
    release.wait();
    writer.join().expect("writer thread")?;
    assert_eq!(
        recovery_store
            .recover(&CancellationToken::new())?
            .retained_objects,
        1
    );
    Ok(())
}

#[test]
fn recovery_rejects_shard_substitution_after_closure_submission() -> TestResult {
    let (fixture, path, baseline) = init()?;
    let cancel = CancellationToken::new();
    let head = operation(&baseline, None, "recovery namespace fixture")?;
    assert!(matches!(
        baseline
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(head, &cancel)?,
        PublishOutcome::Durable(_)
    ));
    drop(baseline);
    let head_bytes = fs::read(path.join("HEAD"))?;
    let object_name = head.to_string();
    let shard = path.join("objects").join(&object_name[..2]);
    let retained = fixture.path().join("retained-original-shard");
    let hook_shard = shard.clone();
    let hook_retained = retained.clone();
    let submitted = Arc::new(AtomicUsize::new(0));
    let observed = submitted.clone();
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            // The single shard is submitted first, then the objects directory.
            // Replace the shard after submission's own final layout check.
            if boundary == DurableBoundary::ClosureDirectorySubmitted
                && observed.fetch_add(1, Ordering::SeqCst) == 1
            {
                fs::rename(&hook_shard, &hook_retained)?;
                fs::create_dir(&hook_shard)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(&path, &options)?;
    let result = store.recover(&cancel);
    assert_eq!(submitted.load(Ordering::SeqCst), 2);
    assert!(store.verify_reachable(head, &cancel).is_err());
    assert!(
        matches!(
            result,
            Err(StoreError::Corrupt {
                kind: "object shard",
                ..
            })
        ),
        "recovery must not acknowledge a detached history closure: {result:?}"
    );
    assert_eq!(fs::read(path.join("HEAD"))?, head_bytes);
    assert_eq!(fs::read_dir(&retained)?.count(), 1);
    assert_eq!(fs::read_dir(&shard)?.count(), 0);

    // Explicitly repair only this owned fixture; failed recovery preserves the
    // original shard, rather than deleting or replacing it with the empty entry.
    fs::remove_dir(&shard)?;
    fs::rename(&retained, &shard)?;
    let repaired = Store::open(&path, &StoreOptions::default())?;
    assert_eq!(repaired.recover(&cancel)?.verified.head, Some(head));
    repaired.verify_reachable(head, &cancel)?;
    Ok(())
}

#[test]
fn killed_process_at_all_boundaries_preserves_valid_head_and_releases_locks() -> TestResult {
    for boundary in [
        DurableBoundary::ObjectTempCreated,
        DurableBoundary::ObjectDataWritten,
        DurableBoundary::ObjectFileSynced,
        DurableBoundary::ObjectLinked,
        DurableBoundary::ObjectDirectorySynced,
        DurableBoundary::ClosureObjectSubmitted,
        DurableBoundary::ClosureDirectorySubmitted,
        DurableBoundary::HeadDataWritten,
        DurableBoundary::HeadFileKernelSynced,
        DurableBoundary::HeadFileSynced,
        DurableBoundary::HeadReplaced,
        DurableBoundary::HeadDirectoriesKernelSynced,
        DurableBoundary::HeadDirectorySynced,
    ] {
        let (_fixture, path, store) = init()?;
        let cancel = CancellationToken::new();
        let old = operation(&store, None, "baseline")?;
        store
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(old, &cancel)?;
        let marker = path.parent().expect("parent").join("ready");
        let mut child = Command::new(std::env::current_exe()?)
            .args(["--exact", "process_crash_writer", "--nocapture"])
            .env("IZU_STORE_CRASH_ROOT", &path)
            .env("IZU_STORE_CRASH_BOUNDARY", format!("{boundary:?}"))
            .env("IZU_STORE_CRASH_READY", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let started = Instant::now();
        while !marker.exists() {
            if let Some(status) = child.try_wait()? {
                panic!("crash child exited early: {status}");
            }
            if started.elapsed() > Duration::from_secs(10) {
                child.kill()?;
                panic!("crash worker did not reach boundary {boundary:?}");
            }
            thread::sleep(Duration::from_millis(5));
        }
        child.kill()?;
        let output = child.wait_with_output()?;
        assert!(!output.status.success());
        let reopened = Store::open(&path, &StoreOptions::default())?;
        let head = reopened
            .current_head(&cancel)?
            .expect("head remains present");
        let is_new_visible = matches!(
            boundary,
            DurableBoundary::HeadReplaced
                | DurableBoundary::HeadDirectoriesKernelSynced
                | DurableBoundary::HeadDirectorySynced
        );
        assert_eq!(head == old, !is_new_visible);
        reopened.verify(&cancel)?;
        reopened.recover(&cancel)?;
        assert_eq!(fs::read_dir(path.join("tmp"))?.count(), 0);
        let tx = reopened.begin(HeadExpectation::Any, &cancel)?;
        let after = operation(&reopened, tx.current_head(), "after crash")?;
        assert!(matches!(
            tx.publish(after, &cancel)?,
            PublishOutcome::Durable(_)
        ));
    }
    Ok(())
}

#[test]
fn process_crash_writer() -> TestResult {
    let Some(root) = std::env::var_os("IZU_STORE_CRASH_ROOT") else {
        return Ok(());
    };
    let boundary = std::env::var("IZU_STORE_CRASH_BOUNDARY")?;
    let marker = std::env::var_os("IZU_STORE_CRASH_READY").expect("ready path");
    let options = StoreOptions {
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
    };
    let store = Store::open(Path::new(&root), &options)?;
    let cancel = CancellationToken::new();
    let tx = store.begin(HeadExpectation::Any, &cancel)?;
    let new = operation(&store, tx.current_head(), "crash candidate")?;
    tx.publish(new, &cancel)?;
    panic!("expected worker to block at boundary");
}

#[test]
fn volatile_override_never_returns_durable_even_on_persistent_filesystem() -> TestResult {
    let (_fixture, path, baseline) = init()?;
    let cancel = CancellationToken::new();
    let options = StoreOptions {
        allow_volatile_for_tests: true,
        ..StoreOptions::default()
    };
    let store = Store::open(&path, &options)?;
    let operation = operation(&baseline, None, "explicit volatile test")?;
    let tx = store.begin(HeadExpectation::Absent, &cancel)?;
    assert!(
        matches!(tx.publish(operation, &cancel)?, PublishOutcome::VisibleButUncertain { receipt, error: StoreError::VolatileTestMode } if receipt.current == operation)
    );
    assert_eq!(store.current_head(&cancel)?, Some(operation));
    Ok(())
}

#[test]
fn shard_substitution_never_receives_a_durable_publication() -> TestResult {
    for boundary in [
        DurableBoundary::ClosureDirectorySubmitted,
        DurableBoundary::HeadFileSynced,
        DurableBoundary::HeadDirectoriesKernelSynced,
        DurableBoundary::HeadDirectorySynced,
    ] {
        let (_fixture, path, baseline) = init()?;
        let cancel = CancellationToken::new();
        let old = operation(&baseline, None, "baseline")?;
        baseline
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(old, &cancel)?;
        let new = operation(&baseline, Some(old), "new")?;
        let previous_head = fs::read(path.join("HEAD"))?;
        let prefix = [old, new]
            .into_iter()
            .map(|id| id.object_id().as_bytes()[0])
            .min()
            .expect("two operation IDs");
        let name = format!("{prefix:02x}");
        let shard_name = name.clone();
        let objects = baseline.root_directory().open_dir(OsStr::new("objects"))?;
        let hook_objects = objects.try_clone()?;
        let fired = Arc::new(AtomicBool::new(false));
        let hook_fired = fired.clone();
        let options = options_at(boundary, move || {
            if !hook_fired.swap(true, Ordering::SeqCst) {
                hook_objects.rename_noreplace(
                    OsStr::new(&shard_name),
                    &hook_objects,
                    OsStr::new("held-shard"),
                )?;
                hook_objects.create_dir(OsStr::new(&shard_name))?;
            }
            Ok(())
        });
        let store = Store::open(&path, &options)?;
        let outcome = store
            .begin(HeadExpectation::At(old), &cancel)?
            .publish(new, &cancel);
        assert!(
            fired.load(Ordering::SeqCst),
            "hook must have run at {boundary:?}"
        );
        let visible = matches!(
            boundary,
            DurableBoundary::HeadDirectoriesKernelSynced | DurableBoundary::HeadDirectorySynced
        );
        if visible {
            assert!(matches!(outcome?, PublishOutcome::VisibleButUncertain {
                receipt, error: StoreError::Corrupt { kind: "object shard", .. }
            } if receipt.current == new && receipt.previous == Some(old)));
        } else {
            assert!(
                matches!(
                    outcome,
                    Err(StoreError::Corrupt {
                        kind: "object shard",
                        ..
                    })
                ),
                "expected prepublication guard error, observed {outcome:?}"
            );
            assert_eq!(fs::read(path.join("HEAD"))?, previous_head);
        }
        // Remove only our empty replacement, then restore the preserved original.
        objects.remove_dir(OsStr::new(&name))?;
        objects.rename_noreplace(OsStr::new("held-shard"), &objects, OsStr::new(&name))?;
        let reopened = Store::open(&path, &StoreOptions::default())?;
        assert_eq!(
            reopened.current_head(&cancel)?,
            Some(if visible { new } else { old })
        );
        reopened.verify(&cancel)?;
        reopened.recover(&cancel)?;
    }
    Ok(())
}

#[test]
fn publication_submits_full_history_and_each_shard_once() -> TestResult {
    let (_fixture, path, baseline) = init()?;
    let cancel = CancellationToken::new();
    let mut current = None;
    let mut shards = BTreeSet::new();
    for index in 0..12 {
        let id = operation(&baseline, current, &format!("prepared-{index}"))?;
        shards.insert(id.object_id().as_bytes()[0]);
        current = Some(id);
    }
    let objects = Arc::new(AtomicUsize::new(0));
    let directories = Arc::new(AtomicUsize::new(0));
    let prepublication_full = Arc::new(AtomicUsize::new(0));
    let postpublication_full = Arc::new(AtomicUsize::new(0));
    let counts = (
        objects.clone(),
        directories.clone(),
        prepublication_full.clone(),
        postpublication_full.clone(),
    );
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            let count = match boundary {
                DurableBoundary::ClosureObjectSubmitted => Some(&counts.0),
                DurableBoundary::ClosureDirectorySubmitted => Some(&counts.1),
                DurableBoundary::HeadFileSynced => Some(&counts.2),
                DurableBoundary::HeadDirectorySynced => Some(&counts.3),
                _ => None,
            };
            if let Some(count) = count {
                count.fetch_add(1, Ordering::SeqCst);
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(&path, &options)?;
    let root = current.expect("prepared history");
    assert!(matches!(
        store.bootstrap_archive(root, &cancel)?,
        PublishOutcome::Durable(_)
    ));
    assert_eq!(
        objects.load(Ordering::SeqCst),
        12,
        "every historical object is submitted"
    );
    assert_eq!(
        directories.load(Ordering::SeqCst),
        shards.len() + 1,
        "each shard plus its objects parent is submitted exactly once"
    );
    assert_eq!(prepublication_full.load(Ordering::SeqCst), 1);
    assert_eq!(postpublication_full.load(Ordering::SeqCst), 1);
    assert_eq!(store.verify_reachable(root, &cancel)?.object_count, 12);
    Ok(())
}
