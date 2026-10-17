use izu_engine::*;
use std::fs;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
fn author() -> Identity {
    Identity {
        name: "Fixture".into(),
        email: "fixture@example.invalid".into(),
    }
}
fn commit(repo: &Repository, id: WorkspaceId, cancel: &CancellationToken) -> Result<CommitReceipt> {
    repo.commit(
        id,
        repo.workspace(id, cancel)?.expected,
        Selection::All,
        "edit".into(),
        author(),
        cancel,
    )
}
#[test]
fn sparse_commit_expand_restore_and_revision_queries_preserve_omissions() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("repo");
    fs::create_dir(&root)?;
    let repo = Repository::init(&root, RepositoryOptions::default())?;
    let cancel = CancellationToken::new();
    fs::create_dir(root.join("src"))?;
    fs::create_dir(root.join("docs"))?;
    fs::write(root.join("src/a"), b"original")?;
    fs::write(root.join("docs/b"), b"keep me")?;
    let original = commit(&repo, repo.workspace_id(), &cancel)?;
    assert!(
        repo.fork_sparse_workspace(
            "early".into(),
            dir.path().join("early"),
            original.revision,
            vec![RepoPath::new("src")?],
            &cancel
        )
        .is_err()
    );
    assert!(!dir.path().join("early").exists());
    repo.migrate_metadata(
        repo.current_operation(&cancel)?,
        MetadataVersion::V3,
        &cancel,
    )?;
    let sparse_root = dir.path().join("sparse");
    let sparse = repo.fork_sparse_workspace(
        "sparse".into(),
        &sparse_root,
        original.revision,
        vec![RepoPath::new("src")?],
        &cancel,
    )?;
    assert!(!sparse_root.join("docs").exists());
    assert!(repo.status(sparse.id, &cancel)?.entries.is_empty());
    fs::write(sparse_root.join("src/a"), b"changed")?;
    let changed = commit(&repo, sparse.id, &cancel)?;
    assert_eq!(
        repo.file_page(changed.revision, RepoPath::new("docs/b")?, 0, 64, &cancel)?
            .bytes,
        b"keep me"
    );
    let before_op = repo.current_operation(&cancel)?;
    let page = repo.changes_page(
        original.revision,
        changed.revision,
        Selection::Paths(vec![RepoPath::new("src")?]),
        None,
        1,
        &cancel,
    )?;
    assert_eq!(page.entries.len(), 1);
    assert_eq!(page.entries[0].path.as_str(), "src/a");
    assert_eq!(
        repo.merge_base(original.revision, changed.revision, &cancel)?,
        Some(original.revision)
    );
    let bytes = repo.file_page(changed.revision, RepoPath::new("src/a")?, 2, 3, &cancel)?;
    assert_eq!(bytes.bytes, b"ang");
    assert_eq!(bytes.next_offset, Some(5));
    assert_eq!(repo.current_operation(&cancel)?, before_op);
    assert!(
        repo.file_page(changed.revision, RepoPath::new("src/a")?, 100, 3, &cancel)
            .is_err()
    );
    // A clean sparse update must keep omitted paths off disk, including after a revert.
    fs::write(sparse_root.join("outside-scope"), b"unique physical source")?;
    repo.update_workspace(
        sparse.id,
        repo.workspace(sparse.id, &cancel)?.expected,
        original.revision,
        &cancel,
    )?;
    assert_eq!(fs::read(sparse_root.join("src/a"))?, b"original");
    assert!(!sparse_root.join("docs").exists());
    assert_eq!(
        fs::read(sparse_root.join("outside-scope"))?,
        b"unique physical source"
    );
    fs::write(sparse_root.join("src/a"), b"unfinished")?;
    let expanded = repo.expand_workspace(
        sparse.id,
        repo.workspace(sparse.id, &cancel)?.expected,
        Selection::All,
        &cancel,
    )?;
    assert!(expanded.workspace.record.sparse_paths.is_none());
    assert_eq!(fs::read(sparse_root.join("docs/b"))?, b"keep me");
    assert_eq!(fs::read(sparse_root.join("src/a"))?, b"unfinished");
    assert_eq!(
        fs::read(sparse_root.join("outside-scope"))?,
        b"unique physical source"
    );
    assert_eq!(fs::read(root.join("src/a"))?, b"original");
    Ok(())
}
#[test]
fn sparse_expansion_refuses_overwriting_unique_out_of_scope_source() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("repo");
    fs::create_dir(&root)?;
    let repo = Repository::init(&root, RepositoryOptions::default())?;
    let c = CancellationToken::new();
    fs::write(root.join("a"), b"a")?;
    fs::write(root.join("b"), b"b")?;
    let base = commit(&repo, repo.workspace_id(), &c)?;
    repo.migrate_metadata(repo.current_operation(&c)?, MetadataVersion::V3, &c)?;
    let path = dir.path().join("sparse");
    let w = repo.fork_sparse_workspace(
        "s".into(),
        &path,
        base.revision,
        vec![RepoPath::new("a")?],
        &c,
    )?;
    fs::write(path.join("b"), b"unique")?;
    assert!(matches!(
        repo.expand_workspace(w.id, w.expected, Selection::All, &c),
        Err(EngineError::UnselectedCollision(_))
    ));
    assert_eq!(fs::read(path.join("b"))?, b"unique");
    assert!(repo.workspace(w.id, &c)?.record.sparse_paths.is_some());
    Ok(())
}

