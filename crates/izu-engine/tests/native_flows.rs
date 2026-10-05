use izu_engine::*;
use izu_model::RevisionOrigin;
use std::collections::BTreeMap;
use std::fs;
use std::sync::{Arc, Barrier};
use std::time::Duration;
use tempfile::TempDir;

fn identity() -> Identity {
    Identity {
        name: "Fixture Author".into(),
        email: "fixture@example.invalid".into(),
    }
}
fn cancel() -> CancellationToken {
    CancellationToken::new()
}
fn test_tempdir() -> TempDir {
    let lane = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap();
    let root = lane.join(".artifacts/tmp");
    fs::create_dir_all(&root).unwrap();
    tempfile::tempdir_in(root).unwrap()
}
fn repo() -> (TempDir, Repository) {
    let directory = test_tempdir();
    let repository = Repository::init(directory.path(), RepositoryOptions::default()).unwrap();
    (directory, repository)
}
fn commit(repository: &Repository, workspace: WorkspaceId, message: &str) -> CommitReceipt {
    repository
        .commit(
            workspace,
            repository.workspace(workspace, &cancel()).unwrap().expected,
            Selection::All,
            message.into(),
            identity(),
            &cancel(),
        )
        .unwrap()
}
fn file(tree: &Tree, name: &str) -> ObjectId {
    match tree.entries.get(&RepoPath::new(name).unwrap()).unwrap() {
        TreeEntry::File { blob, .. } => *blob,
        other => panic!("not a file: {other:?}"),
    }
}

#[cfg(unix)]
#[test]
fn init_refuses_legacy_metadata_of_every_type_without_touching_source() {
    for name in [".ezy", ".ezy-recovery"] {
        for kind in ["directory", "file", "symlink"] {
            let fixture = test_tempdir();
            let root = fixture.path().join("repo");
            let outside = fixture.path().join("outside");
            fs::create_dir(&root).unwrap();
            fs::create_dir(&outside).unwrap();
            fs::write(root.join("source"), b"unique source").unwrap();
            fs::write(outside.join("unique"), b"unique external history").unwrap();
            let legacy = root.join(name);
            match kind {
                "directory" => {
                    fs::create_dir(&legacy).unwrap();
                    fs::write(legacy.join("unique"), b"unique legacy history").unwrap();
                }
                "file" => fs::write(&legacy, b"unique legacy marker").unwrap(),
                "symlink" => std::os::unix::fs::symlink(&outside, &legacy).unwrap(),
                _ => unreachable!(),
            }
            assert!(matches!(
                Repository::init(&root, RepositoryOptions::default()),
                Err(EngineError::AlreadyExists(actual)) if actual == legacy
            ));
            assert!(!root.join(".izu").exists());
            assert!(!root.join(".izu-recovery").exists());
            assert_eq!(fs::read(root.join("source")).unwrap(), b"unique source");
            assert_eq!(
                fs::read(outside.join("unique")).unwrap(),
                b"unique external history"
            );
            match kind {
                "directory" => assert_eq!(
                    fs::read(legacy.join("unique")).unwrap(),
                    b"unique legacy history"
                ),
                "file" => assert_eq!(fs::read(&legacy).unwrap(), b"unique legacy marker"),
                "symlink" => assert_eq!(fs::read_link(&legacy).unwrap(), outside),
                _ => unreachable!(),
            }
        }
    }
}

#[test]
fn init_accepts_existing_and_brand_new_roots_with_existing_parent() {
    for existing in [false, true] {
        let parent = test_tempdir();
        let root = parent.path().join("new-repository");
        fs::write(parent.path().join("unique"), b"unrelated parent source").unwrap();
        if existing {
            fs::create_dir(&root).unwrap();
            fs::write(root.join("source"), b"preexisting source").unwrap();
        }
        let repository = Repository::init(&root, RepositoryOptions::default()).unwrap();
        assert!(root.join(".izu").is_dir());
        assert_eq!(repository.root_path(), fs::canonicalize(&root).unwrap());
        let reopened = Repository::open(&root, RepositoryOptions::default()).unwrap();
        assert_eq!(
            repository.current_operation(&cancel()).unwrap(),
            reopened.current_operation(&cancel()).unwrap()
        );
        assert_eq!(
            fs::read(parent.path().join("unique")).unwrap(),
            b"unrelated parent source"
        );
        if existing {
            assert_eq!(
                fs::read(root.join("source")).unwrap(),
                b"preexisting source"
            );
        }
    }
}

#[test]
fn init_does_not_create_missing_ancestors_or_overwrite_existing_files() {
    let parent = test_tempdir();
    let missing = parent.path().join("missing");
    assert!(Repository::init(missing.join("repo"), RepositoryOptions::default()).is_err());
    assert!(!missing.exists());
    let file = parent.path().join("unknown-file");
    fs::write(&file, b"unique unknown source").unwrap();
    assert!(Repository::init(&file, RepositoryOptions::default()).is_err());
    assert_eq!(fs::read(&file).unwrap(), b"unique unknown source");
}

#[test]
fn native_init_checkpoint_commit_ref_and_reopen_are_exact() {
    let (directory, repository) = repo();
    assert!(!directory.path().join(".git").exists());
    let id = repository.workspace_id();
    let initial = repository.workspace(id, &cancel()).unwrap();
    assert!(matches!(
        repository
            .revision(initial.record.head, &cancel())
            .unwrap()
            .origin,
        Some(RevisionOrigin::Bootstrap)
    ));
    fs::write(directory.path().join("source"), b"first\n").unwrap();
    let snapshot = repository
        .checkpoint(id, initial.expected, Selection::All, &cancel())
        .unwrap();
    assert_eq!(snapshot.head, initial.record.head);
    assert_eq!(
        repository.workspace(id, &cancel()).unwrap().record.head,
        initial.record.head
    );
    assert_ne!(snapshot.tree, initial.expected.working_tree);
    let receipt = commit(&repository, id, "First intentional change");
    let revision = repository.revision(receipt.revision, &cancel()).unwrap();
    assert_eq!(revision.parents, vec![initial.record.head]);
    assert_eq!(revision.origin, None);
    assert_eq!(
        repository
            .log(receipt.revision, 10, &cancel())
            .unwrap()
            .len(),
        2
    );
    let main = RefName::new("main").unwrap();
    assert_eq!(
        repository.view(&cancel()).unwrap().refs[&main],
        initial.record.head
    );
    repository
        .update_ref(
            &main,
            Some(initial.record.head),
            Some(receipt.revision),
            &cancel(),
        )
        .unwrap();
    let reopened = Repository::open(directory.path(), RepositoryOptions::default()).unwrap();
    assert_eq!(reopened.workspace_id(), id);
    assert_eq!(
        reopened.view(&cancel()).unwrap().refs[&main],
        receipt.revision
    );
    let tree = reopened.tree(receipt.tree, &cancel()).unwrap();
    assert_eq!(
        reopened.blob(file(&tree, "source"), &cancel()).unwrap(),
        b"first\n"
    );
    assert!(reopened.status(id, &cancel()).unwrap().entries.is_empty());
    assert!(reopened.verify(&cancel()).unwrap().operations >= 4);
}

#[test]
fn selective_commit_preserves_unselected_dirty_and_untracked_source() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("a"), b"old a").unwrap();
    fs::write(directory.path().join("b"), b"old b").unwrap();
    commit(&repository, id, "base");
    fs::write(directory.path().join("a"), b"new a").unwrap();
    fs::write(directory.path().join("b"), b"new b").unwrap();
    fs::write(directory.path().join("untracked"), b"keep me").unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    repository
        .checkpoint(id, state.expected, Selection::All, &cancel())
        .unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    let receipt = repository
        .commit(
            id,
            state.expected,
            Selection::Paths(vec![RepoPath::new("a").unwrap()]),
            "only a".into(),
            identity(),
            &cancel(),
        )
        .unwrap();
    let tree = repository.tree(receipt.tree, &cancel()).unwrap();
    assert_eq!(
        repository.blob(file(&tree, "b"), &cancel()).unwrap(),
        b"old b"
    );
    assert!(
        !tree
            .entries
            .contains_key(&RepoPath::new("untracked").unwrap())
    );
    assert_eq!(fs::read(directory.path().join("b")).unwrap(), b"new b");
    assert_eq!(
        fs::read(directory.path().join("untracked")).unwrap(),
        b"keep me"
    );
    let dirty = repository.status(id, &cancel()).unwrap();
    assert!(dirty.entries.iter().any(|entry| entry.path.as_str() == "b"));
    assert!(
        dirty
            .entries
            .iter()
            .any(|entry| entry.path.as_str() == "untracked")
    );
}

