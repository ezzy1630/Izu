#![cfg(unix)]

use izu_engine::*;
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::Path;
use tempfile::TempDir;

fn fixture() -> TempDir {
    let lane = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let root = lane.join(".artifacts/tmp");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}

fn author() -> Identity {
    Identity {
        name: "Core regression fixture".into(),
        email: "fixture@localhost".into(),
    }
}

fn commit(repository: &Repository, id: WorkspaceId) -> CommitReceipt {
    let cancel = CancellationToken::new();
    repository
        .commit(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            Selection::All,
            "fixture source".into(),
            author(),
            &cancel,
        )
        .unwrap()
}

fn permissions(path: &Path, mode: u32) {
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}

fn path(value: &str) -> RepoPath {
    RepoPath::new(value).unwrap()
}

#[test]
fn selective_restore_completes_only_required_recorded_ancestors() {
    let fixture = fixture();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::create_dir(fixture.path().join("outer")).unwrap();
    fs::create_dir(fixture.path().join("outer/nested")).unwrap();
    fs::write(fixture.path().join("outer/nested/file"), b"original bytes").unwrap();
    fs::write(
        fixture.path().join("outer/nested/unselected"),
        b"not selected",
    )
    .unwrap();
    fs::write(fixture.path().join("kept"), b"clean baseline").unwrap();
    permissions(&fixture.path().join("outer"), 0o711);
    permissions(&fixture.path().join("outer/nested"), 0o750);
    permissions(&fixture.path().join("outer/nested/file"), 0o641);
    let original = commit(&repository, id);
    fs::remove_dir_all(fixture.path().join("outer")).unwrap();
    let deletion = commit(&repository, id);
    fs::write(fixture.path().join("kept"), b"independent dirty bytes").unwrap();
    permissions(&fixture.path().join("kept"), 0o600);

    let receipt = repository
        .restore(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            original.revision,
            Selection::Paths(vec![path("outer/nested/file")]),
            &cancel,
        )
        .unwrap();
    assert_eq!(receipt.head, deletion.revision);
    assert_eq!(
        fs::read(fixture.path().join("outer/nested/file")).unwrap(),
        b"original bytes"
    );
    assert_eq!(
        fs::metadata(fixture.path().join("outer")).unwrap().mode() & 0o7777,
        0o711
    );
    assert_eq!(
        fs::metadata(fixture.path().join("outer/nested"))
            .unwrap()
            .mode()
            & 0o7777,
        0o750
    );
    assert_eq!(
        fs::metadata(fixture.path().join("outer/nested/file"))
            .unwrap()
            .mode()
            & 0o7777,
        0o641
    );
    assert!(!fixture.path().join("outer/nested/unselected").exists());
    assert_eq!(
        fs::read(fixture.path().join("kept")).unwrap(),
        b"independent dirty bytes"
    );
    assert_eq!(
        fs::metadata(fixture.path().join("kept")).unwrap().mode() & 0o7777,
        0o600
    );
    let tree = repository.tree(receipt.tree, &cancel).unwrap();
    assert_eq!(
        tree.entries[&path("outer")],
        TreeEntry::Directory {
            mode: FileMode::from_unix_permissions(0o711).unwrap()
        }
    );
    assert_eq!(
        tree.entries[&path("outer/nested")],
        TreeEntry::Directory {
            mode: FileMode::from_unix_permissions(0o750).unwrap()
        }
    );
    assert!(!tree.entries.contains_key(&path("outer/nested/unselected")));
    assert_eq!(
        repository
            .capture(id, Selection::All, &cancel)
            .unwrap()
            .tree,
        receipt.tree
    );
    repository.verify(&cancel).unwrap();
}

