use izu_engine::*;
use std::fs;
use std::io;
use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
fn options() -> RepositoryOptions {
    RepositoryOptions {
        store: StoreOptions {
            lock_timeout: Duration::from_millis(50),
            ..StoreOptions::default()
        },
        ..RepositoryOptions::default()
    }
}
fn fixture() -> std::result::Result<(tempfile::TempDir, Repository), Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let repo = Repository::init(dir.path(), options())?;
    Ok((dir, repo))
}
fn blob(repo: &Repository, text: &[u8]) -> std::result::Result<ObjectId, EngineError> {
    repo.put_blob(&mut &text[..], text.len() as u64, &CancellationToken::new())
}
fn lock_timeout(error: EngineError) -> bool {
    matches!(
        error,
        EngineError::Store(izu_store::StoreError::LockTimeout("temporary.lock"))
    )
}

#[test]
fn multi_step_preparation_lease_protects_caller_gaps() -> TestResult {
    let (_dir, repo) = fixture()?;
    let cancel = CancellationToken::new();
    let lease = repo.retention_lease(&cancel)?;
    let id = blob(&repo, b"prepared unpublished unique work")?;
    assert!(lock_timeout(
        repo.reclaim_selected(&[id], &cancel)
            .err()
            .ok_or("reclaim unexpectedly succeeded")?
    ));
    assert_eq!(repo.blob(id, &cancel)?, b"prepared unpublished unique work");
    drop(lease);
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel)?,
        ReclaimOutcome::Durable(_)
    ));
    Ok(())
}

#[test]
fn native_history_and_source_blobs_are_retained() -> TestResult {
    let (dir, repo) = fixture()?;
    let cancel = CancellationToken::new();
    fs::write(dir.path().join("file"), b"committed unique content")?;
    let workspace = repo.workspace_id();
    let receipt = repo.commit(
        workspace,
        repo.workspace(workspace, &cancel)?.expected,
        Selection::All,
        "retain history".into(),
        Identity {
            name: "Fixture".into(),
            email: "fixture@example.invalid".into(),
        },
        &cancel,
    )?;
    let tree = repo.tree(repo.revision(receipt.revision, &cancel)?.tree, &cancel)?;
    let id = match tree.entries.get(&RepoPath::new("file")?) {
        Some(TreeEntry::File { blob, .. }) => *blob,
        _ => return Err("missing committed file".into()),
    };
    assert!(repo.reclaim_selected(&[id], &cancel).is_err());
    assert!(
        repo.reclaim_selected(&[receipt.operation.object_id()], &cancel)
            .is_err()
    );
    assert_eq!(repo.blob(id, &cancel)?, b"committed unique content");
    repo.verify(&cancel)?;
    Ok(())
}

#[test]
fn workspace_guards_and_writer_leases_own_retention() -> TestResult {
    let (_dir, repo) = fixture()?;
    let cancel = CancellationToken::new();
    let id = blob(&repo, b"guarded unique work")?;
    let state = repo.workspace(repo.workspace_id(), &cancel)?;
    let guard = repo.lock_workspace(state.id, state.expected, &cancel)?;
    assert!(lock_timeout(
        repo.reclaim_selected(&[id], &cancel)
            .err()
            .ok_or("guard failed to exclude reclamation")?
    ));
    drop(guard);
    let writer = repo.lease_writer(state.id, &cancel)?;
    assert!(lock_timeout(
        repo.reclaim_selected(&[id], &cancel)
            .err()
            .ok_or("writer failed to exclude reclamation")?
    ));
    writer.finish_stopped()?;
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel)?,
        ReclaimOutcome::Durable(_)
    ));
    Ok(())
}

#[test]
fn abandoned_writer_intent_blocks_discard_until_exact_acknowledgement() -> TestResult {
    let (_dir, repo) = fixture()?;
    let cancel = CancellationToken::new();
    let id = blob(&repo, b"unique crashed preparation")?;
    let writer = repo.lease_writer(repo.workspace_id(), &cancel)?;
    let token = writer.intent().token.clone();
    drop(writer);
    assert!(matches!(
        repo.plan_reclamation(&[id], &cancel),
        Err(EngineError::ReclamationBlocked { .. })
    ));
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel),
        Err(EngineError::ReclamationBlocked { .. })
    ));
    repo.recover(&cancel)?;
    assert!(repo.writer_intent(repo.workspace_id(), &cancel)?.is_some());
    repo.acknowledge_writer_stopped(repo.workspace_id(), &token, &cancel)?;
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel)?,
        ReclaimOutcome::Durable(_)
    ));
    Ok(())
}

