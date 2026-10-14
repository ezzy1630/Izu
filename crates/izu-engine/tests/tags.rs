use izu_engine::*;
use izu_model::{ObjectKind, encode_metadata};
use izu_store::{HeadExpectation, Store};
use std::collections::BTreeMap;
use std::fs;

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;

#[test]
fn tag_leases_publish_atomically_and_protect_arbitrary_targets() -> TestResult {
    let fixture = tempfile::tempdir()?;
    let repository = Repository::init(fixture.path(), RepositoryOptions::default())?;
    let cancel = CancellationToken::new();
    let first = repository.put_blob(&mut &b"original"[..], 8, &cancel)?;
    let second = repository.put_blob(&mut &b"replacement"[..], 11, &cancel)?;
    let original = NativeTag {
        target: TagTarget::Blob { blob: first },
        origin: None,
    };
    let replacement = NativeTag {
        target: TagTarget::Blob { blob: second },
        origin: None,
    };
    let name = RefName::new("release/v1")?;
    repository.update_tag(
        TagUpdate {
            name: name.clone(),
            expected: None,
            new: Some(original.clone()),
        },
        &cancel,
    )?;
    assert!(repository.reclaim_selected(&[first], &cancel).is_err());
    let view = repository.view(&cancel)?;
    let main = RefName::new("main")?;
    let before = repository.current_operation(&cancel)?;
    assert!(
        repository
            .import_history(
                &[],
                &[RefUpdate {
                    name: main.clone(),
                    expected: view.refs.get(&main).copied(),
                    new: None
                }],
                &[TagUpdate {
                    name: name.clone(),
                    expected: None,
                    new: Some(replacement.clone())
                }],
                &cancel
            )
            .is_err()
    );
    assert_eq!(repository.current_operation(&cancel)?, before);
    assert_eq!(repository.view(&cancel)?.refs, view.refs);
    repository.update_tag(
        TagUpdate {
            name: name.clone(),
            expected: Some(original),
            new: Some(replacement.clone()),
        },
        &cancel,
    )?;
    assert_eq!(
        repository.view(&cancel)?.tags.get(&name),
        Some(&replacement)
    );
    repository.verify(&cancel)?;
    Ok(())
}

#[test]
fn explicit_migration_preserves_legacy_bytes_history_and_human_source() -> TestResult {
    let fixture = tempfile::tempdir()?;
    let repository = Repository::init(fixture.path(), RepositoryOptions::default())?;
    let cancel = CancellationToken::new();
    fs::write(fixture.path().join("human"), b"untouched")?;
    // Create a literal v1 HEAD through the original store publication API.
    let store = Store::open(repository.metadata_path(), &StoreOptions::default())?;
    let parent = repository.current_operation(&cancel)?;
    let mut operation = repository.operation(parent, &cancel)?;
    operation.parent = Some(parent);
    operation.view.metadata_version = None;
    operation.description = "legacy fixture".into();
    assert_eq!(operation.view.tags, BTreeMap::new());
    let bytes = encode_metadata(&operation, repository.limits())?;
    let legacy = OperationId::from_object(store.put(ObjectKind::Operation, &bytes, &cancel)?);
    let transaction = store.begin(HeadExpectation::At(parent), &cancel)?;
    assert!(matches!(
        transaction.publish(legacy, &cancel)?,
        izu_store::PublishOutcome::Durable(_)
    ));
    let tag = NativeTag {
        target: TagTarget::Revision {
            revision: *operation
                .view
                .refs
                .get(&RefName::new("main")?)
                .ok_or("no main")?,
        },
        origin: None,
    };
    assert!(
        repository
            .update_tag(
                TagUpdate {
                    name: RefName::new("release")?,
                    expected: None,
                    new: Some(tag)
                },
                &cancel
            )
            .is_err()
    );
    assert!(repository.migrate_metadata_v2(parent, &cancel).is_err());
    let migrated = repository.migrate_metadata_v2(legacy, &cancel)?;
    assert_eq!(
        repository.operation(migrated, &cancel)?.parent,
        Some(legacy)
    );
    assert_eq!(
        repository.view(&cancel)?.metadata_version,
        Some(MetadataVersion::V2)
    );
    assert_eq!(
        store.get(legacy.object_id(), ObjectKind::Operation, &cancel)?,
        bytes
    );
    assert_eq!(fs::read(fixture.path().join("human"))?, b"untouched");
    assert_eq!(repository.migrate_metadata_v2(migrated, &cancel)?, migrated);
    repository.verify(&cancel)?;
    Ok(())
}