#[test]
fn selective_restore_preserves_existing_unselected_ancestor_modes() {
    let fixture = fixture();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::create_dir_all(fixture.path().join("outer/nested")).unwrap();
    fs::write(fixture.path().join("outer/nested/file"), b"original").unwrap();
    permissions(&fixture.path().join("outer"), 0o711);
    permissions(&fixture.path().join("outer/nested"), 0o750);
    let original = commit(&repository, id);
    fs::remove_dir_all(fixture.path().join("outer/nested")).unwrap();
    permissions(&fixture.path().join("outer"), 0o700);
    fs::write(fixture.path().join("outer/kept"), b"unselected").unwrap();
    commit(&repository, id);

    let receipt = repository
        .restore(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            original.revision,
            Selection::Paths(vec![path("outer/nested/file")]),
            &cancel,
        )
        .unwrap();
    assert_eq!(
        fs::metadata(fixture.path().join("outer")).unwrap().mode() & 0o7777,
        0o700
    );
    assert_eq!(
        fs::metadata(fixture.path().join("outer/nested"))
            .unwrap()
            .mode()
            & 0o7777,
        0o750
    );
    assert_eq!(
        fs::read(fixture.path().join("outer/kept")).unwrap(),
        b"unselected"
    );
    let tree = repository.tree(receipt.tree, &cancel).unwrap();
    assert_eq!(
        tree.entries[&path("outer")],
        TreeEntry::Directory {
            mode: FileMode::from_unix_permissions(0o700).unwrap()
        }
    );
    assert_eq!(
        repository
            .capture(id, Selection::All, &cancel)
            .unwrap()
            .tree,
        receipt.tree
    );

    fs::remove_file(fixture.path().join("outer/nested/file")).unwrap();
    permissions(&fixture.path().join("outer/nested"), 0o710);
    commit(&repository, id);
    let receipt = repository
        .restore(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            original.revision,
            Selection::Paths(vec![path("outer/nested/file")]),
            &cancel,
        )
        .unwrap();
    assert_eq!(
        fs::metadata(fixture.path().join("outer/nested"))
            .unwrap()
            .mode()
            & 0o7777,
        0o710
    );
    assert_eq!(
        repository.tree(receipt.tree, &cancel).unwrap().entries[&path("outer/nested")],
        TreeEntry::Directory {
            mode: FileMode::from_unix_permissions(0o710).unwrap()
        }
    );
    repository.verify(&cancel).unwrap();
}

#[test]
fn selective_restore_refuses_unselected_non_directory_ancestors_and_unknown_leaf() {
    for kind in ["file", "symlink", "unknown leaf"] {
        let fixture = fixture();
        let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
        let id = repository.workspace_id();
        let cancel = CancellationToken::new();
        fs::create_dir(fixture.path().join("nested")).unwrap();
        fs::write(fixture.path().join("nested/file"), b"old source").unwrap();
        let original = commit(&repository, id);
        fs::remove_dir_all(fixture.path().join("nested")).unwrap();
        commit(&repository, id);
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("file"), b"outside source").unwrap();
        match kind {
            "file" => fs::write(fixture.path().join("nested"), b"unselected file").unwrap(),
            "symlink" => symlink(&outside, fixture.path().join("nested")).unwrap(),
            "unknown leaf" => {
                fs::create_dir(fixture.path().join("nested")).unwrap();
                fs::write(fixture.path().join("nested/file"), b"unknown source").unwrap();
            }
            _ => unreachable!(),
        }
        let before = repository.current_operation(&cancel).unwrap();
        let error = repository
            .restore(
                id,
                repository.workspace(id, &cancel).unwrap().expected,
                original.revision,
                Selection::Paths(vec![path("nested/file")]),
                &cancel,
            )
            .unwrap_err();
        assert!(
            matches!(
                error,
                EngineError::PathCollision(_) | EngineError::UnselectedCollision(_)
            ),
            "{kind}: {error:?}"
        );
        assert_eq!(repository.current_operation(&cancel).unwrap(), before);
        assert!(!fixture.path().join(".izu-recovery").exists());
        assert_eq!(fs::read(outside.join("file")).unwrap(), b"outside source");
        match kind {
            "file" => assert_eq!(
                fs::read(fixture.path().join("nested")).unwrap(),
                b"unselected file"
            ),
            "symlink" => assert_eq!(
                fs::read_link(fixture.path().join("nested")).unwrap(),
                outside
            ),
            "unknown leaf" => assert_eq!(
                fs::read(fixture.path().join("nested/file")).unwrap(),
                b"unknown source"
            ),
            _ => unreachable!(),
        }
    }
}

