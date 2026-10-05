use izu_engine::{Repository, RepositoryOptions, Selection};
use izu_git::{
    Error, ExportOptions, GitAdapter, GitObjectId, GitSignature, GitSource, GitToolConfig,
    ImportOptions, NonFastForwardPolicy, PushRequest, RefExport, RefImport,
};
use izu_model::{CancellationToken, Identity, RefName, TreeEntry};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn config() -> GitToolConfig {
    GitToolConfig {
        worker: Some(izu_git::WorkerLauncher {
            executable: PathBuf::from(env!("CARGO_BIN_EXE_izu-git-test-worker")),
            prefix_args: Vec::new(),
        }),
        ..GitToolConfig::default()
    }
}
fn adapter() -> GitAdapter {
    GitAdapter::new(config()).expect("installed Git fixture")
}
fn cancel() -> CancellationToken {
    CancellationToken::new()
}
fn reference(name: &str) -> RefName {
    RefName::new(name).expect("fixture ref")
}
fn git(directory: &Path, args: &[&str], input: Option<&[u8]>) -> Vec<u8> {
    let mut command = Command::new("git");
    command.current_dir(directory).env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    if let Some(directory) = std::env::var_os("TMPDIR") {
        command.env("TMPDIR", directory);
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_AUTHOR_NAME", "Source Author")
        .env("GIT_AUTHOR_EMAIL", "source@example.test")
        .env("GIT_AUTHOR_DATE", "1700000000 +0530")
        .env("GIT_COMMITTER_NAME", "Source Committer")
        .env("GIT_COMMITTER_EMAIL", "committer@example.test")
        .env("GIT_COMMITTER_DATE", "1700000001 -0800")
        .args([
            "-c",
            "core.hooksPath=/nonexistent-izu-fixture",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().expect("start fixture Git");
    if let Some(input) = input {
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(input)
            .expect("fixture input");
    } else {
        drop(child.stdin.take());
    }
    let output = child.wait_with_output().expect("fixture Git completion");
    assert!(
        output.status.success(),
        "fixture Git {:?}: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}
fn oid(directory: &Path, reference: &str) -> GitObjectId {
    String::from_utf8(git(directory, &["rev-parse", reference], None))
        .expect("ID text")
        .trim()
        .parse()
        .expect("ID")
}
fn committer() -> GitSignature {
    GitSignature {
        name: "Native Committer".into(),
        email: "native-committer@example.test".into(),
        timestamp_unix_seconds: 1800000000,
        timezone_minutes: 120,
    }
}
fn source_fixture(base: &Path) -> PathBuf {
    let source = base.join("source");
    fs::create_dir(&source).expect("source directory");
    git(
        &source,
        &["init", "--initial-branch=main", "--template="],
        None,
    );
    fs::create_dir(source.join("src")).expect("directory");
    fs::write(source.join("src/message.txt"), b"first\n").expect("source file");
    fs::write(source.join("run.sh"), b"#!/bin/sh\nexit 0\n").expect("executable source");
    #[cfg(unix)]
    {
        use std::os::unix::fs::{PermissionsExt, symlink};
        fs::set_permissions(source.join("run.sh"), fs::Permissions::from_mode(0o755))
            .expect("fixture mode");
        symlink("src/message.txt", source.join("link")).expect("fixture link");
    }
    git(&source, &["add", "--", "."], None);
    git(
        &source,
        &["commit", "-m", "First source\n\nExact body."],
        None,
    );
    source
}
fn native_commit(repository: &Repository, message: &str) -> izu_model::RevisionId {
    let token = cancel();
    let workspace = repository
        .workspace(repository.workspace_id(), &token)
        .expect("workspace");
    let receipt = repository
        .commit(
            workspace.id,
            workspace.expected,
            Selection::All,
            message.into(),
            Identity {
                name: "Native Author".into(),
                email: "native-author@example.test".into(),
            },
            &token,
        )
        .expect("native commit");
    receipt.revision
}

#[test]
fn native_commit_exports_to_clone() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("native");
    fs::create_dir(&root).expect("native directory");
    let repository = Repository::init(&root, RepositoryOptions::default()).expect("native init");
    fs::write(root.join("hello.txt"), b"native source\n").expect("source");
    let previous = repository.view(&cancel()).expect("view").refs[&reference("main")];
    let revision = native_commit(&repository, "Explicit native commit\n");
    repository
        .update_ref(
            &reference("main"),
            Some(previous),
            Some(revision),
            &cancel(),
        )
        .expect("native ref publication");
    let bare = fixture.path().join("export.git");
    let report = adapter()
        .export_to(
            &repository,
            &bare,
            &ExportOptions {
                refs: Vec::new(),
                committer: Some(committer()),
            },
            &cancel(),
        )
        .expect("export");
    let clone = fixture.path().join("git-clone");
    let clone_path = clone.to_str().expect("fixture path");
    let bare_path = bare.to_str().expect("fixture path");
    git(
        fixture.path(),
        &["clone", "--", bare_path, clone_path],
        None,
    );
    assert_eq!(
        fs::read(clone.join("hello.txt")).expect("Git cloned source"),
        b"native source\n"
    );
    assert_eq!(oid(&clone, "HEAD"), report.refs["refs/heads/main"]);
    assert_eq!(
        String::from_utf8(git(&clone, &["rev-list", "--count", "HEAD"], None))
            .expect("count")
            .trim(),
        "1"
    );
    let raw = git(&clone, &["cat-file", "commit", "HEAD"], None);
    assert!(
        String::from_utf8(raw)
            .expect("native commit")
            .contains("Native Author <native-author@example.test>")
    );
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("initialization anchor"))
    );
}

#[test]
fn unchanged_import_export_preserves_ids_and_full_metadata() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    git(&source, &["checkout", "-b", "topic"], None);
    fs::write(source.join("src/topic.txt"), b"topic\n").expect("topic");
    git(&source, &["add", "--", "."], None);
    git(&source, &["commit", "-m", "Topic"], None);
    git(&source, &["checkout", "main"], None);
    fs::write(source.join("src/main.txt"), b"main\n").expect("main");
    git(&source, &["add", "--", "."], None);
    git(&source, &["commit", "-m", "Main"], None);
    git(
        &source,
        &["merge", "--no-ff", "topic", "-m", "Merge with parent order"],
        None,
    );
    let main = oid(&source, "main");
    let topic = oid(&source, "topic");
    let native = fixture.path().join("native");
    let (repository, imported) = adapter()
        .clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    assert_eq!(
        fs::read(native.join("src/message.txt")).expect("native cloned source"),
        b"first\n"
    );
    let exported = fixture.path().join("roundtrip.git");
    let report = adapter()
        .export_to(&repository, &exported, &ExportOptions::default(), &cancel())
        .expect("unchanged export needs no fabricated committer");
    assert_eq!(report.refs["refs/heads/main"], main);
    assert_eq!(report.refs["refs/heads/topic"], topic);
    assert_eq!(
        git(&source, &["cat-file", "commit", &main.to_string()], None),
        git(&exported, &["cat-file", "commit", &main.to_string()], None)
    );
    assert_eq!(imported.revision_mapping.len(), 4);
    let tree = repository
        .tree(
            repository
                .revision(imported.revision_mapping[&main], &cancel())
                .expect("revision")
                .tree,
            &cancel(),
        )
        .expect("tree");
    assert!(matches!(
        tree.entries.get("run.sh"),
        Some(TreeEntry::File {
            mode: izu_model::FileMode::Executable,
            ..
        })
    ));
    #[cfg(unix)]
    assert!(
        matches!(tree.entries.get("link"), Some(TreeEntry::Symlink { target }) if target.as_bytes() == b"src/message.txt")
    );
}

#[test]
fn git_native_local_remote_roundtrip() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("clone into native");
    fs::write(
        repository.root_path().join("src/message.txt"),
        b"native changed\n",
    )
    .expect("new source");
    let revision = native_commit(&repository, "Native source after Git import\n");
    repository
        .update_ref(
            &reference("main"),
            Some(imported.revision_mapping[&old]),
            Some(revision),
            &cancel(),
        )
        .expect("native ref update");
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(old),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: Some(committer()),
    };
    let report = adapter()
        .push_ref(&repository, &request, &cancel())
        .expect("leased fast-forward push");
    assert_eq!(report.previous, Some(old));
    assert_eq!(oid(&remote, "main"), report.published);
    let clone = fixture.path().join("git-after");
    git(
        fixture.path(),
        &[
            "clone",
            "--",
            remote.to_str().expect("path"),
            clone.to_str().expect("path"),
        ],
        None,
    );
    assert_eq!(
        fs::read(clone.join("src/message.txt")).expect("source after push"),
        b"native changed\n"
    );
    assert_eq!(
        String::from_utf8(git(&clone, &["rev-parse", "HEAD^"], None))
            .expect("parent")
            .trim(),
        old.to_string()
    );
    assert_eq!(
        String::from_utf8(git(&clone, &["show", "HEAD^:src/message.txt"], None)).expect("history"),
        "first\n"
    );
}

