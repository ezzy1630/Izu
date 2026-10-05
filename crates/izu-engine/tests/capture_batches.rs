#![cfg(feature = "fault-injection")]

use izu_engine::{
    CancellationToken, EngineError, RepoPath, Repository, RepositoryOptions, Selection, TreeEntry,
};
use izu_store::{DurableBoundary, FaultHook, StoreError};
use std::fs;
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

fn failed_capture_preserves_checkpoint(boundary: DurableBoundary) {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join(".artifacts/tmp");
    fs::create_dir_all(&root).unwrap();
    let fixture = tempfile::tempdir_in(root).unwrap();
    let armed = Arc::new(AtomicBool::new(false));
    let inject = Arc::clone(&armed);
    let mut options = RepositoryOptions::default();
    options.store.fault_hook = Some(FaultHook(Arc::new(move |at| {
        if at == boundary && inject.swap(false, Ordering::SeqCst) {
            return Err(io::Error::from_raw_os_error(28));
        }
        Ok(())
    })));
    let repository = Repository::init(fixture.path(), options).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::write(fixture.path().join("source"), b"acknowledged source").unwrap();
    let previous = repository
        .checkpoint(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            Selection::All,
            &cancel,
        )
        .unwrap();
    let expected = repository.workspace(id, &cancel).unwrap().expected;
    fs::write(fixture.path().join("source"), b"unique edited source").unwrap();
    fs::write(fixture.path().join("new"), b"unique new source").unwrap();
    armed.store(true, Ordering::SeqCst);

    let failed = repository.checkpoint(id, expected, Selection::All, &cancel);
    assert!(
        matches!(failed, Err(EngineError::Store(StoreError::Io { ref source, .. })) if source.raw_os_error() == Some(28)),
        "{boundary:?}: {failed:?}"
    );
    assert!(!armed.load(Ordering::SeqCst), "batch hook was not reached");
    assert_eq!(
        repository.current_operation(&cancel).unwrap(),
        previous.operation
    );
    assert_eq!(
        repository.workspace(id, &cancel).unwrap().expected,
        expected
    );
    assert_eq!(
        fs::read(fixture.path().join("source")).unwrap(),
        b"unique edited source"
    );
    assert_eq!(
        fs::read(fixture.path().join("new")).unwrap(),
        b"unique new source"
    );
    drop(repository);

    let reopened = Repository::open(fixture.path(), RepositoryOptions::default()).unwrap();
    reopened.recover(&cancel).unwrap();
    assert_eq!(
        reopened.current_operation(&cancel).unwrap(),
        previous.operation
    );
    reopened.verify(&cancel).unwrap();
    let old = reopened.tree(previous.tree, &cancel).unwrap();
    let TreeEntry::File { blob, .. } = old.entries[&RepoPath::new("source").unwrap()] else {
        panic!("previous checkpoint lost its source file");
    };
    assert_eq!(
        reopened.blob(blob, &cancel).unwrap(),
        b"acknowledged source"
    );
    let completed = reopened
        .checkpoint(id, expected, Selection::All, &cancel)
        .unwrap();
    let captured = reopened.tree(completed.tree, &cancel).unwrap();
    for (path, bytes) in [
        ("source", b"unique edited source".as_slice()),
        ("new", b"unique new source".as_slice()),
    ] {
        let TreeEntry::File { blob, .. } = captured.entries[&RepoPath::new(path).unwrap()] else {
            panic!("successful capture lost {path}");
        };
        assert_eq!(reopened.blob(blob, &cancel).unwrap(), bytes);
    }
    reopened.verify(&cancel).unwrap();
}

#[test]
fn pre_link_batch_failure_preserves_last_checkpoint_and_unique_source() {
    failed_capture_preserves_checkpoint(DurableBoundary::BatchDataSynced);
}

#[test]
fn final_batch_barrier_failure_preserves_last_checkpoint_and_unique_source() {
    failed_capture_preserves_checkpoint(DurableBoundary::BatchDirectorySynced);
}
