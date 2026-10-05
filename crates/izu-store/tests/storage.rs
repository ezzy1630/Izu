use izu_model::{
    CancellationToken, ChangeId, ChangeState, FileMode, Identity, Limits, ModelError, ObjectId,
    ObjectKind, Operation, OperationId, RefName, RepoPath, RepositoryView, Revision, RevisionId,
    Tree, TreeEntry, TreeId, decode_metadata, encode_metadata, hash_object,
};
use izu_store::{HeadExpectation, PublishOutcome, Store, StoreError, StoreOptions};
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}

fn fixture(options: &StoreOptions) -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().canonicalize()?.join("store");
    let store = Store::init(&path, options)?;
    Ok(Fixture {
        _dir: dir,
        path,
        store,
    })
}

fn object_path(root: &Path, id: ObjectId) -> PathBuf {
    let name = id.to_string();
    root.join("objects").join(&name[..2]).join(&name[2..])
}

fn put_operation(
    store: &Store,
    parent: Option<OperationId>,
    view: RepositoryView,
    description: &str,
) -> Result<OperationId, StoreError> {
    let operation = Operation {
        parent,
        view,
        description: description.to_owned(),
        created_at_unix_ms: 1,
    };
    let payload = encode_metadata(&operation, &store.options().limits)?;
    store
        .put(ObjectKind::Operation, &payload, &CancellationToken::new())
        .map(OperationId::from_object)
}

fn publish_empty(store: &Store, description: &str) -> Result<OperationId, StoreError> {
    let cancel = CancellationToken::new();
    let transaction = store.begin(HeadExpectation::Any, &cancel)?;
    let operation = put_operation(
        store,
        transaction.current_head(),
        RepositoryView::default(),
        description,
    )?;
    assert!(matches!(
        transaction.publish(operation, &cancel)?,
        PublishOutcome::Durable(_)
    ));
    Ok(operation)
}

#[test]
fn object_roundtrip_reopen_and_full_closure() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let store = &fixture.store;
    let cancel = CancellationToken::new();
    let blob = store.put(ObjectKind::Blob, b"original content", &cancel)?;
    let mut tree = Tree::default();
    tree.entries.insert(
        RepoPath::new("file")?,
        TreeEntry::File {
            blob,
            mode: FileMode::Regular,
        },
    );
    let tree_id = TreeId::from_object(store.put(
        ObjectKind::Tree,
        &encode_metadata(&tree, &Limits::default())?,
        &cancel,
    )?);
    let revision = Revision {
        change: ChangeId::from_bytes([1; 16]),
        tree: tree_id,
        parents: Vec::new(),
        description: "initial".into(),
        author: Identity {
            name: "Test".into(),
            email: "test@example.invalid".into(),
        },
        created_at_unix_ms: 1,
        origin: None,
    };
    let revision_id = RevisionId::from_object(store.put(
        ObjectKind::Revision,
        &encode_metadata(&revision, &Limits::default())?,
        &cancel,
    )?);
    let mut view = RepositoryView::default();
    view.refs.insert(RefName::new("main")?, revision_id);
    view.changes
        .insert(revision.change, ChangeState::resolved(revision_id));
    let transaction = store.begin(HeadExpectation::Absent, &cancel)?;
    let operation = put_operation(store, None, view, "initialize")?;
    let receipt = match transaction.publish(operation, &cancel)? {
        PublishOutcome::Durable(receipt) => receipt,
        outcome => panic!("unexpected outcome: {outcome:?}"),
    };
    assert_eq!(receipt.current, operation);
    assert_eq!(receipt.previous, None);
    let reopened = Store::open(&fixture.path, &StoreOptions::default())?;
    assert_eq!(reopened.current_head(&cancel)?, Some(operation));
    assert_eq!(
        reopened.get(blob, ObjectKind::Blob, &cancel)?,
        b"original content"
    );
    let report = reopened.verify(&cancel)?;
    assert_eq!(report.object_count, 4);
    assert_eq!(
        reopened.verify_reachable(operation, &cancel)?.object_count,
        4
    );
    Ok(())
}

