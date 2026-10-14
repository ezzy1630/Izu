use izu_model::{
    CancellationToken, FileMode, ObjectId, ObjectKind, Operation, RepoPath, RepositoryView, Tree,
    TreeEntry, encode_metadata,
};
use izu_store::{ReclaimOutcome, Store, StoreError, StoreOptions};
use std::fs;
use std::path::{Path, PathBuf};

type TestResult = Result<(), Box<dyn std::error::Error>>;

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    store: Store,
}
fn fixture(options: &StoreOptions) -> Result<Fixture, Box<dyn std::error::Error>> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("store");
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
fn tree(store: &Store, blob: ObjectId) -> Result<ObjectId, Box<dyn std::error::Error>> {
    let mut tree = Tree::default();
    tree.entries.insert(
        RepoPath::new("file")?,
        TreeEntry::File {
            blob,
            mode: FileMode::Regular,
        },
    );
    let payload = encode_metadata(&tree, &store.options().limits)?;
    Ok(store.put(ObjectKind::Tree, &payload, &CancellationToken::new())?)
}

#[test]
fn exact_discard_preserves_every_unselected_unique_object() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let selected = f
        .store
        .put(ObjectKind::Blob, b"discard this exact identity", &cancel)?;
    let unique = f
        .store
        .put(ObjectKind::Blob, b"old unique unpublished work", &cancel)?;
    let plan = f.store.plan_reclamation(&[selected], &cancel)?;
    assert_eq!(plan.selected, vec![selected]);
    assert_eq!(plan.retained_objects, 1);
    assert!(matches!(
        f.store.reclaim_selected(&[selected], &cancel)?,
        ReclaimOutcome::Durable(_)
    ));
    assert!(!object_path(&f.path, selected).exists());
    assert_eq!(
        f.store.get(unique, ObjectKind::Blob, &cancel)?,
        b"old unique unpublished work"
    );
    assert_eq!(f.store.verify(&cancel)?.object_count, 1);
    Ok(())
}

#[test]
fn orphan_tree_is_a_retention_root_and_complete_selection_can_be_discarded() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let blob = f
        .store
        .put(ObjectKind::Blob, b"unpublished preparation", &cancel)?;
    let root = tree(&f.store, blob)?;
    assert!(f.store.reclaim_selected(&[blob], &cancel).is_err());
    assert!(object_path(&f.path, blob).exists());
    assert!(matches!(
        f.store.reclaim_selected(&[root, blob], &cancel)?,
        ReclaimOutcome::Durable(_)
    ));
    assert_eq!(f.store.verify(&cancel)?.object_count, 0);
    Ok(())
}

#[test]
fn all_operation_objects_are_retained_even_without_head() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let operation = Operation {
        parent: None,
        view: RepositoryView::default(),
        description: "unpublished recovery receipt".into(),
        created_at_unix_ms: 1,
    };
    let id = f.store.put(
        ObjectKind::Operation,
        &encode_metadata(&operation, &f.store.options().limits)?,
        &cancel,
    )?;
    assert!(f.store.reclaim_selected(&[id], &cancel).is_err());
    assert!(object_path(&f.path, id).exists());
    Ok(())
}

#[test]
fn corrupt_unselected_graph_and_unknown_requests_fail_before_discard() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let blob = f.store.put(ObjectKind::Blob, b"retain", &cancel)?;
    tree(&f.store, ObjectId::from_bytes([7; 32]))?;
    assert!(f.store.reclaim_selected(&[blob], &cancel).is_err());
    assert!(object_path(&f.path, blob).exists());
    assert!(
        f.store
            .plan_reclamation(&[ObjectId::from_bytes([8; 32])], &cancel)
            .is_err()
    );
    assert!(f.store.plan_reclamation(&[blob, blob], &cancel).is_err());
    Ok(())
}

#[test]
fn recovery_restores_interrupted_quarantine_and_retains_unknown_entries() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let blob = f
        .store
        .put(ObjectKind::Blob, b"interrupted unique work", &cancel)?;
    let quarantine = f.path.join("reclamation");
    fs::create_dir(&quarantine)?;
    fs::hard_link(
        object_path(&f.path, blob),
        quarantine.join(blob.to_string()),
    )?;
    fs::remove_file(object_path(&f.path, blob))?;
    fs::write(quarantine.join("operator-note"), b"preserve")?;
    let report = f.store.recover_reclamation(&cancel)?;
    assert_eq!(report.restored_objects, 1);
    assert_eq!(report.retained_unknown_entries, 1);
    assert_eq!(
        f.store.get(blob, ObjectKind::Blob, &cancel)?,
        b"interrupted unique work"
    );
    assert!(quarantine.join("operator-note").exists());
    assert!(!quarantine.join(blob.to_string()).exists());
    Ok(())
}

