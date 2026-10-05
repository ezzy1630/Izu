#![cfg(all(unix, feature = "fault-injection"))]

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use izu_engine::{Repository, RepositoryOptions, Selection, WorkspaceState};
use izu_model::{CancellationToken, OperationId};
use izu_runtime::{Runtime, RuntimeConfig, RuntimeError};
use izu_store::{DurableBoundary, FaultHook};

struct Fixture {
    _temporary: tempfile::TempDir,
    repository: Repository,
    workspace: WorkspaceState,
    cancel: CancellationToken,
}

impl Fixture {
    fn new() -> Self {
        let temporary_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
        fs::create_dir_all(&temporary_root).unwrap();
        let temporary = tempfile::tempdir_in(temporary_root).unwrap();
        let base = temporary.path().canonicalize().unwrap();
        fs::create_dir(base.join("source")).unwrap();
        fs::write(base.join("source/human.txt"), "human root remains here\n").unwrap();
        let repository =
            Repository::init(base.join("source"), RepositoryOptions::default()).unwrap();
        let cancel = CancellationToken::new();
        let root = repository
            .workspace(repository.workspace_id(), &cancel)
            .unwrap();
        let workspace = repository
            .fork_workspace(
                "close-ack".into(),
                base.join("agent"),
                root.expected.head,
                &cancel,
            )
            .unwrap();
        fs::write(
            base.join("agent/unique.txt"),
            "unique work survives an incomplete close",
        )
        .unwrap();
        Self {
            _temporary: temporary,
            repository,
            workspace,
            cancel,
        }
    }

    fn runtime(&self) -> Runtime {
        Runtime::open(
            self.repository.metadata_path().join("runtime"),
            RuntimeConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_izu-runtime-worker"))),
        )
        .unwrap()
    }

    fn with_hook(
        &self,
        boundary: DurableBoundary,
        occurrence: usize,
        action: impl Fn() -> std::io::Result<()> + Send + Sync + 'static,
    ) -> (Repository, Arc<AtomicUsize>) {
        let hits = Arc::new(AtomicUsize::new(0));
        let hook_hits = Arc::clone(&hits);
        let mut options = RepositoryOptions::default();
        options.store.fault_hook = Some(FaultHook(Arc::new(move |at| {
            if at == boundary && hook_hits.fetch_add(1, Ordering::SeqCst) + 1 == occurrence {
                action()?;
            }
            Ok(())
        })));
        (
            Repository::open(self.repository.root_path(), options).unwrap(),
            hits,
        )
    }

    fn close_error(&self, runtime: &mut Runtime, repository: &Repository) -> RuntimeError {
        runtime
            .close_workspace(
                repository,
                self.workspace.id,
                self.workspace.expected,
                &self.cancel,
            )
            .unwrap_err()
    }

    fn assert_source_retained(&self) {
        assert_eq!(
            fs::read_to_string(PathBuf::from(&self.workspace.record.root).join("unique.txt"))
                .unwrap(),
            "unique work survives an incomplete close"
        );
        assert_eq!(
            fs::read_to_string(self.repository.root_path().join("human.txt")).unwrap(),
            "human root remains here\n"
        );
    }

    fn assert_uncertain(
        &self,
        error: &RuntimeError,
        description: &str,
        registered: bool,
    ) -> OperationId {
        let fresh = CancellationToken::new();
        let actual = self.repository.current_operation(&fresh).unwrap();
        let operation = self.repository.operation(actual, &fresh).unwrap();
        println!(
            "visible={actual} description={:?} registered={registered} code={} uncertain_operation={:?} error={error:?}",
            operation.description,
            error.code(),
            error.uncertain_operation(),
        );
        assert_eq!(error.uncertain_operation(), Some(actual), "{error:?}");
        assert_eq!(error.code(), "publication_uncertain");
        assert!(operation.description.starts_with(description));
        assert_eq!(
            operation.view.workspaces.contains_key(&self.workspace.id),
            registered
        );
        self.assert_source_retained();
        actual
    }
}

fn cancel_after_publication(occurrence: usize, description: &str, registered: bool) {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let cancel = fixture.cancel.clone();
    let (repository, hits) = fixture.with_hook(
        DurableBoundary::HeadDirectorySynced,
        occurrence,
        move || {
            cancel.cancel();
            Ok(())
        },
    );
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    let error = fixture.close_error(&mut runtime, &repository);
    assert!(fixture.cancel.is_cancelled());
    assert_eq!(hits.load(Ordering::SeqCst), occurrence);
    let actual = fixture.assert_uncertain(&error, description, registered);
    assert_ne!(actual, before);
}

#[test]
fn checkpoint_readback_cancellation_identifies_visible_operation() {
    cancel_after_publication(1, "Checkpoint workspace ", true);
}

