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
                tags: Vec::new(),
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
    git(&source, &["update-ref", "refs/pull/1/head", "main"], None);
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
    git(&source, &["update-ref", "-d", "refs/pull/1/head"], None);
    fs::write(
        source.join(".gitmodules"),
        b"[submodule \"module\"]\n path = module\n",
    )
    .expect("submodule config");
    git(&source, &["add", "--", ".gitmodules"], None);
    git(&source, &["commit", "-m", "Submodule configuration"], None);
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
        tags: Vec::new(),
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
        tags: Vec::new(),
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
        tags: Vec::new(),
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
        tags: Vec::new(),
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
                tags: Vec::new(),
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
fn unknown_capabilities_and_commit_headers_are_reported() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let tree = oid(&source, "main^{tree}");
    let extra = "x-unsupported preserved-elsewhere\n";
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

#[test]
fn signed_commits_and_attributes_roundtrip_exactly_then_native_edit_exports() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let attributes = b"*.txt text eol=crlf\n*.bin -text\n*.custom filter=unconfigured\n";
    fs::write(source.join(".gitattributes"), attributes).expect("attributes");
    git(&source, &["add", "--", ".gitattributes"], None);
    git(&source, &["commit", "-m", "Attributes"], None);
    let tree = oid(&source, "main^{tree}");
    // Deliberately fake signatures prove byte preservation, not authentication.
    let original = raw_commit(
        &source,
        tree,
        "gpgsig -----BEGIN PGP SIGNATURE-----\n opaque unverified payload\n -----END PGP SIGNATURE-----\ngpgsig-sha256 -----BEGIN SSH SIGNATURE-----\n opaque secondary payload\n -----END SSH SIGNATURE-----\n",
    );
    let original_bytes = git(
        &source,
        &["cat-file", "commit", &original.to_string()],
        None,
    );
    let inventory = adapter()
        .inventory(&GitSource::local(&source), &cancel())
        .expect("inventory");
    assert!(
        inventory.unsupported.is_empty(),
        "{:?}",
        inventory.unsupported
    );
    assert!(
        inventory.warnings.iter().any(
            |warning| warning.contains("committed bytes") && warning.contains("text/EOL/ident")
        )
    );
    let native = fixture.path().join("native");
    let (repository, _) = adapter()
        .clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("native clone");
    assert_eq!(
        fs::read(native.join(".gitattributes")).expect("native attributes"),
        attributes
    );
    assert_eq!(
        fs::read(native.join("src/message.txt")).expect("raw native source"),
        b"first\n"
    );
    let export = fixture.path().join("exact.git");
    let exported = adapter()
        .export_to(&repository, &export, &ExportOptions::default(), &cancel())
        .expect("exact export");
    assert!(
        exported
            .warnings
            .iter()
            .any(|warning| warning.contains("attributes exported")
                && warning.contains("conversions"))
    );
    assert_eq!(oid(&export, "main"), original);
    assert_eq!(
        git(&export, &["cat-file", "commit", "main"], None),
        original_bytes
    );
    assert_eq!(
        git(&export, &["show", "main:.gitattributes"], None),
        attributes
    );
    let previous = repository.view(&cancel()).expect("view").refs[&reference("main")];
    fs::write(native.join("src/message.txt"), b"native change\n").expect("native edit");
    let edited = native_commit(&repository, "Native child of signed history");
    repository
        .update_ref(&reference("main"), Some(previous), Some(edited), &cancel())
        .expect("ref update");
    let edited_export = fixture.path().join("edited.git");
    adapter()
        .export_to(
            &repository,
            &edited_export,
            &ExportOptions {
                tags: Vec::new(),
                refs: Vec::new(),
                committer: Some(committer()),
            },
            &cancel(),
        )
        .expect("edited export");
    assert_eq!(oid(&edited_export, "main^"), original);
    let child = git(&edited_export, &["cat-file", "commit", "main"], None);
    assert!(
        !String::from_utf8(child)
            .expect("child text")
            .contains("gpgsig")
    );
    let checkout = fixture.path().join("checkout");
    git(
        fixture.path(),
        &[
            "clone",
            "--",
            edited_export.to_str().expect("path"),
            checkout.to_str().expect("path"),
        ],
        None,
    );
    assert_eq!(
        fs::read(checkout.join("src/message.txt")).expect("Git applies attributes"),
        b"native change\r\n"
    );
}

