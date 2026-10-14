use izu_model::*;
use std::collections::BTreeMap;

#[test]
fn version_one_canonical_bytes_and_ids_remain_unchanged() -> Result<(), ModelError> {
    let original = br#"{"candidates":[],"changes":{},"evidence":{},"refs":{},"workspaces":{}}"#;
    let limits = Limits::default();
    let legacy: RepositoryView = decode_metadata(original, &limits)?;
    assert_eq!(legacy.metadata_version, None);
    assert!(legacy.tags.is_empty());
    assert_eq!(encode_metadata(&legacy, &limits)?, original);
    assert_eq!(
        encode_metadata(&RepositoryView::default(), &limits)?,
        original
    );
    // Explicit empty/null extension fields are not another spelling of v1.
    for bytes in [
        br#"{"candidates":[],"changes":{},"evidence":{},"metadata_version":null,"refs":{},"workspaces":{}}"#.as_slice(),
        br#"{"candidates":[],"changes":{},"evidence":{},"refs":{},"tags":{},"workspaces":{}}"#.as_slice(),
    ] { assert!(decode_metadata::<RepositoryView>(bytes, &limits).is_err()); }
    Ok(())
}

#[test]
fn tags_require_explicit_supported_version_and_retain_all_edges() -> Result<(), ModelError> {
    let limits = Limits::default();
    let blob = ObjectId::from_bytes([1; 32]);
    let annotation = ObjectId::from_bytes([2; 32]);
    let tag = NativeTag {
        target: TagTarget::Blob { blob },
        origin: Some(GitTagOrigin {
            target_object_id: "a".repeat(40),
            annotations: vec![GitTagAnnotation {
                object_id: "b".repeat(40),
                raw: annotation,
            }],
        }),
    };
    let mut view = RepositoryView {
        tags: BTreeMap::from([(RefName::new("release/v1")?, tag)]),
        ..RepositoryView::default()
    };
    assert!(encode_metadata(&view, &limits).is_err());
    view.metadata_version = Some(MetadataVersion::V2);
    let bytes = encode_metadata(&view, &limits)?;
    assert_eq!(decode_metadata::<RepositoryView>(&bytes, &limits)?, view);
    assert!(
        decode_metadata::<RepositoryView>(
            &String::from_utf8_lossy(&bytes)
                .replace("v2", "v3")
                .into_bytes(),
            &limits
        )
        .is_err()
    );
    let mut edges = Vec::new();
    view.visit_references(&mut |edge| edges.push(edge));
    assert_eq!(
        edges,
        vec![
            ObjectReference {
                id: blob,
                kind: ObjectKind::Blob
            },
            ObjectReference {
                id: annotation,
                kind: ObjectKind::Blob
            }
        ]
    );
    Ok(())
}