#[test]
fn sparse_update_removing_selected_parent_retains_unselected_descendants() -> TestResult {
    let dir = tempfile::tempdir()?;
    let root = dir.path().join("repo");
    fs::create_dir(&root)?;
    let repo = Repository::init(&root, RepositoryOptions::default())?;
    let c = CancellationToken::new();
    fs::create_dir(root.join("src"))?;
    fs::write(root.join("src/a"), b"a")?;
    let base = commit(&repo, repo.workspace_id(), &c)?;
    repo.migrate_metadata(repo.current_operation(&c)?, MetadataVersion::V3, &c)?;
    let path = dir.path().join("sparse");
    let w = repo.fork_sparse_workspace(
        "s".into(),
        &path,
        base.revision,
        vec![RepoPath::new("src/a")?],
        &c,
    )?;
    fs::write(path.join("src/unique"), b"retain")?;
    fs::remove_file(root.join("src/a"))?;
    fs::remove_dir(root.join("src"))?;
    let removed = commit(&repo, repo.workspace_id(), &c)?;
    repo.update_workspace(w.id, w.expected, removed.revision, &c)?;
    assert!(!path.join("src/a").exists());
    assert_eq!(fs::read(path.join("src/unique"))?, b"retain");
    assert!(repo.status(w.id, &c)?.entries.is_empty());
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[test]
fn expansion_reports_visible_uncertainty_and_retains_recovery_source() -> TestResult {
    use izu_store::{DurableBoundary, FaultHook};
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let dir = tempfile::tempdir()?;
    let count = Arc::new(AtomicUsize::new(0));
    let fail_at = Arc::new(AtomicUsize::new(usize::MAX));
    let counter = count.clone();
    let trigger = fail_at.clone();
    let mut options = RepositoryOptions::default();
    options.store.fault_hook = Some(FaultHook(Arc::new(move |boundary| {
        if boundary == DurableBoundary::HeadReplaced
            && counter.fetch_add(1, Ordering::SeqCst) + 1 == trigger.load(Ordering::SeqCst)
        {
            return Err(std::io::Error::other("expansion publication uncertainty"));
        }
        Ok(())
    })));
    let root = dir.path().join("repo");
    fs::create_dir(&root)?;
    let repo = Repository::init(&root, options)?;
    let c = CancellationToken::new();
    fs::write(root.join("a"), b"a")?;
    fs::write(root.join("b"), b"b")?;
    let base = commit(&repo, repo.workspace_id(), &c)?;
    repo.migrate_metadata(repo.current_operation(&c)?, MetadataVersion::V3, &c)?;
    let sparse_root = dir.path().join("sparse");
    let w = repo.fork_sparse_workspace(
        "s".into(),
        &sparse_root,
        base.revision,
        vec![RepoPath::new("a")?],
        &c,
    )?;
    fs::write(sparse_root.join("a"), b"unfinished")?;
    fail_at.store(count.load(Ordering::SeqCst) + 2, Ordering::SeqCst);
    let error = repo
        .expand_workspace(w.id, w.expected, Selection::All, &c)
        .unwrap_err();
    assert!(matches!(
        error,
        EngineError::RestorationUncertain {
            operation: Some(_),
            ..
        }
    ));
    assert_eq!(
        error.uncertain_operation(),
        Some(repo.current_operation(&c)?)
    );
    let recovery = repo.operation(error.recovery_operation().ok_or("missing recovery")?, &c)?;
    let tree = repo.tree(recovery.view.workspaces[&w.id].sources["recovery"].tree, &c)?;
    assert!(tree.entries.contains_key(&RepoPath::new("a")?));
    assert!(!tree.entries.contains_key(&RepoPath::new("b")?));
    assert_eq!(fs::read(sparse_root.join("a"))?, b"unfinished");
    assert_eq!(fs::read(sparse_root.join("b"))?, b"b");
    let reopened = Repository::open(&sparse_root, RepositoryOptions::default())?;
    assert!(reopened.workspace(w.id, &c)?.record.sparse_paths.is_none());
    reopened.recover(&c)?;
    Ok(())
}