#[test]
fn tracked_files_remain_tracked_when_newly_ignored() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("tracked"), b"old").unwrap();
    commit(&repository, id, "tracked");
    fs::write(directory.path().join(".gitignore"), b"tracked\nignored\n").unwrap();
    fs::write(directory.path().join("tracked"), b"new").unwrap();
    fs::write(directory.path().join("ignored"), b"private").unwrap();
    let receipt = commit(&repository, id, "ignore rule");
    let tree = repository.tree(receipt.tree, &cancel()).unwrap();
    assert_eq!(
        repository.blob(file(&tree, "tracked"), &cancel()).unwrap(),
        b"new"
    );
    assert!(
        !tree
            .entries
            .contains_key(&RepoPath::new("ignored").unwrap())
    );
}

#[test]
fn restore_recovery_does_not_promote_ignored_files_to_tracked() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join(".gitignore"), b".env\n").unwrap();
    fs::write(directory.path().join("source"), b"old").unwrap();
    let baseline = commit(&repository, id, "base");
    fs::write(directory.path().join(".env"), b"keep private bytes").unwrap();
    fs::write(directory.path().join("source"), b"dirty").unwrap();
    let restored = repository
        .restore(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            baseline.revision,
            Selection::All,
            &cancel(),
        )
        .unwrap();
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"old");
    assert_eq!(
        fs::read(directory.path().join(".env")).unwrap(),
        b"keep private bytes"
    );
    let recovery = repository
        .operation(restored.recovery_operation, &cancel())
        .unwrap();
    let saved = recovery.view.workspaces[&id].sources["recovery"].tree;
    assert!(
        repository
            .tree(saved, &cancel())
            .unwrap()
            .entries
            .contains_key(&RepoPath::new(".env").unwrap())
    );
    let next = commit(&repository, id, "ordinary next commit");
    assert!(
        !repository
            .tree(next.tree, &cancel())
            .unwrap()
            .entries
            .contains_key(&RepoPath::new(".env").unwrap())
    );
}

#[test]
fn selective_restore_keeps_other_dirty_paths_and_originals() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("a"), b"old a").unwrap();
    fs::write(directory.path().join("b"), b"old b").unwrap();
    let first = commit(&repository, id, "base");
    fs::write(directory.path().join("a"), b"new a").unwrap();
    fs::write(directory.path().join("b"), b"new b").unwrap();
    let restored = repository
        .restore(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            first.revision,
            Selection::Paths(vec![RepoPath::new("a").unwrap()]),
            &cancel(),
        )
        .unwrap();
    assert_eq!(fs::read(directory.path().join("a")).unwrap(), b"old a");
    assert_eq!(fs::read(directory.path().join("b")).unwrap(), b"new b");
    let backup = directory
        .path()
        .join(".izu-recovery")
        .join(restored.recovery_operation.to_string())
        .join("a");
    assert_eq!(fs::read(backup).unwrap(), b"new a");
}

#[test]
fn undo_rejects_unrelated_operation_and_intervening_workspace_change() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"A").unwrap();
    let first = commit(&repository, id, "A");
    let unrelated = repository
        .update_ref(
            &RefName::new("unrelated").unwrap(),
            None,
            Some(first.revision),
            &cancel(),
        )
        .unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    assert!(matches!(
        repository.undo(id, state.expected, &unrelated, Selection::All, &cancel()),
        Err(EngineError::InvalidInput(_))
    ));
    fs::write(directory.path().join("source"), b"B").unwrap();
    let second = commit(&repository, id, "B");
    let state = repository.workspace(id, &cancel()).unwrap();
    assert!(matches!(
        repository.undo(
            id,
            state.expected,
            &first.operation,
            Selection::All,
            &cancel()
        ),
        Err(EngineError::StaleWorkspace { .. })
    ));
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"B");
    repository
        .undo(
            id,
            state.expected,
            &second.operation,
            Selection::All,
            &cancel(),
        )
        .unwrap();
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"A");
    assert_eq!(
        repository.view(&cancel()).unwrap().refs[&RefName::new("unrelated").unwrap()],
        first.revision
    );
}

#[test]
fn undo_refuses_to_replace_new_selected_disk_edits() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"A").unwrap();
    commit(&repository, id, "A");
    fs::write(directory.path().join("source"), b"B").unwrap();
    let second = commit(&repository, id, "B");
    fs::write(directory.path().join("source"), b"new human edit").unwrap();
    assert!(matches!(
        repository.undo(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            &second.operation,
            Selection::All,
            &cancel()
        ),
        Err(EngineError::UncommittedSource { .. })
    ));
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"new human edit"
    );
}

#[test]
fn independent_engine_instances_do_not_lose_other_ref_updates() {
    let (directory, repository) = repo();
    let initial = repository
        .workspace(repository.workspace_id(), &cancel())
        .unwrap()
        .record
        .head;
    let path = directory.path().to_path_buf();
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for name in ["first", "second"] {
        let path = path.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            let repository = Repository::open(path, RepositoryOptions::default()).unwrap();
            barrier.wait();
            repository
                .update_ref(&RefName::new(name).unwrap(), None, Some(initial), &cancel())
                .unwrap()
        }));
    }
    barrier.wait();
    for worker in workers {
        worker.join().unwrap();
    }
    let view = repository.view(&cancel()).unwrap();
    assert_eq!(view.refs[&RefName::new("first").unwrap()], initial);
    assert_eq!(view.refs[&RefName::new("second").unwrap()], initial);
}

#[test]
fn stale_ref_update_is_explicit_and_never_overwrites() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let old = repository.workspace(id, &cancel()).unwrap().record.head;
    fs::write(directory.path().join("source"), b"new").unwrap();
    let new = commit(&repository, id, "new");
    let main = RefName::new("main").unwrap();
    repository
        .update_ref(&main, Some(old), Some(new.revision), &cancel())
        .unwrap();
    assert!(matches!(
        repository.update_ref(&main, Some(old), Some(old), &cancel()),
        Err(EngineError::StaleRef { .. })
    ));
    assert_eq!(
        repository.view(&cancel()).unwrap().refs[&main],
        new.revision
    );
}

#[test]
fn simultaneous_native_restores_serialize_and_keep_final_source_exact() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"A").unwrap();
    let first = commit(&repository, id, "A");
    fs::write(directory.path().join("source"), b"B").unwrap();
    commit(&repository, id, "B");
    let expected = repository.workspace(id, &cancel()).unwrap().expected;
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let path = directory.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            let repository = Repository::open(path, RepositoryOptions::default()).unwrap();
            barrier.wait();
            repository.restore(id, expected, first.revision, Selection::All, &cancel())
        }));
    }
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .any(|result| matches!(result, Err(EngineError::StaleWorkspace { .. }))),
        "{results:?}"
    );
    let state = repository.workspace(id, &cancel()).unwrap();
    assert_eq!(state.record.head, first.revision);
    assert_eq!(
        repository
            .capture(id, Selection::All, &cancel())
            .unwrap()
            .tree,
        state.expected.working_tree
    );
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"A");
}

#[test]
fn private_workspace_open_close_preserves_unique_and_ignored_work() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"base").unwrap();
    let first = commit(&repository, id, "base");
    let target = directory.path().join("private");
    let private = repository
        .fork_workspace("worker".into(), &target, first.revision, &cancel())
        .unwrap();
    let opened = Repository::open(&target, RepositoryOptions::default()).unwrap();
    assert_eq!(opened.workspace_id(), private.id);
    fs::write(target.join(".gitignore"), b".env\n").unwrap();
    fs::write(target.join(".env"), b"private").unwrap();
    fs::write(target.join("unique"), b"unique bytes").unwrap();
    repository
        .close_workspace(private.id, private.expected, &cancel())
        .unwrap();
    assert_eq!(fs::read(target.join("unique")).unwrap(), b"unique bytes");
    assert_eq!(fs::read(target.join(".env")).unwrap(), b"private");
    assert!(matches!(
        repository.workspace(private.id, &cancel()),
        Err(EngineError::UnknownWorkspace(_))
    ));
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"base");
}