#[test]
fn capture_and_publication_refuse_hardlink_identity_without_touching_source() {
    let fixture = fixture();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    let source = fixture.path().join("first");
    let alias = fixture.path().join("second");
    fs::write(&source, b"linked bytes").unwrap();
    fs::hard_link(&source, &alias).unwrap();
    let before = repository.current_operation(&cancel).unwrap();
    let expected = repository.workspace(id, &cancel).unwrap().expected;
    let errors = [
        repository.capture(id, Selection::All, &cancel).unwrap_err(),
        repository.capture_all(id, &cancel).unwrap_err(),
        repository
            .checkpoint(id, expected, Selection::All, &cancel)
            .unwrap_err(),
        repository
            .commit(
                id,
                expected,
                Selection::All,
                "hardlinks".into(),
                author(),
                &cancel,
            )
            .unwrap_err(),
    ];
    for error in errors {
        assert!(
            matches!(&error, EngineError::UnsupportedEntry(entry) if entry == &source || entry == &alias),
            "{error:?}"
        );
        assert_eq!(error.code(), "unsupported_entry");
    }
    let first = fs::metadata(&source).unwrap();
    let second = fs::metadata(&alias).unwrap();
    assert_eq!((first.dev(), first.ino()), (second.dev(), second.ino()));
    assert_eq!((first.nlink(), second.nlink()), (2, 2));
    assert_eq!(fs::read(&source).unwrap(), b"linked bytes");
    assert_eq!(fs::read(&alias).unwrap(), b"linked bytes");
    assert_eq!(repository.current_operation(&cancel).unwrap(), before);
    assert_eq!(
        repository.workspace(id, &cancel).unwrap().expected,
        expected
    );
}

#[test]
fn selective_restore_keeps_ignored_existing_ancestor_mode_without_admitting_private_files() {
    let fixture = fixture();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::create_dir(fixture.path().join("nested")).unwrap();
    fs::write(fixture.path().join("nested/file"), b"saved source").unwrap();
    permissions(&fixture.path().join("nested"), 0o750);
    let original = commit(&repository, id);
    fs::remove_dir_all(fixture.path().join("nested")).unwrap();
    fs::write(fixture.path().join(".izuignore"), b"nested/\n").unwrap();
    commit(&repository, id);
    fs::create_dir(fixture.path().join("nested")).unwrap();
    permissions(&fixture.path().join("nested"), 0o730);
    fs::write(
        fixture.path().join("nested/private"),
        b"ignored unique source",
    )
    .unwrap();
    let receipt = repository
        .restore(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            original.revision,
            Selection::Paths(vec![path("nested/file")]),
            &cancel,
        )
        .unwrap();
    assert_eq!(
        fs::read(fixture.path().join("nested/file")).unwrap(),
        b"saved source"
    );
    assert_eq!(
        fs::read(fixture.path().join("nested/private")).unwrap(),
        b"ignored unique source"
    );
    assert_eq!(
        fs::metadata(fixture.path().join("nested")).unwrap().mode() & 0o7777,
        0o730
    );
    let tree = repository.tree(receipt.tree, &cancel).unwrap();
    assert_eq!(
        tree.entries[&path("nested")],
        TreeEntry::Directory {
            mode: FileMode::from_unix_permissions(0o730).unwrap()
        }
    );
    assert!(!tree.entries.contains_key(&path("nested/private")));
    let recovery = repository
        .operation(receipt.recovery_operation, &cancel)
        .unwrap();
    assert!(
        repository
            .tree(
                recovery.view.workspaces[&id].sources["recovery"].tree,
                &cancel
            )
            .unwrap()
            .entries
            .contains_key(&path("nested/private"))
    );
    assert_eq!(
        repository
            .capture(id, Selection::All, &cancel)
            .unwrap()
            .tree,
        receipt.tree
    );
    repository.verify(&cancel).unwrap();
}