#[test]
fn diverged_target_and_stale_lease_are_refused() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    let native_ref = imported.revision_mapping[&old];
    fs::write(source.join("src/remote-only.txt"), b"remote work\n").expect("divergence");
    git(&source, &["add", "--", "."], None);
    git(&source, &["commit", "-m", "Remote newer"], None);
    let remote_new = oid(&source, "main");
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(remote_new),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: None,
    };
    assert!(matches!(
        adapter().push_ref(&repository, &request, &cancel()),
        Err(Error::NonFastForward)
    ));
    assert_eq!(oid(&remote, "main"), remote_new);
    assert_eq!(
        repository.view(&cancel()).expect("view").refs[&reference("main")],
        native_ref
    );
    let stale = PushRequest {
        expected_old: Some(old),
        ..request
    };
    assert!(matches!(
        adapter().push_ref(&repository, &stale, &cancel()),
        Err(Error::LeaseMismatch { .. })
    ));
    assert_eq!(oid(&remote, "main"), remote_new);
}

#[test]
fn separate_git_dir_target_is_refused_before_write() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, _) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    let human = fixture.path().join("human");
    fs::create_dir(&human).expect("human directory");
    let metadata = fixture.path().join("separate-meta");
    git(
        &human,
        &[
            "init",
            "--template=",
            "--initial-branch=main",
            "--separate-git-dir",
            metadata.to_str().expect("path"),
        ],
        None,
    );
    let request = PushRequest {
        remote: GitSource::local(&metadata),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: None,
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: None,
    };
    assert!(matches!(
        adapter().push_ref(&repository, &request, &cancel()),
        Err(Error::Unsupported(_))
    ));
    assert!(!metadata.join("refs/heads/main").exists());
    assert_eq!(
        git(&source, &["rev-parse", "main"], None),
        format!("{old}\n").as_bytes()
    );
}

