use super::*;
use crate::{Identity, OperationId, Repository, RepositoryOptions, RevisionId, Selection};
use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

type TestResult = std::result::Result<(), Box<dyn std::error::Error>>;
const BOUNDARIES: [Boundary; 6] = [
    Boundary::TargetPrepared,
    Boundary::FileCreated,
    Boundary::FileDataSynced,
    Boundary::EntryPublished,
    Boundary::DirectoriesSubmitted,
    Boundary::DirectorySynced,
];

struct Fixture {
    repository: Repository,
    directory: tempfile::TempDir,
    primary: PathBuf,
    target: PathBuf,
    revision: RevisionId,
    operation: OperationId,
}

fn fixture() -> std::result::Result<Fixture, Box<dyn std::error::Error>> {
    let scratch = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
    fs::create_dir_all(&scratch)?;
    let directory = tempfile::tempdir_in(scratch.canonicalize()?)?;
    let primary = directory.path().join("primary");
    let repository = Repository::init(&primary, RepositoryOptions::default())?;
    fs::create_dir(primary.join("src"))?;
    fs::write(primary.join("src/text"), b"retained source\n")?;
    fs::set_permissions(primary.join("src/text"), fs::Permissions::from_mode(0o640))?;
    fs::write(primary.join("binary"), [0, 255, 1, 128, 13, 10])?;
    symlink("src/text", primary.join("link"))?;
    let cancel = CancellationToken::new();
    let workspace = repository.workspace(repository.workspace_id(), &cancel)?;
    let commit = repository.commit(
        workspace.id,
        workspace.expected,
        Selection::All,
        "fresh source fixture".into(),
        Identity {
            name: "izu fixture".into(),
            email: "fixture@izu.invalid".into(),
        },
        &cancel,
    )?;
    let target = directory.path().join("target");
    Ok(Fixture {
        directory,
        repository,
        primary,
        target,
        revision: commit.revision,
        operation: commit.operation,
    })
}

fn assert_retained(fixture: &Fixture) -> TestResult {
    let cancel = CancellationToken::new();
    let reopened = Repository::open(&fixture.primary, RepositoryOptions::default())?;
    assert_eq!(reopened.current_operation(&cancel)?, fixture.operation);
    assert!(
        !reopened
            .view(&cancel)?
            .workspaces
            .values()
            .any(|record| record.root == fixture.target.to_string_lossy())
    );
    assert_eq!(
        fs::read(fixture.primary.join("src/text"))?,
        b"retained source\n"
    );
    assert_eq!(
        fs::read(fixture.primary.join("binary"))?,
        [0, 255, 1, 128, 13, 10]
    );
    assert_eq!(
        fs::read_link(fixture.primary.join("link"))?,
        Path::new("src/text")
    );
    assert!(!fixture.target.join(".izu").exists());
    reopened.verify(&cancel)?;
    Ok(())
}

#[test]
fn empty_fork_reopens_with_exact_bytes_modes_symlinks_and_receipt() -> TestResult {
    let fixture = fixture()?;
    let cancel = CancellationToken::new();
    let workspace = fixture.repository.fork_workspace(
        "fresh".into(),
        &fixture.target,
        fixture.revision,
        &cancel,
    )?;
    let acknowledged = fixture.repository.current_operation(&cancel)?;
    let reopened = Repository::open(&fixture.target, RepositoryOptions::default())?;
    assert_eq!(reopened.workspace_id(), workspace.id);
    assert_eq!(reopened.current_operation(&cancel)?, acknowledged);
    assert_eq!(
        fs::read(fixture.target.join("src/text"))?,
        b"retained source\n"
    );
    assert_eq!(
        fs::metadata(fixture.target.join("src/text"))?.mode() & 0o7777,
        0o640
    );
    assert_eq!(
        fs::metadata(fixture.target.join("src"))?.mode() & 0o7777,
        fs::metadata(fixture.primary.join("src"))?.mode() & 0o7777
    );
    assert_eq!(
        fs::read(fixture.target.join("binary"))?,
        [0, 255, 1, 128, 13, 10]
    );
    assert_eq!(
        fs::read_link(fixture.target.join("link"))?,
        Path::new("src/text")
    );
    assert_eq!(fs::metadata(fixture.primary.join("src/text"))?.nlink(), 1);
    assert_eq!(fs::metadata(fixture.target.join("src/text"))?.nlink(), 1);
    reopened.verify(&cancel)?;
    Ok(())
}

