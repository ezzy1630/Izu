#![cfg(unix)]

use izu_bundle::{
    BundleError, BundleManifest, BundleOptions, ObjectDescriptor, ObjectSource, Publication,
    RestoreOptions, create, inspect, restore, verify,
};
use izu_engine::{MergePreparation, Repository, RepositoryOptions, Selection};
use izu_model::{
    CancellationToken, ChangeId, CheckOutcome, CheckSpec, ConflictReason, FRAME_HEADER_LEN,
    FileMode, Identity, Limits, ObjectId, ObjectKind, Operation, OperationId, RefName, RepoPath,
    ResolvedTreeEntry, Revision, RevisionId, RevisionOrigin, Tree, TreeEntry, WorkspaceId,
    decode_metadata, encode_metadata, frame_header, hash_object, parse_frame_header,
};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use tempfile::TempDir;

fn author() -> Identity {
    Identity {
        name: "Archive fixture".into(),
        email: "archive@example.test".into(),
    }
}
fn owned_temp() -> TempDir {
    let parent = fs::canonicalize(std::env::temp_dir())
        .expect("actual temporary parent without symlink aliases");
    TempDir::new_in(parent).expect("owned temporary directory")
}

struct Fixture {
    temp: TempDir,
    original: PathBuf,
    repository: Repository,
    workspace: WorkspaceId,
    root: OperationId,
}

fn fixture() -> Fixture {
    let temp = owned_temp();
    let original = temp.path().join("original");
    fs::create_dir(&original).expect("create fixture root");
    let repository =
        Repository::init(&original, RepositoryOptions::default()).expect("initialize repository");
    let workspace = repository.workspace_id();
    fs::write(original.join("hello.txt"), b"captured text\n").expect("write source");
    let cancel = CancellationToken::new();
    let state = repository
        .workspace(workspace, &cancel)
        .expect("workspace state");
    let root = repository
        .checkpoint(workspace, state.expected, Selection::All, &cancel)
        .expect("checkpoint")
        .operation;
    Fixture {
        temp,
        original,
        repository,
        workspace,
        root,
    }
}

fn create_fixture_bundle(fixture: &Fixture) -> PathBuf {
    let output = fixture.temp.path().join("source.izubundle");
    let receipt = create(
        &fixture.repository,
        fixture.root,
        &output,
        &BundleOptions::default(),
        &CancellationToken::new(),
    )
    .expect("create bundle");
    assert!(matches!(receipt.publication, Publication::Durable));
    output
}