#[test]
fn cancellation_and_source_limits_preserve_head() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let initial = repository.current_operation(&cancel()).unwrap();
    fs::write(directory.path().join("source"), b"bytes").unwrap();
    let token = cancel();
    token.cancel();
    assert!(matches!(
        repository.checkpoint(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            Selection::All,
            &token
        ),
        Err(EngineError::Cancelled)
    ));
    assert_eq!(repository.current_operation(&cancel()).unwrap(), initial);
    let mut options = RepositoryOptions::default();
    options.source_limits.max_blob_bytes = 2;
    let limited = Repository::open(directory.path(), options).unwrap();
    assert!(matches!(
        limited.status(id, &cancel()),
        Err(EngineError::Limit { .. })
    ));
    assert_eq!(limited.current_operation(&cancel()).unwrap(), initial);
}

#[test]
fn unknown_store_version_is_not_silently_reinitialized() {
    let (directory, repository) = repo();
    let head = repository.current_operation(&cancel()).unwrap();
    fs::write(directory.path().join(".izu/FORMAT"), b"IZU-STORE 999\n").unwrap();
    assert!(matches!(
        Repository::open(directory.path(), RepositoryOptions::default()),
        Err(EngineError::Store(
            izu_store::StoreError::UnsupportedVersion { .. }
        ))
    ));
    assert_eq!(repository.current_operation(&cancel()).unwrap(), head);
}

#[cfg(unix)]
#[test]
fn symlink_capture_never_reads_targets_and_restore_never_writes_through_parent_links() {
    use std::os::unix::fs::symlink;
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let outside = test_tempdir();
    fs::write(outside.path().join("secret"), b"outside remains").unwrap();
    symlink(outside.path().join("secret"), directory.path().join("link")).unwrap();
    fs::create_dir(directory.path().join("dir")).unwrap();
    fs::write(directory.path().join("dir/inside"), b"inside").unwrap();
    let first = commit(&repository, id, "links");
    assert!(matches!(
        repository.tree(first.tree, &cancel()).unwrap().entries[&RepoPath::new("link").unwrap()],
        TreeEntry::Symlink { .. }
    ));
    fs::remove_file(directory.path().join("dir/inside")).unwrap();
    fs::remove_dir(directory.path().join("dir")).unwrap();
    symlink(outside.path(), directory.path().join("dir")).unwrap();
    repository
        .restore(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            first.revision,
            Selection::All,
            &cancel(),
        )
        .unwrap();
    assert_eq!(
        fs::read(outside.path().join("secret")).unwrap(),
        b"outside remains"
    );
    assert!(!outside.path().join("inside").exists());
    assert_eq!(
        fs::read(directory.path().join("dir/inside")).unwrap(),
        b"inside"
    );
}

#[cfg(unix)]
#[test]
fn capture_preserves_observed_posix_permission_bits() {
    use std::os::unix::fs::PermissionsExt;
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let source = directory.path().join("source");
    fs::write(&source, b"permissions").unwrap();
    for requested in [0o640, 0o711, 0o4755, 0o2755, 0o1755] {
        fs::set_permissions(&source, fs::Permissions::from_mode(requested)).unwrap();
        // Some mounted filesystems strip privileged bits even when chmod
        // succeeds. Capture must preserve the mode the filesystem exposes.
        let observed = fs::metadata(&source).unwrap().permissions().mode() & 0o7777;
        let receipt = commit(&repository, id, "observed permissions");
        let tree = repository.tree(receipt.tree, &cancel()).unwrap();
        let TreeEntry::File { blob, mode } = tree.entries[&RepoPath::new("source").unwrap()] else {
            panic!("captured source is not a file");
        };
        assert_eq!(u32::from(mode.unix_permissions()), observed);
        assert_eq!(repository.blob(blob, &cancel()).unwrap(), b"permissions");
        assert_eq!(fs::read(&source).unwrap(), b"permissions");
    }
}

#[cfg(unix)]
#[test]
fn special_permission_bits_are_preserved_in_history_but_not_granted() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"special").unwrap();
    let first = commit(&repository, id, "special bits");
    let original = repository.tree(first.tree, &cancel()).unwrap();
    for permissions in [0o4755, 0o2755, 0o1755] {
        // Native history can arrive from a filesystem that supports bits the
        // destination does not. Exercise that boundary without relying on chmod.
        let mut tree = original.clone();
        tree.entries.insert(
            RepoPath::new("source").unwrap(),
            TreeEntry::File {
                blob: file(&original, "source"),
                mode: FileMode::from_unix_permissions(permissions).unwrap(),
            },
        );
        let tree_id = repository.put_tree(&tree, &cancel()).unwrap();
        let revision = repository
            .put_revision(
                &Revision {
                    tree: tree_id,
                    ..repository.revision(first.revision, &cancel()).unwrap()
                },
                &cancel(),
            )
            .unwrap();
        let head = repository
            .import_revisions(&[revision], &[], &cancel())
            .unwrap();
        let reopened = Repository::open(directory.path(), RepositoryOptions::default()).unwrap();
        assert_eq!(
            reopened.revision(revision, &cancel()).unwrap().tree,
            tree_id
        );
        assert_eq!(reopened.tree(tree_id, &cancel()).unwrap(), tree);
        let destination = directory.path().join(format!("special-{permissions:o}"));
        let result = reopened.fork_workspace(
            format!("special-{permissions:o}"),
            &destination,
            revision,
            &cancel(),
        );
        assert!(
            matches!(result, Err(EngineError::InvalidInput(_))),
            "{result:?}"
        );
        assert_eq!(reopened.current_operation(&cancel()).unwrap(), head);
        assert!(!destination.join("source").exists());
        assert!(!destination.join(".izu").exists());
        assert_eq!(
            fs::read(directory.path().join("source")).unwrap(),
            b"special"
        );
    }
}

#[test]
fn managed_writer_intent_blocks_restore_after_lease_owner_disappears() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"A").unwrap();
    let first = commit(&repository, id, "A");
    let lease = repository.lease_writer(id, &cancel()).unwrap();
    let intent = lease.intent().clone();
    // A child can still use native metadata/capture operations while the separate
    // materialization lease is held.
    commit(&repository, id, "child native commit");
    drop(lease);
    assert!(matches!(
        repository.restore(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            first.revision,
            Selection::All,
            &cancel()
        ),
        Err(EngineError::WorkspaceBusy { .. })
    ));
    assert_eq!(
        repository
            .writer_intent(id, &cancel())
            .unwrap()
            .unwrap()
            .token,
        intent.token
    );
    let acknowledgement = repository
        .acknowledge_writer_stopped_with_note(
            id,
            &intent.token,
            "Fixture owner stopped and reaped the selected writer group".into(),
            &cancel(),
        )
        .unwrap();
    assert!(
        repository
            .operation(acknowledgement, &cancel())
            .unwrap()
            .description
            .contains("Fixture owner stopped and reaped")
    );
    assert!(repository.writer_intent(id, &cancel()).unwrap().is_none());
    repository
        .restore(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            first.revision,
            Selection::All,
            &cancel(),
        )
        .unwrap();
}

#[test]
fn writer_finish_stopped_and_guard_exclusion_are_explicit() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let lease = repository.lease_writer(id, &cancel()).unwrap();
    let mut options = RepositoryOptions::default();
    options.store.lock_timeout = Duration::from_millis(10);
    let independent = Repository::open(directory.path(), options).unwrap();
    assert!(matches!(
        independent.lock_workspace(id, lease.state().expected, &cancel()),
        Err(EngineError::WorkspaceBusy { .. })
    ));
    lease.finish_stopped().unwrap();
    let guard = independent
        .lock_workspace(
            id,
            independent.workspace(id, &cancel()).unwrap().expected,
            &cancel(),
        )
        .unwrap();
    assert!(guard.is_primary());
    assert_eq!(
        guard.capture(Selection::All, &cancel()).unwrap().tree,
        guard.state().expected.working_tree
    );
}