#[test]
fn merge_tags_roundtrip_real_git_merge_and_drop_on_rewrite() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    git(&source, &["checkout", "-b", "feature"], None);
    git(&source, &["commit", "--allow-empty", "-m", "Feature"], None);
    let feature = oid(&source, "HEAD");
    git(&source, &["checkout", "main"], None);
    git(&source, &["commit", "--allow-empty", "-m", "Main"], None);
    // This fixture's fake signature is deliberately opaque. Git itself creates
    // the enclosing mergetag header when merging this annotated tag.
    let mut annotation = format!("object {feature}\ntype commit\ntag merge-feature\ntagger Tag Author <tag@example.test> 1700000002 -0730\n\nExact nested bytes\n  indented body\n\n").into_bytes();
    annotation.extend_from_slice(b"Opaque bytes: \xc3\xbf\n-----BEGIN PGP SIGNATURE-----\nopaque unverified payload\n-----END PGP SIGNATURE-----\n");
    let tag_id = String::from_utf8(git(
        &source,
        &["hash-object", "-t", "tag", "-w", "--stdin"],
        Some(&annotation),
    ))
    .expect("tag ID");
    git(
        &source,
        &["update-ref", "refs/tags/merge-feature", tag_id.trim()],
        None,
    );
    git(
        &source,
        &[
            "merge",
            "--no-ff",
            "--no-gpg-sign",
            "-m",
            "Tagged merge",
            "merge-feature",
        ],
        None,
    );
    let original = oid(&source, "main");
    let raw = git(&source, &["cat-file", "commit", "main"], None);
    let encoded = annotation
        .split(|byte| *byte == b'\n')
        .enumerate()
        .flat_map(|(index, line)| {
            let mut bytes = if index == 0 {
                b"mergetag ".to_vec()
            } else {
                b" ".to_vec()
            };
            bytes.extend_from_slice(line);
            bytes.push(b'\n');
            bytes
        })
        .collect::<Vec<_>>();
    // Git omits the final empty continuation for the tag's terminal newline.
    assert!(
        raw.windows(encoded.len() - 2)
            .any(|part| part == &encoded[..encoded.len() - 2])
    );
    let native = fixture.path().join("native-mergetag");
    let (repository, inventory) = adapter()
        .clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("validated merge tag clone");
    assert!(inventory.inventory.unsupported.is_empty());
    let exact = fixture.path().join("exact-mergetag.git");
    adapter()
        .export_to(&repository, &exact, &ExportOptions::default(), &cancel())
        .expect("unchanged mergetag export");
    assert_eq!(oid(&exact, "main"), original);
    assert_eq!(git(&exact, &["cat-file", "commit", "main"], None), raw);
    git(&exact, &["fsck", "--strict"], None);
    let previous = repository.view(&cancel()).expect("view").refs[&reference("main")];
    let mut revision = repository.revision(previous, &cancel()).expect("revision");
    revision.description = "Rewritten merge\n".into();
    let rewritten = repository
        .put_revision(&revision, &cancel())
        .expect("rewritten revision");
    repository
        .update_ref(
            &reference("main"),
            Some(previous),
            Some(rewritten),
            &cancel(),
        )
        .expect("rewritten ref");
    let changed = fixture.path().join("changed-mergetag.git");
    let report = adapter()
        .export_to(
            &repository,
            &changed,
            &ExportOptions {
                refs: Vec::new(),
                tags: Vec::new(),
                committer: Some(committer()),
            },
            &cancel(),
        )
        .expect("rewritten export");
    let changed_bytes = git(&changed, &["cat-file", "commit", "main"], None);
    assert!(
        !changed_bytes
            .windows(b"mergetag ".len())
            .any(|part| part == b"mergetag ")
    );
    assert_ne!(oid(&changed, "main"), original);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("merge tag"))
    );
}