#[test]
fn corrupt_quarantine_is_retained_and_prevents_new_discard() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let blob = f.store.put(ObjectKind::Blob, b"still retained", &cancel)?;
    let quarantine = f.path.join("reclamation");
    fs::create_dir(&quarantine)?;
    let corrupt = quarantine.join(ObjectId::from_bytes([9; 32]).to_string());
    fs::write(&corrupt, b"invalid native frame")?;
    assert!(f.store.reclaim_selected(&[blob], &cancel).is_err());
    assert!(corrupt.exists());
    assert!(object_path(&f.path, blob).exists());
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[test]
fn failures_before_and_after_visibility_preserve_recovery_and_report_uncertainty() -> TestResult {
    use izu_store::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    for point in [
        DurableBoundary::ReclamationQuarantineSynced,
        DurableBoundary::ReclamationObjectRemoved,
        DurableBoundary::ReclamationObjectsSynced,
        DurableBoundary::ReclamationQuarantinePurged,
    ] {
        let options = StoreOptions {
            fault_hook: Some(FaultHook(Arc::new(move |boundary| {
                if boundary == point {
                    Err(std::io::Error::other("injected reclamation interruption"))
                } else {
                    Ok(())
                }
            }))),
            ..StoreOptions::default()
        };
        let f = fixture(&options)?;
        let cancel = CancellationToken::new();
        let blob = f
            .store
            .put(ObjectKind::Blob, b"exact deliberate discard", &cancel)?;
        let outcome = f.store.reclaim_selected(&[blob], &cancel);
        if point == DurableBoundary::ReclamationQuarantineSynced {
            assert!(outcome.is_err());
            assert!(object_path(&f.path, blob).exists());
        } else {
            assert!(
                matches!(outcome?, ReclaimOutcome::VisibleButUncertain { receipt, .. } if receipt.selected == vec![blob])
            );
        }
        let reopened = Store::open(&f.path, &StoreOptions::default())?;
        let report = reopened.recover_reclamation(&cancel)?;
        if point != DurableBoundary::ReclamationQuarantinePurged {
            assert!(object_path(&f.path, blob).exists());
            assert!(report.restored_objects <= 1);
        }
        reopened.verify(&cancel)?;
    }
    Ok(())
}

#[test]
fn cancelled_request_does_not_remove_objects() -> TestResult {
    let f = fixture(&StoreOptions::default())?;
    let cancel = CancellationToken::new();
    let blob = f
        .store
        .put(ObjectKind::Blob, b"retained on cancel", &cancel)?;
    cancel.cancel();
    assert!(matches!(
        f.store.reclaim_selected(&[blob], &cancel),
        Err(StoreError::Model(_))
    ));
    assert!(object_path(&f.path, blob).exists());
    Ok(())
}

#[test]
fn active_retention_lease_excludes_reclamation() -> TestResult {
    let options = StoreOptions {
        lock_timeout: std::time::Duration::from_millis(30),
        ..StoreOptions::default()
    };
    let f = fixture(&options)?;
    let cancel = CancellationToken::new();
    let blob = f
        .store
        .put(ObjectKind::Blob, b"active preparation", &cancel)?;
    let lease = f.store.retain(&cancel)?;
    assert!(matches!(
        f.store.reclaim_selected(&[blob], &cancel),
        Err(StoreError::LockTimeout("temporary.lock"))
    ));
    assert!(object_path(&f.path, blob).exists());
    drop(lease);
    assert!(matches!(
        f.store.reclaim_selected(&[blob], &cancel)?,
        ReclaimOutcome::Durable(_)
    ));
    Ok(())
}

#[cfg(feature = "fault-injection")]
#[test]
fn reclamation_crash_child() -> TestResult {
    use izu_store::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let Some(path) = std::env::var_os("IZU_RECLAMATION_CRASH_PATH") else {
        return Ok(());
    };
    let point = std::env::var("IZU_RECLAMATION_CRASH_POINT")?;
    let id: ObjectId = std::env::var("IZU_RECLAMATION_CRASH_ID")?.parse()?;
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            let matched = match boundary {
                DurableBoundary::ReclamationQuarantineSynced => point == "quarantine",
                DurableBoundary::ReclamationObjectRemoved => point == "unlink",
                DurableBoundary::ReclamationObjectsSynced => point == "persisted",
                DurableBoundary::ReclamationQuarantinePurged => point == "purged",
                _ => false,
            };
            if matched {
                std::process::exit(87);
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(Path::new(&path), &options)?;
    store.reclaim_selected(&[id], &CancellationToken::new())?;
    Err("crash hook was not reached".into())
}

#[cfg(feature = "fault-injection")]
#[test]
fn process_exit_releases_lease_and_restores_interrupted_selected_work() -> TestResult {
    for point in ["quarantine", "unlink", "persisted", "purged"] {
        let f = fixture(&StoreOptions::default())?;
        let cancel = CancellationToken::new();
        let blob = f
            .store
            .put(ObjectKind::Blob, b"selected interrupted process", &cancel)?;
        let unique = f
            .store
            .put(ObjectKind::Blob, b"unselected remains", &cancel)?;
        let child = std::process::Command::new(std::env::current_exe()?)
            .args(["--exact", "reclamation_crash_child", "--nocapture"])
            .env("IZU_RECLAMATION_CRASH_PATH", &f.path)
            .env("IZU_RECLAMATION_CRASH_POINT", point)
            .env("IZU_RECLAMATION_CRASH_ID", blob.to_string())
            .output()?;
        assert_eq!(
            child.status.code(),
            Some(87),
            "child output: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let reopened = Store::open(&f.path, &StoreOptions::default())?;
        reopened.recover_reclamation(&cancel)?;
        if point != "purged" {
            assert_eq!(
                reopened.get(blob, ObjectKind::Blob, &cancel)?,
                b"selected interrupted process"
            );
        }
        assert_eq!(
            reopened.get(unique, ObjectKind::Blob, &cancel)?,
            b"unselected remains"
        );
        reopened.verify(&cancel)?;
    }
    Ok(())
}