#[test]
fn conflict_candidate_survives_restart_and_resolution_is_an_immutable_revision() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let main = RefName::new("main").unwrap();
    let initial = repository.view(&cancel()).unwrap().refs[&main];
    fs::write(directory.path().join("source"), b"base\n").unwrap();
    let base = commit(&repository, id, "base");
    repository
        .update_ref(&main, Some(initial), Some(base.revision), &cancel())
        .unwrap();
    let target = test_tempdir();
    let private = repository
        .fork_workspace("worker".into(), target.path(), base.revision, &cancel())
        .unwrap();
    fs::write(directory.path().join("source"), b"ours\n").unwrap();
    let ours = commit(&repository, id, "ours");
    repository
        .update_ref(&main, Some(base.revision), Some(ours.revision), &cancel())
        .unwrap();
    fs::write(target.path().join("source"), b"theirs\n").unwrap();
    let theirs = commit(&repository, private.id, "theirs");
    let result = repository
        .prepare_merge(
            main,
            Some(ours.revision),
            theirs.revision,
            Vec::new(),
            identity(),
            &cancel(),
        )
        .unwrap();
    let (candidate, revision, tree) = match result {
        MergePreparation::Conflicted {
            candidate,
            revision,
            tree,
            conflicts,
            ..
        } => {
            assert_eq!(conflicts.len(), 1);
            (candidate, revision, tree)
        }
        other => panic!("not conflicted: {other:?}"),
    };
    let reopened = Repository::open(directory.path(), RepositoryOptions::default()).unwrap();
    assert_eq!(
        reopened.candidate(candidate, &cancel()).unwrap().result,
        revision
    );
    let conflicted = reopened.tree(tree, &cancel()).unwrap();
    let path = RepoPath::new("source").unwrap();
    let theirs = match &conflicted.entries[&path] {
        TreeEntry::Conflict { theirs, .. } => theirs.clone(),
        other => panic!("not native conflict: {other:?}"),
    };
    assert!(reopened.land(candidate, &cancel()).is_err());
    let resolved = reopened
        .resolve_candidate(
            candidate,
            BTreeMap::from([(path, theirs)]),
            identity(),
            &cancel(),
        )
        .unwrap();
    let new_revision = match resolved {
        MergePreparation::Ready { revision, .. } => revision,
        other => panic!("not resolved: {other:?}"),
    };
    assert_ne!(revision, new_revision);
    assert_eq!(
        reopened.revision(revision, &cancel()).unwrap().change,
        reopened.revision(new_revision, &cancel()).unwrap().change
    );
    assert!(matches!(
        reopened.tree(tree, &cancel()).unwrap().entries[&RepoPath::new("source").unwrap()],
        TreeEntry::Conflict { .. }
    ));
}

#[test]
fn archive_adoption_preserves_operation_history_and_remaps_only_selected_source() {
    let (original, repository) = repo();
    let id = repository.workspace_id();
    fs::write(original.path().join("source"), b"archived bytes").unwrap();
    let first = commit(&repository, id, "archive source");
    let private_root = test_tempdir();
    repository
        .fork_workspace(
            "historical-worker".into(),
            private_root.path(),
            first.revision,
            &cancel(),
        )
        .unwrap();
    let archived_root = repository.current_operation(&cancel()).unwrap();
    let before_ops = repository.operations(100, &cancel()).unwrap().len();
    fs::write(
        original.path().join("source"),
        b"new human source is preserved",
    )
    .unwrap();
    fs::write(
        private_root.path().join("source"),
        b"private unique source is preserved",
    )
    .unwrap();
    let parent = test_tempdir();
    let stage = parent.path().join("stage");
    let final_root = parent.path().join("restored");
    fs::create_dir(&stage).unwrap();
    let mut restored =
        Repository::initialize_archive(&stage, RepositoryOptions::default()).unwrap();
    let store = izu_store::Store::open(
        repository.metadata_path(),
        &izu_store::StoreOptions::default(),
    )
    .unwrap();
    for object in store.list_objects(&cancel()).unwrap() {
        let bytes = repository
            .object(object.id, object.kind, &cancel())
            .unwrap();
        assert_eq!(
            restored.put_object(object.kind, &bytes, &cancel()).unwrap(),
            object.id
        );
    }
    let receipt = restored
        .adopt_archive(archived_root, id, &stage, &final_root, &cancel())
        .unwrap();
    assert_eq!(receipt.head, first.revision);
    let parent_path = fs::canonicalize(parent.path()).unwrap();
    let directory = izu_platform::Directory::open(&parent_path).unwrap();
    directory
        .rename_noreplace(
            std::ffi::OsStr::new("stage"),
            &directory,
            std::ffi::OsStr::new("restored"),
        )
        .unwrap();
    directory.sync().unwrap();
    let reopened = Repository::open(&final_root, RepositoryOptions::default()).unwrap();
    assert_eq!(reopened.workspace_id(), id);
    assert_eq!(
        reopened.operations(100, &cancel()).unwrap().len(),
        before_ops + 1
    );
    assert_eq!(reopened.view(&cancel()).unwrap().workspaces.len(), 1);
    assert_eq!(
        fs::read(final_root.join("source")).unwrap(),
        b"archived bytes"
    );
    assert_eq!(
        fs::read(original.path().join("source")).unwrap(),
        b"new human source is preserved"
    );
    assert_eq!(
        fs::read(private_root.path().join("source")).unwrap(),
        b"private unique source is preserved"
    );
    reopened.verify(&cancel()).unwrap();
}

#[test]
fn malformed_bootstrap_source_is_rejected_at_publication_and_read() {
    let (directory, repository) = repo();
    fs::write(directory.path().join("source"), b"not bootstrap source").unwrap();
    let source = commit(&repository, repository.workspace_id(), "source");
    let mut invalid = repository.revision(source.revision, &cancel()).unwrap();
    invalid.parents.clear();
    invalid.origin = Some(RevisionOrigin::Bootstrap);
    assert!(matches!(
        repository.put_revision(&invalid, &cancel()),
        Err(EngineError::InvalidInput(_))
    ));
    // A raw transport may import metadata before dependencies; the engine's
    // read and publication boundary still enforces cross-object semantics.
    let bytes = izu_model::encode_metadata(&invalid, repository.limits()).unwrap();
    let imported = repository
        .put_object(izu_model::ObjectKind::Revision, &bytes, &cancel())
        .unwrap();
    let revision = RevisionId::from_object(imported);
    assert!(matches!(
        repository.revision(revision, &cancel()),
        Err(EngineError::InvalidInput(_))
    ));
    assert!(
        repository
            .update_ref(
                &RefName::new("bad").unwrap(),
                None,
                Some(revision),
                &cancel()
            )
            .is_err()
    );
}

#[cfg(feature = "fault-injection")]
#[test]
fn restoration_uncertainty_identifies_actual_visible_final_operation() {
    use izu_store::{DurableBoundary, FaultHook};
    use std::sync::atomic::{AtomicUsize, Ordering};
    let directory = test_tempdir();
    let head_publications = Arc::new(AtomicUsize::new(0));
    let fail_at = Arc::new(AtomicUsize::new(usize::MAX));
    let counter = Arc::clone(&head_publications);
    let trigger = Arc::clone(&fail_at);
    let mut options = RepositoryOptions::default();
    options.store.fault_hook = Some(FaultHook(Arc::new(move |boundary| {
        if boundary == DurableBoundary::HeadReplaced {
            let number = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if number == trigger.load(Ordering::SeqCst) {
                return Err(std::io::Error::other(
                    "fixture final publication uncertainty",
                ));
            }
        }
        Ok(())
    })));
    let repository = Repository::init(directory.path(), options).unwrap();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"committed").unwrap();
    let first = commit(&repository, id, "source");
    fs::write(directory.path().join("source"), b"dirty saved bytes").unwrap();
    fail_at.store(
        head_publications.load(Ordering::SeqCst) + 2,
        Ordering::SeqCst,
    );
    let failure = repository
        .restore(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            first.revision,
            Selection::All,
            &cancel(),
        )
        .unwrap_err();
    let visible = repository.current_operation(&cancel()).unwrap();
    assert_eq!(failure.uncertain_operation(), Some(visible));
    assert_ne!(failure.recovery_operation(), Some(visible));
    assert!(matches!(
        failure,
        EngineError::RestorationUncertain {
            operation: Some(_),
            ..
        }
    ));
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"committed"
    );
    let independent = Repository::open(directory.path(), RepositoryOptions::default()).unwrap();
    assert_eq!(independent.current_operation(&cancel()).unwrap(), visible);
    independent.recover(&cancel()).unwrap();
}

fn check_candidate(repository: &Repository) -> CandidateId {
    let main = RefName::new("main").unwrap();
    let expected = repository.view(&cancel()).unwrap().refs[&main];
    let source = repository
        .workspace(repository.workspace_id(), &cancel())
        .unwrap()
        .record
        .head;
    match repository
        .prepare_merge(
            main,
            Some(expected),
            source,
            vec![CheckSpec {
                name: "unit".into(),
                argv: vec!["fixture-test".into()],
                environment: None,
            }],
            identity(),
            &cancel(),
        )
        .unwrap()
    {
        MergePreparation::Ready { candidate, .. } => candidate,
        other => panic!("unexpected conflict: {other:?}"),
    }
}