#[test]
fn existing_corrupt_object_refuses_publication() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let (repository, _) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    let blob = String::from_utf8(git(&source, &["rev-parse", "main:src/message.txt"], None))
        .expect("blob ID");
    let blob = blob.trim();
    let remote = fixture.path().join("remote.git");
    fs::create_dir(&remote).expect("remote");
    git(&remote, &["init", "--bare", "--template="], None);
    let shard = remote.join("objects").join(&blob[..2]);
    fs::create_dir(&shard).expect("shard");
    let corrupt = shard.join(&blob[2..]);
    fs::write(&corrupt, b"not an object").expect("corrupt collision fixture");
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: None,
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: None,
    };
    assert!(
        adapter()
            .push_ref(&repository, &request, &cancel())
            .is_err()
    );
    assert!(!remote.join("refs/heads/main").exists());
    assert_eq!(
        fs::read(corrupt).expect("corrupt object retained"),
        b"not an object"
    );
}

#[cfg(unix)]
fn install_hook(remote: &Path, name: &str, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    let path = remote.join("hooks").join(name);
    fs::write(&path, script).expect("owned server hook");
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).expect("fixture hook mode");
}

#[cfg(unix)]
#[test]
fn pre_receive_rejection_preserves_target_and_runs_server_policy() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    fs::write(repository.root_path().join("new.txt"), b"new native source").expect("source");
    let revision = native_commit(&repository, "New native source");
    repository
        .update_ref(
            &reference("main"),
            Some(imported.revision_mapping[&old]),
            Some(revision),
            &cancel(),
        )
        .expect("native ref");
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    install_hook(
        &remote,
        "pre-receive",
        "#!/bin/sh\ncat >/dev/null\nprintf '%s' 'policy ran' >policy-marker\necho 'owned server refuses this change' >&2\nexit 1\n",
    );
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(old),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: Some(committer()),
    };
    assert!(matches!(
        adapter().push_ref(&repository, &request, &cancel()),
        Err(Error::CommandFailed { .. })
    ));
    assert_eq!(oid(&remote, "main"), old);
    assert_eq!(
        fs::read(remote.join("policy-marker")).expect("server policy evidence"),
        b"policy ran"
    );
}