struct ReadonlyCleanup(Vec<Directory>);
impl ReadonlyCleanup {
    fn finish(mut self) -> std_io::Result<()> {
        for directory in &self.0 {
            directory
                .file()
                .set_permissions(fs::Permissions::from_mode(0o700))?;
        }
        self.0.clear();
        Ok(())
    }
}
impl Drop for ReadonlyCleanup {
    fn drop(&mut self) {
        for directory in &self.0 {
            let _ = directory
                .file()
                .set_permissions(fs::Permissions::from_mode(0o700));
        }
    }
}

#[test]
fn nested_unicode_and_readonly_modes_survive_registered_fork() -> TestResult {
    let mut fixture = fixture()?;
    let mut cleanup = ReadonlyCleanup(Vec::new());
    fs::create_dir(fixture.primary.join("α"))?;
    fs::create_dir(fixture.primary.join("α/雪"))?;
    fs::write(
        fixture.primary.join("α/雪/read-only"),
        b"nested unicode source",
    )?;
    fs::set_permissions(
        fixture.primary.join("α/雪/read-only"),
        fs::Permissions::from_mode(0o444),
    )?;
    for relative in ["α", "α/雪"] {
        let directory = Directory::open(&fixture.primary.join(relative))?;
        directory
            .file()
            .set_permissions(fs::Permissions::from_mode(0o555))?;
        cleanup.0.push(directory);
    }
    let cancel = CancellationToken::new();
    let workspace = fixture
        .repository
        .workspace(fixture.repository.workspace_id(), &cancel)?;
    let committed = fixture.repository.commit(
        workspace.id,
        workspace.expected,
        Selection::All,
        "nested readonly source".into(),
        Identity {
            name: "izu fixture".into(),
            email: "fixture@izu.invalid".into(),
        },
        &cancel,
    )?;
    fixture.revision = committed.revision;
    fixture.operation = committed.operation;
    let private = fixture.repository.fork_workspace(
        "unicode".into(),
        &fixture.target,
        fixture.revision,
        &cancel,
    )?;
    for relative in ["α", "α/雪"] {
        let directory = Directory::open(&fixture.target.join(relative))?;
        assert_eq!(directory.metadata_self()?.mode() & 0o7777, 0o555);
        cleanup.0.push(directory);
    }
    assert_eq!(
        fs::read(fixture.target.join("α/雪/read-only"))?,
        b"nested unicode source"
    );
    assert_eq!(
        fs::metadata(fixture.target.join("α/雪/read-only"))?.mode() & 0o7777,
        0o444
    );
    let reopened = Repository::open(&fixture.target, RepositoryOptions::default())?;
    assert_eq!(reopened.workspace_id(), private.id);
    assert_eq!(
        reopened.workspace(private.id, &cancel)?.expected,
        private.expected
    );
    reopened.verify(&cancel)?;
    cleanup.finish()?;
    Ok(())
}

#[test]
fn fresh_target_checks_pinned_emptiness_and_budget_before_source_mutation() -> TestResult {
    let fixture = fixture()?;
    fs::create_dir(&fixture.target)?;
    fs::write(fixture.target.join("unknown"), b"preserve unknown source")?;
    let root = Directory::open(&fixture.target)?;
    let cancel = CancellationToken::new();
    assert!(matches!(
        FreshTarget::new(&root, &fixture.target, &Tree::default(), 4096, &cancel),
        Err(EngineError::DirectoryNotEmpty(_))
    ));
    assert!(!fixture.target.join(".izu-recovery").exists());
    assert_eq!(
        fs::read(fixture.target.join("unknown"))?,
        b"preserve unknown source"
    );
    assert!(matches!(
        fixture.repository.fork_workspace(
            "nonempty".into(),
            &fixture.target,
            fixture.revision,
            &cancel
        ),
        Err(EngineError::DirectoryNotEmpty(_))
    ));
    assert_retained(&fixture)?;
    let empty = fixture.directory.path().join("empty");
    fs::create_dir(&empty)?;
    let root = Directory::open(&empty)?;
    let tree = fixture.repository.tree(
        fixture.repository.revision(fixture.revision, &cancel)?.tree,
        &cancel,
    )?;
    assert!(matches!(
        FreshTarget::new(&root, &empty, &tree, 1, &cancel),
        Err(EngineError::Limit { .. })
    ));
    assert_eq!(fs::read_dir(&empty)?.count(), 0);
    let moved = fixture.directory.path().join("moved-empty");
    fs::rename(&empty, &moved)?;
    fs::create_dir(&empty)?;
    fs::write(empty.join("unknown"), b"replacement root")?;
    assert!(matches!(
        FreshTarget::new(&root, &empty, &Tree::default(), 4096, &cancel),
        Err(EngineError::SourceChanged(_))
    ));
    assert_eq!(fs::read(empty.join("unknown"))?, b"replacement root");
    assert_eq!(fs::read_dir(&moved)?.count(), 0);
    Ok(())
}

