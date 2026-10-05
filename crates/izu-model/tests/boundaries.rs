use std::collections::{BTreeMap, BTreeSet};

use izu_model::*;
use serde::{Deserialize, Serialize};

fn object(byte: u8) -> ObjectId {
    ObjectId::from_bytes([byte; 32])
}
fn revision(byte: u8) -> RevisionId {
    RevisionId::from_object(object(byte))
}

#[test]
fn identifiers_reject_short_uppercase_and_non_ascii() {
    for text in [
        "",
        "0",
        &"0".repeat(63),
        &"0".repeat(65),
        &"A".repeat(64),
        &"é".repeat(32),
        &"g".repeat(64),
    ] {
        assert!(text.parse::<ObjectId>().is_err(), "accepted {text:?}");
    }
    let id = object(0xab);
    assert_eq!(id.to_string(), "ab".repeat(32));
    assert_eq!(id.to_string().parse::<ObjectId>().unwrap(), id);
    assert_eq!(
        ChangeId::from_bytes([1; 16])
            .to_string()
            .parse::<ChangeId>()
            .unwrap(),
        ChangeId::from_bytes([1; 16])
    );
    assert!(id.to_string().parse::<ChangeId>().is_err());
    assert_eq!(RevisionId::from_object(id).object_id(), id);
}

#[test]
fn serde_identifiers_validate_owned_borrowed_and_escaped_strings() {
    let id = object(0x22);
    let encoded = serde_json::to_string(&id).unwrap();
    assert_eq!(serde_json::from_str::<ObjectId>(&encoded).unwrap(), id);
    assert_eq!(
        serde_json::from_value::<ObjectId>(serde_json::json!(id.to_string())).unwrap(),
        id
    );
    let escaped = format!("\"\\u0032{}\"", &id.to_string()[1..]);
    assert_eq!(serde_json::from_str::<ObjectId>(&escaped).unwrap(), id);
    assert!(serde_json::from_str::<ObjectId>("\"BAD\"").is_err());
    assert!(serde_json::from_str::<ObjectId>("123").is_err());
}

#[test]
fn check_attempt_identity_is_validated_as_a_distinct_128_bit_token() {
    let attempt = CheckAttemptId::from_bytes([0xab; 16]);
    assert_eq!(attempt.to_string(), "ab".repeat(16));
    assert_eq!(
        attempt.to_string().parse::<CheckAttemptId>().unwrap(),
        attempt
    );
    assert_eq!(
        serde_json::from_value::<CheckAttemptId>(serde_json::json!(attempt.to_string())).unwrap(),
        attempt
    );
    for invalid in ["", "0", &"A".repeat(32), &"g".repeat(32), &"0".repeat(64)] {
        assert!(invalid.parse::<CheckAttemptId>().is_err());
    }
}

#[test]
fn paths_are_exact_posix_utf8_without_normalization() {
    for text in [
        "", "/root", "a/", "a//b", ".", "..", "a/./b", "a/../b", "a\0b",
    ] {
        assert!(RepoPath::new(text).is_err(), "accepted {text:?}");
    }
    for text in [
        "path with space",
        "snow/雪",
        "a\\b",
        "a\nb",
        "é",
        "e\u{301}",
    ] {
        assert_eq!(RepoPath::new(text).unwrap().as_str(), text);
    }
    assert_ne!(
        RepoPath::new("é").unwrap(),
        RepoPath::new("e\u{301}").unwrap()
    );
    assert!(RepoPath::new("a".repeat(4097)).is_err());
}

#[test]
fn repository_metadata_paths_cannot_be_decoded_for_materialization() {
    for path in [
        ".git",
        ".git/config",
        ".GIT/config",
        ".Git/config",
        "sub/.gIt/config",
        ".izu/HEAD",
        "sub/.IZU/HEAD",
        ".izu-recovery/record",
        "sub/.IZU-RECOVERY/record",
        ".ezy/HEAD",
        "sub/.EZY/HEAD",
        ".ezy-recovery/record",
        "sub/.EZY-RECOVERY/record",
    ] {
        assert!(
            RepoPath::new(path).is_err(),
            "accepted metadata path {path}"
        );
        let raw = serde_json::to_string(path).unwrap();
        assert!(serde_json::from_str::<RepoPath>(&raw).is_err());
    }
    assert!(RepoPath::new(".gitignore").is_ok());
    assert!(RepoPath::new("izu/config").is_ok());
    assert!(RepoPath::new(".izu-notes").is_ok());
    assert!(RepoPath::new(".ezy-notes").is_ok());
}