#[cfg(target_os = "linux")]
#[test]
fn known_volatile_destination_is_rejected_before_creating_output_or_stage() {
    let fixture = fixture();
    let volatile = tempfile::tempdir_in("/dev/shm")
        .expect("Linux durability regression requires a private /dev/shm fixture");
    let directory = izu_platform::Directory::open(volatile.path()).expect("open volatile parent");
    assert_eq!(
        directory
            .require_persistent_filesystem()
            .expect_err("fixture must be classified as volatile")
            .kind(),
        std::io::ErrorKind::Unsupported
    );

    let output = volatile.path().join("archive.izubundle");
    let result = create(
        &fixture.repository,
        fixture.root,
        &output,
        &BundleOptions::default(),
        &CancellationToken::new(),
    );
    assert!(
        matches!(&result, Err(BundleError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::Unsupported),
        "a known volatile destination must not acknowledge an archive: {result:?}"
    );
    assert_eq!(
        fs::read_dir(volatile.path())
            .expect("inspect parent")
            .count(),
        0
    );

    let input = create_fixture_bundle(&fixture);
    let destination = volatile.path().join("restored");
    let result = restore(
        &input,
        &destination,
        &RestoreOptions::default(),
        &CancellationToken::new(),
    );
    assert!(
        matches!(&result, Err(BundleError::Io { source, .. })
            if source.kind() == std::io::ErrorKind::Unsupported),
        "a known volatile destination must be refused before restoration: {result:?}"
    );
    assert_eq!(
        fs::read_dir(volatile.path())
            .expect("inspect parent")
            .count(),
        0
    );
}

#[test]
fn cold_restore_preserves_captured_source_history_candidates_evidence_and_git_bytes() {
    let mut fixture = fixture();
    let cancel = CancellationToken::new();
    fs::write(
        fixture.original.join("run.sh"),
        b"#!/bin/sh\nprintf archive\n",
    )
    .expect("write executable");
    fs::set_permissions(
        fixture.original.join("run.sh"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("set executable");
    symlink("hello.txt", fixture.original.join("hello-link")).expect("relative symlink");
    let state = fixture
        .repository
        .workspace(fixture.workspace, &cancel)
        .expect("state");
    let committed = fixture
        .repository
        .commit(
            fixture.workspace,
            state.expected,
            Selection::All,
            "first captured revision".into(),
            author(),
            &cancel,
        )
        .expect("commit native source");
    let main_before =
        fixture.repository.view(&cancel).expect("view").refs[&RefName::new("main").expect("main")];
    let raw_commit =
        b"tree abcdef\nauthor Original <original@example.test> 1 +0930\n\nraw extra bytes\n";
    let raw_id = fixture
        .repository
        .put_blob(&mut &raw_commit[..], raw_commit.len() as u64, &cancel)
        .expect("origin blob");
    let origin_revision = Revision {
        change: ChangeId::from_bytes([9; 16]),
        tree: committed.tree,
        parents: vec![committed.revision],
        description: "native imported metadata fixture".into(),
        author: author(),
        created_at_unix_ms: 7,
        origin: Some(RevisionOrigin::Git {
            object_id: "0123456789012345678901234567890123456789".into(),
            raw_commit: raw_id,
        }),
    };
    let origin_id = fixture
        .repository
        .put_revision(&origin_revision, &cancel)
        .expect("native origin revision");
    fixture
        .repository
        .import_revisions(&[origin_id], &[], &cancel)
        .expect("register origin");
    let environment = b"bounded fixture environment digest, no credentials";
    let environment_id = fixture
        .repository
        .put_blob(&mut &environment[..], environment.len() as u64, &cancel)
        .expect("environment blob");
    let merge = fixture
        .repository
        .prepare_merge(
            RefName::new("main").expect("main"),
            Some(main_before),
            origin_id,
            vec![CheckSpec {
                name: "fixture-check".into(),
                argv: vec!["false".into()],
                environment: Some(environment_id),
            }],
            author(),
            &cancel,
        )
        .expect("prepare native merge candidate");
    let candidate_id = match merge {
        MergePreparation::Ready { candidate, .. } => candidate,
        MergePreparation::Conflicted { .. } => panic!("fixture merge has no conflicts"),
    };
    let candidate = fixture
        .repository
        .candidate(candidate_id, &cancel)
        .expect("candidate");
    assert_eq!(
        fixture
            .repository
            .revision(candidate.result, &cancel)
            .expect("merge result")
            .parents,
        vec![main_before, origin_id]
    );
    let attempt = fixture
        .repository
        .start_check(candidate_id, "fixture-check", &cancel)
        .expect("publish pending fixture check attempt");
    let pending_evidence = attempt.pending;
    let mut evidence = pending_evidence.clone();
    evidence.outcome = CheckOutcome::Failed { exit_code: Some(1) };
    evidence.finished_at_unix_ms = Some(evidence.started_at_unix_ms + 1);
    let evidence_id = fixture
        .repository
        .record_evidence(&evidence, &cancel)
        .expect("failed fixture evidence");
    let historical_path = fixture.temp.path().join("historical-workspace");
    fs::create_dir(&historical_path).expect("owned historical workspace");
    fixture
        .repository
        .fork_workspace(
            "historical".into(),
            &historical_path,
            committed.revision,
            &cancel,
        )
        .expect("second archived workspace");
    fs::write(
        historical_path.join("leave-alone"),
        b"historical absolute paths are metadata\n",
    )
    .expect("sentinel");
    fs::write(
        fixture.original.join("hello.txt"),
        b"uncommitted checkpoint\n",
    )
    .expect("uncommitted edit");
    let state = fixture
        .repository
        .workspace(fixture.workspace, &cancel)
        .expect("state");
    fixture.root = fixture
        .repository
        .checkpoint(fixture.workspace, state.expected, Selection::All, &cancel)
        .expect("working checkpoint")
        .operation;
    let archived_operation = fixture
        .repository
        .operation(fixture.root, &cancel)
        .expect("archived operation");
    let archived_history: Vec<_> = fixture
        .repository
        .operations(1000, &cancel)
        .expect("operation history")
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    let output = create_fixture_bundle(&fixture);
    let report = verify(&output, &BundleOptions::default(), &cancel).expect("verify archive");
    assert!(report.objects.operations >= 6);
    assert!(report.objects.candidates >= 1 && report.objects.evidence >= 2);
    drop(fixture.repository);
    fs::remove_dir_all(&fixture.original).expect("remove original owned repository");
    assert!(!fixture.original.join(".izu").exists());
    let destination = fixture.temp.path().join("restored-elsewhere");
    let options = RestoreOptions {
        workspace: Some(fixture.workspace),
        ..RestoreOptions::default()
    };
    let restored = restore(&output, &destination, &options, &cancel).expect("cold restore");
    assert!(matches!(restored.publication, Publication::Durable));
    assert_eq!(
        fs::read(destination.join("hello.txt")).expect("restored working file"),
        b"uncommitted checkpoint\n"
    );
    assert_eq!(
        fs::read(destination.join("run.sh")).expect("restored executable"),
        b"#!/bin/sh\nprintf archive\n"
    );
    assert_eq!(
        fs::metadata(destination.join("run.sh"))
            .expect("mode")
            .permissions()
            .mode()
            & 0o7777,
        0o755
    );
    assert_eq!(
        fs::read_link(destination.join("hello-link")).expect("link"),
        Path::new("hello.txt")
    );
    assert_eq!(
        fs::read(historical_path.join("leave-alone")).expect("untouched historical root"),
        b"historical absolute paths are metadata\n"
    );
    assert!(!fixture.original.exists());
    let recovered = Repository::open(&destination, RepositoryOptions::default())
        .expect("open cold restored repository");
    assert_eq!(
        recovered
            .operation(fixture.root, &cancel)
            .expect("exact archived operation"),
        archived_operation
    );
    assert_eq!(
        recovered
            .revision(origin_id, &cancel)
            .expect("exact origin revision"),
        origin_revision
    );
    assert_eq!(
        recovered
            .candidate(candidate_id, &cancel)
            .expect("exact candidate"),
        candidate
    );
    assert_eq!(
        recovered
            .evidence(evidence_id, &cancel)
            .expect("exact evidence"),
        evidence
    );
    assert_eq!(
        recovered
            .evidence(attempt.evidence, &cancel)
            .expect("exact historical pending evidence"),
        pending_evidence
    );
    assert_eq!(
        recovered.blob(raw_id, &cancel).expect("original Git bytes"),
        raw_commit
    );
    assert_eq!(
        recovered
            .blob(environment_id, &cancel)
            .expect("environment bytes"),
        environment
    );
    let recovered_view = recovered.view(&cancel).expect("restored view");
    assert_eq!(recovered_view.workspaces.len(), 1);
    assert_eq!(
        recovered_view.workspaces[&fixture.workspace].root,
        destination.to_str().expect("UTF-8 destination")
    );
    let recovered_history: Vec<_> = recovered
        .operations(1000, &cancel)
        .expect("traversable old operation history")
        .into_iter()
        .map(|entry| entry.id)
        .collect();
    assert_eq!(
        &recovered_history[recovered_history.len() - archived_history.len()..],
        archived_history.as_slice()
    );
    recovered
        .verify(&cancel)
        .expect("independent engine graph verification");
}

struct StreamingSource<'a> {
    source: &'a Repository,
    largest_write: AtomicUsize,
}
struct ObservedWriter<'a> {
    output: &'a mut dyn Write,
    largest_write: &'a AtomicUsize,
}
impl Write for ObservedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.largest_write.fetch_max(bytes.len(), Ordering::Relaxed);
        self.output.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}