#[test]
fn enospc_and_cancellation_at_each_fresh_boundary_never_register() -> TestResult {
    for boundary in BOUNDARIES {
        for cancelled in [false, true] {
            let fixture = fixture()?;
            let cancel = CancellationToken::new();
            let signal = cancel.clone();
            let observed = Arc::new(AtomicBool::new(false));
            let injected = Arc::clone(&observed);
            let _hook = test_support::install(move |current, _| {
                if current == boundary && !injected.swap(true, Ordering::SeqCst) {
                    if cancelled {
                        signal.cancel();
                    } else {
                        return Err(std_io::Error::from_raw_os_error(28));
                    }
                }
                Ok(())
            });
            let result = fixture.repository.fork_workspace(
                "failed".into(),
                &fixture.target,
                fixture.revision,
                &cancel,
            );
            assert!(observed.load(Ordering::SeqCst), "missed {boundary:?}");
            if cancelled {
                assert!(
                    matches!(result, Err(EngineError::Cancelled)),
                    "{boundary:?}: {result:?}"
                );
            } else {
                assert!(
                    matches!(result, Err(EngineError::Io { ref source, .. }) if source.raw_os_error() == Some(28)),
                    "{boundary:?}: {result:?}"
                );
            }
            drop(_hook);
            assert_retained(&fixture)?;
            if matches!(boundary, Boundary::FileCreated) {
                assert!(
                    fixture.target.join("binary").is_file(),
                    "fresh creation was unlinked"
                );
                assert_eq!(fs::metadata(fixture.target.join("binary"))?.len(), 0);
            }
            if matches!(
                boundary,
                Boundary::FileDataSynced
                    | Boundary::EntryPublished
                    | Boundary::DirectoriesSubmitted
                    | Boundary::DirectorySynced
            ) {
                assert_eq!(
                    fs::read(fixture.target.join("binary"))?,
                    [0, 255, 1, 128, 13, 10]
                );
            }
        }
    }
    Ok(())
}

#[test]
fn replaced_direct_name_is_retained_and_original_fd_writes_only_displaced_file() -> TestResult {
    let fixture = fixture()?;
    let retained = fixture.directory.path().join("retained-created-file");
    let saved = retained.clone();
    let target = fixture.target.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let injected = Arc::clone(&observed);
    let _hook = test_support::install(move |at, _| {
        if at == Boundary::FileCreated && !injected.swap(true, Ordering::SeqCst) {
            fs::rename(target.join("binary"), &saved)?;
            fs::write(target.join("binary"), b"unknown replacement")?;
        }
        Ok(())
    });
    let result = fixture.repository.fork_workspace(
        "tampered".into(),
        &fixture.target,
        fixture.revision,
        &CancellationToken::new(),
    );
    assert!(
        matches!(result, Err(EngineError::SourceChanged(_))),
        "{result:?}"
    );
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(fs::read(retained)?, [0, 255, 1, 128, 13, 10]);
    assert_eq!(
        fs::read(fixture.target.join("binary"))?,
        b"unknown replacement"
    );
    drop(_hook);
    assert_retained(&fixture)
}

