use std::collections::{BTreeMap, BTreeSet};

use izu_model::*;

fn object(byte: u8) -> ObjectId {
    ObjectId::from_bytes([byte; 32])
}
fn revision(byte: u8) -> RevisionId {
    RevisionId::from_object(object(byte))
}
fn tree_id(byte: u8) -> TreeId {
    TreeId::from_object(object(byte))
}
fn references(value: &impl ReferencedObjects) -> BTreeSet<ObjectReference> {
    let mut result = BTreeSet::new();
    value.visit_references(&mut |edge| {
        result.insert(edge);
    });
    result
}
fn edge(kind: ObjectKind, byte: u8) -> ObjectReference {
    ObjectReference {
        kind,
        id: object(byte),
    }
}

#[test]
fn conflicts_roundtrip_and_retain_all_immutable_alternative_blobs() {
    let conflict = TreeEntry::Conflict {
        base: Some(ResolvedTreeEntry::File {
            blob: object(1),
            mode: FileMode::Regular,
        }),
        ours: Some(ResolvedTreeEntry::File {
            blob: object(2),
            mode: FileMode::Executable,
        }),
        theirs: Some(ResolvedTreeEntry::File {
            blob: object(3),
            mode: FileMode::Unix { permissions: 0o600 },
        }),
        reason: ConflictReason::Binary,
    };
    let tree = Tree {
        entries: BTreeMap::from([(RepoPath::new("binary").unwrap(), conflict)]),
    };
    let bytes = encode_metadata(&tree, &Limits::default()).unwrap();
    assert_eq!(
        decode_metadata::<Tree>(&bytes, &Limits::default()).unwrap(),
        tree
    );
    assert_eq!(
        references(&tree),
        BTreeSet::from([
            edge(ObjectKind::Blob, 1),
            edge(ObjectKind::Blob, 2),
            edge(ObjectKind::Blob, 3)
        ])
    );
    let nested = String::from_utf8(bytes)
        .unwrap()
        .replace("\"kind\":\"file\"", "\"kind\":\"conflict\"");
    assert!(decode_metadata::<Tree>(nested.as_bytes(), &Limits::default()).is_err());
}

#[test]
fn revision_closure_keeps_ordered_parents_tree_and_original_git_blob() {
    let record = Revision {
        change: ChangeId::from_bytes([1; 16]),
        tree: tree_id(2),
        parents: vec![revision(3), revision(4)],
        description: String::new(),
        author: Identity {
            name: "A".into(),
            email: "a@b".into(),
        },
        created_at_unix_ms: 0,
        origin: Some(RevisionOrigin::Git {
            object_id: "ab".repeat(20),
            raw_commit: object(5),
        }),
    };
    assert_eq!(
        references(&record),
        BTreeSet::from([
            edge(ObjectKind::Tree, 2),
            edge(ObjectKind::Revision, 3),
            edge(ObjectKind::Revision, 4),
            edge(ObjectKind::Blob, 5)
        ])
    );
    let mut invalid = record;
    invalid.origin = Some(RevisionOrigin::Git {
        object_id: "AB".repeat(20),
        raw_commit: object(5),
    });
    assert!(encode_metadata(&invalid, &Limits::default()).is_err());
}