#[test]
fn malformed_merge_tags_fail_before_native_publication() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let parent = oid(&source, "main");
    let tree = oid(&source, "main^{tree}");
    let embedded = |body: &str| format!("mergetag {}\n", body.replace('\n', "\n "));
    for body in [
        format!("object {parent}\ntype tree\ntag wrong-kind\n\nmessage"),
        format!("object {tree}\ntype commit\ntag wrong-target\n\nmessage"),
        format!("object {parent}\ntype commit\ntag duplicate\ntag duplicate\n\nmessage"),
        format!("object {parent}\ntype commit\ntag no-separator"),
        format!("object {parent}\ntype commit\ntag unknown\nunknown metadata\n\nmessage"),
        "object not-an-id\ntype commit\ntag malformed\n\nmessage".to_owned(),
    ] {
        raw_commit(
            &source,
            tree,
            &format!("parent {parent}\n{}", embedded(&body)),
        );
        let target = fixture.path().join("unpublished-mergetag");
        assert!(matches!(
            adapter().clone_into(
                &target,
                &GitSource::local(&source),
                &ImportOptions::default(),
                &cancel()
            ),
            Err(Error::InvalidObject { .. })
        ));
        assert!(!target.exists());
    }
    let valid = embedded(&format!(
        "object {parent}\ntype commit\ntag bounded\n\n{}",
        "x".repeat(2048)
    ));
    raw_commit(&source, tree, &format!("parent {parent}\n{valid}"));
    let mut bounded_config = config();
    bounded_config.limits.max_object_bytes = 1024;
    let target = fixture.path().join("unpublished-bounded-mergetag");
    assert!(matches!(
        GitAdapter::new(bounded_config)
            .expect("bounded adapter")
            .clone_into(
                &target,
                &GitSource::local(&source),
                &ImportOptions::default(),
                &cancel()
            ),
        Err(Error::Limit(_))
    ));
    assert!(!target.exists());
}

#[test]
fn malformed_commit_continuations_and_duplicate_signatures_fail_before_clone() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let tree = oid(&source, "main^{tree}");
    for extra in [
        " orphan continuation\n",
        "gpgsig opaque\ngpgsig duplicate\n",
        "gpgsig \n",
    ] {
        raw_commit(&source, tree, extra);
        let target = fixture.path().join("unpublished");
        assert!(
            adapter()
                .clone_into(
                    &target,
                    &GitSource::local(&source),
                    &ImportOptions::default(),
                    &cancel()
                )
                .is_err()
        );
        assert!(!target.exists());
    }
}