#[test]
fn selective_restore_refuses_conflicted_target_ancestors_before_source_changes() {
    let fixture = fixture();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::create_dir(fixture.path().join("nested")).unwrap();
    fs::write(fixture.path().join("nested/file"), b"original source").unwrap();
    let original = commit(&repository, id);
    let mut target = repository.tree(original.tree, &cancel).unwrap();
    target.entries.insert(
        path("nested"),
        TreeEntry::Conflict {
            base: Some(ResolvedTreeEntry::Directory {
                mode: FileMode::Regular,
            }),
            ours: Some(ResolvedTreeEntry::Directory {
                mode: FileMode::Executable,
            }),
            theirs: None,
            reason: ConflictReason::DeleteModify,
        },
    );
    let tree = repository.put_tree(&target, &cancel).unwrap();
    let revision = repository
        .put_revision(
            &Revision {
                tree,
                ..repository.revision(original.revision, &cancel).unwrap()
            },
            &cancel,
        )
        .unwrap();
    let before = repository.current_operation(&cancel).unwrap();
    let error = repository
        .restore(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            revision,
            Selection::Paths(vec![path("nested/file")]),
            &cancel,
        )
        .unwrap_err();
    assert!(matches!(error, EngineError::InvalidInput(reason) if reason.contains("conflict")));
    assert_eq!(repository.current_operation(&cancel).unwrap(), before);
    assert_eq!(
        fs::read(fixture.path().join("nested/file")).unwrap(),
        b"original source"
    );
    assert!(!fixture.path().join(".izu-recovery").exists());
}

#[test]
fn checkpoint_and_commit_remain_allowed_with_live_and_unknown_writer_intent() {
    let fixture = fixture();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::write(fixture.path().join("source"), b"original").unwrap();
    let original = commit(&repository, id);
    let lease = repository.lease_writer(id, &cancel).unwrap();
    let token = lease.intent().token.clone();
    fs::write(fixture.path().join("source"), b"live writer capture").unwrap();
    let checkpoint = repository
        .checkpoint(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            Selection::All,
            &cancel,
        )
        .unwrap();
    assert_eq!(checkpoint.head, original.revision);
    let live_commit = commit(&repository, id);
    assert_eq!(live_commit.tree, checkpoint.tree);
    assert_eq!(
        repository
            .writer_intent(id, &cancel)
            .unwrap()
            .unwrap()
            .token,
        token
    );
    drop(lease);
    fs::write(fixture.path().join("source"), b"unknown writer capture").unwrap();
    let checkpoint = repository
        .checkpoint(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            Selection::All,
            &cancel,
        )
        .unwrap();
    assert_eq!(checkpoint.head, live_commit.revision);
    let unknown_commit = commit(&repository, id);
    assert_eq!(unknown_commit.tree, checkpoint.tree);
    assert_eq!(
        repository
            .writer_intent(id, &cancel)
            .unwrap()
            .unwrap()
            .token,
        token
    );
    assert!(matches!(
        repository.restore(
            id,
            repository.workspace(id, &cancel).unwrap().expected,
            original.revision,
            Selection::All,
            &cancel
        ),
        Err(EngineError::WorkspaceBusy { .. })
    ));
    assert_eq!(
        fs::read(fixture.path().join("source")).unwrap(),
        b"unknown writer capture"
    );
    repository
        .acknowledge_writer_stopped(id, &token, &cancel)
        .unwrap();
    repository.verify(&cancel).unwrap();
}

#[test]
fn recovery_capture_refuses_ignored_hardlinks_and_external_link_identity() {
    let fixture = fixture();
    let root = fixture.path().join("source-root");
    fs::create_dir(&root).unwrap();
    let repository = Repository::init(&root, RepositoryOptions::default()).unwrap();
    let id = repository.workspace_id();
    let cancel = CancellationToken::new();
    fs::write(root.join(".izuignore"), b"private-*\n").unwrap();
    fs::write(root.join("source"), b"saved").unwrap();
    let original = commit(&repository, id);
    fs::write(root.join("source"), b"dirty source").unwrap();
    let source = root.join("private-first");
    let outside = fixture.path().join("outside-link");
    fs::write(&source, b"private linked bytes").unwrap();
    fs::hard_link(&source, &outside).unwrap();
    let expected = repository.workspace(id, &cancel).unwrap().expected;
    let before = repository.current_operation(&cancel).unwrap();
    assert!(repository.capture(id, Selection::All, &cancel).is_ok());
    assert!(
        matches!(repository.capture_all(id, &cancel), Err(EngineError::UnsupportedEntry(entry)) if entry == source)
    );
    assert!(
        matches!(repository.restore(id, expected, original.revision, Selection::All, &cancel), Err(EngineError::UnsupportedEntry(entry)) if entry == source)
    );
    assert_eq!(repository.current_operation(&cancel).unwrap(), before);
    assert_eq!(
        repository.workspace(id, &cancel).unwrap().expected,
        expected
    );
    assert_eq!(fs::read(root.join("source")).unwrap(), b"dirty source");
    assert_eq!(fs::read(&outside).unwrap(), b"private linked bytes");
    assert_eq!(fs::metadata(&source).unwrap().nlink(), 2);
    assert!(!root.join(".izu-recovery").exists());
}