#[test]
fn server_non_fast_forward_policy_remains_authoritative() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, _) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    fs::write(source.join("remote-only.txt"), b"remote source").expect("remote source");
    git(&source, &["add", "."], None);
    git(&source, &["commit", "-m", "Remote source"], None);
    let advanced = oid(&source, "main");
    assert_ne!(old, advanced);
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    git(
        &remote,
        &["config", "receive.denyNonFastForwards", "true"],
        None,
    );
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(advanced),
        non_fast_forward: NonFastForwardPolicy::ExplicitAllow,
        committer: None,
    };
    assert!(matches!(
        adapter().push_ref(&repository, &request, &cancel()),
        Err(Error::CommandFailed { .. })
    ));
    assert_eq!(oid(&remote, "main"), advanced);
}

#[cfg(unix)]
#[test]
fn external_target_movement_during_receive_is_not_overwritten() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    fs::write(
        repository.root_path().join("native-only.txt"),
        b"native source",
    )
    .expect("native source");
    let revision = native_commit(&repository, "Native source");
    repository
        .update_ref(
            &reference("main"),
            Some(imported.revision_mapping[&old]),
            Some(revision),
            &cancel(),
        )
        .expect("native ref");
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    let tree = oid(&remote, "main^{tree}");
    let moved: GitObjectId = String::from_utf8(git(
        &remote,
        &["commit-tree", &tree.to_string(), "-p", &old.to_string()],
        Some(b"Concurrent external work\n"),
    ))
    .expect("external commit ID")
    .trim()
    .parse()
    .expect("ID");
    install_hook(
        &remote,
        "pre-receive",
        &format!(
            "#!/bin/sh\ncat >/dev/null\n/usr/bin/env -u GIT_QUARANTINE_PATH -u GIT_OBJECT_DIRECTORY -u GIT_ALTERNATE_OBJECT_DIRECTORIES /usr/bin/git --git-dir=. update-ref refs/heads/main {moved} {old}\n"
        ),
    );
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(old),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: Some(committer()),
    };
    assert!(
        matches!(adapter().push_ref(&repository, &request, &cancel()), Err(Error::LeaseMismatch { expected: Some(expected), observed: Some(observed) }) if expected == old && observed == moved)
    );
    assert_eq!(oid(&remote, "main"), moved);
}

