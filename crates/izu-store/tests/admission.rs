#![cfg(feature = "fault-injection")]

use izu_model::{
    CancellationToken, ObjectKind, Operation, OperationId, RepositoryView, encode_metadata,
};
use izu_store::{
    DurableBoundary, FaultHook, HeadExpectation, PublishOutcome, Store, StoreError, StoreOptions,
};
use std::ffi::OsStr;
use std::fs;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn fixture() -> Result<(tempfile::TempDir, Store), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().canonicalize()?.join("store");
    let store = Store::init(&path, &StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let operation = Operation {
        parent: None,
        view: RepositoryView::default(),
        description: "admission fixture".into(),
        created_at_unix_ms: 1,
    };
    let payload = encode_metadata(&operation, &store.options().limits)?;
    let id = OperationId::from_object(store.put(ObjectKind::Operation, &payload, &cancel)?);
    assert!(matches!(
        store
            .begin(HeadExpectation::Absent, &cancel)?
            .publish(id, &cancel)?,
        PublishOutcome::Durable(_)
    ));
    Ok((directory, store))
}

#[test]
fn registered_older_request_prevents_a_later_barger() -> TestResult {
    let (_directory, store) = fixture()?;
    let (registered, registration) = mpsc::channel();
    let (resume, resume_wait) = mpsc::channel();
    let resume_wait = Mutex::new(resume_wait);
    let first = AtomicBool::new(true);
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::HeadAdmissionRegistered
                && first.swap(false, Ordering::AcqRel)
            {
                registered.send(()).map_err(io::Error::other)?;
                resume_wait
                    .lock()
                    .map_err(|_| io::Error::other("resume mutex poisoned"))?
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(io::Error::other)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let older = Store::open(&_directory.path().canonicalize()?.join("store"), &options)?;
    let (barger_registered, barger_registration) = mpsc::channel();
    let barger_options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::HeadAdmissionRegistered {
                barger_registered.send(()).map_err(io::Error::other)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let barger = Store::open(
        &_directory.path().canonicalize()?.join("store"),
        &barger_options,
    )?;
    let holder = store.begin(HeadExpectation::Any, &CancellationToken::new())?;
    let order = Mutex::new(Vec::new());
    std::thread::scope(|scope| -> TestResult {
        let (older_ready, acquired) = mpsc::channel();
        let (release_older, release_wait) = mpsc::channel();
        let order = &order;
        let older = &older;
        let old = scope.spawn(move || -> Result<(), String> {
            let transaction = older
                .begin(HeadExpectation::Any, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            order
                .lock()
                .map_err(|_| "order mutex poisoned")?
                .push("older");
            older_ready.send(()).map_err(|error| error.to_string())?;
            release_wait
                .recv_timeout(Duration::from_secs(2))
                .map_err(|error| error.to_string())?;
            drop(transaction);
            Ok(())
        });
        registration.recv_timeout(Duration::from_secs(1))?;
        drop(holder);
        let (barger_ready, barger_result) = mpsc::channel();
        let store = &barger;
        let new = scope.spawn(move || -> Result<(), String> {
            let transaction = store
                .begin(HeadExpectation::Any, &CancellationToken::new())
                .map_err(|error| error.to_string())?;
            order
                .lock()
                .map_err(|_| "order mutex poisoned")?
                .push("barger");
            barger_ready.send(()).map_err(|error| error.to_string())?;
            drop(transaction);
            Ok(())
        });
        barger_registration.recv_timeout(Duration::from_secs(1))?;
        let early_barger = barger_result.recv_timeout(Duration::from_millis(30));
        resume.send(())?;
        acquired.recv_timeout(Duration::from_secs(1))?;
        release_older.send(())?;
        let old_result = old.join().expect("older request thread");
        let new_result = new.join().expect("barger thread");
        assert!(old_result.is_ok(), "older request: {old_result:?}");
        assert!(new_result.is_ok(), "barger: {new_result:?}");
        assert!(
            matches!(early_barger, Err(mpsc::RecvTimeoutError::Timeout)),
            "later request barged before the registered older request: {early_barger:?}"
        );
        Ok(())
    })?;
    assert_eq!(*order.lock().expect("order"), ["older", "barger"]);
    Ok(())
}

fn signal_registration(sender: mpsc::Sender<Instant>) -> StoreOptions {
    StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::HeadAdmissionRegistered {
                sender.send(Instant::now()).map_err(io::Error::other)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    }
}

#[test]
fn queued_cancellation_releases_claim_without_releasing_the_head_holder() -> TestResult {
    let (directory, store) = fixture()?;
    let (registered, registration) = mpsc::channel();
    let waiting = Store::open(
        &directory.path().canonicalize()?.join("store"),
        &signal_registration(registered),
    )?;
    let cancel = CancellationToken::new();
    let holder = store.begin(HeadExpectation::Any, &CancellationToken::new())?;
    std::thread::scope(|scope| -> TestResult {
        let cancel = &cancel;
        let request = scope.spawn(move || waiting.begin(HeadExpectation::Any, cancel).map(drop));
        registration.recv_timeout(Duration::from_secs(1))?;
        cancel.cancel();
        let result = request.join().expect("cancelled request");
        assert!(
            matches!(
                result,
                Err(StoreError::Model(izu_model::ModelError::Cancelled))
            ),
            "{result:?}"
        );
        // A fresh independent HEAD descriptor still conflicts with the holder.
        let native = store
            .root_directory()
            .open_existing_lock(OsStr::new("head.lock"))?;
        assert!(matches!(
            native.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        drop(native);
        Ok(())
    })?;
    drop(holder);
    drop(store.begin(HeadExpectation::Any, &CancellationToken::new())?);
    Ok(())
}

#[test]
fn cancellation_or_hook_error_after_registration_leaves_no_claim() -> TestResult {
    for injected_error in [false, true] {
        let (directory, store) = fixture()?;
        let cancel = CancellationToken::new();
        let cancelled = cancel.clone();
        let options = StoreOptions {
            fault_hook: Some(FaultHook(Arc::new(move |boundary| {
                if boundary == DurableBoundary::HeadAdmissionRegistered {
                    if injected_error {
                        return Err(io::Error::from_raw_os_error(28));
                    }
                    cancelled.cancel();
                }
                Ok(())
            }))),
            ..StoreOptions::default()
        };
        let selected = Store::open(&directory.path().canonicalize()?.join("store"), &options)?;
        let result = selected.begin(HeadExpectation::Any, &cancel);
        if injected_error {
            assert!(
                matches!(result, Err(StoreError::Io { source, .. }) if source.raw_os_error() == Some(28))
            );
        } else {
            assert!(matches!(
                result,
                Err(StoreError::Model(izu_model::ModelError::Cancelled))
            ));
        }
        drop(store.begin(HeadExpectation::Any, &CancellationToken::new())?);
    }
    Ok(())
}

#[test]
fn one_deadline_covers_the_insertion_gate_and_native_head_wait() -> TestResult {
    let (directory, store) = fixture()?;
    let (registered, registration) = mpsc::channel();
    let options = StoreOptions {
        lock_timeout: Duration::from_millis(200),
        ..signal_registration(registered)
    };
    let waiting = Store::open(&directory.path().canonicalize()?.join("store"), &options)?;
    let native = store
        .root_directory()
        .open_existing_lock(OsStr::new("head.lock"))?;
    native.try_lock()?;
    let queue = store
        .root_directory()
        .open_dir(OsStr::new("head-admission-v1"))?;
    let gate = queue.open_existing_lock(OsStr::new("gate.lock"))?;
    gate.try_lock()?;
    std::thread::scope(|scope| -> TestResult {
        let (started, start_wait) = mpsc::channel();
        let request = scope.spawn(move || {
            started.send(Instant::now()).expect("start observer");
            let result = waiting
                .begin(HeadExpectation::Any, &CancellationToken::new())
                .map(drop);
            (result, Instant::now())
        });
        let beginning = start_wait.recv_timeout(Duration::from_secs(1))?;
        std::thread::sleep(Duration::from_millis(120));
        drop(gate);
        let registered = registration.recv_timeout(Duration::from_secs(1))?;
        let (result, ended) = request.join().expect("bounded request");
        assert!(
            matches!(result, Err(StoreError::LockTimeout("head.lock"))),
            "{result:?}"
        );
        assert!(registered.duration_since(beginning) >= Duration::from_millis(100));
        assert!(
            ended.duration_since(registered) < Duration::from_millis(150),
            "native phase restarted the deadline: {:?}",
            ended.duration_since(registered)
        );
        Ok(())
    })?;
    drop(native);
    drop(store.begin(HeadExpectation::Any, &CancellationToken::new())?);
    Ok(())
}

#[test]
fn coordination_bootstrap_and_dead_partial_or_oversized_records_are_recoverable() -> TestResult {
    for empty_directory in [false, true] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().canonicalize()?.join("store");
        let store = Store::init(&path, &StoreOptions::default())?;
        // Preserve the initialized coordination outside this inactive store to
        // model FORMAT 1 before admission, or creator death before gate creation.
        fs::rename(
            path.join("head-admission-v1"),
            directory.path().join("saved-admission"),
        )?;
        if empty_directory {
            let queue = store
                .root_directory()
                .ensure_dir(OsStr::new("head-admission-v1"))?;
            assert!(matches!(
                queue.open_existing_lock(OsStr::new("gate.lock")),
                Err(error) if error.kind() == io::ErrorKind::NotFound
            ));
        } else {
            assert!(matches!(
                store.root_directory().open_dir(OsStr::new("head-admission-v1")),
                Err(error) if error.kind() == io::ErrorKind::NotFound
            ));
        }
        drop(store);
        let store = Store::open(&path, &StoreOptions::default())?;
        drop(store.begin(HeadExpectation::Absent, &CancellationToken::new())?);
        let slot = path.join("head-admission-v1/slot-0000.lock");
        for stale in [b"partial".as_slice(), &[0; 80]] {
            fs::write(&slot, stale)?;
            assert_eq!(fs::metadata(&slot)?.len(), stale.len() as u64);
            drop(store.begin(HeadExpectation::Absent, &CancellationToken::new())?);
            assert_eq!(fs::metadata(&slot)?.len(), 56);
            assert_eq!(store.current_head(&CancellationToken::new())?, None);
            store.recover(&CancellationToken::new())?;
        }
        assert_eq!(
            fs::metadata(directory.path().join("saved-admission/slot-0000.lock"))?.len(),
            56
        );
    }
    Ok(())
}

#[test]
fn unknown_nonblank_or_linked_coordination_entries_are_preserved_and_refused() -> TestResult {
    for fault in ["unknown", "nonblank", "linked"] {
        let (directory, store) = fixture()?;
        let queue = directory
            .path()
            .canonicalize()?
            .join("store/head-admission-v1");
        let inspected = match fault {
            "unknown" => {
                let path = queue.join("foreign");
                fs::write(&path, b"keep me")?;
                path
            }
            "nonblank" => {
                let path = queue.join("gate.lock");
                fs::write(&path, b"foreign gate")?;
                path
            }
            "linked" => {
                let path = queue.join("gate-copy");
                fs::hard_link(queue.join("gate.lock"), &path)?;
                path
            }
            _ => unreachable!(),
        };
        let retained = fs::read(&inspected)?;
        let result = store.begin(HeadExpectation::Any, &CancellationToken::new());
        assert!(
            matches!(result, Err(StoreError::Corrupt { .. })),
            "{fault}: {result:?}"
        );
        assert_eq!(fs::read(&inspected)?, retained);
    }
    Ok(())
}

#[test]
fn missing_head_confirmation_keeps_its_place_before_a_later_writer() -> TestResult {
    let (directory, store) = fixture()?;
    let path = directory.path().canonicalize()?.join("store");
    let (registered, registration) = mpsc::channel();
    let (resume, resume_wait) = mpsc::channel();
    let resume_wait = Mutex::new(resume_wait);
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::HeadAdmissionRegistered {
                registered.send(()).map_err(io::Error::other)?;
                resume_wait
                    .lock()
                    .map_err(|_| io::Error::other("resume mutex poisoned"))?
                    .recv_timeout(Duration::from_secs(2))
                    .map_err(io::Error::other)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let reading = Store::open(&path, &options)?;
    let (writer_registered, writer_registration) = mpsc::channel();
    let writing = Store::open(&path, &signal_registration(writer_registered))?;
    let holder = store.begin(HeadExpectation::Any, &CancellationToken::new())?;
    // Owned fixture mutation forces the real absence-confirmation call site.
    store.root_directory().rename_replace(
        OsStr::new("HEAD"),
        store.root_directory(),
        OsStr::new("saved-head"),
    )?;
    std::thread::scope(|scope| -> TestResult {
        let reader = scope.spawn(|| reading.current_head(&CancellationToken::new()));
        registration.recv_timeout(Duration::from_secs(1))?;
        drop(holder);
        let (writer_ready, writer_acquired) = mpsc::channel();
        let writing = &writing;
        let writer = scope.spawn(move || {
            let transaction = writing.begin(HeadExpectation::Any, &CancellationToken::new())?;
            writer_ready
                .send(())
                .map_err(|error| io::Error::other(error.to_string()))
                .map_err(|source| StoreError::Io {
                    action: "observe test writer",
                    source,
                })?;
            drop(transaction);
            Ok::<_, StoreError>(())
        });
        writer_registration.recv_timeout(Duration::from_secs(1))?;
        let early_writer = writer_acquired.recv_timeout(Duration::from_millis(30));
        resume.send(())?;
        assert_eq!(reader.join().expect("absence reader")?, None);
        writer.join().expect("later writer")?;
        assert!(
            matches!(early_writer, Err(mpsc::RecvTimeoutError::Timeout)),
            "writer bypassed registered shared confirmation: {early_writer:?}"
        );
        Ok(())
    })?;
    Ok(())
}

struct OwnedChild(std::process::Child);
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn queued_process_death_releases_its_claim_and_preserves_the_head_holder() -> TestResult {
    const CHILD_PATH: &str = "IZU_TEST_HEAD_QUEUED_CHILD";
    const READY: &str = "HEAD_CLAIM_REGISTERED";
    if let Some(path) = std::env::var_os(CHILD_PATH) {
        use std::io::Write;
        let options = StoreOptions {
            fault_hook: Some(FaultHook(Arc::new(|boundary| {
                if boundary == DurableBoundary::HeadAdmissionRegistered {
                    println!("HEAD_CLAIM_REGISTERED");
                    io::stdout().flush()?;
                    loop {
                        std::thread::park();
                    }
                }
                Ok(())
            }))),
            ..StoreOptions::default()
        };
        let store = Store::open(std::path::Path::new(&path), &options)?;
        drop(store.begin(HeadExpectation::Any, &CancellationToken::new())?);
        return Err("queued child unexpectedly returned".into());
    }
    let (directory, store) = fixture()?;
    let holder = store.begin(HeadExpectation::Any, &CancellationToken::new())?;
    let mut child = OwnedChild(
        std::process::Command::new(std::env::current_exe()?)
            .args([
                "--exact",
                "queued_process_death_releases_its_claim_and_preserves_the_head_holder",
                "--nocapture",
            ])
            .env(CHILD_PATH, directory.path().canonicalize()?.join("store"))
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::inherit())
            .spawn()?,
    );
    std::thread::scope(|scope| -> TestResult {
        use std::io::BufRead;
        let stdout = child.0.stdout.take().ok_or("child stdout missing")?;
        let (ready, observed) = mpsc::channel();
        let reader = scope.spawn(move || {
            for line in io::BufReader::new(stdout).lines() {
                if line.is_ok_and(|line| line.trim() == READY) {
                    let _ = ready.send(());
                    break;
                }
            }
        });
        let registration = observed.recv_timeout(Duration::from_secs(2));
        let killed = child.0.kill();
        let waited = child.0.wait();
        reader.join().expect("child readiness reader");
        killed?;
        waited?;
        registration?;
        Ok(())
    })?;
    let native = store
        .root_directory()
        .open_existing_lock(OsStr::new("head.lock"))?;
    assert!(matches!(
        native.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(native);
    drop(holder);
    drop(store.begin(HeadExpectation::Any, &CancellationToken::new())?);
    Ok(())
}