#[test]
fn exclusive_symlink_collision_keeps_unknown_and_prior_named_source() -> TestResult {
    let fixture = fixture()?;
    let target = fixture.target.clone();
    let observed = Arc::new(AtomicBool::new(false));
    let injected = Arc::clone(&observed);
    let _hook = test_support::install(move |at, _| {
        if at == Boundary::FileCreated && !injected.swap(true, Ordering::SeqCst) {
            fs::write(target.join("link"), b"unknown collision")?;
        }
        Ok(())
    });
    let result = fixture.repository.fork_workspace(
        "collision".into(),
        &fixture.target,
        fixture.revision,
        &CancellationToken::new(),
    );
    assert!(
        matches!(result, Err(EngineError::Io { ref source, .. }) if source.kind() == std_io::ErrorKind::AlreadyExists),
        "{result:?}"
    );
    assert!(observed.load(Ordering::SeqCst));
    assert_eq!(
        fs::read(fixture.target.join("binary"))?,
        [0, 255, 1, 128, 13, 10]
    );
    assert_eq!(fs::read(fixture.target.join("link"))?, b"unknown collision");
    drop(_hook);
    assert_retained(&fixture)
}

#[test]
fn equal_byte_entry_and_directory_substitution_fail_on_both_sides_of_barrier() -> TestResult {
    for boundary in [Boundary::DirectoriesSubmitted, Boundary::DirectorySynced] {
        for kind in ["binary", "src", "link"] {
            let fixture = fixture()?;
            let saved = fixture.directory.path().join("retained-entry");
            let retained = saved.clone();
            let injected = Arc::new(AtomicBool::new(false));
            let observed = Arc::clone(&injected);
            let target = fixture.target.clone();
            let _hook = test_support::install(move |at, _| {
                if at == boundary && !injected.swap(true, Ordering::SeqCst) {
                    let path = target.join(kind);
                    fs::rename(&path, &retained)?;
                    match kind {
                        "src" => {
                            fs::create_dir(&path)?;
                            fs::write(path.join("text"), b"retained source\n")?;
                            fs::set_permissions(
                                path.join("text"),
                                fs::Permissions::from_mode(0o640),
                            )?;
                            fs::set_permissions(&path, fs::metadata(&retained)?.permissions())?;
                        }
                        "link" => symlink("src/text", &path)?,
                        "binary" => {
                            fs::write(&path, [0, 255, 1, 128, 13, 10])?;
                            fs::set_permissions(&path, fs::metadata(&retained)?.permissions())?;
                        }
                        _ => unreachable!(),
                    }
                }
                Ok(())
            });
            let result = fixture.repository.fork_workspace(
                "replaced".into(),
                &fixture.target,
                fixture.revision,
                &CancellationToken::new(),
            );
            assert!(
                matches!(result, Err(EngineError::SourceChanged(_))),
                "{boundary:?}: {result:?}"
            );
            assert!(observed.load(Ordering::SeqCst));
            let retained_metadata = fs::symlink_metadata(&saved)?;
            if kind == "link" {
                assert!(retained_metadata.file_type().is_symlink());
                assert_eq!(fs::read_link(&saved)?, Path::new("src/text"));
            }
            match kind {
                "src" => assert_eq!(
                    fs::read(fixture.target.join("src/text"))?,
                    b"retained source\n"
                ),
                "link" => assert_eq!(
                    fs::read_link(fixture.target.join("link"))?,
                    Path::new("src/text")
                ),
                "binary" => assert_eq!(
                    fs::read(fixture.target.join("binary"))?,
                    [0, 255, 1, 128, 13, 10]
                ),
                _ => unreachable!(),
            }
            drop(_hook);
            assert_retained(&fixture)?;
        }
    }
    Ok(())
}

#[test]
fn source_root_substitution_preserves_unknown_namespace_and_displaced_source() -> TestResult {
    for boundary in [Boundary::DirectoriesSubmitted, Boundary::DirectorySynced] {
        let fixture = fixture()?;
        let retained = fixture.directory.path().join("retained-directory");
        let saved = retained.clone();
        let target = fixture.target.clone();
        let observed = Arc::new(AtomicBool::new(false));
        let injected = Arc::clone(&observed);
        let _hook = test_support::install(move |at, _| {
            if at == boundary && !injected.swap(true, Ordering::SeqCst) {
                fs::rename(&target, &saved)?;
                fs::create_dir(&target)?;
                fs::write(target.join("unknown"), b"preserve unknown namespace")?;
            }
            Ok(())
        });
        let result = fixture.repository.fork_workspace(
            "root-tamper".into(),
            &fixture.target,
            fixture.revision,
            &CancellationToken::new(),
        );
        assert!(
            matches!(result, Err(EngineError::SourceChanged(_))),
            "{result:?}"
        );
        assert!(observed.load(Ordering::SeqCst));
        assert_eq!(
            fs::read(fixture.target.join("unknown"))?,
            b"preserve unknown namespace"
        );
        assert_eq!(fs::read(retained.join("src/text"))?, b"retained source\n");
        assert_eq!(fs::read(retained.join("binary"))?, [0, 255, 1, 128, 13, 10]);
        drop(_hook);
        assert_retained(&fixture)?;
    }
    Ok(())
}