fn terminal(attempt: &CheckAttemptReceipt, outcome: CheckOutcome, elapsed: i64) -> CheckEvidence {
    let mut result = attempt.pending.clone();
    result.outcome = outcome;
    result.finished_at_unix_ms = Some(result.started_at_unix_ms + elapsed);
    result
}

#[test]
fn pending_check_invalidates_previous_pass_and_late_result_cannot_override() {
    let (directory, repository) = repo();
    fs::write(directory.path().join("source"), b"source").unwrap();
    commit(&repository, repository.workspace_id(), "source");
    let candidate = check_candidate(&repository);
    let first = repository
        .start_check(candidate, "unit", &cancel())
        .unwrap();
    assert!(matches!(
        repository.land(candidate, &cancel()),
        Err(EngineError::MissingCheck(_))
    ));
    let old = terminal(&first, CheckOutcome::Passed, 100_000);
    repository.record_evidence(&old, &cancel()).unwrap();
    let next = repository
        .start_check(candidate, "unit", &cancel())
        .unwrap();
    assert!(matches!(
        repository.land(candidate, &cancel()),
        Err(EngineError::MissingCheck(_))
    ));
    assert!(matches!(
        repository.record_evidence(&old, &cancel()),
        Err(EngineError::StaleCheckAttempt { .. })
    ));
    let passed = terminal(&next, CheckOutcome::Passed, 1);
    let evidence = repository.record_evidence(&passed, &cancel()).unwrap();
    assert_eq!(
        repository.record_evidence(&passed, &cancel()).unwrap(),
        evidence
    );
    let landed = repository.land(candidate, &cancel()).unwrap();
    assert_eq!(landed.evidence, vec![evidence]);
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"source"
    );
}

#[test]
fn latest_failed_check_blocks_old_pass_even_clock_moves_backwards() {
    let (_directory, repository) = repo();
    let candidate = check_candidate(&repository);
    let first = repository
        .start_check(candidate, "unit", &cancel())
        .unwrap();
    repository
        .record_evidence(&terminal(&first, CheckOutcome::Passed, 100_000), &cancel())
        .unwrap();
    let next = repository
        .start_check(candidate, "unit", &cancel())
        .unwrap();
    let failed = terminal(&next, CheckOutcome::Failed { exit_code: Some(1) }, 0);
    assert!(failed.finished_at_unix_ms.unwrap() < first.pending.started_at_unix_ms + 100_000);
    repository.record_evidence(&failed, &cancel()).unwrap();
    assert!(matches!(
        repository.land(candidate, &cancel()),
        Err(EngineError::MissingCheck(_))
    ));
    let retained = repository.operations(100, &cancel()).unwrap();
    assert!(retained.iter().any(|entry| {
        entry
            .operation
            .view
            .evidence
            .get(&candidate)
            .is_some_and(|records| records.contains(&first.evidence))
    }));
}

#[test]
fn concurrent_check_attempts_have_one_active_token_and_stale_completion_fails() {
    let (directory, repository) = repo();
    let candidate = check_candidate(&repository);
    let barrier = Arc::new(Barrier::new(3));
    let mut workers = Vec::new();
    for _ in 0..2 {
        let path = directory.path().to_path_buf();
        let barrier = Arc::clone(&barrier);
        workers.push(std::thread::spawn(move || {
            let repository = Repository::open(path, RepositoryOptions::default()).unwrap();
            barrier.wait();
            repository
                .start_check(candidate, "unit", &cancel())
                .unwrap()
        }));
    }
    barrier.wait();
    let attempts: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_ne!(attempts[0].attempt, attempts[1].attempt);
    let current = repository.view(&cancel()).unwrap().evidence[&candidate]
        .iter()
        .next()
        .copied()
        .unwrap();
    for attempt in attempts {
        let result =
            repository.record_evidence(&terminal(&attempt, CheckOutcome::Passed, 1), &cancel());
        if attempt.evidence == current {
            assert!(result.is_ok());
        } else {
            assert!(matches!(result, Err(EngineError::StaleCheckAttempt { .. })));
        }
    }
    repository.land(candidate, &cancel()).unwrap();
}

#[test]
fn evidence_without_active_attempt_or_with_other_inputs_cannot_pass() {
    let (_directory, repository) = repo();
    let candidate = check_candidate(&repository);
    let attempt = repository
        .start_check(candidate, "unit", &cancel())
        .unwrap();
    let mut bad = terminal(&attempt, CheckOutcome::Passed, 1);
    bad.argv = vec!["different".into()];
    assert!(matches!(
        repository.record_evidence(&bad, &cancel()),
        Err(EngineError::EvidenceMismatch(_))
    ));
    let mut bad = terminal(&attempt, CheckOutcome::Passed, 1);
    bad.started_at_unix_ms -= 1;
    assert!(repository.record_evidence(&bad, &cancel()).is_err());
    assert!(matches!(
        repository.land(candidate, &cancel()),
        Err(EngineError::MissingCheck(_))
    ));
}

#[test]
fn commit_to_ref_updates_head_and_main_atomically_and_private_fork_leaves_source() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let state = repository.workspace(id, &cancel()).unwrap();
    fs::write(directory.path().join("source"), b"committed").unwrap();
    let main = RefName::new("main").unwrap();
    let committed = repository
        .commit_to_ref(
            id,
            state.expected,
            Selection::All,
            "source".into(),
            identity(),
            RefExpectation {
                name: main.clone(),
                expected: Some(state.expected.head),
            },
            &cancel(),
        )
        .unwrap();
    let operation = repository
        .operation(committed.operation, &cancel())
        .unwrap();
    assert_eq!(operation.view.workspaces[&id].head, committed.revision);
    assert_eq!(operation.view.refs[&main], committed.revision);
    let private = repository
        .fork_managed_workspace("managed".into(), committed.revision, &cancel())
        .unwrap();
    assert!(
        std::path::Path::new(&private.record.root)
            .starts_with(repository.metadata_path().join("workspaces"))
    );
    assert_eq!(
        repository
            .capture(private.id, Selection::All, &cancel())
            .unwrap()
            .tree,
        committed.tree
    );
    assert!(repository.status(id, &cancel()).unwrap().entries.is_empty());
    let old_state = repository.workspace(id, &cancel()).unwrap();
    repository
        .update_ref(
            &main,
            Some(committed.revision),
            Some(state.expected.head),
            &cancel(),
        )
        .unwrap();
    fs::write(directory.path().join("source"), b"dirty remains").unwrap();
    let failed = repository.commit_to_ref(
        id,
        old_state.expected,
        Selection::All,
        "stale".into(),
        identity(),
        RefExpectation {
            name: main.clone(),
            expected: Some(committed.revision),
        },
        &cancel(),
    );
    assert!(matches!(failed, Err(EngineError::StaleRef { .. })));
    assert_eq!(
        repository.workspace(id, &cancel()).unwrap().record.head,
        committed.revision
    );
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"dirty remains"
    );
}

#[test]
fn selective_nested_commit_roundtrip_exact_tree() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::create_dir(directory.path().join("nested")).unwrap();
    fs::write(directory.path().join("nested/file"), b"content").unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    let committed = repository
        .commit(
            id,
            state.expected,
            Selection::Paths(vec![RepoPath::new("nested/file").unwrap()]),
            "nested file".into(),
            identity(),
            &cancel(),
        )
        .unwrap();
    let tree = repository.tree(committed.tree, &cancel()).unwrap();
    assert!(matches!(
        tree.entries[&RepoPath::new("nested").unwrap()],
        TreeEntry::Directory { .. }
    ));
    let target = test_tempdir();
    let private = repository
        .fork_workspace(
            "nested-worker".into(),
            target.path(),
            committed.revision,
            &cancel(),
        )
        .unwrap();
    assert_eq!(
        repository
            .capture(private.id, Selection::All, &cancel())
            .unwrap()
            .tree,
        committed.tree
    );
}

#[test]
fn implicit_parent_case_alias_rejected_before_restore() {
    let (_directory, repository) = repo();
    let blob = repository
        .put_blob(&mut b"x".as_slice(), 1, &cancel())
        .unwrap();
    let tree = Tree {
        entries: BTreeMap::from([
            (
                RepoPath::new("A/x").unwrap(),
                TreeEntry::File {
                    blob,
                    mode: FileMode::Regular,
                },
            ),
            (
                RepoPath::new("a/y").unwrap(),
                TreeEntry::File {
                    blob,
                    mode: FileMode::Regular,
                },
            ),
        ]),
    };
    assert!(repository.put_tree(&tree, &cancel()).is_err());
}