#[cfg(unix)]
#[test]
fn interrupted_push_after_publication_and_external_movement_reports_uncertainty() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    fs::write(repository.root_path().join("native.txt"), b"native source").expect("native source");
    let revision = native_commit(&repository, "Native source");
    repository
        .update_ref(
            &reference("main"),
            Some(imported.revision_mapping[&old]),
            Some(revision),
            &cancel(),
        )
        .expect("native ref");
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    let tree = oid(&remote, "main^{tree}");
    let moved: GitObjectId = String::from_utf8(git(
        &remote,
        &["commit-tree", &tree.to_string(), "-p", &old.to_string()],
        Some(b"External movement\n"),
    ))
    .expect("ID")
    .trim()
    .parse()
    .expect("ID");
    install_hook(
        &remote,
        "post-receive",
        &format!(
            "#!/bin/sh\ncat >/dev/null\n/usr/bin/git --git-dir=. update-ref refs/heads/main {moved}\nprintf '%s' 'after publication' >post-marker\n/bin/sleep 30\n"
        ),
    );
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(old),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: Some(committer()),
    };
    let cancellation = cancel();
    let finished = std::sync::atomic::AtomicBool::new(false);
    let error = std::thread::scope(|scope| {
        let observer = scope.spawn(|| {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
            loop {
                if fs::read(remote.join("post-marker")).ok().as_deref()
                    == Some(b"after publication")
                {
                    cancellation.cancel();
                    return true;
                }
                if finished.load(std::sync::atomic::Ordering::Acquire)
                    || std::time::Instant::now() >= deadline
                {
                    cancellation.cancel();
                    return false;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        });
        // Interruption must follow observed publication, regardless of process startup load.
        let result = adapter().push_ref(&repository, &request, &cancellation);
        finished.store(true, std::sync::atomic::Ordering::Release);
        assert!(
            observer.join().expect("publication observer"),
            "post-receive publication was not observed before interruption: {result:?}"
        );
        result.expect_err("lost publication response")
    });
    assert!(matches!(error, Error::PublicationUncertain(_)), "{error:?}");
    let diagnostic = error.to_string();
    assert!(diagnostic.contains("attempted"));
    assert!(diagnostic.contains("refs/heads/main"));
    assert!(diagnostic.contains(&remote.display().to_string()));
    assert_eq!(
        fs::read(remote.join("post-marker")).expect("visible publication hook evidence"),
        b"after publication"
    );
    assert_eq!(oid(&remote, "main"), moved);
}

#[test]
fn bare_target_with_attached_worktree_is_refused_before_write() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, _) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            "--",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    let human = fixture.path().join("human-worktree");
    git(
        &remote,
        &["worktree", "add", human.to_str().expect("path"), "main"],
        None,
    );
    let before = fs::read(human.join("src/message.txt")).expect("human source");
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(old),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: None,
    };
    assert!(matches!(
        adapter().push_ref(&repository, &request, &cancel()),
        Err(Error::Unsupported(_))
    ));
    assert_eq!(oid(&remote, "main"), old);
    assert_eq!(
        fs::read(human.join("src/message.txt")).expect("source retained"),
        before
    );
}

#[test]
fn unsupported_source_features_leave_native_history_unpublished() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    git(&source, &["tag", "v1"], None);
    let native = fixture.path().join("native");
    assert!(matches!(
        adapter().clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel()
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(!native.exists());
    git(&source, &["tag", "-d", "v1"], None);
    fs::write(source.join(".gitattributes"), b"*.txt filter=custom\n").expect("attributes");
    git(&source, &["add", "--", ".gitattributes"], None);
    git(&source, &["commit", "-m", "Attributes"], None);
    assert!(matches!(
        adapter().clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel()
        ),
        Err(Error::Unsupported(_))
    ));
    assert!(!native.exists());
}