#[test]
fn operation_closure_contains_every_view_edge_without_filesystem_side_effects() {
    let candidate = CandidateId::from_object(object(10));
    let view = RepositoryView {
        changes: BTreeMap::from([(
            ChangeId::from_bytes([1; 16]),
            ChangeState {
                heads: BTreeSet::from([revision(2), revision(3)]),
            },
        )]),
        refs: BTreeMap::from([(RefName::new("main").unwrap(), revision(4))]),
        workspaces: BTreeMap::from([(
            WorkspaceId::from_bytes([1; 16]),
            WorkspaceRecord {
                name: "main".into(),
                root: "/old/never-materialize".into(),
                head: revision(5),
                sources: BTreeMap::from([(
                    "working".into(),
                    SourceRecord {
                        path: None,
                        tree: tree_id(6),
                        revision: Some(revision(7)),
                    },
                )]),
            },
        )]),
        candidates: BTreeSet::from([candidate]),
        evidence: BTreeMap::from([(
            candidate,
            BTreeSet::from([EvidenceId::from_object(object(11))]),
        )]),
    };
    let operation = Operation {
        parent: Some(OperationId::from_object(object(1))),
        view,
        description: "checkpoint".into(),
        created_at_unix_ms: 0,
    };
    let bytes = encode_metadata(&operation, &Limits::default()).unwrap();
    let restored =
        decode_object_metadata(ObjectKind::Operation, &bytes, &Limits::default()).unwrap();
    let expected = BTreeSet::from([
        edge(ObjectKind::Operation, 1),
        edge(ObjectKind::Revision, 2),
        edge(ObjectKind::Revision, 3),
        edge(ObjectKind::Revision, 4),
        edge(ObjectKind::Revision, 5),
        edge(ObjectKind::Tree, 6),
        edge(ObjectKind::Revision, 7),
        edge(ObjectKind::Candidate, 10),
        edge(ObjectKind::Evidence, 11),
    ]);
    assert_eq!(references(&restored), expected);
    assert!(
        matches!(restored,MetadataObject::Operation(Operation{view,..}) if view.workspaces.values().next().unwrap().root=="/old/never-materialize")
    );
    assert!(decode_object_metadata(ObjectKind::Tree, &bytes, &Limits::default()).is_err());
    assert!(decode_object_metadata(ObjectKind::Blob, &bytes, &Limits::default()).is_err());
}

fn candidate() -> IntegrationCandidate {
    IntegrationCandidate {
        target: RefName::new("main").unwrap(),
        expected_target: Some(revision(1)),
        sources: vec![revision(2)],
        result: revision(3),
        result_tree: tree_id(4),
        checks: vec![CheckSpec {
            name: "tests".into(),
            argv: vec!["cargo".into(), "test".into()],
            environment: Some(object(5)),
        }],
        created_at_unix_ms: 0,
    }
}
fn evidence() -> CheckEvidence {
    CheckEvidence {
        candidate: CandidateId::from_object(object(6)),
        attempt: CheckAttemptId::from_bytes([7; 16]),
        check: "tests".into(),
        inputs: CheckInputs {
            revision: revision(3),
            tree: tree_id(4),
            environment: Some(object(5)),
        },
        outcome: CheckOutcome::Passed,
        argv: vec!["cargo".into(), "test".into()],
        started_at_unix_ms: i64::MIN,
        finished_at_unix_ms: Some(i64::MAX),
    }
}

#[test]
fn candidates_and_evidence_closure_retain_exact_sources_results_and_environment() {
    assert_eq!(
        references(&candidate()),
        BTreeSet::from([
            edge(ObjectKind::Revision, 1),
            edge(ObjectKind::Revision, 2),
            edge(ObjectKind::Revision, 3),
            edge(ObjectKind::Tree, 4),
            edge(ObjectKind::Blob, 5)
        ])
    );
    assert_eq!(
        references(&evidence()),
        BTreeSet::from([
            edge(ObjectKind::Candidate, 6),
            edge(ObjectKind::Revision, 3),
            edge(ObjectKind::Tree, 4),
            edge(ObjectKind::Blob, 5)
        ])
    );
    let bytes = encode_metadata(&evidence(), &Limits::default()).unwrap();
    assert!(matches!(
        decode_object_metadata(ObjectKind::Evidence, &bytes, &Limits::default()).unwrap(),
        MetadataObject::Evidence(_)
    ));
}

#[test]
fn pending_attempt_is_valid_metadata_but_cannot_establish_candidate_success() {
    let mut json = serde_json::to_value(evidence()).unwrap();
    json["attempt"] = serde_json::json!("07".repeat(16));
    json["outcome"] = serde_json::json!({"status":"pending"});
    json["finished_at_unix_ms"] = serde_json::Value::Null;
    let bytes = serde_json::to_vec(&json).unwrap();
    assert_eq!(bytes.len(), 509);
    assert_eq!(
        hash_object(ObjectKind::Evidence, &bytes, &Limits::default())
            .unwrap()
            .to_string(),
        "99d5dc1d860a498c9a20f02b998a42b47071f3a5855262a220df7618ab07b492"
    );
    let pending = decode_metadata::<CheckEvidence>(&bytes, &Limits::default());
    assert!(pending.is_ok(), "pending evidence rejected: {pending:?}");
    let pending = pending.unwrap();
    assert!(
        pending
            .validate_inputs_for_candidate(pending.candidate, &candidate())
            .is_ok()
    );
    assert!(
        pending
            .validate_for_candidate(pending.candidate, &candidate())
            .is_err()
    );
}