#[test]
fn reference_names_are_bounded_and_portable() {
    for name in [
        "",
        "/main",
        "main/",
        "a//b",
        ".",
        "..",
        "name with space",
        "a\\b",
        "a\0b",
        "雪",
    ] {
        assert!(RefName::new(name).is_err(), "accepted {name:?}");
    }
    for name in ["main", "release/v1.2", "0-_.", "Case_Matters"] {
        assert_eq!(RefName::new(name).unwrap().as_str(), name);
    }
    assert!(RefName::new("x".repeat(256)).is_err());
}

#[test]
fn symlinks_retain_non_utf8_and_are_never_resolved() {
    let bytes = vec![b'.', b'.', b'/', 0xff, b'/', b'x'];
    let target = SymlinkTarget::new(bytes.clone()).unwrap();
    assert_eq!(serde_json::to_string(&target).unwrap(), "\"2e2e2fff2f78\"");
    assert_eq!(
        serde_json::from_value::<SymlinkTarget>(serde_json::json!("2e2e2fff2f78"))
            .unwrap()
            .as_bytes(),
        bytes
    );
    for text in ["\"\"", "\"00\"", "\"0\"", "\"FF\"", "\"gg\""] {
        assert!(serde_json::from_str::<SymlinkTarget>(text).is_err());
    }
    assert!(SymlinkTarget::new(vec![1; 4097]).is_err());
    assert!(SymlinkTarget::new(b"/absolute/../unresolved".to_vec()).is_ok());
}