#[test]
fn separate_git_directory_and_linked_worktree_import_preserve_source_objects() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let original = oid(&source, "main");
    let metadata = fixture.path().join("separate.git");
    git(
        &source,
        &[
            "init",
            "--separate-git-dir",
            metadata.to_str().expect("path"),
        ],
        None,
    );
    // Relative gitfiles are interpreted against the working directory, not the
    // process cwd or the owned cache.
    fs::write(source.join(".git"), b"gitdir: ../separate.git\n").expect("relative Git file");
    let native = fixture.path().join("native-separated");
    let (repository, _) = adapter()
        .clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("separate Git directory source");
    let exported = fixture.path().join("separate-export.git");
    adapter()
        .export_to(&repository, &exported, &ExportOptions::default(), &cancel())
        .expect("export");
    assert_eq!(oid(&exported, "main"), original);

    let linked = fixture.path().join("linked");
    git(
        &source,
        &[
            "worktree",
            "add",
            "-b",
            "linked",
            linked.to_str().expect("path"),
        ],
        None,
    );
    fs::write(linked.join("src/message.txt"), b"linked edit\n").expect("edit");
    git(&linked, &["add", "src/message.txt"], None);
    git(&linked, &["commit", "-m", "Linked branch"], None);
    let linked_tip = oid(&linked, "linked");
    let (repository, report) = adapter()
        .clone_into(
            &fixture.path().join("native-linked"),
            &GitSource::local(&linked),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("linked worktree source");
    assert_eq!(
        report.inventory.symbolic_head.as_deref(),
        Some("refs/heads/linked")
    );
    assert_eq!(report.inventory.branches["refs/heads/main"], original);
    let exported = fixture.path().join("linked-export.git");
    adapter()
        .export_to(&repository, &exported, &ExportOptions::default(), &cancel())
        .expect("export linked branches");
    assert_eq!(oid(&exported, "main"), original);
    assert_eq!(oid(&exported, "linked"), linked_tip);
    assert_eq!(
        fs::read(fixture.path().join("native-linked/src/message.txt")).expect("linked checkout"),
        b"linked edit\n"
    );
}

#[test]
fn malformed_or_symbolic_gitfiles_fail_before_native_initialization() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let metadata = fixture.path().join("metadata.git");
    fs::rename(source.join(".git"), &metadata).expect("move metadata");
    for bytes in [
        b"gitdir: \n".as_slice(),
        b"gitdir: ../metadata.git\nextra\n",
        b"not a Git file\n",
    ] {
        fs::write(source.join(".git"), bytes).expect("Git file");
        let native = fixture.path().join("unpublished");
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
    #[cfg(unix)]
    {
        fs::remove_file(source.join(".git")).expect("remove Git file");
        std::os::unix::fs::symlink(&metadata, source.join(".git")).expect("symbolic metadata");
        assert!(
            adapter()
                .inventory(&GitSource::local(&source), &cancel())
                .is_err()
        );
    }
}

#[test]
fn lfs_requires_explicit_pointer_policy_and_reports_unavailable_content() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let pointer = b"version https://git-lfs.github.com/spec/v1\noid sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef\nsize 123456\n";
    fs::write(source.join("large.bin"), pointer).expect("LFS pointer");
    fs::write(
        source.join(".gitattributes"),
        b"*.bin filter=lfs diff=lfs merge=lfs -text\n",
    )
    .expect("attributes");
    git(&source, &["add", "."], None);
    git(&source, &["commit", "-m", "LFS pointer"], None);
    let original = oid(&source, "main");
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
    let mut config = config();
    config.lfs_policy = izu_git::LfsPolicy::PreservePointers;
    let adapter = GitAdapter::new(config).expect("pointer-preserving adapter");
    let (repository, imported) = adapter
        .clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("pointer import");
    assert!(
        imported
            .inventory
            .warnings
            .iter()
            .any(|warning| warning.contains("external LFS object content is unavailable"))
    );
    assert_eq!(
        fs::read(native.join("large.bin")).expect("native pointer"),
        pointer
    );
    let exported = fixture.path().join("exported.git");
    let report = adapter
        .export_to(&repository, &exported, &ExportOptions::default(), &cancel())
        .expect("pointer export");
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("has not been uploaded"))
    );
    assert_eq!(oid(&exported, "main"), original);
    assert_eq!(git(&exported, &["show", "main:large.bin"], None), pointer);
}