#[test]
fn strict_reads_reject_corruption_truncation_kind_and_version() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let id = fixture.store.put(ObjectKind::Blob, b"payload", &cancel)?;
    assert!(matches!(
        fixture.store.get(id, ObjectKind::Operation, &cancel),
        Err(StoreError::UnexpectedKind { .. })
    ));
    let path = object_path(&fixture.path, id);
    let original = fs::read(&path)?;
    for bytes in [
        original[..10].to_vec(),
        original[..original.len() - 1].to_vec(),
        {
            let mut b = original.clone();
            b.push(0);
            b
        },
        {
            let mut b = original.clone();
            b[7] = 1;
            b
        },
        {
            let mut b = original.clone();
            b[8] = 255;
            b
        },
        {
            let mut b = original.clone();
            let last = b.len() - 1;
            b[last] ^= 1;
            b
        },
        {
            let mut b = original.clone();
            b[9..17].copy_from_slice(&u64::MAX.to_be_bytes());
            b
        },
    ] {
        fs::write(&path, bytes)?;
        assert!(fixture.store.get(id, ObjectKind::Blob, &cancel).is_err());
        let mut output = Vec::new();
        assert!(fixture.store.read_blob(id, &mut output, &cancel).is_err());
        assert!(output.is_empty(), "corrupt data must not reach output");
        assert!(fixture.store.verify(&cancel).is_err());
    }
    fs::write(&path, &original)?;
    fs::write(fixture.path.join("FORMAT"), b"IZU-STORE 99\n")?;
    assert!(matches!(
        Store::open(&fixture.path, &StoreOptions::default()),
        Err(StoreError::UnsupportedVersion { .. })
    ));
    Ok(())
}

fn referenced_revision_operation(
    store: &Store,
    revision: RevisionId,
) -> Result<OperationId, StoreError> {
    let mut view = RepositoryView::default();
    view.refs.insert(RefName::new("main")?, revision);
    put_operation(
        store,
        None,
        view,
        "parentless root with referenced metadata",
    )
}

fn test_revision(tree: TreeId, description: &str) -> Revision {
    Revision {
        change: ChangeId::from_bytes([1; 16]),
        tree,
        parents: Vec::new(),
        description: description.into(),
        author: Identity {
            name: "Test".into(),
            email: "test@example.invalid".into(),
        },
        created_at_unix_ms: 1,
        origin: None,
    }
}

#[test]
fn reachable_metadata_still_requires_canonical_schema_and_verified_hash() -> TestResult {
    let cancel = CancellationToken::new();
    let valid = encode_metadata(
        &test_revision(
            TreeId::from_object(ObjectId::from_bytes([0; 32])),
            "valid schema",
        ),
        &Limits::default(),
    )?;
    let mut noncanonical = Vec::with_capacity(valid.len() + 1);
    noncanonical.push(b' ');
    noncanonical.extend_from_slice(&valid);
    for (invalid, canonical_rejection) in
        [(b"{}".as_slice(), false), (noncanonical.as_slice(), true)]
    {
        let fixture = fixture(&StoreOptions::default())?;
        let referenced =
            RevisionId::from_object(fixture.store.put(ObjectKind::Revision, invalid, &cancel)?);
        let root = referenced_revision_operation(&fixture.store, referenced)?;
        let verified = fixture.store.verify_reachable(root, &cancel);
        assert!(
            matches!(verified, Err(StoreError::Model(_))),
            "{verified:?}"
        );
        if canonical_rejection {
            assert!(
                matches!(verified, Err(StoreError::Model(ModelError::NonCanonical))),
                "{verified:?}"
            );
        }
        let published = fixture
            .store
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(root, &cancel);
        assert!(
            matches!(published, Err(StoreError::Model(_))),
            "{published:?}"
        );
        if canonical_rejection {
            assert!(
                matches!(published, Err(StoreError::Model(ModelError::NonCanonical))),
                "{published:?}"
            );
        }
        assert_eq!(fixture.store.current_head(&cancel)?, None);
    }

    let fixture = fixture(&StoreOptions::default())?;
    let referenced = RevisionId::from_object(fixture.store.put(
        ObjectKind::Revision,
        &encode_metadata(
            &test_revision(
                TreeId::from_object(ObjectId::from_bytes([0; 32])),
                "valid metadata reference",
            ),
            &Limits::default(),
        )?,
        &cancel,
    )?);
    let root = referenced_revision_operation(&fixture.store, referenced)?;
    let path = object_path(&fixture.path, referenced.object_id());
    let mut framed = fs::read(&path)?;
    let position = framed
        .windows(b"valid metadata reference".len())
        .position(|window| window == b"valid metadata reference")
        .ok_or("fixture metadata description is missing")?;
    framed[position] = b'x';
    fs::write(&path, framed)?;
    let verified = fixture.store.verify_reachable(root, &cancel);
    assert!(
        matches!(
            verified,
            Err(StoreError::Corrupt {
                kind: "object",
                reason: "hash mismatch"
            })
        ),
        "{verified:?}"
    );
    let published = fixture
        .store
        .begin(HeadExpectation::Absent, &cancel)?
        .publish(root, &cancel);
    assert!(
        matches!(
            published,
            Err(StoreError::Corrupt {
                kind: "object",
                reason: "hash mismatch"
            })
        ),
        "{published:?}"
    );
    assert_eq!(fixture.store.current_head(&cancel)?, None);
    Ok(())
}