impl ObjectSource for StreamingSource<'_> {
    fn object_info(
        &self,
        id: ObjectId,
        cancel: &CancellationToken,
    ) -> izu_bundle::Result<ObjectDescriptor> {
        ObjectSource::object_info(self.source, id, cancel)
    }
    fn read_object_to(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        output: &mut dyn Write,
        cancel: &CancellationToken,
    ) -> izu_bundle::Result<u64> {
        ObjectSource::read_object_to(
            self.source,
            id,
            kind,
            &mut ObservedWriter {
                output,
                largest_write: &self.largest_write,
            },
            cancel,
        )
    }
}

#[test]
fn binary_archive_streams_above_store_allocation_budget() {
    let temp = owned_temp();
    let original = temp.path().join("binary-original");
    fs::create_dir(&original).expect("source root");
    let mut repository_options = RepositoryOptions::default();
    repository_options.store.max_in_memory_bytes = 64 * 1024;
    let repository = Repository::init(&original, repository_options).expect("init");
    let mut binary = File::create(original.join("binary.dat")).expect("binary source");
    let chunk: Vec<_> = (0..64 * 1024).map(|index| (index % 251) as u8).collect();
    let mut expected_hash = Sha256::new();
    for _ in 0..128 {
        binary.write_all(&chunk).expect("stream source chunk");
        expected_hash.update(&chunk);
    }
    drop(binary);
    let cancel = CancellationToken::new();
    let id = repository.workspace_id();
    let state = repository.workspace(id, &cancel).expect("state");
    let root = repository
        .checkpoint(id, state.expected, Selection::All, &cancel)
        .expect("capture binary")
        .operation;
    let source = StreamingSource {
        source: &repository,
        largest_write: AtomicUsize::new(0),
    };
    let output = temp.path().join("binary.izubundle");
    create(&source, root, &output, &BundleOptions::default(), &cancel).expect("stream archive");
    assert!(source.largest_write.load(Ordering::Relaxed) <= 64 * 1024);
    drop(repository);
    fs::remove_dir_all(&original).expect("delete original binary source and store");
    let restored = temp.path().join("binary-restored");
    restore(&output, &restored, &RestoreOptions::default(), &cancel).expect("restore binary");
    assert_eq!(
        fs::metadata(restored.join("binary.dat"))
            .expect("size")
            .len(),
        8 * 1024 * 1024
    );
    let mut file = File::open(restored.join("binary.dat")).expect("read independently");
    let mut actual_hash = Sha256::new();
    let mut read_buffer = [0_u8; 64 * 1024];
    loop {
        let count = file.read(&mut read_buffer).expect("read chunk");
        if count == 0 {
            break;
        }
        actual_hash.update(&read_buffer[..count]);
    }
    assert_eq!(actual_hash.finalize(), expected_hash.finalize());
}

#[derive(Clone)]
struct Record {
    id: ObjectId,
    kind: ObjectKind,
    payload: Vec<u8>,
}
fn decode_records(bytes: &[u8]) -> (BundleManifest, Vec<Record>) {
    let mut encoded_len = [0; 4];
    encoded_len.copy_from_slice(&bytes[12..16]);
    let length = u32::from_be_bytes(encoded_len) as usize;
    let manifest: BundleManifest =
        decode_metadata(&bytes[16..16 + length], &Limits::default()).expect("manifest");
    let mut cursor = 16 + length;
    let mut records = Vec::new();
    for _ in 0..manifest.object_count {
        let mut id = [0; 32];
        id.copy_from_slice(&bytes[cursor..cursor + 32]);
        cursor += 32;
        let (kind, length) = parse_frame_header(
            &bytes[cursor..cursor + FRAME_HEADER_LEN],
            &Limits::default(),
        )
        .expect("header");
        cursor += FRAME_HEADER_LEN;
        let payload = bytes[cursor..cursor + length as usize].to_vec();
        cursor += length as usize;
        records.push(Record {
            id: ObjectId::from_bytes(id),
            kind,
            payload,
        });
    }
    (manifest, records)
}
fn encode_records(mut manifest: BundleManifest, records: &[Record]) -> Vec<u8> {
    manifest.object_count = records.len() as u64;
    manifest.payload_bytes = records
        .iter()
        .map(|record| record.payload.len() as u64)
        .sum();
    let manifest = encode_metadata(&manifest, &Limits::default()).expect("canonical manifest");
    let mut bytes = b"IZUBND1\0".to_vec();
    bytes.extend_from_slice(&1_u16.to_be_bytes());
    bytes.extend_from_slice(&0_u16.to_be_bytes());
    bytes.extend_from_slice(&(manifest.len() as u32).to_be_bytes());
    bytes.extend_from_slice(&manifest);
    for record in records {
        bytes.extend_from_slice(record.id.as_bytes());
        bytes.extend_from_slice(
            &frame_header(record.kind, record.payload.len() as u64, &Limits::default())
                .expect("frame"),
        );
        bytes.extend_from_slice(&record.payload);
    }
    let digest = Sha256::digest(&bytes);
    bytes.extend_from_slice(b"IZUEND1\0");
    bytes.extend_from_slice(&digest);
    bytes
}
fn verify_bytes(
    fixture: &Fixture,
    bytes: &[u8],
) -> izu_bundle::Result<izu_bundle::VerificationReport> {
    let input = fixture.temp.path().join("tampered.izubundle");
    fs::write(&input, bytes).expect("write owned hostile input");
    verify(&input, &BundleOptions::default(), &CancellationToken::new())
}

#[test]
fn rejects_truncation_unknown_version_trailer_corruption_and_trailing_data() {
    let fixture = fixture();
    let output = create_fixture_bundle(&fixture);
    let bytes = fs::read(output).expect("small fixture bundle");
    for cutoff in [
        0,
        7,
        15,
        16,
        bytes.len() / 2,
        bytes.len() - 41,
        bytes.len() - 1,
    ] {
        assert!(
            verify_bytes(&fixture, &bytes[..cutoff]).is_err(),
            "cutoff {cutoff}"
        );
    }
    let mut unknown = bytes.clone();
    unknown[9] = 2;
    assert!(matches!(
        verify_bytes(&fixture, &unknown),
        Err(BundleError::UnsupportedVersion)
    ));
    let mut corrupt = bytes.clone();
    let last = corrupt.len() - 1;
    corrupt[last] ^= 1;
    assert!(matches!(
        verify_bytes(&fixture, &corrupt),
        Err(BundleError::TrailerMismatch)
    ));
    let mut trailing = bytes;
    trailing.extend_from_slice(b"extra");
    assert!(matches!(
        verify_bytes(&fixture, &trailing),
        Err(BundleError::Invalid(_))
    ));
}