#[test]
fn completed_evidence_without_attempt_token_is_not_implicitly_upgraded() {
    let mut json = serde_json::to_value(evidence()).unwrap();
    json.as_object_mut().unwrap().remove("attempt");
    let bytes = serde_json::to_vec(&json).unwrap();
    assert!(decode_metadata::<CheckEvidence>(&bytes, &Limits::default()).is_err());
}

#[test]
fn pending_and_terminal_lifecycle_combinations_are_validated_before_publication() {
    let limits = Limits::default();
    let mut record = evidence();
    record.outcome = CheckOutcome::Pending;
    assert!(encode_metadata(&record, &limits).is_err());
    assert!(
        serde_json::from_value::<CheckEvidence>(serde_json::to_value(&record).unwrap()).is_err()
    );
    record.finished_at_unix_ms = None;
    let bytes = encode_metadata(&record, &limits).unwrap();
    assert_eq!(
        decode_metadata::<CheckEvidence>(&bytes, &limits).unwrap(),
        record
    );
    for outcome in [
        CheckOutcome::Passed,
        CheckOutcome::Failed { exit_code: Some(1) },
        CheckOutcome::Cancelled,
    ] {
        record.outcome = outcome;
        assert!(encode_metadata(&record, &limits).is_err());
        assert!(
            record
                .validate_for_candidate(record.candidate, &candidate())
                .is_err()
        );
        record.finished_at_unix_ms = Some(record.started_at_unix_ms);
        assert!(encode_metadata(&record, &limits).is_ok());
        record.finished_at_unix_ms = None;
    }
}

#[test]
fn terminal_attempt_must_match_the_current_pending_token_and_bindings() {
    let exact = evidence();
    let mut pending = exact.clone();
    pending.outcome = CheckOutcome::Pending;
    pending.finished_at_unix_ms = None;
    for outcome in [
        CheckOutcome::Passed,
        CheckOutcome::Failed { exit_code: Some(1) },
        CheckOutcome::Cancelled,
    ] {
        let mut terminal = exact.clone();
        terminal.outcome = outcome;
        assert!(terminal.validate_for_pending_attempt(&pending).is_ok());
    }
    assert!(pending.validate_for_pending_attempt(&pending).is_err());
    assert!(exact.validate_for_pending_attempt(&exact).is_err());

    let mut variations = Vec::new();
    let mut changed = exact.clone();
    changed.attempt = CheckAttemptId::from_bytes([9; 16]);
    variations.push(changed);
    let mut changed = exact.clone();
    changed.candidate = CandidateId::from_object(object(9));
    variations.push(changed);
    let mut changed = exact.clone();
    changed.check = "other".into();
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.revision = revision(9);
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.tree = tree_id(9);
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.environment = Some(object(9));
    variations.push(changed);
    let mut changed = exact.clone();
    changed.argv = vec!["true".into()];
    variations.push(changed);
    let mut changed = exact.clone();
    changed.started_at_unix_ms = 0;
    variations.push(changed);
    for changed in variations {
        assert!(matches!(
            changed.validate_for_pending_attempt(&pending),
            Err(ModelError::EvidenceMismatch { .. })
        ));
    }
}

#[test]
fn rerun_pending_has_a_new_immutable_identity_and_rejects_the_earlier_pass() {
    let earlier_pass = evidence();
    let mut rerun = earlier_pass.clone();
    rerun.attempt = CheckAttemptId::from_bytes([8; 16]);
    rerun.outcome = CheckOutcome::Pending;
    rerun.finished_at_unix_ms = None;
    assert!(earlier_pass.validate_for_pending_attempt(&rerun).is_err());
    assert!(
        rerun
            .validate_for_candidate(rerun.candidate, &candidate())
            .is_err()
    );
    let limits = Limits::default();
    let old = encode_metadata(&earlier_pass, &limits).unwrap();
    let pending = encode_metadata(&rerun, &limits).unwrap();
    assert_ne!(
        hash_object(ObjectKind::Evidence, &old, &limits).unwrap(),
        hash_object(ObjectKind::Evidence, &pending, &limits).unwrap()
    );
    assert_eq!(references(&rerun), references(&earlier_pass));
}