#[test]
fn reachable_metadata_retains_memory_object_and_byte_budgets() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let tree = TreeId::from_object(fixture.store.put(
        ObjectKind::Tree,
        &encode_metadata(&Tree::default(), &Limits::default())?,
        &cancel,
    )?);
    let referenced = RevisionId::from_object(fixture.store.put(
        ObjectKind::Revision,
        &encode_metadata(
            &test_revision(tree, &"large reference".repeat(256)),
            &Limits::default(),
        )?,
        &cancel,
    )?);
    let root = referenced_revision_operation(&fixture.store, referenced)?;
    let root_bytes = fixture
        .store
        .object_info(root.object_id(), &cancel)?
        .payload_len;
    let referenced_bytes = fixture
        .store
        .object_info(referenced.object_id(), &cancel)?
        .payload_len;
    for (options, limit) in [
        (
            StoreOptions {
                max_in_memory_bytes: root_bytes,
                ..StoreOptions::default()
            },
            "in-memory object",
        ),
        (
            StoreOptions {
                max_scan_objects: 1,
                ..StoreOptions::default()
            },
            "reachable objects",
        ),
        (
            StoreOptions {
                max_scan_bytes: root_bytes + referenced_bytes - 1,
                ..StoreOptions::default()
            },
            "reachable bytes",
        ),
    ] {
        let limited = Store::open(&fixture.path, &options)?;
        let verified = limited.verify_reachable(root, &cancel);
        assert!(
            matches!(verified, Err(StoreError::LimitExceeded(actual)) if actual == limit),
            "{verified:?}"
        );
        let published = limited
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(root, &cancel);
        assert!(
            matches!(published, Err(StoreError::LimitExceeded(actual)) if actual == limit),
            "{published:?}"
        );
        assert_eq!(limited.current_head(&cancel)?, None);
    }
    Ok(())
}

#[test]
fn immutable_collisions_never_overwrite_existing_data() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let id = hash_object(ObjectKind::Blob, b"expected", &Limits::default())?;
    let path = object_path(&fixture.path, id);
    fs::create_dir_all(path.parent().expect("object parent"))?;
    fs::write(&path, b"other content")?;
    assert!(
        fixture
            .store
            .put(ObjectKind::Blob, b"expected", &cancel)
            .is_err()
    );
    assert_eq!(fs::read(&path)?, b"other content");
    Ok(())
}

#[cfg(unix)]
#[test]
fn objects_shards_and_head_reject_symlinks() -> TestResult {
    use std::os::unix::fs::symlink;
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let id = fixture.store.put(ObjectKind::Blob, b"keep", &cancel)?;
    let path = object_path(&fixture.path, id);
    let outside = fixture.path.parent().expect("store parent").join("outside");
    fs::write(&outside, b"untouched")?;
    fs::remove_file(&path)?;
    symlink(&outside, &path)?;
    assert!(fixture.store.get(id, ObjectKind::Blob, &cancel).is_err());
    assert!(
        fixture
            .store
            .put(ObjectKind::Blob, b"keep", &cancel)
            .is_err()
    );
    assert_eq!(fs::read(&outside)?, b"untouched");
    let new_id = hash_object(ObjectKind::Blob, b"new shard", &Limits::default())?;
    let shard = object_path(&fixture.path, new_id)
        .parent()
        .expect("shard")
        .to_path_buf();
    if !shard.exists() {
        symlink(fixture.path.parent().expect("parent"), &shard)?;
    }
    assert!(
        fixture
            .store
            .put(ObjectKind::Blob, b"new shard", &cancel)
            .is_err()
    );
    symlink(&outside, fixture.path.join("HEAD"))?;
    assert!(fixture.store.current_head(&cancel).is_err());
    assert_eq!(fs::read(outside)?, b"untouched");
    Ok(())
}

struct Chunks {
    remaining: u64,
}
impl Read for Chunks {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        let n = output.len().min(self.remaining as usize).min(101);
        output[..n].fill(42);
        self.remaining -= n as u64;
        Ok(n)
    }
}