#[test]
fn explicit_branch_selection_reports_every_omitted_ref_and_checks_only_its_graph() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    let source = source_fixture(fixture.path());
    let main = oid(&source, "main");
    git(&source, &["tag", "v1"], None);
    git(
        &source,
        &["update-ref", "refs/pull/1/head", &main.to_string()],
        None,
    );
    let tree = oid(&source, "main^{tree}");
    let unsupported = raw_commit(&source, tree, "x-unmapped metadata\n");
    git(
        &source,
        &[
            "update-ref",
            "refs/heads/unmapped",
            &unsupported.to_string(),
        ],
        None,
    );
    git(
        &source,
        &["update-ref", "refs/heads/main", &main.to_string()],
        None,
    );
    let source = GitSource::local(&source);
    let options = ImportOptions {
        refs: vec![RefImport {
            git_ref: "refs/heads/main".into(),
            native_ref: reference("main"),
            expected: None,
        }],
    };
    let inventory = adapter()
        .inventory_with_options(&source, &options, &cancel())
        .expect("selected inventory");
    assert!(inventory.unsupported.is_empty());
    assert_eq!(inventory.commits, 1);
    assert_eq!(inventory.omitted_refs.len(), 3);
    assert_eq!(inventory.omitted_refs["refs/tags/v1"], main);
    assert_eq!(inventory.omitted_refs["refs/pull/1/head"], main);
    assert_eq!(inventory.omitted_refs["refs/heads/unmapped"], unsupported);
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("selected-native"),
            &source,
            &options,
            &cancel(),
        )
        .expect("explicit selected clone");
    assert_eq!(imported.inventory.omitted_refs, inventory.omitted_refs);
    assert_eq!(
        repository.view(&cancel()).expect("native refs").refs.len(),
        1
    );
    assert!(matches!(
        adapter().clone_into(
            &fixture.path().join("all-refused"),
            &source,
            &ImportOptions::default(),
            &cancel()
        ),
        Err(Error::Unsupported(_))
    ));
}

#[test]
fn nonportable_source_branch_names_require_an_explicit_native_mapping() {
    let fixture = tempfile::tempdir().expect("Neural fixture");
    let path = source_fixture(fixture.path());
    let id = oid(&path, "main");
    git(
        &path,
        &["update-ref", "refs/heads/日本", &id.to_string()],
        None,
    );
    let source = GitSource::local(&path);
    let inventory = adapter()
        .inventory(&source, &cancel())
        .expect("portable-name inventory");
    assert!(
        inventory
            .unsupported
            .iter()
            .any(|item| item.contains("explicit portable native reference mapping"))
    );
    let all = fixture.path().join("unmapped");
    assert!(matches!(
        adapter().clone_into(&all, &source, &ImportOptions::default(), &cancel()),
        Err(Error::Unsupported(_))
    ));
    assert!(!all.exists());
    let options = ImportOptions {
        refs: vec![
            RefImport {
                git_ref: "refs/heads/main".into(),
                native_ref: reference("main"),
                expected: None,
            },
            RefImport {
                git_ref: "refs/heads/日本".into(),
                native_ref: reference("japan"),
                expected: None,
            },
        ],
    };
    let (repository, imported) = adapter()
        .clone_into(&fixture.path().join("mapped"), &source, &options, &cancel())
        .expect("explicit mapping");
    assert_eq!(
        repository.view(&cancel()).expect("refs").refs[&reference("japan")],
        imported.revision_mapping[&id]
    );
    assert!(imported.inventory.omitted_refs.is_empty());
}

#[test]
fn new_native_export_requires_explicit_committer() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("native");
    fs::create_dir(&root).expect("native");
    let repository = Repository::init(&root, RepositoryOptions::default()).expect("init");
    let previous = repository.view(&cancel()).expect("view").refs[&reference("main")];
    fs::write(root.join("file"), b"source").expect("source");
    let revision = native_commit(&repository, "Native");
    repository
        .update_ref(
            &reference("main"),
            Some(previous),
            Some(revision),
            &cancel(),
        )
        .expect("publish native");
    let output = fixture.path().join("output.git");
    assert!(matches!(
        adapter().export_to(&repository, &output, &ExportOptions::default(), &cancel()),
        Err(Error::MissingCommitter)
    ));
    assert!(!output.exists());
}

#[test]
fn explicit_fetch_lease_and_invalid_ref_validation() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let old = oid(&source, "main");
    let (repository, imported) = adapter()
        .clone_into(
            &fixture.path().join("native"),
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("clone");
    fs::write(source.join("new.txt"), b"new\n").expect("source");
    git(&source, &["add", "--", "."], None);
    git(&source, &["commit", "-m", "New"], None);
    let options = ImportOptions {
        refs: vec![RefImport {
            git_ref: "refs/heads/main".into(),
            native_ref: reference("main"),
            expected: Some(imported.revision_mapping[&old]),
        }],
    };
    let fetched = adapter()
        .fetch_into(&repository, &GitSource::local(&source), &options, &cancel())
        .expect("explicit fetch");
    assert_eq!(fetched.revision_mapping.len(), 2);
    let output = fixture.path().join("unsafe.git");
    let export = ExportOptions {
        refs: vec![RefExport {
            native_ref: reference("main"),
            git_ref: "refs/heads/main\nupdate refs/heads/evil".into(),
        }],
        committer: None,
    };
    assert!(matches!(
        adapter().export_to(&repository, &output, &export, &cancel()),
        Err(Error::InvalidRef(_))
    ));
    assert!(!output.exists());
}

