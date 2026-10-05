use izu_engine::{CancellationToken, Repository, RepositoryOptions, Selection};
use std::fs;
use std::path::Path;

#[test]
fn fork_receipt_retains_its_publication_after_another_controller_advances_history() {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
    fs::create_dir_all(&scratch).unwrap();
    let fixture = tempfile::tempdir_in(scratch).unwrap();
    let repository = Repository::init(fixture.path(), RepositoryOptions::default()).unwrap();
    let cancel = CancellationToken::new();
    let primary = repository
        .workspace(repository.workspace_id(), &cancel)
        .unwrap();
    let previous = repository.current_operation(&cancel).unwrap();
    let receipt = repository
        .fork_managed_workspace_receipt("receipt".into(), primary.expected.head, &cancel)
        .unwrap();
    assert_eq!(
        repository.current_operation(&cancel).unwrap(),
        receipt.operation
    );
    let created = repository.operation(receipt.operation, &cancel).unwrap();
    assert_eq!(created.parent, Some(previous));
    assert_eq!(
        created.view.workspaces[&receipt.workspace.id],
        receipt.workspace.record
    );
    assert!(Path::new(&receipt.workspace.record.root).is_dir());

    let other = Repository::open(fixture.path(), RepositoryOptions::default()).unwrap();
    fs::write(fixture.path().join("other-controller"), b"retained source").unwrap();
    let advanced = other
        .checkpoint(primary.id, primary.expected, Selection::All, &cancel)
        .unwrap();
    assert_ne!(advanced.operation, receipt.operation);
    assert_eq!(
        advanced.operation,
        repository.current_operation(&cancel).unwrap()
    );
    drop(other);
    drop(repository);

    let reopened = Repository::open(fixture.path(), RepositoryOptions::default()).unwrap();
    let fork = reopened.operation(receipt.operation, &cancel).unwrap();
    assert_eq!(fork.parent, Some(previous));
    assert_eq!(
        fork.view.workspaces[&receipt.workspace.id],
        receipt.workspace.record
    );
    assert_eq!(
        reopened
            .workspace(receipt.workspace.id, &cancel)
            .unwrap()
            .expected,
        receipt.workspace.expected
    );
    assert_eq!(
        fs::read(fixture.path().join("other-controller")).unwrap(),
        b"retained source"
    );
    reopened.verify(&cancel).unwrap();
}