#[test]
fn streaming_lengths_import_identity_limits_and_cancellation() -> TestResult {
    let options = StoreOptions {
        max_in_memory_bytes: 1024,
        ..StoreOptions::default()
    };
    let fixture = fixture(&options)?;
    let cancel = CancellationToken::new();
    let id = fixture
        .store
        .put_blob(&mut Chunks { remaining: 200_000 }, 200_000, &cancel)?;
    assert!(matches!(
        fixture.store.get(id, ObjectKind::Blob, &cancel),
        Err(StoreError::LimitExceeded(_))
    ));
    let mut output = Vec::new();
    assert_eq!(fixture.store.read_blob(id, &mut output, &cancel)?, 200_000);
    assert_eq!(output, vec![42; 200_000]);
    assert!(matches!(
        fixture.store.put_blob(&mut &b"short"[..], 7, &cancel),
        Err(StoreError::InputLengthMismatch)
    ));
    assert!(matches!(
        fixture.store.put_blob(&mut &b"too long"[..], 2, &cancel),
        Err(StoreError::InputLengthMismatch)
    ));
    let wrong_id = ObjectId::from_bytes([0; 32]);
    assert!(matches!(
        fixture
            .store
            .import_object(ObjectKind::Blob, wrong_id, &mut &b"data"[..], 4, &cancel),
        Err(StoreError::Model(ModelError::HashMismatch))
    ));
    assert!(
        !object_path(
            &fixture.path,
            hash_object(ObjectKind::Blob, b"data", &Limits::default())?
        )
        .exists()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        fixture.store.put(ObjectKind::Blob, b"cancel", &cancelled),
        Err(StoreError::Model(ModelError::Cancelled))
    ));
    assert!(matches!(
        fixture.store.begin(HeadExpectation::Any, &cancelled),
        Err(StoreError::Model(ModelError::Cancelled))
    ));
    let limited = Store::open(
        &fixture.path,
        &StoreOptions {
            max_scan_bytes: 10,
            ..options
        },
    )?;
    assert!(matches!(
        limited.verify(&cancel),
        Err(StoreError::LimitExceeded(_))
    ));
    Ok(())
}

#[test]
fn head_compare_and_parent_check_prevent_stale_publication() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let first = publish_empty(&fixture.store, "first")?;
    assert!(matches!(
        fixture.store.begin(HeadExpectation::Absent, &cancel),
        Err(StoreError::HeadConflict { .. })
    ));
    let second = publish_empty(&fixture.store, "second")?;
    assert!(matches!(
        fixture.store.begin(HeadExpectation::At(first), &cancel),
        Err(StoreError::HeadConflict { .. })
    ));
    let stale = put_operation(
        &fixture.store,
        Some(first),
        RepositoryView::default(),
        "stale",
    )?;
    let transaction = fixture.store.begin(HeadExpectation::Any, &cancel)?;
    assert!(matches!(
        transaction.publish(stale, &cancel),
        Err(StoreError::HistoryMismatch { .. })
    ));
    assert_eq!(fixture.store.current_head(&cancel)?, Some(second));
    Ok(())
}

#[test]
fn publish_rejects_missing_or_wrong_kind_transitive_references() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    for id in [
        ObjectId::from_bytes([0; 32]),
        fixture
            .store
            .put(ObjectKind::Blob, b"wrong kind", &cancel)?,
    ] {
        let mut view = RepositoryView::default();
        view.refs
            .insert(RefName::new("main")?, RevisionId::from_object(id));
        let operation = put_operation(&fixture.store, None, view, "bad reference")?;
        let transaction = fixture.store.begin(HeadExpectation::Absent, &cancel)?;
        assert!(transaction.publish(operation, &cancel).is_err());
        assert_eq!(fixture.store.current_head(&cancel)?, None);
    }
    Ok(())
}

#[test]
fn lock_wait_is_bounded_and_recovers_on_drop() -> TestResult {
    let fixture = fixture(&StoreOptions {
        lock_timeout: Duration::from_millis(30),
        ..StoreOptions::default()
    })?;
    let cancel = CancellationToken::new();
    let first = fixture.store.begin(HeadExpectation::Any, &cancel)?;
    let start = Instant::now();
    assert!(matches!(
        fixture.store.begin(HeadExpectation::Any, &cancel),
        Err(StoreError::LockTimeout("head.lock"))
    ));
    assert!(start.elapsed() < Duration::from_secs(1));
    drop(first);
    fixture.store.begin(HeadExpectation::Any, &cancel)?;
    Ok(())
}