#[cfg(unix)]
#[test]
fn explicit_ssh_tool_boundary_fetch_and_leased_push_use_real_git_protocol() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let remote = fixture.path().join("remote.git");
    git(
        fixture.path(),
        &[
            "clone",
            "--bare",
            source.to_str().expect("path"),
            remote.to_str().expect("path"),
        ],
        None,
    );
    let original = oid(&remote, "main");
    // This fixture proves the selected tool's protocol boundary against actual
    // upload/receive-pack. It deliberately does not claim network SSH auth.
    let executable = fixture.path().join("ssh-fixture");
    fs::write(&executable, b"#!/bin/sh\nset -eu\ncase \" $* \" in *' -F /dev/null '*' StrictHostKeyChecking=yes '*' IdentityAgent=none '*) ;; *) exit 91 ;; esac\nfor last do :; done\nexport GIT_CONFIG_NOSYSTEM=1 GIT_CONFIG_GLOBAL=/dev/null\nexec /bin/sh -c \"$last\"\n").expect("SSH fixture executable");
    fs::set_permissions(&executable, fs::Permissions::from_mode(0o700)).expect("executable mode");
    let identity_file = fixture.path().join("identity");
    let known_hosts_file = fixture.path().join("known-hosts");
    fs::write(&identity_file, b"fixture identity").expect("identity");
    fs::write(&known_hosts_file, b"fixture host keys").expect("host keys");
    let url = format!("ssh://git@example.test{}", remote.display());
    let source = GitSource::ssh(&url).expect("SSH source");
    assert!(matches!(
        adapter().inventory(&source, &cancel()),
        Err(Error::Unsupported(_))
    ));
    let mut config = config();
    config.ssh = Some(izu_git::SshTransport {
        url: url.clone(),
        executable,
        identity_file,
        known_hosts_file,
    });
    let adapter = GitAdapter::new(config).expect("SSH adapter");
    let native = fixture.path().join("native");
    let (repository, report) = adapter
        .clone_into(&native, &source, &ImportOptions::default(), &cancel())
        .expect("SSH clone");
    assert_eq!(report.inventory.branches["refs/heads/main"], original);
    let previous = repository.view(&cancel()).expect("view").refs[&reference("main")];
    fs::write(native.join("src/message.txt"), b"SSH native child\n").expect("native edit");
    let edited = native_commit(&repository, "SSH child");
    repository
        .update_ref(&reference("main"), Some(previous), Some(edited), &cancel())
        .expect("ref update");
    let request = PushRequest {
        remote: source.clone(),
        native_ref: reference("main"),
        git_ref: "refs/heads/main".into(),
        expected_old: Some(original),
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: Some(committer()),
    };
    let pushed = adapter
        .push_ref(&repository, &request, &cancel())
        .expect("SSH leased push");
    assert_eq!(oid(&remote, "main"), pushed.published);
    assert_eq!(oid(&remote, "main^"), original);
    assert_eq!(
        git(&remote, &["show", "main:src/message.txt"], None),
        b"SSH native child\n"
    );
    assert!(matches!(
        adapter.push_ref(&repository, &request, &cancel()),
        Err(Error::LeaseMismatch { .. })
    ));
    assert!(
        adapter
            .inventory(
                &GitSource::ssh(format!("ssh://git@other.test{}", remote.display()))
                    .expect("other SSH URL"),
                &cancel()
            )
            .is_err()
    );
}

#[test]
fn ssh_urls_refuse_remote_shell_syntax_credentials_and_normalized_paths() {
    for value in [
        "git@example.test:repo.git",
        "ssh://example.test/repo.git",
        "ssh://git:secret@example.test/repo.git",
        "ssh://git@example.test/../repo.git",
        "ssh://git@example.test/repo.git?query",
        "ssh://git@example.test/repo.git#fragment",
        "ssh://git@example.test/repo%27.git",
        "ssh://git@example.test/repo;command",
    ] {
        assert!(GitSource::ssh(value).is_err(), "accepted {value}");
    }
    assert!(GitSource::ssh("ssh://git@example.test:2222/owner/repo.git").is_ok());
}