#[test]
fn oversized_restore_is_rejected_before_source_moves_or_temp_writes() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"old").unwrap();
    let original = commit(&repository, id, "original");
    let blob = repository
        .put_blob(&mut b"12345678".as_slice(), 8, &cancel())
        .unwrap();
    let tree = repository
        .put_tree(
            &Tree {
                entries: BTreeMap::from([(
                    RepoPath::new("source").unwrap(),
                    TreeEntry::File {
                        blob,
                        mode: FileMode::Regular,
                    },
                )]),
            },
            &cancel(),
        )
        .unwrap();
    let mut revision = repository.revision(original.revision, &cancel()).unwrap();
    revision.tree = tree;
    revision.parents = vec![original.revision];
    revision.origin = None;
    let oversized = repository.put_revision(&revision, &cancel()).unwrap();
    let mut options = RepositoryOptions::default();
    options.source_limits.max_blob_bytes = 4;
    let constrained = Repository::open(directory.path(), options).unwrap();
    let failed = constrained
        .restore(
            id,
            constrained.workspace(id, &cancel()).unwrap().expected,
            oversized,
            Selection::All,
            &cancel(),
        )
        .unwrap_err();
    assert!(matches!(failed, EngineError::RestorationFailed { .. }));
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"old");
    assert!(!directory.path().join(".izu-recovery").exists());
    assert_eq!(
        constrained.workspace(id, &cancel()).unwrap().record.head,
        original.revision
    );
}

#[test]
fn update_rejects_dirty_source_and_eligible_untracked_then_preserves_ignored() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"old").unwrap();
    fs::write(directory.path().join(".gitignore"), b"ignored\n").unwrap();
    let original = commit(&repository, id, "original");
    let target = test_tempdir();
    let private = repository
        .fork_workspace(
            "updated-source".into(),
            target.path(),
            original.revision,
            &cancel(),
        )
        .unwrap();
    fs::write(target.path().join("source"), b"new").unwrap();
    let newer = commit(&repository, private.id, "new source");
    let expected = repository.workspace(id, &cancel()).unwrap().expected;
    fs::write(directory.path().join("source"), b"dirty").unwrap();
    assert!(matches!(
        repository.update_workspace(id, expected, newer.revision, &cancel()),
        Err(EngineError::UncommittedSource { .. })
    ));
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"dirty");
    fs::write(directory.path().join("source"), b"old").unwrap();
    fs::write(directory.path().join("eligible"), b"unique").unwrap();
    assert!(matches!(
        repository.update_workspace(id, expected, newer.revision, &cancel()),
        Err(EngineError::UncommittedSource { .. })
    ));
    assert_eq!(
        fs::read(directory.path().join("eligible")).unwrap(),
        b"unique"
    );
    fs::remove_file(directory.path().join("eligible")).unwrap();
    fs::write(directory.path().join("ignored"), b"private state").unwrap();
    let updated = repository
        .update_workspace(id, expected, newer.revision, &cancel())
        .unwrap();
    assert_eq!(updated.head, newer.revision);
    assert_eq!(updated.tree, newer.tree);
    assert_eq!(fs::read(directory.path().join("source")).unwrap(), b"new");
    assert_eq!(
        fs::read(directory.path().join("ignored")).unwrap(),
        b"private state"
    );
    assert!(repository.status(id, &cancel()).unwrap().entries.is_empty());
}

#[test]
fn archive_pinned_stage_survives_locator_substitution_without_writing_replacement() {
    use izu_model::{MetadataObject, ObjectKind, ReferencedObjects};
    use std::collections::BTreeSet;
    let (original, repository) = repo();
    let id = repository.workspace_id();
    fs::write(original.path().join("source"), b"archive source").unwrap();
    commit(&repository, id, "archive");
    let head = repository.current_operation(&cancel()).unwrap();
    let parent = test_tempdir();
    let parent_path = fs::canonicalize(parent.path()).unwrap();
    let stage_path = parent_path.join("stage");
    fs::create_dir(&stage_path).unwrap();
    let stage = izu_platform::Directory::open(&stage_path).unwrap();
    fs::rename(&stage_path, parent_path.join("owned")).unwrap();
    fs::create_dir(&stage_path).unwrap();
    fs::write(stage_path.join("unique"), b"replacement source").unwrap();
    let mut restored =
        Repository::initialize_archive_at(&stage, &stage_path, RepositoryOptions::default())
            .unwrap();
    let mut pending = vec![izu_model::ObjectReference {
        id: head.object_id(),
        kind: ObjectKind::Operation,
    }];
    let mut seen = BTreeSet::new();
    while let Some(reference) = pending.pop() {
        if !seen.insert(reference.id) {
            continue;
        }
        let bytes = repository
            .object(reference.id, reference.kind, &cancel())
            .unwrap();
        if reference.kind != ObjectKind::Blob {
            let object: MetadataObject =
                izu_model::decode_object_metadata(reference.kind, &bytes, repository.limits())
                    .unwrap();
            object.visit_references(&mut |reference| pending.push(reference));
        }
        restored
            .import_object(
                reference.kind,
                reference.id,
                &mut bytes.as_slice(),
                bytes.len() as u64,
                &cancel(),
            )
            .unwrap();
    }
    let receipt = restored
        .adopt_archive_at(head, id, &stage, &parent_path.join("final"), &cancel())
        .unwrap();
    assert_eq!(receipt.workspace, id);
    assert_eq!(
        fs::read(parent_path.join("owned/source")).unwrap(),
        b"archive source"
    );
    assert_eq!(
        fs::read(stage_path.join("unique")).unwrap(),
        b"replacement source"
    );
    assert!(!stage_path.join(".izu").exists());
    assert!(!stage_path.join("source").exists());
}

#[test]
fn scoped_revert_preserves_later_independent_and_unselected_dirty_source() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("a"), b"original").unwrap();
    fs::write(directory.path().join("b"), b"base").unwrap();
    commit(&repository, id, "base");
    fs::write(directory.path().join("a"), b"change").unwrap();
    let changing = commit(&repository, id, "change a");
    fs::write(directory.path().join("b"), b"later").unwrap();
    let later = commit(&repository, id, "later b");
    fs::write(directory.path().join("b"), b"dirty unselected").unwrap();
    fs::write(directory.path().join("unique"), b"unknown stays").unwrap();
    let result = repository
        .revert(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            changing.revision,
            Selection::Paths(vec![RepoPath::new("a").unwrap()]),
            "revert a".into(),
            identity(),
            &cancel(),
        )
        .unwrap();
    let RevertPreparation::Applied {
        receipt,
        revision,
        tree,
    } = result
    else {
        panic!("expected clean revert")
    };
    assert_eq!(receipt.head, revision);
    assert_eq!(
        repository.revision(revision, &cancel()).unwrap().parents,
        vec![later.revision]
    );
    assert_eq!(
        repository
            .blob(
                file(&repository.tree(tree, &cancel()).unwrap(), "b"),
                &cancel()
            )
            .unwrap(),
        b"later"
    );
    assert_eq!(fs::read(directory.path().join("a")).unwrap(), b"original");
    assert_eq!(
        fs::read(directory.path().join("b")).unwrap(),
        b"dirty unselected"
    );
    assert_eq!(
        fs::read(directory.path().join("unique")).unwrap(),
        b"unknown stays"
    );
    assert!(
        repository
            .status(id, &cancel())
            .unwrap()
            .entries
            .iter()
            .any(|change| change.path.as_str() == "b")
    );
}

#[test]
fn conflicting_revert_survives_restart_and_requires_explicit_resolution() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"original\n").unwrap();
    commit(&repository, id, "base");
    fs::write(directory.path().join("source"), b"changed\n").unwrap();
    let changing = commit(&repository, id, "change");
    fs::write(directory.path().join("source"), b"later edit\n").unwrap();
    let later = commit(&repository, id, "later edit");
    let result = repository
        .revert(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            changing.revision,
            Selection::All,
            "revert change".into(),
            identity(),
            &cancel(),
        )
        .unwrap();
    let RevertPreparation::Conflicted {
        operation,
        revision,
        tree,
        conflicts,
    } = result
    else {
        panic!("expected durable conflict")
    };
    assert_eq!(conflicts.len(), 1);
    let reopened = Repository::open(directory.path(), RepositoryOptions::default()).unwrap();
    assert_eq!(reopened.current_operation(&cancel()).unwrap(), operation);
    assert_eq!(
        reopened.workspace(id, &cancel()).unwrap().record.head,
        later.revision
    );
    assert!(matches!(
        reopened.tree(tree, &cancel()).unwrap().entries[&RepoPath::new("source").unwrap()],
        TreeEntry::Conflict { .. }
    ));
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"later edit\n"
    );
    let resolved_blob = reopened
        .put_blob(&mut b"chosen resolution\n".as_slice(), 18, &cancel())
        .unwrap();
    let resolved = reopened
        .resolve_conflicts(
            revision,
            BTreeMap::from([(
                RepoPath::new("source").unwrap(),
                Some(ResolvedTreeEntry::File {
                    blob: resolved_blob,
                    mode: FileMode::Regular,
                }),
            )]),
            identity(),
            &cancel(),
        )
        .unwrap();
    assert_eq!(
        resolved.revision.change,
        reopened.revision(revision, &cancel()).unwrap().change
    );
    assert_ne!(resolved.id, revision);
    reopened
        .restore(
            id,
            reopened.workspace(id, &cancel()).unwrap().expected,
            resolved.id,
            Selection::All,
            &cancel(),
        )
        .unwrap();
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"chosen resolution\n"
    );
    reopened.verify(&cancel()).unwrap();
}