#[test]
fn head_absence_is_confirmed_without_relocking_guarded_readers() -> TestResult {
    let fixture = fixture(&StoreOptions {
        lock_timeout: Duration::from_millis(30),
        ..StoreOptions::default()
    })?;
    let cancel = CancellationToken::new();
    assert_eq!(fixture.store.current_head(&cancel)?, None);
    let publication = fixture.store.begin(HeadExpectation::Absent, &cancel)?;
    assert_eq!(publication.current_head(), None);
    let observed = fixture.store.current_head(&cancel);
    assert!(
        matches!(observed, Err(StoreError::LockTimeout("head.lock"))),
        "absence must wait for the in-flight publication: {observed:?}"
    );
    // Recovery's exclusive temporary lease already excludes HEAD replacement.
    // Taking the head lock here would reverse publication's acquisition order.
    assert_eq!(fixture.store.recover(&cancel)?.verified.head, None);
    drop(publication);
    assert_eq!(fixture.store.current_head(&cancel)?, None);
    Ok(())
}

#[test]
fn head_absence_confirmation_can_be_cancelled_while_waiting() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let _publication = fixture.store.begin(HeadExpectation::Absent, &cancel)?;
    std::thread::scope(|scope| -> TestResult {
        let (started, start) = std::sync::mpsc::channel();
        let (result, observed) = std::sync::mpsc::channel();
        let store = &fixture.store;
        let reader_cancel = cancel.clone();
        let reader = scope.spawn(move || {
            started.send(()).expect("reader start");
            result
                .send(store.current_head(&reader_cancel))
                .expect("reader result");
        });
        start.recv_timeout(Duration::from_secs(1))?;
        assert!(
            matches!(
                observed.recv_timeout(Duration::from_millis(30)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ),
            "a reader must not report absence while publication holds the lock"
        );
        cancel.cancel();
        let result = observed.recv_timeout(Duration::from_secs(1))?;
        assert!(
            matches!(result, Err(StoreError::Model(ModelError::Cancelled))),
            "waiting for absence confirmation must remain cancellable: {result:?}"
        );
        reader.join().expect("reader thread");
        Ok(())
    })
}

#[test]
fn head_reads_stay_present_during_atomic_selector_replacement() -> TestResult {
    use std::ffi::OsStr;
    use std::io::Write;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Barrier};

    const READERS: usize = 8;
    const REPLACEMENTS: usize = 1_000;
    let fixture = fixture(&StoreOptions::default())?;
    let first = publish_empty(&fixture.store, "first selector")?;
    let first_selector = fs::read(fixture.path.join("HEAD"))?;
    let second = publish_empty(&fixture.store, "second selector")?;
    let second_selector = fs::read(fixture.path.join("HEAD"))?;
    let readers = (0..READERS)
        .map(|_| Store::open(&fixture.path, &StoreOptions::default()))
        .collect::<Result<Vec<_>, _>>()?;
    let ready = Arc::new(Barrier::new(READERS + 1));
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| -> TestResult {
        let jobs = readers
            .iter()
            .map(|store| {
                let ready = ready.clone();
                let stop = &stop;
                scope.spawn(move || {
                    let cancel = CancellationToken::new();
                    ready.wait();
                    let mut reads = 0;
                    while !stop.load(Ordering::Acquire) {
                        let observed = store.current_head(&cancel);
                        match observed {
                            Ok(Some(head)) if head == first || head == second => reads += 1,
                            other => return Err(format!("unstable HEAD read: {other:?}")),
                        }
                    }
                    Ok(reads)
                })
            })
            .collect::<Vec<_>>();
        let root = fixture.store.root_directory();
        let temporary = root.open_dir(OsStr::new("tmp"))?;
        let lock = root.open_existing_lock(OsStr::new("head.lock"))?;
        ready.wait();
        // Exercise the same descriptor-relative rename and permanent publication
        // lock with two already durable selectors, avoiding history growth.
        let replacements = (|| -> TestResult {
            for index in 0..REPLACEMENTS {
                lock.lock()?;
                let result = (|| -> TestResult {
                    let name = format!("head-read-{index}");
                    let mut file = temporary.create_new_file(OsStr::new(&name))?;
                    file.write_all(if index % 2 == 0 {
                        &first_selector
                    } else {
                        &second_selector
                    })?;
                    temporary.rename_replace(OsStr::new(&name), root, OsStr::new("HEAD"))?;
                    Ok(())
                })();
                lock.unlock()?;
                result?;
            }
            Ok(())
        })();
        stop.store(true, Ordering::Release);
        let mut successful_reads = 0;
        let mut failures = Vec::new();
        for job in jobs {
            match job.join().expect("HEAD reader thread") {
                Ok(reads) => successful_reads += reads,
                Err(error) => failures.push(error),
            }
        }
        replacements?;
        assert!(failures.is_empty(), "{failures:?}");
        assert!(successful_reads >= READERS, "HEAD readers did not run");
        Ok(())
    })
}

