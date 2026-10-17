use izu_model::{CancellationToken, ObjectKind};
use izu_store::{Store, StoreOptions};
use std::{fs, io::Cursor};
type TestResult = Result<(), Box<dyn std::error::Error>>;
#[test]
fn reused_blob_is_byte_checked_and_changed_source_is_staged() -> TestResult {
    let d = tempfile::tempdir()?;
    let root = d.path().join("store");
    let c = CancellationToken::new();
    let store = Store::init(&root, &StoreOptions::default())?;
    let original = store.put(ObjectKind::Blob, b"original", &c)?;
    let mut batch = store.begin_object_batch(&c)?;
    let same = batch.stage_blob_reusing(&mut Cursor::new(b"original"), 8, original, &c)?;
    assert_eq!(same.object_id(), original);
    assert_eq!(fs::read_dir(root.join("tmp"))?.count(), 0);
    let changed = batch
        .stage_blob_reusing(&mut Cursor::new(b"modified"), 8, original, &c)?
        .object_id();
    assert_ne!(changed, original);
    assert_eq!(batch.finish(&c)?.object_count, 2);
    assert_eq!(store.get(changed, ObjectKind::Blob, &c)?, b"modified");
    assert_eq!(store.get(original, ObjectKind::Blob, &c)?, b"original");
    Ok(())
}
#[test]
fn reuse_rejects_corruption_and_poisons_the_batch() -> TestResult {
    let d = tempfile::tempdir()?;
    let root = d.path().join("store");
    let c = CancellationToken::new();
    let store = Store::init(&root, &StoreOptions::default())?;
    let id = store.put(ObjectKind::Blob, b"original", &c)?;
    let text = id.to_string();
    let path = root.join("objects").join(&text[..2]).join(&text[2..]);
    let mut bytes = fs::read(&path)?;
    if let Some(last) = bytes.last_mut() {
        *last ^= 1;
    }
    fs::write(&path, bytes)?;
    let mut batch = store.begin_object_batch(&c)?;
    assert!(
        batch
            .stage_blob_reusing(&mut Cursor::new(b"original"), 8, id, &c)
            .is_err()
    );
    assert!(batch.finish(&c).is_err());
    Ok(())
}