#[test]
fn unicode_filesystem_aliases_are_rejected_before_tree_publication() {
    let (_directory, repository) = repo();
    let blob = repository
        .put_blob(&mut b"x".as_slice(), 1, &cancel())
        .unwrap();
    for (first, second) in [
        ("σ", "ς"),
        ("Straße", "STRASSE"),
        ("ſ", "s"),
        ("é", "e\u{301}"),
    ] {
        let tree = Tree {
            entries: BTreeMap::from([
                (
                    RepoPath::new(first).unwrap(),
                    TreeEntry::File {
                        blob,
                        mode: FileMode::Regular,
                    },
                ),
                (
                    RepoPath::new(second).unwrap(),
                    TreeEntry::File {
                        blob,
                        mode: FileMode::Regular,
                    },
                ),
            ]),
        };
        assert!(
            repository.put_tree(&tree, &cancel()).is_err(),
            "unsafe aliases {first}/{second}"
        );
    }
}

#[test]
fn delete_directory_modify_child_merge_persists_path_conflict() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::create_dir(directory.path().join("nested")).unwrap();
    fs::write(directory.path().join("nested/source"), b"base").unwrap();
    let base = commit(&repository, id, "base");
    let main = RefName::new("main").unwrap();
    let initial = repository.view(&cancel()).unwrap().refs[&main];
    repository
        .update_ref(&main, Some(initial), Some(base.revision), &cancel())
        .unwrap();
    let private_root = test_tempdir();
    let private = repository
        .fork_workspace(
            "editing-child".into(),
            private_root.path(),
            base.revision,
            &cancel(),
        )
        .unwrap();
    fs::write(private_root.path().join("nested/source"), b"modified").unwrap();
    let theirs = commit(&repository, private.id, "modify child");
    fs::remove_file(directory.path().join("nested/source")).unwrap();
    fs::remove_dir(directory.path().join("nested")).unwrap();
    let deleted = commit(&repository, id, "delete directory");
    repository
        .update_ref(
            &main,
            Some(base.revision),
            Some(deleted.revision),
            &cancel(),
        )
        .unwrap();
    let merged = repository
        .prepare_merge(
            main,
            Some(deleted.revision),
            theirs.revision,
            vec![],
            identity(),
            &cancel(),
        )
        .unwrap();
    let MergePreparation::Conflicted { tree, .. } = merged else {
        panic!("expected path conflict")
    };
    let tree = repository.tree(tree, &cancel()).unwrap();
    assert!(matches!(
        tree.entries[&RepoPath::new("nested").unwrap()],
        TreeEntry::Conflict {
            reason: ConflictReason::Path,
            ..
        }
    ));
    assert!(matches!(
        tree.entries[&RepoPath::new("nested/source").unwrap()],
        TreeEntry::Conflict { .. }
    ));
}

#[test]
fn resolving_file_directory_conflict_requires_explicit_descendant_removal() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let initial = repository.workspace(id, &cancel()).unwrap().record.head;
    let private_root = test_tempdir();
    let private = repository
        .fork_workspace(
            "directory-side".into(),
            private_root.path(),
            initial,
            &cancel(),
        )
        .unwrap();
    fs::write(directory.path().join("x"), b"file alternative").unwrap();
    let ours = commit(&repository, id, "file side");
    fs::create_dir(private_root.path().join("x")).unwrap();
    fs::write(
        private_root.path().join("x/child"),
        b"directory alternative",
    )
    .unwrap();
    let theirs = commit(&repository, private.id, "directory side");
    let main = RefName::new("main").unwrap();
    repository
        .update_ref(&main, Some(initial), Some(ours.revision), &cancel())
        .unwrap();
    let MergePreparation::Conflicted { revision, tree, .. } = repository
        .prepare_merge(
            main,
            Some(ours.revision),
            theirs.revision,
            vec![],
            identity(),
            &cancel(),
        )
        .unwrap()
    else {
        panic!("expected type conflict")
    };
    let blob = file(&repository.tree(ours.tree, &cancel()).unwrap(), "x");
    let only_ancestor = BTreeMap::from([(
        RepoPath::new("x").unwrap(),
        Some(ResolvedTreeEntry::File {
            blob,
            mode: FileMode::Regular,
        }),
    )]);
    assert!(
        repository
            .resolve_conflicts(revision, only_ancestor, identity(), &cancel())
            .is_err()
    );
    let resolved = repository
        .resolve_conflicts(
            revision,
            BTreeMap::from([
                (
                    RepoPath::new("x").unwrap(),
                    Some(ResolvedTreeEntry::File {
                        blob,
                        mode: FileMode::Regular,
                    }),
                ),
                (RepoPath::new("x/child").unwrap(), None),
            ]),
            identity(),
            &cancel(),
        )
        .unwrap();
    let resolved_tree = repository.tree(resolved.revision.tree, &cancel()).unwrap();
    assert_eq!(resolved_tree.entries.len(), 1);
    assert_eq!(file(&resolved_tree, "x"), blob);
    assert!(matches!(
        repository.tree(tree, &cancel()).unwrap().entries[&RepoPath::new("x").unwrap()],
        TreeEntry::Conflict { .. }
    ));
    assert_eq!(
        fs::read(directory.path().join("x")).unwrap(),
        b"file alternative"
    );
    assert_eq!(
        fs::read(private_root.path().join("x/child")).unwrap(),
        b"directory alternative"
    );
}

#[test]
fn primary_revert_publishes_intentional_revision_and_expected_ref_together() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let main = RefName::new("main").unwrap();
    fs::write(directory.path().join("source"), b"original").unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    let original = repository
        .commit_to_ref(
            id,
            state.expected,
            Selection::All,
            "original".into(),
            identity(),
            RefExpectation {
                name: main.clone(),
                expected: Some(state.expected.head),
            },
            &cancel(),
        )
        .unwrap();
    fs::write(directory.path().join("source"), b"changed").unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    let changed = repository
        .commit_to_ref(
            id,
            state.expected,
            Selection::All,
            "change".into(),
            identity(),
            RefExpectation {
                name: main.clone(),
                expected: Some(original.revision),
            },
            &cancel(),
        )
        .unwrap();
    let result = repository
        .revert_to_ref(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            changed.revision,
            Selection::All,
            "revert change".into(),
            identity(),
            RefExpectation {
                name: main.clone(),
                expected: Some(changed.revision),
            },
            &cancel(),
        )
        .unwrap();
    let RevertPreparation::Applied {
        receipt, revision, ..
    } = result
    else {
        panic!("expected clean revert")
    };
    let view = repository
        .operation(receipt.operation, &cancel())
        .unwrap()
        .view;
    assert_eq!(view.refs[&main], revision);
    assert_eq!(view.workspaces[&id].head, revision);
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"original"
    );
    assert_eq!(
        repository.revision(revision, &cancel()).unwrap().parents,
        vec![changed.revision]
    );
}