#[test]
fn rejects_native_hash_missing_closure_duplicates_and_unreachable_records() {
    let fixture = fixture();
    let bytes = fs::read(create_fixture_bundle(&fixture)).expect("read bundle");
    let (manifest, records) = decode_records(&bytes);
    let blob_index = records
        .iter()
        .position(|record| record.kind == ObjectKind::Blob)
        .expect("fixture blob");
    let mut corrupt = records.clone();
    corrupt[blob_index].payload[0] ^= 1;
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest.clone(), &corrupt)),
        Err(BundleError::Model(izu_model::ModelError::HashMismatch))
    ));
    let mut missing = records.clone();
    missing.remove(blob_index);
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest.clone(), &missing)),
        Err(BundleError::MissingObject { .. })
    ));
    let mut duplicate = records.clone();
    duplicate.push(duplicate[blob_index].clone());
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest.clone(), &duplicate)),
        Err(BundleError::DuplicateObject(_))
    ));
    duplicate.last_mut().expect("duplicate").payload[0] ^= 1;
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest.clone(), &duplicate)),
        Err(BundleError::DuplicateObject(_))
    ));
    let mut extra = records;
    let payload = b"outside root".to_vec();
    extra.push(Record {
        id: hash_object(ObjectKind::Blob, &payload, &Limits::default()).expect("hash"),
        kind: ObjectKind::Blob,
        payload,
    });
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest, &extra)),
        Err(BundleError::UnreachableObject(_))
    ));
}

#[test]
fn preserves_unresolved_conflict_alternatives_and_rejects_a_missing_alternative() {
    let mut fixture = fixture();
    let cancel = CancellationToken::new();
    let contents: [&[u8]; 3] = [
        b"base conflict bytes",
        b"ours conflict bytes",
        b"theirs conflict bytes",
    ];
    let blobs: Vec<_> = contents
        .iter()
        .map(|bytes| {
            let mut input = *bytes;
            fixture
                .repository
                .put_blob(&mut input, bytes.len() as u64, &cancel)
                .expect("conflict alternative blob")
        })
        .collect();
    let file = |blob| {
        Some(ResolvedTreeEntry::File {
            blob,
            mode: FileMode::Regular,
        })
    };
    let conflict_tree = Tree {
        entries: BTreeMap::from([(
            RepoPath::new("conflict.txt").expect("path"),
            TreeEntry::Conflict {
                base: file(blobs[0]),
                ours: file(blobs[1]),
                theirs: file(blobs[2]),
                reason: ConflictReason::Content,
            },
        )]),
    };
    let tree_id = fixture
        .repository
        .put_tree(&conflict_tree, &cancel)
        .expect("unresolved tree");
    let view = fixture.repository.view(&cancel).expect("view");
    let main = RefName::new("main").expect("main");
    let candidate_id = fixture
        .repository
        .prepare_candidate(
            main.clone(),
            Some(view.refs[&main]),
            vec![view.workspaces[&fixture.workspace].head],
            tree_id,
            vec![CheckSpec {
                name: "after-resolution".into(),
                argv: vec!["false".into()],
                environment: None,
            }],
            author(),
            &cancel,
        )
        .expect("persist unresolved candidate");
    fixture.root = fixture
        .repository
        .current_operation(&cancel)
        .expect("captured candidate operation");
    let output = create_fixture_bundle(&fixture);
    let (manifest, records) = decode_records(&fs::read(&output).expect("fixture archive"));
    for id in &blobs {
        assert!(records.iter().any(|record| record.id == *id));
    }
    let missing: Vec<_> = records
        .into_iter()
        .filter(|record| record.id != blobs[0])
        .collect();
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest, &missing)),
        Err(BundleError::MissingObject { id, .. }) if id == blobs[0]
    ));
    drop(fixture.repository);
    fs::remove_dir_all(&fixture.original).expect("delete original source and store");
    let destination = fixture.temp.path().join("restored-conflict-history");
    restore(&output, &destination, &RestoreOptions::default(), &cancel)
        .expect("restore clean working source with unresolved historical candidate");
    assert_eq!(
        fs::read(destination.join("hello.txt")).expect("captured source"),
        b"captured text\n"
    );
    assert!(!destination.join("conflict.txt").exists());
    let recovered = Repository::open(&destination, RepositoryOptions::default())
        .expect("cold recovered repository");
    assert_eq!(
        recovered
            .candidate(candidate_id, &cancel)
            .expect("candidate")
            .result_tree,
        tree_id
    );
    assert_eq!(
        recovered
            .tree(tree_id, &cancel)
            .expect("exact conflict tree"),
        conflict_tree
    );
    for (id, bytes) in blobs.into_iter().zip(contents) {
        assert_eq!(
            recovered.blob(id, &cancel).expect("exact alternative blob"),
            bytes
        );
    }
}