#[test]
fn close_readback_cancellation_identifies_visible_operation() {
    // Close first publishes its final recovery capture, then removes registration.
    cancel_after_publication(3, "Close workspace ", false);
}

#[test]
fn interrupted_engine_close_preserves_later_recovery_operation() {
    cancel_after_publication(2, "Save final source for workspace ", true);
}

#[test]
fn cancellation_before_close_does_not_publish() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    fixture.cancel.cancel();
    let error = fixture.close_error(&mut runtime, &fixture.repository);
    assert!(matches!(error, RuntimeError::Cancelled), "{error:?}");
    assert_eq!(error.uncertain_operation(), None);
    let fresh = CancellationToken::new();
    assert_eq!(
        fixture.repository.current_operation(&fresh).unwrap(),
        before
    );
    assert_eq!(
        fixture
            .repository
            .workspace(fixture.workspace.id, &fresh)
            .unwrap()
            .expected,
        fixture.workspace.expected,
    );
    fixture.assert_source_retained();
}

#[test]
fn cancellation_before_checkpoint_publication_does_not_report_uncertainty() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    let cancel = fixture.cancel.clone();
    let (repository, hits) = fixture.with_hook(DurableBoundary::HeadFileSynced, 1, move || {
        cancel.cancel();
        Ok(())
    });
    let error = fixture.close_error(&mut runtime, &repository);
    assert!(fixture.cancel.is_cancelled());
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    assert_eq!(error.uncertain_operation(), None);
    assert!(!matches!(
        error,
        RuntimeError::DurabilityUncertain(_)
            | RuntimeError::Engine(izu_engine::EngineError::PublicationUncertain { .. })
    ));
    let fresh = CancellationToken::new();
    assert_eq!(
        fixture.repository.current_operation(&fresh).unwrap(),
        before
    );
    assert_eq!(
        fixture
            .repository
            .workspace(fixture.workspace.id, &fresh)
            .unwrap()
            .expected,
        fixture.workspace.expected,
    );
    fixture.assert_source_retained();
}

#[test]
fn failure_before_engine_close_publication_identifies_prior_checkpoint() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let (repository, hits) = fixture.with_hook(DurableBoundary::HeadDataWritten, 2, || {
        Err(std::io::Error::other(
            "injected final-source preparation failure",
        ))
    });
    let error = fixture.close_error(&mut runtime, &repository);
    assert_eq!(hits.load(Ordering::SeqCst), 2);
    fixture.assert_uncertain(&error, "Checkpoint workspace ", true);
}

#[test]
fn failure_before_final_close_publication_preserves_later_recovery_operation() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let (repository, hits) = fixture.with_hook(DurableBoundary::HeadDataWritten, 3, || {
        Err(std::io::Error::other(
            "injected final-close preparation failure",
        ))
    });
    let error = fixture.close_error(&mut runtime, &repository);
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    fixture.assert_uncertain(&error, "Save final source for workspace ", true);
}

#[test]
fn corrupt_checkpoint_tree_readback_identifies_visible_checkpoint() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let tree = fixture
        .repository
        .capture(fixture.workspace.id, Selection::All, &fixture.cancel)
        .unwrap()
        .tree
        .to_string();
    let object = fixture
        .repository
        .metadata_path()
        .join("objects")
        .join(&tree[..2])
        .join(&tree[2..]);
    assert!(object.is_file());
    let (repository, hits) =
        fixture.with_hook(DurableBoundary::HeadDirectorySynced, 1, move || {
            fs::write(&object, b"corrupt fixture tree object")
        });
    let error = fixture.close_error(&mut runtime, &repository);
    assert_eq!(hits.load(Ordering::SeqCst), 1);
    fixture.assert_uncertain(&error, "Checkpoint workspace ", true);
    assert!(
        error
            .to_string()
            .contains("checkpoint tree readback failed")
    );
}

#[test]
fn registry_acknowledgement_failure_identifies_visible_close() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let registry = fixture.repository.metadata_path().join("runtime");
    let retained = fixture.repository.metadata_path().join("retained-runtime");
    let retained_readback = retained.clone();
    let (repository, hits) =
        fixture.with_hook(DurableBoundary::HeadDirectorySynced, 3, move || {
            fs::rename(&registry, &retained)?;
            fs::create_dir(&registry)
        });
    let error = fixture.close_error(&mut runtime, &repository);
    assert_eq!(hits.load(Ordering::SeqCst), 3);
    fixture.assert_uncertain(&error, "Close workspace ", false);
    assert!(
        error
            .to_string()
            .contains("runtime close acknowledgement failed")
    );
    assert!(retained_readback.is_dir());
}