#[test]
fn source_object_limits_are_enforced_before_native_init() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let mut config = config();
    config.limits.max_object_bytes = 8;
    let adapter = GitAdapter::new(config).expect("Git available");
    let native = fixture.path().join("limited");
    assert!(matches!(
        adapter.clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel()
        ),
        Err(Error::Limit(_))
    ));
    assert!(!native.exists());
}

#[test]
fn native_export_blob_limit_is_checked_before_output_publication() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("native");
    fs::create_dir(&root).expect("native");
    let repository = Repository::init(&root, RepositoryOptions::default()).expect("native init");
    let initial = repository.view(&cancel()).expect("view").refs[&reference("main")];
    fs::write(root.join("large.bin"), vec![42; 4096]).expect("native blob");
    let revision = native_commit(&repository, "Bounded export");
    repository
        .update_ref(&reference("main"), Some(initial), Some(revision), &cancel())
        .expect("native ref");
    let mut limits = config();
    limits.limits.max_object_bytes = 1024;
    let bounded = GitAdapter::new(limits).expect("bounded adapter");
    let output = fixture.path().join("never-created.git");
    assert!(matches!(
        bounded.export_to(
            &repository,
            &output,
            &ExportOptions {
                refs: Vec::new(),
                committer: Some(committer())
            },
            &cancel()
        ),
        Err(Error::Limit(_))
    ));
    assert!(!output.exists());
}

fn raw_commit(source: &Path, tree: GitObjectId, extra_headers: &str) -> GitObjectId {
    let raw = format!(
        "tree {tree}\nauthor Source Author <source@example.test> 1700000000 +0530\ncommitter Source Committer <committer@example.test> 1700000001 -0800\n{extra_headers}\nFixture commit\n"
    );
    let id: GitObjectId = String::from_utf8(git(
        source,
        &["hash-object", "-w", "-t", "commit", "--stdin"],
        Some(raw.as_bytes()),
    ))
    .expect("commit ID")
    .trim()
    .parse()
    .expect("commit");
    git(
        source,
        &["update-ref", "refs/heads/main", &id.to_string()],
        None,
    );
    id
}

#[test]
fn unknown_capabilities_and_signatures_are_reported() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let tree = oid(&source, "main^{tree}");
    for extra in [
        "gpgsig -----BEGIN PGP SIGNATURE-----\n fake signature\n -----END PGP SIGNATURE-----\n",
        "x-unsupported preserved-elsewhere\n",
    ] {
        raw_commit(&source, tree, extra);
        let inventory = adapter()
            .inventory(&GitSource::local(&source), &cancel())
            .expect("feature inventory");
        assert!(!inventory.unsupported.is_empty());
        let target = fixture.path().join("never-initialized");
        assert!(matches!(
            adapter().clone_into(
                &target,
                &GitSource::local(&source),
                &ImportOptions::default(),
                &cancel()
            ),
            Err(Error::Unsupported(_))
        ));
        assert!(!target.exists());
    }
    git(
        &source,
        &["config", "extensions.unknownextension", "true"],
        None,
    );
    let inventory = adapter()
        .inventory(&GitSource::local(&source), &cancel())
        .expect("config inventory without executing source config");
    assert!(
        inventory
            .unsupported
            .iter()
            .any(|feature| feature.contains("unknownextension"))
    );
}