#[test]
fn malformed_and_unknown_retention_entries_fail_closed() -> TestResult {
    let (_dir, repo) = fixture()?;
    let cancel = CancellationToken::new();
    let id = blob(&repo, b"retain around unknown intents")?;
    let locks = repo.metadata_path().join("workspace-locks");
    for name in ["operator-note", "live-invalid.intent"] {
        let path = locks.join(name);
        fs::write(&path, b"unknown")?;
        assert!(matches!(
            repo.reclaim_selected(&[id], &cancel),
            Err(EngineError::ReclamationBlocked { .. })
        ));
        assert!(path.exists());
        fs::remove_file(path)?;
    }
    let intent = locks.join(format!("live-{}.intent", repo.workspace_id()));
    fs::write(&intent, b"{invalid")?;
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel),
        Err(EngineError::ReclamationBlocked { .. })
    ));
    assert_eq!(repo.blob(id, &cancel)?, b"retain around unknown intents");
    Ok(())
}

struct BlockingOutput {
    started: mpsc::Sender<()>,
    resume: mpsc::Receiver<()>,
}
impl io::Write for BlockingOutput {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.started.send(()).map_err(io::Error::other)?;
        self.resume.recv().map_err(io::Error::other)?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
#[test]
fn active_engine_blob_reader_excludes_reclamation() -> TestResult {
    let (_dir, repo) = fixture()?;
    let id = blob(&repo, b"active output stream")?;
    let (started_tx, started_rx) = mpsc::channel();
    let (resume_tx, resume_rx) = mpsc::channel();
    std::thread::scope(|scope| -> TestResult {
        let reader = scope.spawn(|| {
            repo.read_blob(
                id,
                &mut BlockingOutput {
                    started: started_tx,
                    resume: resume_rx,
                },
                &CancellationToken::new(),
            )
        });
        started_rx.recv_timeout(Duration::from_secs(5))?;
        let result = repo.reclaim_selected(&[id], &CancellationToken::new());
        resume_tx.send(())?;
        reader.join().map_err(|_| "reader panicked")??;
        assert!(lock_timeout(
            result
                .err()
                .ok_or("active reader failed to exclude reclaim")?
        ));
        Ok(())
    })?;
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[test]
fn engine_recovery_restores_interrupted_reclamation_and_preserves_history() -> TestResult {
    use izu_store::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let dir = tempfile::tempdir()?;
    let mut settings = options();
    settings.store.fault_hook = Some(FaultHook(Arc::new(|boundary| {
        if boundary == DurableBoundary::ReclamationObjectRemoved {
            Err(io::Error::other("injected interrupted unlink"))
        } else {
            Ok(())
        }
    })));
    let repo = Repository::init(dir.path(), settings)?;
    let cancel = CancellationToken::new();
    let initial = repo.current_operation(&cancel)?;
    let id = blob(&repo, b"exact interrupted discard")?;
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel)?,
        ReclaimOutcome::VisibleButUncertain { .. }
    ));
    let reopened = Repository::open(dir.path(), options())?;
    reopened.recover(&cancel)?;
    assert_eq!(reopened.blob(id, &cancel)?, b"exact interrupted discard");
    assert_eq!(reopened.current_operation(&cancel)?, initial);
    reopened.verify(&cancel)?;
    Ok(())
}

#[test]
fn crashed_engine_writer_child() -> TestResult {
    let Some(path) = std::env::var_os("IZU_ENGINE_CRASHED_WRITER_PATH") else {
        return Ok(());
    };
    let repo = Repository::open(Path::new(&path), options())?;
    let _writer = repo.lease_writer(repo.workspace_id(), &CancellationToken::new())?;
    std::process::exit(87);
}

#[test]
fn process_exit_keeps_writer_intent_and_blocks_reclamation() -> TestResult {
    let (dir, repo) = fixture()?;
    let cancel = CancellationToken::new();
    let id = blob(&repo, b"unpublished unique work before writer crash")?;
    let child = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "crashed_engine_writer_child", "--nocapture"])
        .env("IZU_ENGINE_CRASHED_WRITER_PATH", dir.path())
        .output()?;
    assert_eq!(
        child.status.code(),
        Some(87),
        "{}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(matches!(
        repo.reclaim_selected(&[id], &cancel),
        Err(EngineError::ReclamationBlocked { .. })
    ));
    assert_eq!(
        repo.blob(id, &cancel)?,
        b"unpublished unique work before writer crash"
    );
    Ok(())
}