#[cfg(feature = "fault-injection")]
mod publication {
    use super::*;
    use izu_store::{DurableBoundary, FaultHook};
    use std::path::PathBuf;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    };

    #[derive(Clone, Copy, Debug)]
    enum Action {
        Capture,
        CaptureAll,
        Status,
        GuardCapture,
        LeasedCapture,
        Checkpoint,
        Commit,
        SelectiveCommit,
        CommitToRef,
        Revise,
        Close,
    }

    impl Action {
        fn run(
            self,
            repository: &Repository,
            id: WorkspaceId,
            expected: WorkspaceExpectation,
            cancel: &CancellationToken,
        ) -> Result<()> {
            match self {
                Self::Capture => repository.capture(id, Selection::All, cancel).map(|_| ()),
                Self::CaptureAll => repository.capture_all(id, cancel).map(|_| ()),
                Self::Status => repository.status(id, cancel).map(|_| ()),
                Self::GuardCapture => repository
                    .lock_workspace(id, expected, cancel)?
                    .capture(Selection::All, cancel)
                    .map(|_| ()),
                Self::LeasedCapture => {
                    let lease = repository.lease_workspace(id, expected, cancel)?;
                    let result = repository
                        .capture_leased(&lease, Selection::All, cancel)
                        .map(|_| ());
                    lease.finish_stopped()?;
                    result
                }
                Self::Checkpoint => repository
                    .checkpoint(id, expected, Selection::All, cancel)
                    .map(|_| ()),
                Self::Commit | Self::SelectiveCommit => repository
                    .commit(
                        id,
                        expected,
                        if matches!(self, Self::SelectiveCommit) {
                            Selection::Paths(vec![path("source")])
                        } else {
                            Selection::All
                        },
                        "source publication".into(),
                        author(),
                        cancel,
                    )
                    .map(|_| ()),
                Self::CommitToRef => repository
                    .commit_to_ref(
                        id,
                        expected,
                        Selection::All,
                        "source publication".into(),
                        author(),
                        RefExpectation {
                            name: RefName::new("main").unwrap(),
                            expected: Some(expected.head),
                        },
                        cancel,
                    )
                    .map(|_| ()),
                Self::Revise => repository
                    .revise(
                        id,
                        expected,
                        Selection::All,
                        "revised source".into(),
                        author(),
                        cancel,
                    )
                    .map(|_| ()),
                Self::Close => repository.close_workspace(id, expected, cancel).map(|_| ()),
            }
        }
    }

    struct Substitution {
        repository: Repository,
        id: WorkspaceId,
        root: PathBuf,
        retained: PathBuf,
        enabled: Arc<AtomicBool>,
        observed: Arc<AtomicUsize>,
        _fixture: TempDir,
    }

    fn substitution(action: Action, boundary: DurableBoundary, occurrence: usize) -> Substitution {
        let fixture = fixture();
        let primary = fixture.path().join("source-root");
        let root = if matches!(action, Action::Close) {
            fixture.path().join("private-root")
        } else {
            primary.clone()
        };
        let retained = fixture.path().join("retained-original-root");
        let hook_root = root.clone();
        let hook_retained = retained.clone();
        let enabled = Arc::new(AtomicBool::new(false));
        let armed = Arc::clone(&enabled);
        let observed = Arc::new(AtomicUsize::new(0));
        let written = Arc::clone(&observed);
        let mut options = RepositoryOptions::default();
        options.store.fault_hook = Some(FaultHook(Arc::new(move |at| {
            if armed.load(Ordering::SeqCst)
                && at == boundary
                && written.fetch_add(1, Ordering::SeqCst) == occurrence
            {
                fs::rename(&hook_root, &hook_retained)?;
                fs::create_dir(&hook_root)?;
                fs::write(
                    hook_root.join("replacement-only"),
                    b"independent replacement",
                )?;
            }
            Ok(())
        })));
        let repository = Repository::init(&primary, options).unwrap();
        let primary_id = repository.workspace_id();
        fs::write(primary.join("source"), b"original committed").unwrap();
        let original = commit(&repository, primary_id);
        if matches!(action, Action::CommitToRef) {
            let cancel = CancellationToken::new();
            let name = RefName::new("main").unwrap();
            repository
                .update_ref(
                    &name,
                    repository.view(&cancel).unwrap().refs.get(&name).copied(),
                    Some(original.revision),
                    &cancel,
                )
                .unwrap();
        }
        let id = if matches!(action, Action::Close) {
            repository
                .fork_workspace(
                    "private-fixture".into(),
                    &root,
                    original.revision,
                    &CancellationToken::new(),
                )
                .unwrap()
                .id
        } else {
            primary_id
        };
        fs::write(root.join("source"), b"original captured").unwrap();
        Substitution {
            repository,
            id,
            root,
            retained,
            enabled,
            observed,
            _fixture: fixture,
        }
    }

    #[test]
    fn source_capture_and_publication_reject_root_substitution_after_capture() {
        for action in [
            Action::Checkpoint,
            Action::Capture,
            Action::CaptureAll,
            Action::Status,
            Action::GuardCapture,
            Action::LeasedCapture,
            Action::Commit,
            Action::SelectiveCommit,
            Action::CommitToRef,
            Action::Revise,
            Action::Close,
        ] {
            // One selected Blob is written before the Tree. The replacement is
            // deliberately later than all per-file source metadata checks.
            let fixture = substitution(action, DurableBoundary::ObjectDataWritten, 1);
            let cancel = CancellationToken::new();
            let expected = fixture
                .repository
                .workspace(fixture.id, &cancel)
                .unwrap()
                .expected;
            let before = fixture.repository.current_operation(&cancel).unwrap();
            fixture.enabled.store(true, Ordering::SeqCst);
            let result = action.run(&fixture.repository, fixture.id, expected, &cancel);
            assert!(fixture.observed.load(Ordering::SeqCst) >= 2);
            let error =
                result.expect_err("a replaced source locator cannot receive a successful receipt");
            assert!(
                matches!(
                    error,
                    EngineError::SourceChanged(_) | EngineError::Io { .. }
                ),
                "{action:?}: {error:?}"
            );
            assert_eq!(
                fixture.repository.current_operation(&cancel).unwrap(),
                before,
                "{action:?}"
            );
            assert_eq!(
                fixture
                    .repository
                    .workspace(fixture.id, &cancel)
                    .unwrap()
                    .expected,
                expected
            );
            assert_eq!(
                fs::read(fixture.root.join("replacement-only")).unwrap(),
                b"independent replacement"
            );
            assert_eq!(
                fs::read(fixture.retained.join("source")).unwrap(),
                b"original captured"
            );
            assert!(fixture.retained.join(".izu").exists());
            assert!(!fixture.root.join(".izu").exists());
        }
    }

    #[test]
    fn visible_source_publication_reports_exact_operation_and_retained_context() {
        for (action, occurrence) in [
            (Action::Checkpoint, 0),
            (Action::Commit, 0),
            (Action::SelectiveCommit, 0),
            (Action::CommitToRef, 0),
            (Action::Revise, 0),
            (Action::Close, 0),
            (Action::Close, 1),
        ] {
            let fixture = substitution(action, DurableBoundary::HeadReplaced, occurrence);
            let cancel = CancellationToken::new();
            let expected = fixture
                .repository
                .workspace(fixture.id, &cancel)
                .unwrap()
                .expected;
            let before = fixture.repository.current_operation(&cancel).unwrap();
            fixture.enabled.store(true, Ordering::SeqCst);
            let error = action
                .run(&fixture.repository, fixture.id, expected, &cancel)
                .expect_err("a detached source after visible publication requires uncertainty");
            let visible = fixture.repository.current_operation(&cancel).unwrap();
            assert_ne!(visible, before);
            assert_eq!(
                error.uncertain_operation(),
                Some(visible),
                "{action:?}: {error:?}"
            );
            assert_eq!(error.code(), "publication_uncertain");
            let EngineError::PublicationUncertain { reason, .. } = error else {
                panic!("unexpected uncertainty type")
            };
            assert!(
                reason.contains("retained") && reason.contains(&fixture.root.display().to_string()),
                "{reason}"
            );
            let operation = fixture.repository.operation(visible, &cancel).unwrap();
            let record = if let Some(record) = operation.view.workspaces.get(&fixture.id) {
                record
            } else {
                assert!(matches!(action, Action::Close) && occurrence == 1);
                let saved = fixture
                    .repository
                    .operation(operation.parent.unwrap(), &cancel)
                    .unwrap();
                let tree = saved.view.workspaces[&fixture.id].sources["recovery"].tree;
                assert_eq!(
                    fixture
                        .repository
                        .tree(tree, &cancel)
                        .unwrap()
                        .entries
                        .len(),
                    1
                );
                assert!(reason.contains(&tree.to_string()), "{reason}");
                assert_eq!(
                    fs::read(fixture.retained.join("source")).unwrap(),
                    b"original captured"
                );
                continue;
            };
            let tree = if matches!(action, Action::Close) {
                record.sources["recovery"].tree
            } else {
                record.sources["working"].tree
            };
            assert!(reason.contains(&tree.to_string()), "{reason}");
            let captured = fixture.repository.tree(tree, &cancel).unwrap();
            let TreeEntry::File { blob, .. } = captured.entries[&path("source")] else {
                panic!("captured source missing")
            };
            assert_eq!(
                fixture.repository.blob(blob, &cancel).unwrap(),
                b"original captured"
            );
            assert_eq!(
                fs::read(fixture.root.join("replacement-only")).unwrap(),
                b"independent replacement"
            );
            assert_eq!(
                fs::read(fixture.retained.join("source")).unwrap(),
                b"original captured"
            );
        }
    }

    #[test]
    fn link_added_during_descriptor_capture_is_never_acknowledged() {
        let fixture = fixture();
        let root = fixture.path().join("source-root");
        let source = root.join("source");
        let linked_source = source.clone();
        let outside = fixture.path().join("outside-link");
        let linked_outside = outside.clone();
        let enabled = Arc::new(AtomicBool::new(false));
        let armed = Arc::clone(&enabled);
        let mut options = RepositoryOptions::default();
        options.store.fault_hook = Some(FaultHook(Arc::new(move |at| {
            if at == DurableBoundary::ObjectDataWritten && armed.swap(false, Ordering::SeqCst) {
                fs::hard_link(&linked_source, &linked_outside)?;
            }
            Ok(())
        })));
        let repository = Repository::init(&root, options).unwrap();
        let cancel = CancellationToken::new();
        let before = repository.current_operation(&cancel).unwrap();
        fs::write(&source, b"observed source").unwrap();
        enabled.store(true, Ordering::SeqCst);
        let error = repository
            .capture(repository.workspace_id(), Selection::All, &cancel)
            .unwrap_err();
        assert!(
            matches!(&error, EngineError::SourceChanged(entry) | EngineError::UnsupportedEntry(entry) if entry == &source),
            "{error:?}"
        );
        assert_eq!(fs::metadata(&source).unwrap().nlink(), 2);
        assert_eq!(fs::read(&outside).unwrap(), b"observed source");
        assert_eq!(repository.current_operation(&cancel).unwrap(), before);
    }

    #[test]
    fn source_publication_checks_repository_metadata_namespace_before_and_after_visibility() {
        for boundary in [
            DurableBoundary::ObjectDataWritten,
            DurableBoundary::HeadReplaced,
        ] {
            let fixture = fixture();
            let root = fixture.path().join("source-root");
            let metadata = root.join(".izu");
            let retained = fixture.path().join("retained-metadata");
            let displaced = metadata.clone();
            let displaced_retained = retained.clone();
            let enabled = Arc::new(AtomicBool::new(false));
            let armed = Arc::clone(&enabled);
            let observed = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&observed);
            let occurrence = usize::from(boundary == DurableBoundary::ObjectDataWritten);
            let mut options = RepositoryOptions::default();
            options.store.fault_hook = Some(FaultHook(Arc::new(move |at| {
                if armed.load(Ordering::SeqCst)
                    && at == boundary
                    && counter.fetch_add(1, Ordering::SeqCst) == occurrence
                {
                    fs::rename(&displaced, &displaced_retained)?;
                    fs::create_dir(&displaced)?;
                    fs::write(displaced.join("replacement-only"), b"unrelated metadata")?;
                }
                Ok(())
            })));
            let repository = Repository::init(&root, options).unwrap();
            let id = repository.workspace_id();
            let cancel = CancellationToken::new();
            fs::write(root.join("source"), b"original source").unwrap();
            let before = repository.current_operation(&cancel).unwrap();
            enabled.store(true, Ordering::SeqCst);
            let error = repository
                .checkpoint(
                    id,
                    repository.workspace(id, &cancel).unwrap().expected,
                    Selection::All,
                    &cancel,
                )
                .unwrap_err();
            let visible = repository.current_operation(&cancel).unwrap();
            if boundary == DurableBoundary::HeadReplaced {
                assert_ne!(visible, before);
                assert_eq!(error.uncertain_operation(), Some(visible));
                assert_eq!(error.code(), "publication_uncertain");
            } else {
                assert_eq!(visible, before);
                assert!(matches!(
                    error,
                    EngineError::SourceChanged(_) | EngineError::Io { .. }
                ));
            }
            assert_eq!(
                fs::read(metadata.join("replacement-only")).unwrap(),
                b"unrelated metadata"
            );
            assert_eq!(fs::read(root.join("source")).unwrap(), b"original source");
            assert!(retained.join("HEAD").is_file());
        }
    }

    #[test]
    fn private_source_publication_checks_workspace_marker_before_and_after_visibility() {
        for boundary in [
            DurableBoundary::ObjectDataWritten,
            DurableBoundary::HeadReplaced,
        ] {
            let fixture = fixture();
            let primary = fixture.path().join("primary-root");
            let root = fixture.path().join("private-root");
            let retained = fixture.path().join("retained-marker");
            let marker = root.join(".izu");
            let displaced = marker.clone();
            let displaced_retained = retained.clone();
            let enabled = Arc::new(AtomicBool::new(false));
            let armed = Arc::clone(&enabled);
            let observed = Arc::new(AtomicUsize::new(0));
            let counter = Arc::clone(&observed);
            let occurrence = usize::from(boundary == DurableBoundary::ObjectDataWritten);
            let mut options = RepositoryOptions::default();
            options.store.fault_hook = Some(FaultHook(Arc::new(move |at| {
                if armed.load(Ordering::SeqCst)
                    && at == boundary
                    && counter.fetch_add(1, Ordering::SeqCst) == occurrence
                {
                    fs::rename(&displaced, &displaced_retained)?;
                    fs::write(&displaced, b"unrelated marker")?;
                }
                Ok(())
            })));
            let repository = Repository::init(&primary, options).unwrap();
            let cancel = CancellationToken::new();
            let baseline = repository
                .workspace(repository.workspace_id(), &cancel)
                .unwrap();
            let private = repository
                .fork_workspace(
                    "private-marker".into(),
                    &root,
                    baseline.record.head,
                    &cancel,
                )
                .unwrap();
            fs::write(root.join("source"), b"private source").unwrap();
            let before = repository.current_operation(&cancel).unwrap();
            enabled.store(true, Ordering::SeqCst);
            let error = repository
                .checkpoint(private.id, private.expected, Selection::All, &cancel)
                .unwrap_err();
            let visible = repository.current_operation(&cancel).unwrap();
            if boundary == DurableBoundary::HeadReplaced {
                assert_ne!(visible, before);
                assert_eq!(error.uncertain_operation(), Some(visible));
                assert_eq!(error.code(), "publication_uncertain");
            } else {
                assert_eq!(visible, before);
                assert!(matches!(
                    error,
                    EngineError::InvalidMarker(_) | EngineError::SourceChanged(_)
                ));
            }
            assert_eq!(fs::read(&marker).unwrap(), b"unrelated marker");
            assert_eq!(fs::read(root.join("source")).unwrap(), b"private source");
            assert!(fs::read(retained).unwrap().starts_with(b"{"));
        }
    }
}