#[test]
fn native_permissions_preserve_all_posix_bits_with_unique_forms() {
    for permissions in 0..=0o7777 {
        let mode = FileMode::from_unix_permissions(permissions).unwrap();
        assert_eq!(u32::from(mode.unix_permissions()), permissions);
        assert_eq!(mode.is_executable(), permissions & 0o111 != 0);
        let encoded = serde_json::to_vec(&mode).unwrap();
        assert_eq!(serde_json::from_slice::<FileMode>(&encoded).unwrap(), mode);
    }
    assert_eq!(
        FileMode::from_unix_permissions(0o644).unwrap(),
        FileMode::Regular
    );
    assert_eq!(
        FileMode::from_unix_permissions(0o755).unwrap(),
        FileMode::Executable
    );
    assert!(FileMode::from_unix_permissions(0o10000).is_err());
    assert!(serde_json::from_str::<FileMode>(r#"{"kind":"unix","permissions":420}"#).is_err());
    assert!(serde_json::from_str::<FileMode>(r#"{"kind":"unix","permissions":65535}"#).is_err());
}

#[test]
fn canonical_tree_is_deterministic_and_has_a_golden_empty_payload() {
    let limits = Limits::default();
    assert_eq!(
        encode_metadata(&Tree::default(), &limits).unwrap(),
        br#"{"entries":{}}"#
    );
    let a = RepoPath::new("a").unwrap();
    let z = RepoPath::new("z").unwrap();
    let entry = TreeEntry::File {
        blob: object(1),
        mode: FileMode::Regular,
    };
    let forward = Tree {
        entries: BTreeMap::from([(a.clone(), entry.clone()), (z.clone(), entry.clone())]),
    };
    let reverse = Tree {
        entries: [(z, entry.clone()), (a, entry)].into_iter().collect(),
    };
    assert_eq!(
        encode_metadata(&forward, &limits).unwrap(),
        encode_metadata(&reverse, &limits).unwrap()
    );
    assert_eq!(
        decode_metadata::<Tree>(&encode_metadata(&forward, &limits).unwrap(), &limits).unwrap(),
        forward
    );
}

#[test]
fn canonical_decoder_rejects_alternate_json_and_duplicate_keys() {
    let limits = Limits::default();
    for bytes in [
        br#"{ "entries":{}}"#.as_slice(),
        br#"{"entries":{}} "#.as_slice(),
        br#"{"entries":{},"entries":{}}"#.as_slice(),
        br#"{"entries":{},"unknown":1}"#.as_slice(),
        br#"{"entries":{}}{}"#.as_slice(),
    ] {
        assert!(
            decode_metadata::<Tree>(bytes, &limits).is_err(),
            "accepted {}",
            String::from_utf8_lossy(bytes)
        );
    }
    let mut tree = Tree::default();
    tree.entries.insert(
        RepoPath::new("é").unwrap(),
        TreeEntry::Directory {
            mode: FileMode::Regular,
        },
    );
    let canonical = String::from_utf8(encode_metadata(&tree, &limits).unwrap()).unwrap();
    let escaped = canonical.replace("é", "\\u00e9");
    assert!(matches!(
        decode_metadata::<Tree>(escaped.as_bytes(), &limits),
        Err(ModelError::NonCanonical)
    ));
}

#[test]
fn duplicate_set_entries_are_not_silently_discarded() {
    let id = revision(2).to_string();
    let payload = format!("{{\"heads\":[\"{id}\",\"{id}\"]}}");
    assert!(matches!(
        decode_metadata::<ChangeState>(payload.as_bytes(), &Limits::default()),
        Err(ModelError::NonCanonical)
    ));
}

#[test]
fn impossible_record_states_and_unknown_fields_fail_serde_boundary() {
    assert!(serde_json::from_str::<ChangeState>(r#"{"heads":[]}"#).is_err());
    assert!(serde_json::from_str::<Identity>(r#"{"name":"","email":"a@b"}"#).is_err());
    assert!(serde_json::from_str::<Identity>(r#"{"name":"A","email":"a@b","x":1}"#).is_err());
    assert!(serde_json::from_str::<Tree>(r#"{"entries":{"x":{"kind":"conflict","base":null,"ours":null,"theirs":null,"reason":"content"}}}"#).is_err());
}

#[test]
fn file_and_symlink_ancestor_collisions_are_rejected() {
    let child = (
        RepoPath::new("a/b").unwrap(),
        TreeEntry::Directory {
            mode: FileMode::Regular,
        },
    );
    for ancestor in [
        TreeEntry::File {
            blob: object(1),
            mode: FileMode::Regular,
        },
        TreeEntry::Symlink {
            target: SymlinkTarget::new(b"somewhere".to_vec()).unwrap(),
        },
    ] {
        let tree = Tree {
            entries: BTreeMap::from([(RepoPath::new("a").unwrap(), ancestor), child.clone()]),
        };
        assert!(tree.validate(&Limits::default()).is_err());
    }
    let tree = Tree {
        entries: BTreeMap::from([
            (
                RepoPath::new("a").unwrap(),
                TreeEntry::Directory {
                    mode: FileMode::Executable,
                },
            ),
            child,
        ]),
    };
    assert!(tree.validate(&Limits::default()).is_ok());
}

#[test]
fn nested_entries_require_every_explicit_parent_directory_mode() {
    let mut tree = Tree {
        entries: BTreeMap::from([(
            RepoPath::new("parent/child/file").unwrap(),
            TreeEntry::File {
                blob: object(1),
                mode: FileMode::Regular,
            },
        )]),
    };
    assert!(tree.validate(&Limits::default()).is_err());
    tree.entries.insert(
        RepoPath::new("parent").unwrap(),
        TreeEntry::Directory {
            mode: FileMode::Unix { permissions: 0o701 },
        },
    );
    assert!(tree.validate(&Limits::default()).is_err());
    tree.entries.insert(
        RepoPath::new("parent/child").unwrap(),
        TreeEntry::Directory {
            mode: FileMode::Unix { permissions: 0o711 },
        },
    );
    let bytes = encode_metadata(&tree, &Limits::default()).unwrap();
    assert_eq!(
        decode_metadata::<Tree>(&bytes, &Limits::default()).unwrap(),
        tree
    );
}

#[test]
fn tree_namespace_rejects_canonical_and_full_casefold_aliases_without_rewriting_paths() {
    for (first, second) in [
        ("README", "readme"),
        ("é", "e\u{301}"),
        ("İ", "i\u{307}"),
        ("σ", "ς"),
        ("Straße", "STRASSE"),
        ("ſ", "s"),
    ] {
        let tree = Tree {
            entries: BTreeMap::from([
                (
                    RepoPath::new(first).unwrap(),
                    TreeEntry::Directory {
                        mode: FileMode::Executable,
                    },
                ),
                (
                    RepoPath::new(second).unwrap(),
                    TreeEntry::Directory {
                        mode: FileMode::Executable,
                    },
                ),
            ]),
        };
        assert!(
            tree.validate(&Limits::default()).is_err(),
            "accepted alias {first:?}/{second:?}"
        );
        assert!(
            decode_metadata::<Tree>(&serde_json::to_vec(&tree).unwrap(), &Limits::default())
                .is_err()
        );
    }
    let tree = Tree {
        entries: BTreeMap::from([
            (
                RepoPath::new("Dir").unwrap(),
                TreeEntry::Directory {
                    mode: FileMode::Executable,
                },
            ),
            (
                RepoPath::new("dir").unwrap(),
                TreeEntry::Directory {
                    mode: FileMode::Executable,
                },
            ),
            (
                RepoPath::new("dir/file").unwrap(),
                TreeEntry::File {
                    blob: object(1),
                    mode: FileMode::Regular,
                },
            ),
        ]),
    };
    assert!(tree.validate(&Limits::default()).is_err());
}

#[test]
fn namespace_key_pins_unicode_data_and_preserves_original_path_spelling() {
    assert_eq!(caseless::UNICODE_VERSION, (16, 0, 0));
    assert_eq!(unicode_normalization::UNICODE_VERSION, (17, 0, 0));
    for (original, key) in [
        ("σ", "σ"),
        ("ς", "σ"),
        ("Straße", "strasse"),
        ("STRASSE", "strasse"),
        ("ſ", "s"),
        ("é", "e\u{301}"),
        ("İ", "i\u{307}"),
    ] {
        let path = RepoPath::new(original).unwrap();
        assert_eq!(path.namespace_key().unwrap(), key);
        assert_eq!(path.as_str(), original);
        assert_eq!(
            serde_json::from_slice::<RepoPath>(&serde_json::to_vec(&path).unwrap()).unwrap(),
            path
        );
    }
}

#[test]
fn revision_parents_are_ordered_and_duplicates_are_invalid() {
    let mut record = Revision {
        change: ChangeId::from_bytes([1; 16]),
        tree: TreeId::from_object(object(3)),
        parents: vec![revision(1), revision(2)],
        description: "change".into(),
        author: Identity {
            name: "Author".into(),
            email: "a@b".into(),
        },
        created_at_unix_ms: 0,
        origin: None,
    };
    let first = encode_metadata(&record, &Limits::default()).unwrap();
    record.parents.reverse();
    assert_ne!(encode_metadata(&record, &Limits::default()).unwrap(), first);
    record.parents = vec![revision(1), revision(1)];
    assert!(encode_metadata(&record, &Limits::default()).is_err());
}

#[test]
fn limits_reject_untrusted_lengths_before_allocation() {
    let limits = Limits {
        max_metadata_bytes: 8,
        ..Limits::default()
    };
    assert!(matches!(
        decode_metadata::<Tree>(br#"{"entries":{}}"#, &limits),
        Err(ModelError::LimitExceeded { .. })
    ));
    assert!(matches!(
        encode_metadata(&Tree::default(), &limits),
        Err(ModelError::LimitExceeded { .. })
    ));
    assert!(
        Limits {
            max_metadata_bytes: u64::MAX,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        Limits {
            max_path_bytes: 4097,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
    assert!(
        frame_header(
            ObjectKind::Blob,
            u64::MAX,
            &Limits {
                max_blob_bytes: u64::MAX,
                ..Limits::default()
            }
        )
        .is_err()
    );
    let mut header = *b"IZUOBJ1\0\x01\0\0\0\0\0\0\0\0";
    header[9..].copy_from_slice(&u64::MAX.to_be_bytes());
    assert!(parse_frame_header(&header, &Limits::default()).is_err());
}

#[test]
fn configurable_blob_limit_cannot_allow_overflowing_total_frame_length() {
    let limits = Limits {
        max_blob_bytes: u64::MAX,
        ..Limits::default()
    };
    assert!(limits.validate().is_ok());

    let largest_payload = u64::MAX - FRAME_HEADER_LEN as u64;
    let header = frame_header(ObjectKind::Blob, largest_payload, &limits).unwrap();
    assert_eq!(
        parse_frame_header(&header, &limits).unwrap(),
        (ObjectKind::Blob, largest_payload)
    );
    assert!(ObjectHasher::new(ObjectKind::Blob, largest_payload, &limits).is_ok());

    for payload_len in (largest_payload + 1)..=u64::MAX {
        assert!(matches!(
            frame_header(ObjectKind::Blob, payload_len, &limits),
            Err(ModelError::InvalidFrame { .. })
        ));
        let mut untrusted_header = header;
        untrusted_header[9..].copy_from_slice(&payload_len.to_be_bytes());
        assert!(matches!(
            parse_frame_header(&untrusted_header, &limits),
            Err(ModelError::InvalidFrame { .. })
        ));
        assert!(matches!(
            ObjectHasher::new(ObjectKind::Blob, payload_len, &limits),
            Err(ModelError::InvalidFrame { .. })
        ));
    }

    let normal = Limits::default();
    let header = frame_header(ObjectKind::Blob, normal.max_blob_bytes, &normal).unwrap();
    assert_eq!(
        parse_frame_header(&header, &normal).unwrap(),
        (ObjectKind::Blob, 64 * 1024 * 1024 * 1024)
    );
}

#[test]
fn json_depth_and_node_cost_are_bounded_before_typed_decode() {
    let mut nested = vec![b'['; 65];
    nested.extend_from_slice(b"null");
    nested.extend(vec![b']'; 65]);
    assert!(matches!(
        decode_metadata::<Tree>(&nested, &Limits::default()),
        Err(ModelError::InvalidMetadata { .. })
    ));
    let payload = format!("[{}null]", "null,".repeat(1_000_000));
    assert!(matches!(
        decode_metadata::<Tree>(payload.as_bytes(), &Limits::default()),
        Err(ModelError::LimitExceeded {
            resource: "JSON nodes",
            ..
        })
    ));
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
struct IntegerVector {
    max: u64,
    min: i64,
    text: String,
}
impl Validate for IntegerVector {
    fn validate(&self, _: &Limits) -> Result<(), ModelError> {
        Ok(())
    }
}

#[test]
fn canonical_integer_and_string_rules_are_cross_language_explicit() {
    let vector = IntegerVector {
        max: u64::MAX,
        min: i64::MIN,
        text: "é/雪\u{0000}\u{0008}\t\n\u{000c}\r\"\\".into(),
    };
    let bytes = encode_metadata(&vector, &Limits::default()).unwrap();
    assert_eq!(
        std::str::from_utf8(&bytes).unwrap(),
        "{\"max\":18446744073709551615,\"min\":-9223372036854775808,\"text\":\"é/雪\\u0000\\b\\t\\n\\f\\r\\\"\\\\\"}"
    );
    assert_eq!(
        decode_metadata::<IntegerVector>(&bytes, &Limits::default()).unwrap(),
        vector
    );
    assert_eq!(bytes.len(), 91);
    assert_eq!(
        hash_object(ObjectKind::Blob, &bytes, &Limits::default())
            .unwrap()
            .to_string(),
        "ede0676c5ced754c53ccef40826bfe6f34553dcf1891fa91fdc18428bb87f8ed"
    );
    for payload in [
        r#"{"max":1.0,"min":0,"text":""}"#,
        r#"{"max":1e0,"min":0,"text":""}"#,
        r#"{"max":18446744073709551616,"min":0,"text":""}"#,
        r#"{"max":0,"min":-0,"text":""}"#,
    ] {
        assert!(
            decode_metadata::<IntegerVector>(payload.as_bytes(), &Limits::default()).is_err(),
            "accepted {payload}"
        );
    }
}

#[test]
fn object_frames_have_fixed_version_kind_length_and_hash_vectors() {
    let limits = Limits::default();
    assert_eq!(
        frame_header(ObjectKind::Blob, 3, &limits).unwrap(),
        *b"IZUOBJ1\0\x01\0\0\0\0\0\0\0\x03"
    );
    assert_eq!(
        hash_object(ObjectKind::Blob, b"abc", &limits)
            .unwrap()
            .to_string(),
        "739ba683e351a7f2d57d22b2f2f4e55fefc54fb903f32fee4ef0831b64812359"
    );
    assert_eq!(
        hash_object(ObjectKind::Blob, b"", &limits)
            .unwrap()
            .to_string(),
        "9856ab41499a1bffe4177de835d2c7d3ce66a8a57f0ad4921c6329cc83291587"
    );
    assert_ne!(
        hash_object(ObjectKind::Blob, b"abc", &limits).unwrap(),
        hash_object(ObjectKind::Tree, b"abc", &limits).unwrap()
    );
    let header = frame_header(ObjectKind::Evidence, 32, &limits).unwrap();
    assert_eq!(
        parse_frame_header(&header, &limits).unwrap(),
        (ObjectKind::Evidence, 32)
    );
    let mut unknown = header;
    unknown[7] = 2;
    assert!(parse_frame_header(&unknown, &limits).is_err());
    unknown = header;
    unknown[8] = 99;
    assert!(parse_frame_header(&unknown, &limits).is_err());
    assert!(parse_frame_header(&header[..16], &limits).is_err());
}

#[test]
fn prior_ezy_prototype_frames_are_rejected_without_identity_aliases() {
    let limits = Limits::default();
    let legacy_header = *b"EZYOBJ1\0\x01\0\0\0\0\0\0\0\x03";
    assert!(matches!(
        parse_frame_header(&legacy_header, &limits),
        Err(ModelError::InvalidFrame {
            reason: "unknown magic or format version"
        })
    ));

    let current_header = frame_header(ObjectKind::Blob, 3, &limits).unwrap();
    assert_eq!(
        parse_frame_header(&current_header, &limits).unwrap(),
        (ObjectKind::Blob, 3)
    );
    let legacy_abc: ObjectId = "aa4883a68ab69ed9cd630e680b202d649d52bfdbc588f68a9d7bda0a83792c17"
        .parse()
        .unwrap();
    assert_ne!(
        hash_object(ObjectKind::Blob, b"abc", &limits).unwrap(),
        legacy_abc
    );
}

#[test]
fn streaming_hash_enforces_exact_length_without_payload_allocation() {
    let limits = Limits::default();
    let mut stream = ObjectHasher::new(ObjectKind::Blob, 3, &limits).unwrap();
    stream.update(b"a").unwrap();
    stream.update(b"bc").unwrap();
    assert_eq!(
        stream.finish().unwrap(),
        hash_object(ObjectKind::Blob, b"abc", &limits).unwrap()
    );
    assert!(
        ObjectHasher::new(ObjectKind::Blob, 3, &limits)
            .unwrap()
            .finish()
            .is_err()
    );
    let mut stream = ObjectHasher::new(ObjectKind::Blob, 2, &limits).unwrap();
    assert!(stream.update(b"abc").is_err());
    let limits = Limits {
        max_blob_bytes: 0,
        ..limits
    };
    assert!(ObjectHasher::new(ObjectKind::Blob, 0, &limits).is_ok());
    assert!(ObjectHasher::new(ObjectKind::Blob, 1, &limits).is_err());
}

#[test]
fn cancellation_signal_is_shared_and_monotonic() {
    let token = CancellationToken::new();
    let clone = token.clone();
    assert!(clone.check().is_ok());
    token.cancel();
    assert!(clone.is_cancelled());
    assert!(matches!(clone.check(), Err(ModelError::Cancelled)));
    clone.cancel();
    assert!(token.as_atomic().load(std::sync::atomic::Ordering::Acquire));
}

#[test]
fn view_does_not_accept_orphan_or_empty_evidence_sets() {
    let candidate = CandidateId::from_object(object(1));
    let mut view = RepositoryView::default();
    view.evidence.insert(
        candidate,
        BTreeSet::from([EvidenceId::from_object(object(2))]),
    );
    assert!(view.validate(&Limits::default()).is_err());
    view.candidates.insert(candidate);
    assert!(view.validate(&Limits::default()).is_ok());
    view.evidence.insert(candidate, BTreeSet::new());
    assert!(view.validate(&Limits::default()).is_err());
}