#[test]
fn private_source_landing_never_materializes_human_work() {
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    let main = RefName::new("main").unwrap();
    fs::write(directory.path().join("source"), b"base").unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    let base = repository
        .commit_to_ref(
            id,
            state.expected,
            Selection::All,
            "base".into(),
            identity(),
            RefExpectation {
                name: main.clone(),
                expected: Some(state.expected.head),
            },
            &cancel(),
        )
        .unwrap();
    let private = repository
        .fork_managed_workspace("landing-worker".into(), base.revision, &cancel())
        .unwrap();
    fs::write(
        std::path::Path::new(&private.record.root).join("source"),
        b"worker result",
    )
    .unwrap();
    let worker = commit(&repository, private.id, "worker result");
    let MergePreparation::Ready {
        candidate,
        revision,
        ..
    } = repository
        .prepare_merge(
            main.clone(),
            Some(base.revision),
            worker.revision,
            vec![CheckSpec {
                name: "unit".into(),
                argv: vec!["fixture-test".into()],
                environment: None,
            }],
            identity(),
            &cancel(),
        )
        .unwrap()
    else {
        panic!("expected clean candidate")
    };
    fs::write(directory.path().join("source"), b"human dirty work").unwrap();
    assert!(matches!(
        repository.land(candidate, &cancel()),
        Err(EngineError::MissingCheck(_))
    ));
    let attempt = repository
        .start_check(candidate, "unit", &cancel())
        .unwrap();
    repository
        .record_evidence(&terminal(&attempt, CheckOutcome::Passed, 1), &cancel())
        .unwrap();
    let landed = repository.land(candidate, &cancel()).unwrap();
    assert_eq!(landed.revision, revision);
    assert_eq!(repository.view(&cancel()).unwrap().refs[&main], revision);
    assert_eq!(
        repository.workspace(id, &cancel()).unwrap().record.head,
        base.revision
    );
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"human dirty work"
    );
    assert!(matches!(
        repository.update_workspace(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            revision,
            &cancel()
        ),
        Err(EngineError::UncommittedSource { .. })
    ));
    assert!(matches!(
        repository.land(candidate, &cancel()),
        Err(EngineError::StaleRef { .. })
    ));
}

#[cfg(feature = "fault-injection")]
#[test]
fn disk_full_during_capture_preserves_head_and_unique_source() {
    use izu_store::{DurableBoundary, FaultHook};
    use std::sync::atomic::{AtomicBool, Ordering};
    let directory = test_tempdir();
    let enabled = Arc::new(AtomicBool::new(false));
    let injected = Arc::clone(&enabled);
    let mut options = RepositoryOptions::default();
    options.store.fault_hook = Some(FaultHook(Arc::new(move |boundary| {
        if boundary == DurableBoundary::ObjectDataWritten && injected.load(Ordering::SeqCst) {
            return Err(std::io::Error::from_raw_os_error(28));
        }
        Ok(())
    })));
    let repository = Repository::init(directory.path(), options).unwrap();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"base").unwrap();
    commit(&repository, id, "base");
    let head = repository.current_operation(&cancel()).unwrap();
    let state = repository.workspace(id, &cancel()).unwrap();
    fs::write(directory.path().join("source"), b"unique dirty source").unwrap();
    enabled.store(true, Ordering::SeqCst);
    assert!(
        repository
            .commit(
                id,
                state.expected,
                Selection::All,
                "cannot persist".into(),
                identity(),
                &cancel()
            )
            .is_err()
    );
    assert_eq!(repository.current_operation(&cancel()).unwrap(), head);
    assert_eq!(
        repository.workspace(id, &cancel()).unwrap().expected,
        state.expected
    );
    assert_eq!(
        fs::read(directory.path().join("source")).unwrap(),
        b"unique dirty source"
    );
    enabled.store(false, Ordering::SeqCst);
    repository.recover(&cancel()).unwrap();
}

#[test]
fn writer_lease_pins_actual_source_and_capture_does_not_reacquire_live_lock() {
    use std::io::Read;
    let (directory, repository) = repo();
    let id = repository.workspace_id();
    fs::write(directory.path().join("source"), b"actual source root").unwrap();
    let committed = commit(&repository, id, "source");
    let lease = repository
        .lease_workspace(
            id,
            repository.workspace(id, &cancel()).unwrap().expected,
            &cancel(),
        )
        .unwrap();
    let mut file = lease
        .source_directory()
        .open_read(std::ffi::OsStr::new("source"))
        .unwrap();
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).unwrap();
    assert_eq!(bytes, b"actual source root");
    let independent = Repository::open(directory.path(), RepositoryOptions::default()).unwrap();
    assert_eq!(
        independent
            .capture_leased(&lease, Selection::All, &cancel())
            .unwrap()
            .tree,
        committed.tree
    );
    lease.finish_stopped().unwrap();
}

#[test]
fn leased_capture_rejects_foreign_repository_and_stale_workspace() {
    let (first_root, first) = repo();
    let (second_root, second) = repo();
    let id = first.workspace_id();
    fs::write(first_root.path().join("source"), b"first private source").unwrap();
    commit(&first, id, "first source");
    fs::write(second_root.path().join("source"), b"second private source").unwrap();
    let lease = first.lease_writer(id, &cancel()).unwrap();
    let before = second.current_operation(&cancel()).unwrap();
    assert!(
        matches!(second.capture_leased(&lease, Selection::All, &cancel()), Err(EngineError::InvalidInput(message)) if message.contains("another repository"))
    );
    assert_eq!(second.current_operation(&cancel()).unwrap(), before);
    assert_eq!(
        fs::read(second_root.path().join("source")).unwrap(),
        b"second private source"
    );
    // Native child commits remain legal while the separate live lease is held.
    commit(&first, id, "child source revision");
    assert!(matches!(
        first.capture_leased(&lease, Selection::All, &cancel()),
        Err(EngineError::StaleWorkspace { .. })
    ));
    lease.finish_stopped().unwrap();
}

#[cfg(unix)]
#[test]
fn leased_capture_rejects_root_substitution_and_wrong_root_types() {
    use std::io::Read;
    use std::os::unix::fs::symlink;
    for kind in ["directory", "file", "symlink"] {
        let parent = test_tempdir();
        let root = parent.path().join("source-root");
        fs::create_dir(&root).unwrap();
        let repository = Repository::init(&root, RepositoryOptions::default()).unwrap();
        let id = repository.workspace_id();
        fs::write(root.join("source"), b"retained original").unwrap();
        commit(&repository, id, "original");
        let lease = repository.lease_writer(id, &cancel()).unwrap();
        let retained = parent.path().join("retained-root");
        fs::rename(&root, &retained).unwrap();
        match kind {
            "directory" => {
                fs::create_dir(&root).unwrap();
                fs::write(root.join("source"), b"replacement source").unwrap();
            }
            "file" => fs::write(&root, b"replacement file").unwrap(),
            "symlink" => symlink(&retained, &root).unwrap(),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                repository.capture_leased(&lease, Selection::All, &cancel()),
                Err(EngineError::SourceChanged(_) | EngineError::Io { .. })
            ),
            "accepted {kind} substitution"
        );
        let mut pinned = lease
            .source_directory()
            .open_read(std::ffi::OsStr::new("source"))
            .unwrap();
        let mut bytes = Vec::new();
        pinned.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"retained original");
        assert_eq!(
            fs::read(retained.join("source")).unwrap(),
            b"retained original"
        );
        assert!(repository.writer_intent(id, &cancel()).unwrap().is_some());
        lease.finish_stopped().unwrap();
        if kind == "directory" {
            assert_eq!(
                fs::read(root.join("source")).unwrap(),
                b"replacement source"
            );
        }
    }
}

#[cfg(all(feature = "fault-injection", unix))]
#[test]
fn mutation_during_capture_remains_source_changed_and_never_publishes() {
    use izu_store::{DurableBoundary, FaultHook};
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    let directory = test_tempdir();
    let source = directory.path().join("source");
    let edited = source.clone();
    let enabled = Arc::new(AtomicBool::new(false));
    let injected = Arc::clone(&enabled);
    let mut options = RepositoryOptions::default();
    options.store.fault_hook = Some(FaultHook(Arc::new(move |boundary| {
        if boundary == DurableBoundary::ObjectTempCreated && injected.swap(false, Ordering::SeqCst)
        {
            fs::write(&edited, b"second")?;
            fs::set_permissions(&edited, fs::Permissions::from_mode(0o600))?;
        }
        Ok(())
    })));
    let repository = Repository::init(directory.path(), options).unwrap();
    let head = repository.current_operation(&cancel()).unwrap();
    fs::write(&source, b"first0").unwrap();
    fs::set_permissions(&source, fs::Permissions::from_mode(0o640)).unwrap();
    enabled.store(true, Ordering::SeqCst);
    let error = repository
        .capture(repository.workspace_id(), Selection::All, &cancel())
        .unwrap_err();
    assert!(matches!(&error, EngineError::SourceChanged(path) if path == &source));
    assert_eq!(error.code(), "source_changed");
    assert_eq!(fs::read(&source).unwrap(), b"second");
    assert_eq!(repository.current_operation(&cancel()).unwrap(), head);
}