#[test]
fn check_success_is_bound_to_every_required_immutable_input() {
    let candidate = candidate();
    let exact = evidence();
    let id = exact.candidate;
    assert!(exact.validate_for_candidate(id, &candidate).is_ok());
    let mut variations = Vec::new();
    let mut changed = exact.clone();
    changed.candidate = CandidateId::from_object(object(7));
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.revision = revision(7);
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.tree = tree_id(7);
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.environment = None;
    variations.push(changed);
    let mut changed = exact.clone();
    changed.inputs.environment = Some(object(7));
    variations.push(changed);
    let mut changed = exact.clone();
    changed.check = "different".into();
    variations.push(changed);
    let mut changed = exact.clone();
    changed.argv = vec!["true".into()];
    variations.push(changed);
    let mut changed = exact.clone();
    changed.outcome = CheckOutcome::Failed { exit_code: Some(1) };
    variations.push(changed);
    let mut changed = exact.clone();
    changed.outcome = CheckOutcome::Cancelled;
    variations.push(changed);
    for changed in variations {
        assert!(matches!(
            changed.validate_for_candidate(id, &candidate),
            Err(ModelError::EvidenceMismatch { .. })
        ));
    }
}

#[test]
fn failed_or_temporally_impossible_evidence_is_not_valid_metadata() {
    let mut record = evidence();
    record.outcome = CheckOutcome::Failed { exit_code: Some(0) };
    assert!(encode_metadata(&record, &Limits::default()).is_err());
    record.outcome = CheckOutcome::Passed;
    record.started_at_unix_ms = 2;
    record.finished_at_unix_ms = Some(1);
    assert!(encode_metadata(&record, &Limits::default()).is_err());
    record.started_at_unix_ms = 0;
    record.finished_at_unix_ms = Some(1);
    record.argv.push("nul\0arg".into());
    assert!(encode_metadata(&record, &Limits::default()).is_err());
}

#[test]
fn divergent_changes_never_infer_a_winning_hash() {
    let change = ChangeState {
        heads: BTreeSet::from([revision(1), revision(2)]),
    };
    assert!(change.is_divergent());
    assert_eq!(change.single_head(), None);
    assert!(!ChangeState::resolved(revision(1)).is_divergent());
    assert_eq!(
        ChangeState::resolved(revision(1)).single_head(),
        Some(revision(1))
    );
}

#[test]
fn unicode_control_and_integer_minimum_revision_has_golden_id() {
    let record = Revision {
        change: ChangeId::from_bytes([1; 16]),
        tree: tree_id(2),
        parents: vec![],
        description: "é/雪\u{0000}\u{0008}\t\n\u{000c}\r\"\\".into(),
        author: Identity {
            name: "Ézzy".into(),
            email: "a@b".into(),
        },
        created_at_unix_ms: i64::MIN,
        origin: None,
    };
    let bytes = encode_metadata(&record, &Limits::default()).unwrap();
    assert_eq!(bytes.len(), 271);
    assert_eq!(
        hash_object(ObjectKind::Revision, &bytes, &Limits::default())
            .unwrap()
            .to_string(),
        "8bff10ea806eb0f8b3121c09574fac3dd191e8c75e053e3145d5e9b7cec29f19"
    );
    assert_eq!(
        hash_object(ObjectKind::Tree, br#"{"entries":{}}"#, &Limits::default())
            .unwrap()
            .to_string(),
        "24d2a1d3109cdc927435777079f89db1ec1bcc2b900ad23eeedd83a1ad96cadb"
    );
}

#[test]
fn bootstrap_provenance_is_explicit_and_cannot_have_parents() {
    let mut record = Revision {
        change: ChangeId::from_bytes([1; 16]),
        tree: tree_id(2),
        parents: vec![],
        description: "arbitrary text does not classify intent".into(),
        author: Identity {
            name: "Real Author".into(),
            email: "real@host".into(),
        },
        created_at_unix_ms: 0,
        origin: Some(RevisionOrigin::Bootstrap),
    };
    let bytes = encode_metadata(&record, &Limits::default()).unwrap();
    assert!(
        String::from_utf8(bytes.clone())
            .unwrap()
            .contains("\"origin\":{\"kind\":\"bootstrap\"}")
    );
    assert_eq!(
        decode_metadata::<Revision>(&bytes, &Limits::default()).unwrap(),
        record
    );
    record.parents.push(revision(3));
    assert!(encode_metadata(&record, &Limits::default()).is_err());
    record.origin = None;
    assert!(encode_metadata(&record, &Limits::default()).is_ok());
}
