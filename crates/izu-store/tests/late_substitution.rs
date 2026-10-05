#![cfg(feature = "fault-injection")]

use izu_model::{CancellationToken, ObjectKind};
use izu_store::{DurableBoundary, FaultHook, Store, StoreOptions};
use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
fn object_path(root: &Path, id: izu_model::ObjectId) -> PathBuf {
    let text = id.to_string();
    root.join("objects").join(&text[..2]).join(&text[2..])
}
#[test]
fn strict_put_refuses_finalized_object_substitution_before_ack()
-> Result<(), Box<dyn std::error::Error>> {
    let fixture = tempfile::Builder::new().prefix("strict-late-").tempdir()?;
    let dir = fixture.path().to_path_buf();
    let root = dir.join("store");
    let cancel = CancellationToken::new();
    drop(Store::init(&root, &StoreOptions::default())?);
    let expected = izu_model::hash_object(
        ObjectKind::Blob,
        b"strict original bytes",
        &StoreOptions::default().limits,
    )?;
    let published_path = object_path(&root, expected);
    let retained = dir.join("retained-original-frame");
    let substituted = Arc::new(AtomicBool::new(false));
    let s = substituted.clone();
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::ObjectDirectorySynced && !s.swap(true, Ordering::SeqCst)
            {
                let path = published_path.clone();
                let mut bytes = fs::read(&path)?;
                let last = bytes.len() - 1;
                bytes[last] ^= 1;
                fs::rename(&path, &retained)?;
                fs::write(&path, &bytes)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(&root, &options)?;
    let result = store.put(ObjectKind::Blob, b"strict original bytes", &cancel);
    assert!(substituted.load(Ordering::SeqCst));
    let expected = izu_model::hash_object(
        ObjectKind::Blob,
        b"strict original bytes",
        &StoreOptions::default().limits,
    )?;
    let readback = store.get(expected, ObjectKind::Blob, &cancel);
    println!(
        "strict_substitution_result={result:?}; readback={readback:?}; published_path={}; fixture={}",
        object_path(&root, expected).display(),
        dir.display()
    );
    assert!(
        readback.is_err(),
        "fixture did not substitute the prepared payload"
    );
    assert!(
        result.is_err(),
        "strict put acknowledged an object ID whose final entry names the substituted frame"
    );
    Ok(())
}

#[test]
fn visible_head_substitution_reports_uncertainty_before_ack()
-> Result<(), Box<dyn std::error::Error>> {
    use izu_model::{Operation, OperationId, RepositoryView, encode_metadata};
    use izu_store::{HeadExpectation, PublishOutcome};
    let fixture = tempfile::Builder::new().prefix("head-late-").tempdir()?;
    let dir = fixture.path().to_path_buf();
    let root = dir.join("store");
    let cancel = CancellationToken::new();
    let base = Store::init(&root, &StoreOptions::default())?;
    let encode_op = |parent, label: &str| Operation {
        parent,
        view: RepositoryView::default(),
        description: label.into(),
        created_at_unix_ms: 1,
    };
    let original = OperationId::from_object(base.put(
        ObjectKind::Operation,
        &encode_metadata(&encode_op(None, "baseline"), &base.options().limits)?,
        &cancel,
    )?);
    assert!(matches!(
        base.begin(HeadExpectation::Absent, &cancel)?
            .publish(original, &cancel)?,
        PublishOutcome::Durable(_)
    ));
    let original_head = fs::read(root.join("HEAD"))?;
    let proposed = OperationId::from_object(base.put(
        ObjectKind::Operation,
        &encode_metadata(
            &encode_op(Some(original), "proposal"),
            &base.options().limits,
        )?,
        &cancel,
    )?);
    drop(base);
    let head_path = root.join("HEAD");
    let retained = dir.join("retained-original-selector");
    let old_bytes = original_head.clone();
    let substituted = Arc::new(AtomicBool::new(false));
    let s = substituted.clone();
    let options = StoreOptions {
        fault_hook: Some(FaultHook(Arc::new(move |boundary| {
            if boundary == DurableBoundary::HeadDirectorySynced && !s.swap(true, Ordering::SeqCst) {
                let path = head_path.clone();
                fs::rename(&path, &retained)?;
                fs::write(&path, &old_bytes)?;
            }
            Ok(())
        }))),
        ..StoreOptions::default()
    };
    let store = Store::open(&root, &options)?;
    let result = store
        .begin(HeadExpectation::At(original), &cancel)?
        .publish(proposed, &cancel);
    let readback = store.current_head(&cancel)?;
    println!(
        "head_substitution_result={result:?}; proposed={proposed}; actual_head={readback:?}; fixture={}",
        dir.display()
    );
    assert!(substituted.load(Ordering::SeqCst));
    assert_eq!(
        readback,
        Some(original),
        "fixture must put the previous valid selector at the renamed temporary name"
    );
    assert!(
        matches!(result,Ok(PublishOutcome::VisibleButUncertain{receipt,..})if receipt.current==proposed&&receipt.previous==Some(original)),
        "postvisible HEAD substitution requires actual publication receipt with uncertainty"
    );
    Ok(())
}