#[test]
fn malicious_source_paths_are_refused_and_symlinks_are_data() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let blob: GitObjectId = String::from_utf8(git(
        &source,
        &["hash-object", "-w", "--stdin"],
        Some(b"payload"),
    ))
    .expect("blob")
    .trim()
    .parse()
    .expect("ID");
    for component in ["..", ".git", ".izu"] {
        let mut tree = format!("100644 {component}\0").into_bytes();
        tree.extend_from_slice(blob.as_bytes());
        let tree: GitObjectId = String::from_utf8(git(
            &source,
            &["hash-object", "--literally", "-w", "-t", "tree", "--stdin"],
            Some(&tree),
        ))
        .expect("tree ID")
        .trim()
        .parse()
        .expect("ID");
        raw_commit(&source, tree, "");
        let native = fixture.path().join(format!("refused-{component}"));
        assert!(
            adapter()
                .clone_into(
                    &native,
                    &GitSource::local(&source),
                    &ImportOptions::default(),
                    &cancel()
                )
                .is_err()
        );
        assert!(!native.exists());
    }
    let mut aliases = b"100644 STRASSE\0".to_vec();
    aliases.extend_from_slice(blob.as_bytes());
    aliases.extend_from_slice("100644 Straße\0".as_bytes());
    aliases.extend_from_slice(blob.as_bytes());
    let aliases: GitObjectId = String::from_utf8(git(
        &source,
        &["hash-object", "--literally", "-w", "-t", "tree", "--stdin"],
        Some(&aliases),
    ))
    .expect("alias tree ID")
    .trim()
    .parse()
    .expect("ID");
    raw_commit(&source, aliases, "");
    let native = fixture.path().join("aliases-refused-before-init");
    assert!(
        adapter()
            .clone_into(
                &native,
                &GitSource::local(&source),
                &ImportOptions::default(),
                &cancel()
            )
            .is_err()
    );
    assert!(!native.exists());
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let root = fixture.path().join("link-source");
        fs::create_dir(&root).expect("root");
        git(
            &root,
            &["init", "--initial-branch=main", "--template="],
            None,
        );
        let outside = fixture.path().join("outside-marker");
        fs::write(&outside, b"must stay unchanged").expect("outside fixture");
        symlink("../outside-marker", root.join("escaping-link")).expect("escaping target data");
        git(&root, &["add", "--", "."], None);
        git(&root, &["commit", "-m", "Symlink as data"], None);
        let native = fixture.path().join("native-links");
        let (repository, _) = adapter()
            .clone_into(
                &native,
                &GitSource::local(&root),
                &ImportOptions::default(),
                &cancel(),
            )
            .expect("import symlink data");
        assert_eq!(
            fs::read_link(native.join("escaping-link")).expect("native symlink"),
            PathBuf::from("../outside-marker")
        );
        assert_eq!(fs::read(&outside).expect("outside"), b"must stay unchanged");
        let output = fixture.path().join("symlink-export.git");
        let report = adapter()
            .export_to(&repository, &output, &ExportOptions::default(), &cancel())
            .expect("raw symlink export");
        assert_eq!(report.refs["refs/heads/main"], oid(&root, "main"));
    }
}

#[test]
fn corrupt_git_object_and_missing_tool_are_explicit_failures() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let commit = oid(&source, "main").to_string();
    let object_path = source
        .join(".git/objects")
        .join(&commit[..2])
        .join(&commit[2..]);
    fs::remove_file(&object_path).expect("remove owned read-only object fixture");
    fs::write(object_path, b"corrupt object").expect("corrupt fixture");
    let native = fixture.path().join("not-created");
    assert!(
        adapter()
            .clone_into(
                &native,
                &GitSource::local(&source),
                &ImportOptions::default(),
                &cancel()
            )
            .is_err()
    );
    assert!(!native.exists());
    let missing = GitToolConfig {
        executable: fixture.path().join("no-installed-git"),
        ..config()
    };
    assert!(matches!(
        GitAdapter::new(missing),
        Err(Error::Unavailable(_))
    ));
    assert!(GitSource::https("https://user:secret@example.test/repo.git").is_err());
    assert!(GitSource::https("ssh://example.test/repo.git").is_err());
}