#[test]
fn verifies_reference_kinds_depth_and_graph_budgets() {
    let fixture = fixture();
    let output = create_fixture_bundle(&fixture);
    let bytes = fs::read(&output).expect("bundle");
    let (mut manifest, mut records) = decode_records(&bytes);
    let blob = records
        .iter()
        .find(|record| record.kind == ObjectKind::Blob)
        .expect("blob")
        .id;
    let root_index = records
        .iter()
        .position(|record| record.id == manifest.root_operation.object_id())
        .expect("root");
    let mut operation: Operation =
        decode_metadata(&records[root_index].payload, &Limits::default()).expect("operation");
    operation.view.refs.insert(
        RefName::new("wrong-kind").expect("name"),
        RevisionId::from_object(blob),
    );
    records[root_index].payload =
        encode_metadata(&operation, &Limits::default()).expect("canonical operation");
    records[root_index].id = hash_object(
        ObjectKind::Operation,
        &records[root_index].payload,
        &Limits::default(),
    )
    .expect("operation hash");
    manifest.root_operation = OperationId::from_object(records[root_index].id);
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest, &records)),
        Err(BundleError::UnexpectedKind { .. })
    ));
    let (mut manifest, mut records) = decode_records(&bytes);
    let root_index = records
        .iter()
        .position(|record| record.id == manifest.root_operation.object_id())
        .expect("root");
    let mut operation: Operation =
        decode_metadata(&records[root_index].payload, &Limits::default()).expect("operation");
    let (change, heads) = operation.view.changes.pop_first().expect("logical change");
    let mut wrong_change = *change.as_bytes();
    wrong_change[0] ^= 1;
    operation
        .view
        .changes
        .insert(ChangeId::from_bytes(wrong_change), heads);
    records[root_index].payload = encode_metadata(&operation, &Limits::default())
        .expect("canonical operation with wrong logical owner");
    records[root_index].id = hash_object(
        ObjectKind::Operation,
        &records[root_index].payload,
        &Limits::default(),
    )
    .expect("operation hash");
    manifest.root_operation = OperationId::from_object(records[root_index].id);
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest, &records)),
        Err(BundleError::Invalid(
            "revision head belongs to a different logical change"
        ))
    ));
    for options in [
        BundleOptions {
            max_depth: 2,
            ..BundleOptions::default()
        },
        BundleOptions {
            max_references: 1,
            ..BundleOptions::default()
        },
        BundleOptions {
            max_graph_bytes: 1,
            ..BundleOptions::default()
        },
        BundleOptions {
            max_total_bytes: 16,
            ..BundleOptions::default()
        },
    ] {
        assert!(matches!(
            verify(&output, &options, &CancellationToken::new()),
            Err(BundleError::Limit(_))
        ));
    }
}