#[test]
fn cross_device_participant_is_refused() {
    let source = EntryIdentity {
        device: 1,
        inode: 2,
    };
    let other = EntryIdentity {
        device: 3,
        inode: 4,
    };
    assert!(
        matches!(source.require_device(other, Path::new("owned-source")),
        Err(EngineError::Io { source, .. }) if source.kind() == std_io::ErrorKind::Unsupported)
    );
}

struct OwnedChild {
    child: Child,
    reap_started: Option<Instant>,
}
impl OwnedChild {
    fn stop(&mut self) -> std_io::Result<()> {
        if self.child.try_wait()?.is_some() {
            return Ok(());
        }
        let start = *self.reap_started.get_or_insert_with(Instant::now);
        self.child.kill()?;
        loop {
            if self.child.try_wait()?.is_some() {
                return Ok(());
            }
            if start.elapsed() >= Duration::from_secs(10) {
                return Err(std_io::Error::new(
                    std_io::ErrorKind::TimedOut,
                    format!(
                        "owned child {} was not reaped within ten seconds",
                        self.child.id()
                    ),
                ));
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[test]
fn killed_fresh_fork_reopens_without_registered_or_lost_source() -> TestResult {
    for boundary in [
        Boundary::FileDataSynced,
        Boundary::DirectoriesSubmitted,
        Boundary::DirectorySynced,
    ] {
        let fixture = fixture()?;
        let marker = fixture.directory.path().join("ready");
        let mut child = OwnedChild {
            child: Command::new(std::env::current_exe()?)
                .args([
                    "--exact",
                    "source::fresh::tests::fresh_process_child",
                    "--nocapture",
                ])
                .current_dir(fixture.directory.path())
                .env_clear()
                .env("TMPDIR", fixture.directory.path())
                .env("IZU_FRESH_TEST_PRIMARY", &fixture.primary)
                .env("IZU_FRESH_TEST_TARGET", &fixture.target)
                .env("IZU_FRESH_TEST_BOUNDARY", format!("{boundary:?}"))
                .env("IZU_FRESH_TEST_READY", &marker)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?,
            reap_started: None,
        };
        let start = Instant::now();
        while !marker.exists() {
            assert!(
                child.child.try_wait()?.is_none(),
                "child exited before {boundary:?}"
            );
            assert!(
                start.elapsed() < Duration::from_secs(10),
                "child missed {boundary:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        child.stop()?;
        assert_retained(&fixture)?;
        let reopened = Repository::open(&fixture.primary, RepositoryOptions::default())?;
        let recovered = reopened.recover(&CancellationToken::new())?;
        assert_eq!(recovered.head, Some(fixture.operation));
        let retry = fixture.directory.path().join("retry");
        reopened.fork_workspace(
            "retry".into(),
            retry,
            fixture.revision,
            &CancellationToken::new(),
        )?;
    }
    Ok(())
}

#[test]
fn fresh_process_child() -> TestResult {
    let Some(primary) = std::env::var_os("IZU_FRESH_TEST_PRIMARY") else {
        return Ok(());
    };
    let target = std::env::var_os("IZU_FRESH_TEST_TARGET").ok_or("missing owned target")?;
    let boundary = std::env::var("IZU_FRESH_TEST_BOUNDARY")?;
    let marker = std::env::var_os("IZU_FRESH_TEST_READY").ok_or("missing owned marker")?;
    let _hook = test_support::install(move |at, _| {
        if format!("{at:?}") == boundary {
            fs::write(&marker, b"ready")?;
            loop {
                std::thread::park();
            }
        }
        Ok(())
    });
    let repository = Repository::open(Path::new(&primary), RepositoryOptions::default())?;
    let cancel = CancellationToken::new();
    let primary = repository.workspace(repository.workspace_id(), &cancel)?;
    repository.fork_workspace(
        "killed".into(),
        Path::new(&target),
        primary.expected.head,
        &cancel,
    )?;
    Err("expected a blocked fresh boundary".into())
}