#[test]
fn tag_inventory_preserves_lightweight_annotated_signed_nested_and_noncommit_roots() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let commit = oid(&source, "main");
    let tree = oid(&source, "main^{tree}");
    let blob = oid(&source, "main:src/message.txt");
    git(&source, &["tag", "light", "main"], None);
    git(&source, &["tag", "tree", "main^{tree}"], None);
    git(&source, &["tag", "blob", "main:src/message.txt"], None);
    git(
        &source,
        &["tag", "-a", "annotation", "-m", "Annotated original"],
        None,
    );
    let empty_tree = String::from_utf8(git(&source, &["mktree"], Some(b""))).expect("empty tree");
    let directory_tree = String::from_utf8(git(
        &source,
        &["mktree"],
        Some(format!("040000 tree {}\tempty\n", empty_tree.trim()).as_bytes()),
    ))
    .expect("directory tree");
    git(
        &source,
        &[
            "tag",
            "-a",
            "empty-directory-tree",
            "-m",
            "Empty directory",
            directory_tree.trim(),
        ],
        None,
    );
    git(
        &source,
        &[
            "tag",
            "-a",
            "annotated-blob",
            "-m",
            "Blob annotation",
            &blob.to_string(),
        ],
        None,
    );
    let annotation = oid(&source, "refs/tags/annotation");
    let mut signed_raw = format!("object {annotation}\ntype tag\ntag signed-nested\ntagger Tag Author <tag@example.test> 1700000002 -0730\n\nOpaque binary message: ").into_bytes();
    signed_raw.extend_from_slice(b"\xff\n-----BEGIN SSH SIGNATURE-----\nopaque unverified bytes\n-----END SSH SIGNATURE-----\n");
    let signed: GitObjectId = String::from_utf8(git(
        &source,
        &["hash-object", "-t", "tag", "-w", "--stdin"],
        Some(&signed_raw),
    ))
    .expect("tag ID text")
    .trim()
    .parse()
    .expect("tag ID");
    git(
        &source,
        &["update-ref", "refs/tags/signed-nested", &signed.to_string()],
        None,
    );
    let inventory = adapter()
        .inventory(&GitSource::local(&source), &cancel())
        .expect("tag inventory");
    assert_eq!(inventory.tags.len(), 7);
    assert_eq!(inventory.tags["refs/tags/light"].target, commit);
    assert!(
        inventory.tags["refs/tags/light"]
            .annotated_objects
            .is_empty()
    );
    assert_eq!(inventory.tags["refs/tags/tree"].target, tree);
    assert_eq!(
        inventory.tags["refs/tags/tree"].target_kind,
        izu_git::GitTagTargetKind::Tree
    );
    assert_eq!(inventory.tags["refs/tags/blob"].target, blob);
    assert_eq!(
        inventory.tags["refs/tags/blob"].target_kind,
        izu_git::GitTagTargetKind::Blob
    );
    let nested = &inventory.tags["refs/tags/signed-nested"];
    assert_eq!(nested.object, signed);
    assert_eq!(nested.target, commit);
    assert_eq!(nested.annotated_objects, [signed, annotation]);
    assert_eq!(nested.signed_objects, [signed]);
    assert_eq!(nested.annotation_names, ["signed-nested", "annotation"]);
    assert_eq!(nested.annotation_byte_lengths[0], signed_raw.len());
    assert_eq!(
        git(&source, &["cat-file", "tag", &signed.to_string()], None),
        signed_raw
    );
    let native = fixture.path().join("native");
    let (repository, imported) = adapter()
        .clone_into(
            &native,
            &GitSource::local(&source),
            &ImportOptions::default(),
            &cancel(),
        )
        .expect("adopt tags");
    assert_eq!(repository.view(&cancel()).expect("view").tags.len(), 7);
    let bare = fixture.path().join("tags-export.git");
    let exported = adapter()
        .export_to(&repository, &bare, &ExportOptions::default(), &cancel())
        .expect("export tags");
    for (name, tag) in &imported.inventory.tags {
        assert_eq!(exported.refs[name], tag.object);
        assert_eq!(oid(&bare, name), tag.object);
    }
    assert_eq!(
        git(&bare, &["cat-file", "tag", &signed.to_string()], None),
        signed_raw
    );
    git(&bare, &["fsck", "--strict"], None);
    let before = repository.view(&cancel()).expect("before stale tags");
    fs::write(source.join("new.txt"), b"remote branch advance").expect("source edit");
    git(&source, &["add", "."], None);
    git(&source, &["commit", "-m", "advance"], None);
    let conflicting = ImportOptions {
        refs: vec![RefImport {
            git_ref: "refs/heads/main".into(),
            native_ref: reference("main"),
            expected: Some(before.refs[&reference("main")]),
        }],
        tags: vec![izu_git::TagImport {
            git_ref: "refs/tags/light".into(),
            native_tag: reference("light"),
            expected: None,
        }],
    };
    assert!(
        adapter()
            .fetch_into(
                &repository,
                &GitSource::local(&source),
                &conflicting,
                &cancel()
            )
            .is_err()
    );
    assert_eq!(repository.view(&cancel()).expect("atomic reject"), before);
    fs::write(native.join("local.txt"), b"native branch edit").expect("native edit");
    let edited = native_commit(&repository, "Native branch edit");
    repository
        .update_ref(
            &reference("main"),
            Some(before.refs[&reference("main")]),
            Some(edited),
            &cancel(),
        )
        .expect("advance native branch");
    let edited_export = fixture.path().join("edited-tags.git");
    let report = adapter()
        .export_to(
            &repository,
            &edited_export,
            &ExportOptions {
                refs: Vec::new(),
                tags: Vec::new(),
                committer: Some(committer()),
            },
            &cancel(),
        )
        .expect("export edited branch with original tags");
    assert_ne!(report.refs["refs/heads/main"], commit);
    for (name, tag) in &imported.inventory.tags {
        assert_eq!(report.refs[name], tag.object);
    }
    git(&edited_export, &["fsck", "--strict"], None);
    let tag_only = fixture.path().join("tag-only.git");
    let tag_options = ExportOptions {
        refs: Vec::new(),
        tags: vec![izu_git::TagExport {
            native_tag: reference("signed-nested"),
            git_ref: "refs/tags/signed-nested".into(),
        }],
        committer: None,
    };
    let tag_report = adapter()
        .export_to(&repository, &tag_only, &tag_options, &cancel())
        .expect("tag-only export excludes rewritten branch");
    assert_eq!(tag_report.refs.len(), 1);
    assert!(
        tag_report
            .refs
            .keys()
            .all(|name| name.starts_with("refs/tags/"))
    );
    assert_eq!(
        git(&tag_only, &["symbolic-ref", "HEAD"], None),
        b"refs/heads/main\n"
    );
    let unique_commit = report.refs["refs/heads/main"];
    let unique_blob = oid(&edited_export, "main:local.txt");
    for unique in [unique_commit, unique_blob] {
        assert_eq!(
            git(
                &tag_only,
                &["cat-file", "--batch-check"],
                Some(format!("{unique}\n").as_bytes())
            ),
            format!("{unique} missing\n").as_bytes()
        );
    }
    let expected_objects = String::from_utf8(git(
        &source,
        &["rev-list", "--objects", &signed.to_string()],
        None,
    ))
    .expect("tag closure")
    .lines()
    .map(|line| line.split(' ').next().expect("object").to_owned())
    .collect::<std::collections::BTreeSet<_>>();
    let object_ids = |directory: &Path| {
        String::from_utf8(git(
            directory,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)",
            ],
            None,
        ))
        .expect("object inventory")
        .lines()
        .map(str::to_owned)
        .collect::<std::collections::BTreeSet<_>>()
    };
    assert_eq!(object_ids(&tag_only), expected_objects);
    let remote = fixture.path().join("tag-remote.git");
    git(
        fixture.path(),
        &["init", "--bare", remote.to_str().expect("remote path")],
        None,
    );
    let request = PushRequest {
        remote: GitSource::local(&remote),
        native_ref: reference("signed-nested"),
        git_ref: "refs/tags/signed-nested".into(),
        expected_old: None,
        non_fast_forward: NonFastForwardPolicy::Reject,
        committer: None,
    };
    let pushed = adapter()
        .push_ref(&repository, &request, &cancel())
        .expect("push tag");
    assert_eq!(pushed.verified, signed);
    assert!(matches!(
        adapter().push_ref(&repository, &request, &cancel()),
        Err(Error::LeaseMismatch { .. })
    ));
    git(&remote, &["fsck", "--strict"], None);
    assert_eq!(object_ids(&remote), expected_objects);
    let view = repository
        .view(&cancel())
        .expect("view before rewrite fixture");
    let original_tag = view.tags[&reference("signed-nested")].clone();
    let mut rewritten_tag = original_tag;
    rewritten_tag.target = izu_model::TagTarget::Revision { revision: edited };
    repository
        .update_tag(
            izu_engine::TagUpdate {
                name: reference("rewritten"),
                expected: None,
                new: Some(rewritten_tag),
            },
            &cancel(),
        )
        .expect("explicit rewritten tag fixture");
    let before_failed_export = repository.view(&cancel()).expect("before failed export");
    let rejected = fixture.path().join("rejected-rewritten-tag.git");
    let rewritten_options = ExportOptions {
        refs: Vec::new(),
        tags: vec![izu_git::TagExport {
            native_tag: reference("rewritten"),
            git_ref: "refs/tags/rewritten".into(),
        }],
        committer: Some(committer()),
    };
    assert!(matches!(
        adapter().export_to(&repository, &rejected, &rewritten_options, &cancel()),
        Err(Error::InvalidSource(_))
    ));
    assert!(!rejected.exists());
    assert_eq!(
        repository.view(&cancel()).expect("state retained"),
        before_failed_export
    );
    let rejected_push = PushRequest {
        native_ref: reference("rewritten"),
        git_ref: "refs/tags/rewritten".into(),
        committer: Some(committer()),
        ..request.clone()
    };
    assert!(matches!(
        adapter().push_ref(&repository, &rejected_push, &cancel()),
        Err(Error::InvalidSource(_))
    ));
    assert_eq!(
        repository.view(&cancel()).expect("push state retained"),
        before_failed_export
    );
    assert_eq!(
        git(
            &remote,
            &["for-each-ref", "--format=%(refname)", "refs/tags/rewritten"],
            None
        ),
        b""
    );
    assert_eq!(object_ids(&remote), expected_objects);
    let mut config = config();
    config.limits.max_tree_depth = 1;
    assert!(matches!(
        GitAdapter::new(config)
            .expect("bounded adapter")
            .inventory(&GitSource::local(&source), &cancel()),
        Err(Error::Limit("Git tag chain depth"))
    ));
}

#[test]
fn malformed_tag_targets_fail_before_native_publication() {
    let fixture = tempfile::tempdir().expect("fixture");
    let source = source_fixture(fixture.path());
    let commit = oid(&source, "main");
    for headers in [
        format!("object {commit}\ntype tree\ntag wrong-kind\n"),
        format!("object {commit}\ntype commit\ntag duplicate\ntag duplicate\n"),
        format!("object {commit}\ntype commit\n"),
        format!("object {commit}\ntype commit\ntag unknown\nunknown header\n"),
    ] {
        let raw = format!("{headers}\nmessage\n");
        let id = String::from_utf8(git(
            &source,
            &["hash-object", "--literally", "-t", "tag", "-w", "--stdin"],
            Some(raw.as_bytes()),
        ))
        .expect("tag ID");
        fs::create_dir_all(source.join(".git/refs/tags")).expect("tag ref directory");
        fs::write(source.join(".git/refs/tags/malformed"), &id)
            .expect("corrupt source ref fixture");
        let target = fixture.path().join("unpublished");
        assert!(
            adapter()
                .clone_into(
                    &target,
                    &GitSource::local(&source),
                    &ImportOptions::default(),
                    &cancel()
                )
                .is_err()
        );
        assert!(!target.exists());
    }
}