#[test]
fn uses_private_artifacts_and_refuses_symlink_aliases_and_destinations() {
    let fixture = fixture();
    let output = create_fixture_bundle(&fixture);
    assert_eq!(
        fs::metadata(&output)
            .expect("artifact metadata")
            .permissions()
            .mode()
            & 0o7777,
        0o600
    );
    let alias = fixture.temp.path().join("alias");
    symlink(fixture.temp.path(), &alias).expect("owned alias");
    assert!(
        inspect(
            &alias.join("source.izubundle"),
            &BundleOptions::default(),
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(
        restore(
            &output,
            &alias.join("unpublished"),
            &RestoreOptions::default(),
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(!fixture.temp.path().join("unpublished").exists());
    let protected = fixture.temp.path().join("dangling-target");
    symlink("does-not-exist", &protected).expect("dangling user symlink");
    assert!(matches!(
        restore(
            &output,
            &protected,
            &RestoreOptions::default(),
            &CancellationToken::new()
        ),
        Err(BundleError::DestinationExists)
    ));
    assert_eq!(
        fs::read_link(&protected).expect("preserved dangling link"),
        Path::new("does-not-exist")
    );
}

#[test]
fn refuses_unknown_native_fields_traversal_oversized_inputs_and_existing_destinations() {
    let fixture = fixture();
    let output = create_fixture_bundle(&fixture);
    let bytes = fs::read(&output).expect("bundle");
    let (manifest, records) = decode_records(&bytes);
    let tree_index = records
        .iter()
        .position(|record| record.kind == ObjectKind::Tree)
        .expect("tree");
    let mut unknown = records.clone();
    unknown[tree_index].payload =
        b"{\"entries\":{},\"private_field\":\"must not be removed\"}".to_vec();
    unknown[tree_index].id = hash_object(
        ObjectKind::Tree,
        &unknown[tree_index].payload,
        &Limits::default(),
    )
    .expect("hash");
    assert!(matches!(
        verify_bytes(&fixture, &encode_records(manifest.clone(), &unknown)),
        Err(BundleError::Model(_))
    ));
    let blob = records
        .iter()
        .find(|record| record.kind == ObjectKind::Blob)
        .expect("blob")
        .id;
    let mut traversal = records;
    traversal[tree_index].payload = format!("{{\"entries\":{{\"../escape\":{{\"blob\":\"{blob}\",\"kind\":\"file\",\"mode\":{{\"kind\":\"regular\"}}}}}}}}").into_bytes();
    traversal[tree_index].id = hash_object(
        ObjectKind::Tree,
        &traversal[tree_index].payload,
        &Limits::default(),
    )
    .expect("hash");
    let hostile = fixture.temp.path().join("traversal.izubundle");
    fs::write(&hostile, encode_records(manifest, &traversal)).expect("hostile bundle");
    let attempted = fixture.temp.path().join("must-not-exist");
    assert!(
        restore(
            &hostile,
            &attempted,
            &RestoreOptions::default(),
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(!attempted.exists() && !fixture.temp.path().join("escape").exists());
    let mut oversized = bytes.clone();
    oversized[12..16].copy_from_slice(&u32::MAX.to_be_bytes());
    assert!(matches!(
        verify_bytes(&fixture, &oversized),
        Err(BundleError::Limit("manifest bytes"))
    ));
    let limited = BundleOptions {
        max_objects: 1,
        ..BundleOptions::default()
    };
    assert!(matches!(
        verify(&output, &limited, &CancellationToken::new()),
        Err(BundleError::Limit("objects"))
    ));
    let existing = fixture.temp.path().join("existing");
    fs::create_dir(&existing).expect("existing target");
    fs::write(existing.join("user-file"), b"preserve").expect("user source");
    assert!(matches!(
        restore(
            &output,
            &existing,
            &RestoreOptions::default(),
            &CancellationToken::new()
        ),
        Err(BundleError::DestinationExists)
    ));
    assert_eq!(
        fs::read(existing.join("user-file")).expect("preserved file"),
        b"preserve"
    );
    assert!(matches!(
        create(
            &fixture.repository,
            fixture.root,
            &output,
            &BundleOptions::default(),
            &CancellationToken::new()
        ),
        Err(BundleError::DestinationExists)
    ));
    assert_eq!(fs::read(output).expect("preserved artifact"), bytes);
}

#[test]
fn inspection_is_distinct_from_full_verification_and_cancellation_has_no_output() {
    let fixture = fixture();
    let output = create_fixture_bundle(&fixture);
    let mut bytes = fs::read(&output).expect("read");
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    fs::write(&output, bytes).expect("tamper trailer only");
    assert_eq!(
        inspect(
            &output,
            &BundleOptions::default(),
            &CancellationToken::new()
        )
        .expect("manifest inspection")
        .manifest
        .root_operation,
        fixture.root
    );
    assert!(
        verify(
            &output,
            &BundleOptions::default(),
            &CancellationToken::new()
        )
        .is_err()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    let attempted = fixture.temp.path().join("cancelled.izubundle");
    assert!(
        create(
            &fixture.repository,
            fixture.root,
            &attempted,
            &BundleOptions::default(),
            &cancelled
        )
        .is_err()
    );
    assert!(!attempted.exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn prepublication_disk_full_cancellation_and_postpublication_uncertainty() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let fixture = fixture();
    let mut options = BundleOptions {
        fault_hook: Some(FaultHook(Arc::new(|point| {
            if point == DurableBoundary::BeforePublication {
                Err(std::io::Error::from_raw_os_error(28))
            } else {
                Ok(())
            }
        }))),
        ..BundleOptions::default()
    };
    let no_space = fixture.temp.path().join("disk-full.izubundle");
    assert!(
        create(
            &fixture.repository,
            fixture.root,
            &no_space,
            &options,
            &CancellationToken::new()
        )
        .is_err()
    );
    assert!(!no_space.exists());
    let cancel = CancellationToken::new();
    let signal = cancel.clone();
    options.fault_hook = Some(FaultHook(Arc::new(move |point| {
        if point == DurableBoundary::DataWritten {
            signal.cancel();
        }
        Ok(())
    })));
    let cancelled = fixture
        .temp
        .path()
        .join("cancel-before-publication.izubundle");
    assert!(
        create(
            &fixture.repository,
            fixture.root,
            &cancelled,
            &options,
            &cancel
        )
        .is_err()
    );
    assert!(!cancelled.exists());
    options.fault_hook = Some(FaultHook(Arc::new(|point| {
        if point == DurableBoundary::Published {
            Err(std::io::Error::from_raw_os_error(28))
        } else {
            Ok(())
        }
    })));
    let uncertain = fixture.temp.path().join("uncertain.izubundle");
    let receipt = create(
        &fixture.repository,
        fixture.root,
        &uncertain,
        &options,
        &CancellationToken::new(),
    )
    .expect("visible uncertainty receipt");
    assert!(matches!(
        receipt.publication,
        Publication::VisibleButUncertain { .. }
    ));
    verify(
        &uncertain,
        &BundleOptions::default(),
        &CancellationToken::new(),
    )
    .expect("visible exact artifact");
    let mut restore_options = RestoreOptions::default();
    restore_options.bundle.fault_hook = Some(FaultHook(Arc::new(|point| {
        if point == DurableBoundary::RestoreBeforePublication {
            Err(std::io::Error::from_raw_os_error(28))
        } else {
            Ok(())
        }
    })));
    let failed_restore = fixture.temp.path().join("failed-restore");
    let failure = restore(
        &uncertain,
        &failed_restore,
        &restore_options,
        &CancellationToken::new(),
    )
    .expect_err("disk-full failure keeps a private recovery stage");
    let failed_stage = match failure {
        BundleError::StagingRetained {
            stage,
            recovery_operation,
            ..
        } => {
            assert!(recovery_operation.is_some());
            stage
        }
        error => panic!("expected retained stage: {error}"),
    };
    assert_eq!(
        fs::read(failed_stage.join("hello.txt")).expect("retained source"),
        b"captured text\n"
    );
    assert!(!failed_restore.exists());
    restore_options.bundle.fault_hook = Some(FaultHook(Arc::new(|point| {
        if point == DurableBoundary::RestorePublished {
            Err(std::io::Error::from_raw_os_error(28))
        } else {
            Ok(())
        }
    })));
    let visible_restore = fixture.temp.path().join("visible-restore");
    let receipt = restore(
        &uncertain,
        &visible_restore,
        &restore_options,
        &CancellationToken::new(),
    )
    .expect("visible restore uncertainty");
    assert!(matches!(
        receipt.publication,
        Publication::VisibleButUncertain { .. }
    ));
    assert_eq!(
        fs::read(visible_restore.join("hello.txt")).expect("visible captured source"),
        b"captured text\n"
    );
    let restored = Repository::open(&visible_restore, RepositoryOptions::default())
        .expect("visible repository");
    assert_eq!(
        restored
            .current_operation(&CancellationToken::new())
            .expect("visible operation"),
        receipt.recovery.operation
    );
    let restore_cancel = CancellationToken::new();
    let restore_signal = restore_cancel.clone();
    restore_options.bundle.fault_hook = Some(FaultHook(Arc::new(move |point| {
        if point == DurableBoundary::RestoreStaged {
            restore_signal.cancel();
        }
        Ok(())
    })));
    let cancelled_restore = fixture.temp.path().join("cancelled-restore");
    let failure = restore(
        &uncertain,
        &cancelled_restore,
        &restore_options,
        &restore_cancel,
    )
    .expect_err("cancellation keeps a private recovery stage");
    assert!(matches!(failure, BundleError::StagingRetained { .. }));
    assert!(!cancelled_restore.exists());
}

#[cfg(feature = "fault-injection")]
fn stage_path(parent: &Path, prefix: &str) -> PathBuf {
    fs::read_dir(parent)
        .expect("owned parent")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with(prefix)
        })
        .expect("created stage")
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_restore_stage_is_preserved_on_failure() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let fixture = fixture();
    let input = create_fixture_bundle(&fixture);
    let parent = fixture.temp.path().to_path_buf();
    let hook_parent = parent.clone();
    let options = RestoreOptions {
        bundle: BundleOptions {
            fault_hook: Some(FaultHook(Arc::new(move |point| {
                if point == DurableBoundary::RestoreStaged {
                    let stage = stage_path(&hook_parent, ".izu-restore-");
                    fs::rename(&stage, hook_parent.join("displaced-owned-stage"))?;
                    fs::create_dir(&stage)?;
                    fs::write(stage.join("user-file"), b"preserve replacement user data")?;
                    return Err(std::io::Error::from_raw_os_error(28));
                }
                Ok(())
            }))),
            ..BundleOptions::default()
        },
        ..RestoreOptions::default()
    };
    let destination = parent.join("must-not-publish");
    let error = restore(&input, &destination, &options, &CancellationToken::new())
        .expect_err("changed staging name must block cleanup");
    assert!(matches!(error, BundleError::StagingCleanup { .. }));
    let replacement = stage_path(&parent, ".izu-restore-");
    assert_eq!(
        fs::read(replacement.join("user-file")).expect("replacement user data must survive"),
        b"preserve replacement user data"
    );
    assert!(parent.join("displaced-owned-stage/.izu").is_dir());
    assert!(!destination.exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_artifact_stage_is_preserved_on_failure() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let fixture = fixture();
    let parent = fixture.temp.path().to_path_buf();
    let hook_parent = parent.clone();
    let options = BundleOptions {
        fault_hook: Some(FaultHook(Arc::new(move |point| {
            if point == DurableBoundary::BeforePublication {
                let stage = stage_path(&hook_parent, ".izu-bundle-");
                fs::rename(&stage, hook_parent.join("displaced-owned-file"))?;
                fs::write(&stage, b"preserve replacement user data")?;
                return Err(std::io::Error::from_raw_os_error(28));
            }
            Ok(())
        }))),
        ..BundleOptions::default()
    };
    let destination = parent.join("must-not-publish.izubundle");
    assert!(
        create(
            &fixture.repository,
            fixture.root,
            &destination,
            &options,
            &CancellationToken::new()
        )
        .is_err()
    );
    let replacement = stage_path(&parent, ".izu-bundle-");
    assert_eq!(
        fs::read(&replacement).expect("replacement user data must survive"),
        b"preserve replacement user data"
    );
    verify(
        &parent.join("displaced-owned-file"),
        &BundleOptions::default(),
        &CancellationToken::new(),
    )
    .expect("original captured artifact retained");
    assert!(!destination.exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_artifact_stage_cannot_be_acknowledged_as_the_captured_bundle() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let fixture = fixture();
    let parent = fixture.temp.path().to_path_buf();
    let hook_parent = parent.clone();
    let options = BundleOptions {
        fault_hook: Some(FaultHook(Arc::new(move |point| {
            if point == DurableBoundary::BeforePublication {
                let stage = stage_path(&hook_parent, ".izu-bundle-");
                fs::rename(&stage, hook_parent.join("displaced-owned-file"))?;
                fs::write(&stage, b"replacement is not a bundle")?;
            }
            Ok(())
        }))),
        ..BundleOptions::default()
    };
    let destination = parent.join("must-not-publish.izubundle");
    create(
        &fixture.repository,
        fixture.root,
        &destination,
        &options,
        &CancellationToken::new(),
    )
    .expect_err("substitution must not receive a durable archive receipt");
    let replacement = stage_path(&parent, ".izu-bundle-");
    assert_eq!(
        fs::read(&replacement).expect("preserved replacement"),
        b"replacement is not a bundle"
    );
    assert!(!destination.exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_restore_stage_cannot_be_published_as_the_restored_repository() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let fixture = fixture();
    let input = create_fixture_bundle(&fixture);
    let parent = fixture.temp.path().to_path_buf();
    let hook_parent = parent.clone();
    let options = RestoreOptions {
        bundle: BundleOptions {
            fault_hook: Some(FaultHook(Arc::new(move |point| {
                if point == DurableBoundary::RestoreBeforePublication {
                    let stage = stage_path(&hook_parent, ".izu-restore-");
                    fs::rename(&stage, hook_parent.join("displaced-owned-stage"))?;
                    fs::create_dir(&stage)?;
                    fs::write(stage.join("user-file"), b"replacement is not a repository")?;
                }
                Ok(())
            }))),
            ..BundleOptions::default()
        },
        ..RestoreOptions::default()
    };
    let destination = parent.join("must-not-publish");
    restore(&input, &destination, &options, &CancellationToken::new())
        .expect_err("substitution must not receive a durable restore receipt");
    let replacement = stage_path(&parent, ".izu-restore-");
    assert_eq!(
        fs::read(replacement.join("user-file")).expect("preserved replacement"),
        b"replacement is not a repository"
    );
    assert!(!destination.exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_artifact_parent_prevents_false_durable_acknowledgement() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    for boundary in [
        DurableBoundary::BeforePublication,
        DurableBoundary::Published,
    ] {
        let fixture = fixture();
        let parent = fixture.temp.path().join("publication-parent");
        fs::create_dir(&parent).expect("private publication parent");
        let displaced = fixture.temp.path().join("displaced-parent");
        let hook_parent = parent.clone();
        let hook_displaced = displaced.clone();
        let options = BundleOptions {
            fault_hook: Some(FaultHook(Arc::new(move |point| {
                if point == boundary {
                    fs::rename(&hook_parent, &hook_displaced)?;
                    fs::create_dir(&hook_parent)?;
                }
                Ok(())
            }))),
            ..BundleOptions::default()
        };
        let destination = parent.join("source.izubundle");
        let outcome = create(
            &fixture.repository,
            fixture.root,
            &destination,
            &options,
            &CancellationToken::new(),
        );
        assert_eq!(
            fs::read_dir(&parent).expect("replacement parent").count(),
            0
        );
        match boundary {
            DurableBoundary::BeforePublication => {
                outcome.expect_err("changed parent locator must block publication");
                assert!(!displaced.join("source.izubundle").exists());
            }
            DurableBoundary::Published => {
                let receipt = outcome.expect("visible uncertainty receipt");
                assert!(matches!(
                    receipt.publication,
                    Publication::VisibleButUncertain { .. }
                ));
                verify(
                    &displaced.join("source.izubundle"),
                    &BundleOptions::default(),
                    &CancellationToken::new(),
                )
                .expect("exact archive under displaced pinned parent");
            }
            _ => unreachable!("fixed boundaries"),
        }
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_restore_parent_preserves_stage_without_false_durable_acknowledgement() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    for boundary in [
        DurableBoundary::RestoreBeforePublication,
        DurableBoundary::RestorePublished,
    ] {
        let fixture = fixture();
        let input = create_fixture_bundle(&fixture);
        let parent = fixture.temp.path().join("publication-parent");
        fs::create_dir(&parent).expect("private publication parent");
        let displaced = fixture.temp.path().join("displaced-parent");
        let hook_parent = parent.clone();
        let hook_displaced = displaced.clone();
        let options = RestoreOptions {
            bundle: BundleOptions {
                fault_hook: Some(FaultHook(Arc::new(move |point| {
                    if point == boundary {
                        fs::rename(&hook_parent, &hook_displaced)?;
                        fs::create_dir(&hook_parent)?;
                    }
                    Ok(())
                }))),
                ..BundleOptions::default()
            },
            ..RestoreOptions::default()
        };
        let destination = parent.join("restored");
        let outcome = restore(&input, &destination, &options, &CancellationToken::new());
        assert_eq!(
            fs::read_dir(&parent).expect("replacement parent").count(),
            0
        );
        let captured = match boundary {
            DurableBoundary::RestoreBeforePublication => {
                assert!(matches!(outcome, Err(BundleError::StagingCleanup { .. })));
                assert!(!displaced.join("restored").exists());
                stage_path(&displaced, ".izu-restore-")
            }
            DurableBoundary::RestorePublished => {
                let receipt = outcome.expect("visible uncertainty receipt");
                assert!(matches!(
                    receipt.publication,
                    Publication::VisibleButUncertain { .. }
                ));
                displaced.join("restored")
            }
            _ => unreachable!("fixed boundaries"),
        };
        assert_eq!(
            fs::read(captured.join("hello.txt")).expect("preserved captured source"),
            b"captured text\n"
        );
        assert!(captured.join(".izu").is_dir());
    }
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_restore_stage_before_initialization_cannot_redirect_writes() {
    replaced_stage_cannot_redirect_writes(izu_bundle::DurableBoundary::RestoreStageCreated);
}

#[cfg(feature = "fault-injection")]
#[test]
fn replaced_restore_stage_before_adoption_cannot_redirect_writes() {
    replaced_stage_cannot_redirect_writes(izu_bundle::DurableBoundary::RestoreObjectsImported);
}

#[cfg(feature = "fault-injection")]
fn replaced_stage_cannot_redirect_writes(boundary: izu_bundle::DurableBoundary) {
    use izu_bundle::FaultHook;
    use std::sync::Arc;
    let fixture = fixture();
    let input = create_fixture_bundle(&fixture);
    let parent = fixture.temp.path().to_path_buf();
    let hook_parent = parent.clone();
    let options = RestoreOptions {
        bundle: BundleOptions {
            fault_hook: Some(FaultHook(Arc::new(move |point| {
                if point == boundary {
                    let stage = stage_path(&hook_parent, ".izu-restore-");
                    fs::rename(&stage, hook_parent.join("displaced-owned-stage"))?;
                    fs::create_dir(&stage)?;
                }
                Ok(())
            }))),
            ..BundleOptions::default()
        },
        ..RestoreOptions::default()
    };
    let destination = parent.join("must-not-publish");
    restore(&input, &destination, &options, &CancellationToken::new())
        .expect_err("stage substitution must block final publication");
    let replacement = stage_path(&parent, ".izu-restore-");
    assert_eq!(
        fs::read_dir(&replacement)
            .expect("replacement readback")
            .count(),
        0,
        "initialization and source adoption must not write into the replacement directory"
    );
    assert_eq!(
        fs::read(parent.join("displaced-owned-stage/hello.txt"))
            .expect("pinned original staging source"),
        b"captured text\n"
    );
    assert!(parent.join("displaced-owned-stage/.izu").is_dir());
    assert!(!destination.exists());
}

#[cfg(feature = "fault-injection")]
#[test]
fn unknown_work_added_to_failed_restore_stage_is_retained() {
    use izu_bundle::{DurableBoundary, FaultHook};
    use std::sync::Arc;
    let fixture = fixture();
    let input = create_fixture_bundle(&fixture);
    let parent = fixture.temp.path().to_path_buf();
    let hook_parent = parent.clone();
    let options = RestoreOptions {
        bundle: BundleOptions {
            fault_hook: Some(FaultHook(Arc::new(move |point| {
                if point == DurableBoundary::RestoreStaged {
                    let stage = stage_path(&hook_parent, ".izu-restore-");
                    fs::write(
                        stage.join("unique-user-file"),
                        b"unique work from another writer",
                    )?;
                    return Err(std::io::Error::from_raw_os_error(28));
                }
                Ok(())
            }))),
            ..BundleOptions::default()
        },
        ..RestoreOptions::default()
    };
    let destination = parent.join("must-not-publish");
    let error = restore(&input, &destination, &options, &CancellationToken::new())
        .expect_err("failed restore retains unique work");
    let reported_stage = match error {
        BundleError::StagingRetained {
            stage,
            recovery_operation,
            ..
        } => {
            assert!(recovery_operation.is_some());
            stage
        }
        error => panic!("expected retained stage: {error}"),
    };
    let retained = fs::read_dir(&parent)
        .expect("parent readback")
        .map(|entry| entry.expect("entry").path())
        .find(|path| {
            path.file_name()
                .expect("name")
                .to_string_lossy()
                .starts_with(".izu-restore-")
        });
    assert!(
        retained.is_some(),
        "a stage containing unknown unique work must survive failure"
    );
    assert_eq!(retained.as_ref().expect("retained stage"), &reported_stage);
    assert_eq!(
        fs::read(retained.expect("retained stage").join("unique-user-file"))
            .expect("unique work readback"),
        b"unique work from another writer"
    );
    assert!(!destination.exists());
}