#[test]
fn recovery_cleans_only_inactive_owned_temps_and_retains_objects() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    fixture
        .store
        .put(ObjectKind::Blob, b"uncommitted source", &cancel)?;
    let stale = fixture
        .path
        .join("tmp/izu-0123456789abcdef0123456789abcdef.tmp");
    fs::write(&stale, b"left by terminated writer")?;
    fs::write(fixture.path.join("tmp/human-file"), b"keep")?;
    let report = fixture.store.recover(&cancel)?;
    assert_eq!(report.removed_temporary_files, 1);
    assert_eq!(report.unknown_temporary_entries, 1);
    assert_eq!(report.retained_objects, 1);
    assert!(!stale.exists());
    assert_eq!(fs::read(fixture.path.join("tmp/human-file"))?, b"keep");
    Ok(())
}

#[test]
fn concurrent_processes_extend_history_without_lost_updates() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let exe = std::env::current_exe()?;
    let mut children = Vec::new();
    for index in 0..4 {
        children.push(
            Command::new(&exe)
                .args(["--exact", "process_writer", "--nocapture"])
                .env("IZU_STORE_TEST_ROOT", &fixture.path)
                .env("IZU_STORE_TEST_WRITER", index.to_string())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?,
        );
    }
    for child in children {
        let output = child.wait_with_output()?;
        assert!(
            output.status.success(),
            "child failed: {} {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let cancel = CancellationToken::new();
    let mut current = fixture.store.current_head(&cancel)?;
    let latest: Operation = decode_metadata(
        &fixture.store.get(
            current.expect("HEAD").object_id(),
            ObjectKind::Operation,
            &cancel,
        )?,
        &Limits::default(),
    )?;
    assert_eq!(latest.view.refs.len(), 20);
    let mut descriptions = Vec::new();
    while let Some(id) = current {
        let operation: Operation = decode_metadata(
            &fixture
                .store
                .get(id.object_id(), ObjectKind::Operation, &cancel)?,
            &Limits::default(),
        )?;
        descriptions.push(operation.description);
        current = operation.parent;
    }
    descriptions.sort();
    assert_eq!(descriptions.len(), 20);
    for writer in 0..4 {
        for step in 0..5 {
            assert!(descriptions.contains(&format!("writer-{writer}-{step}")));
        }
    }
    assert_eq!(
        fixture
            .store
            .verify_reachable(fixture.store.current_head(&cancel)?.expect("head"), &cancel)?
            .object_count,
        25
    );
    Ok(())
}

#[test]
fn process_writer() -> TestResult {
    let Some(root) = std::env::var_os("IZU_STORE_TEST_ROOT") else {
        return Ok(());
    };
    let writer = std::env::var("IZU_STORE_TEST_WRITER")?;
    let store = Store::open(Path::new(&root), &StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let tree_id = TreeId::from_object(store.put(
        ObjectKind::Tree,
        &encode_metadata(&Tree::default(), &Limits::default())?,
        &cancel,
    )?);
    let revision = Revision {
        change: ChangeId::from_bytes([writer.parse::<u8>()?; 16]),
        tree: tree_id,
        parents: Vec::new(),
        description: format!("writer-{writer}"),
        author: Identity {
            name: "Writer".into(),
            email: "writer@example.invalid".into(),
        },
        created_at_unix_ms: 1,
        origin: None,
    };
    let revision_id = RevisionId::from_object(store.put(
        ObjectKind::Revision,
        &encode_metadata(&revision, &Limits::default())?,
        &cancel,
    )?);
    for step in 0..5 {
        let tx = store.begin(HeadExpectation::Any, &cancel)?;
        let mut view = match tx.current_head() {
            Some(head) => {
                decode_metadata::<Operation>(
                    &store.get(head.object_id(), ObjectKind::Operation, &cancel)?,
                    &Limits::default(),
                )?
                .view
            }
            None => RepositoryView::default(),
        };
        view.refs.insert(
            RefName::new(format!("writer-{writer}-{step}"))?,
            revision_id,
        );
        let next = put_operation(
            &store,
            tx.current_head(),
            view,
            &format!("writer-{writer}-{step}"),
        )?;
        assert!(matches!(
            tx.publish(next, &cancel)?,
            PublishOutcome::Durable(_)
        ));
    }
    Ok(())
}

#[test]
fn head_checksum_and_missing_head_object_are_rejected() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let operation = publish_empty(&fixture.store, "head")?;
    let path = fixture.path.join("HEAD");
    let original = fs::read(&path)?;
    let mut corrupted = original.clone();
    corrupted[10] ^= 1;
    fs::write(&path, corrupted)?;
    assert!(Store::open(&fixture.path, &StoreOptions::default()).is_err());
    fs::write(&path, original)?;
    fs::remove_file(object_path(&fixture.path, operation.object_id()))?;
    assert!(Store::open(&fixture.path, &StoreOptions::default()).is_err());
    Ok(())
}

#[test]
fn archive_bootstrap_retains_parent_chain_and_rejects_active_stores() -> TestResult {
    let source = fixture(&StoreOptions::default())?;
    let archived_parent = publish_empty(&source.store, "archived first")?;
    let archived_root = publish_empty(&source.store, "archived second")?;
    let destination = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let incomplete = fixture(&StoreOptions::default())?;
    let payload = source
        .store
        .get(archived_root.object_id(), ObjectKind::Operation, &cancel)?;
    incomplete
        .store
        .put(ObjectKind::Operation, &payload, &cancel)?;
    assert!(
        incomplete
            .store
            .bootstrap_archive(archived_root, &cancel)
            .is_err()
    );
    assert_eq!(incomplete.store.current_head(&cancel)?, None);
    for info in source.store.list_objects(&cancel)? {
        let payload = source.store.get(info.id, info.kind, &cancel)?;
        destination.store.import_object(
            info.kind,
            info.id,
            &mut payload.as_slice(),
            info.payload_len,
            &cancel,
        )?;
    }
    let tx = destination.store.begin(HeadExpectation::Absent, &cancel)?;
    assert!(matches!(
        tx.publish(archived_root, &cancel),
        Err(StoreError::HistoryMismatch { .. })
    ));
    assert!(
        matches!(destination.store.bootstrap_archive(archived_root, &cancel)?, PublishOutcome::Durable(receipt) if receipt.current == archived_root && receipt.previous.is_none())
    );
    assert_eq!(
        destination
            .store
            .verify_reachable(archived_root, &cancel)?
            .object_count,
        2
    );
    assert!(matches!(
        destination
            .store
            .bootstrap_archive(archived_parent, &cancel),
        Err(StoreError::HeadConflict { .. })
    ));
    assert!(matches!(
        source.store.bootstrap_archive(archived_root, &cancel),
        Err(StoreError::HeadConflict { .. })
    ));
    assert_eq!(
        destination.store.current_head(&cancel)?,
        Some(archived_root)
    );
    Ok(())
}

#[test]
fn anchored_init_uses_original_directory_after_stage_name_substitution() -> TestResult {
    use izu_platform::Directory;
    use std::ffi::OsStr;
    let fixture = tempfile::tempdir()?;
    let root_path = fixture.path().canonicalize()?;
    let root = Directory::open(&root_path)?;
    let stage = root.create_dir(OsStr::new("stage"))?;
    let pinned = stage.try_clone()?;
    drop(stage);
    root.rename_noreplace(OsStr::new("stage"), &root, OsStr::new("original"))?;
    root.create_dir(OsStr::new("stage"))?;
    let store = Store::init_at(&pinned, OsStr::new("store"), &StoreOptions::default())?;
    assert!(root_path.join("original/store/FORMAT").exists());
    assert!(!root_path.join("stage/store").exists());
    let head = publish_empty(&store, "anchored creation")?;
    let opened = Store::open(&root_path.join("original/store"), &StoreOptions::default())?;
    assert_eq!(opened.current_head(&CancellationToken::new())?, Some(head));
    assert!(Store::init_at(&pinned, OsStr::new("store"), &StoreOptions::default()).is_err());
    assert_eq!(store.current_head(&CancellationToken::new())?, Some(head));

    let metadata = store.root_directory().try_clone()?;
    pinned.rename_noreplace(OsStr::new("store"), &pinned, OsStr::new("native"))?;
    pinned.create_dir(OsStr::new("store"))?;
    let engine_lock = metadata.create_new_file(OsStr::new("engine-owned.lock"))?;
    izu_platform::sync_file(&engine_lock)?;
    metadata.sync()?;
    let next = publish_empty(&store, "pinned metadata after locator replacement")?;
    let reopened = Store::open(&root_path.join("original/native"), &StoreOptions::default())?;
    assert_eq!(
        reopened.current_head(&CancellationToken::new())?,
        Some(next)
    );
    assert!(root_path.join("original/native/engine-owned.lock").exists());
    assert!(!root_path.join("original/store/HEAD").exists());
    assert!(!root_path.join("original/store/engine-owned.lock").exists());
    Ok(())
}

#[cfg(target_os = "linux")]
#[test]
fn normal_store_rejects_volatile_filesystem_before_creating_store() -> TestResult {
    let Ok(base) = fs::canonicalize("/dev/shm") else {
        return Ok(());
    };
    let fixture = tempfile::tempdir_in(base)?;
    let path = fixture.path().join("store");
    assert!(matches!(
        Store::init(&path, &StoreOptions::default()),
        Err(StoreError::UnsupportedDurability(_))
    ));
    assert!(!path.exists());
    #[cfg(feature = "fault-injection")]
    {
        let options = StoreOptions {
            allow_volatile_for_tests: true,
            ..StoreOptions::default()
        };
        let store = Store::init(&path, &options)?;
        let cancel = CancellationToken::new();
        let operation = put_operation(
            &store,
            None,
            RepositoryView::default(),
            "explicit volatile fixture",
        )?;
        let tx = store.begin(HeadExpectation::Absent, &cancel)?;
        assert!(
            matches!(tx.publish(operation, &cancel)?, PublishOutcome::VisibleButUncertain { receipt, error: StoreError::VolatileTestMode } if receipt.current == operation)
        );
        assert_eq!(store.current_head(&cancel)?, Some(operation));
        assert!(matches!(
            Store::open(&path, &StoreOptions::default()),
            Err(StoreError::UnsupportedDurability(_))
        ));
    }
    Ok(())
}

#[test]
fn prototype_format_is_rejected_without_reset_or_implicit_migration() -> TestResult {
    use sha2::{Digest, Sha256};
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let operation = publish_empty(&fixture.store, "IZU format")?;
    let format_path = fixture.path.join("FORMAT");
    let head_path = fixture.path.join("HEAD");
    let native_format = fs::read(&format_path)?;
    let native_head = fs::read(&head_path)?;
    fs::write(&format_path, b"EZY-STORE 1\n")?;
    assert!(matches!(
        Store::open(&fixture.path, &StoreOptions::default()),
        Err(StoreError::UnsupportedVersion { kind: "store" })
    ));
    assert!(Store::init(&fixture.path, &StoreOptions::default()).is_err());
    assert_eq!(fs::read(&format_path)?, b"EZY-STORE 1\n");
    assert_eq!(fs::read(&head_path)?, native_head);
    fs::write(&format_path, &native_format)?;
    let mut prototype_head = native_head.clone();
    prototype_head[..8].copy_from_slice(b"EZYHEAD1");
    let checksum = Sha256::digest(&prototype_head[..40]);
    prototype_head[40..].copy_from_slice(&checksum);
    fs::write(&head_path, &prototype_head)?;
    assert!(matches!(
        Store::open(&fixture.path, &StoreOptions::default()),
        Err(StoreError::UnsupportedVersion { kind: "HEAD" })
    ));
    assert_eq!(fs::read(&head_path)?, prototype_head);
    fs::write(&head_path, &native_head)?;
    let object_path = object_path(&fixture.path, operation.object_id());
    let original_object = fs::read(&object_path)?;
    let mut prototype_object = original_object.clone();
    prototype_object[..8].copy_from_slice(b"EZYOBJ1\0");
    fs::write(&object_path, &prototype_object)?;
    assert!(matches!(
        fixture
            .store
            .get(operation.object_id(), ObjectKind::Operation, &cancel),
        Err(StoreError::Model(ModelError::InvalidFrame { .. }))
    ));
    assert_eq!(fs::read(&object_path)?, prototype_object);
    fs::write(&object_path, &original_object)?;
    Store::open(&fixture.path, &StoreOptions::default())?.verify(&cancel)?;
    Ok(())
}

#[test]
fn publication_still_rejects_corruption_in_previously_acknowledged_history() -> TestResult {
    let fixture = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let first = publish_empty(&fixture.store, "first")?;
    let current = publish_empty(&fixture.store, "current")?;
    let next = put_operation(
        &fixture.store,
        Some(current),
        RepositoryView::default(),
        "next",
    )?;
    let path = object_path(&fixture.path, first.object_id());
    let original = fs::read(&path)?;
    let mut corrupt = original.clone();
    *corrupt.last_mut().expect("operation payload") ^= 1;
    fs::write(&path, corrupt)?;
    let tx = fixture.store.begin(HeadExpectation::At(current), &cancel)?;
    assert!(tx.publish(next, &cancel).is_err());
    assert_eq!(fixture.store.current_head(&cancel)?, Some(current));
    fs::write(&path, &original)?;
    fixture.store.recover(&cancel)?;
    Ok(())
}
