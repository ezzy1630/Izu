//! Acceptance through the exact Cargo-built executable and live stdio pipes.
use serde_json::{Value, json};
#[cfg(unix)]
use std::collections::VecDeque;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
#[cfg(unix)]
use std::process::{ChildStderr, ChildStdin, ChildStdout, ExitStatus};
use std::time::{Duration, Instant};

const BINARY: &str = env!("CARGO_BIN_EXE_izu");

#[test]
fn exact_reclamation_uses_engine_guards_and_preserves_history_and_source() {
    use izu_engine::{CancellationToken, Repository, RepositoryOptions};
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("human.txt"),
        b"retain unique human bytes\n",
    )
    .unwrap();
    fixture.ok(&["init", "."]);
    let cancel = CancellationToken::new();
    let repo = Repository::open(&fixture.root, RepositoryOptions::default()).unwrap();
    let head = repo.current_operation(&cancel).unwrap();
    let first = repo
        .put_blob(&mut &b"explicitly abandoned preparation"[..], 32, &cancel)
        .unwrap();
    let second = repo
        .put_blob(&mut &b"other unique preparation"[..], 24, &cancel)
        .unwrap();
    let plan = fixture.ok(&["storage", "plan", &first.to_string()]);
    assert_eq!(plan["selected"], json!([first]));
    assert_eq!(
        repo.blob(first, &cancel).unwrap(),
        b"explicitly abandoned preparation"
    );
    // A crashed controller's durable intent blocks the actual CLI even after
    // its kernel leases are released. The fixture launched no child process.
    let writer = repo.lease_writer(repo.workspace_id(), &cancel).unwrap();
    let token = writer.intent().token.clone();
    drop(writer);
    let (output, blocked) = fixture.run(&[
        "storage",
        "reclaim",
        "--discard-unpublished",
        &first.to_string(),
    ]);
    assert!(!output.status.success(), "{blocked}");
    assert_eq!(blocked["outcome"]["error"]["code"], "conflict");
    assert!(repo.blob(first, &cancel).is_ok());
    repo.acknowledge_writer_stopped(repo.workspace_id(), &token, &cancel)
        .unwrap();
    let reclaimed = fixture.ok(&[
        "storage",
        "reclaim",
        "--discard-unpublished",
        &first.to_string(),
    ]);
    assert_eq!(reclaimed["selected"], json!([first]));
    assert!(repo.blob(first, &cancel).is_err());
    assert_eq!(
        repo.blob(second, &cancel).unwrap(),
        b"other unique preparation"
    );
    let (output, rejected) = fixture.run(&[
        "storage",
        "reclaim",
        "--discard-unpublished",
        &head.to_string(),
    ]);
    assert!(!output.status.success(), "{rejected}");
    assert!(repo.operation(head, &cancel).is_ok());
    assert_eq!(
        fs::read(fixture.root.join("human.txt")).unwrap(),
        b"retain unique human bytes\n"
    );
    fixture.ok(&["recover"]);
    fixture.ok(&["verify"]);
}

#[test]
fn git_clone_materializes_default_branch_and_preserves_occupied_destination() {
    let fixture = Fixture::new();
    let upstream = fixture.root.join("upstream");
    fs::create_dir(&upstream).expect("owned upstream");
    let git = |args: &[&str]| {
        let output = Command::new("git")
            .current_dir(&upstream)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap_or_default())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .args([
                "-c",
                "core.hooksPath=/dev/null",
                "-c",
                "user.name=owned fixture",
                "-c",
                "user.email=fixture@izu.invalid",
            ])
            .args(args)
            .output()
            .expect("explicit installed Git fixture tool");
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    };
    git(&["init", "--initial-branch=main"]);
    fs::write(upstream.join("source.txt"), b"adopt this source\n").expect("upstream bytes");
    git(&["add", "source.txt"]);
    git(&["commit", "-m", "Owned upstream source"]);
    let preflight = fixture.ok(&["git", "preflight", upstream.to_str().unwrap()]);
    assert_eq!(preflight["supported"], true);
    assert_eq!(preflight["inventory"]["commits"], 1);
    assert!(!fixture.root.join(".izu").exists());
    let destination = fixture.root.join("adopted");
    let receipt = fixture.ok(&[
        "git",
        "clone",
        upstream.to_str().unwrap(),
        destination.to_str().unwrap(),
    ]);
    assert_eq!(
        fs::read(destination.join("source.txt")).unwrap(),
        b"adopt this source\n"
    );
    assert!(!destination.join(".git").exists());
    let context = json!({"repository":destination,"workspace":null});
    let (output, inspected) =
        fixture.json(&json!({"schema_version":1,"operation":{"kind":"inspect","context":context}}));
    assert!(output.status.success(), "{inspected}");
    assert_eq!(inspected["outcome"]["result"]["data"], receipt["workspace"]);
    let occupied = fixture.root.join("occupied");
    fs::create_dir(&occupied).unwrap();
    fs::write(occupied.join("human.txt"), b"preserve human work\n").unwrap();
    let (output, rejected) = fixture.run(&[
        "git",
        "clone",
        upstream.to_str().unwrap(),
        occupied.to_str().unwrap(),
    ]);
    assert!(!output.status.success(), "{rejected}");
    assert!(!occupied.join(".izu").exists());
    assert_eq!(
        fs::read(occupied.join("human.txt")).unwrap(),
        b"preserve human work\n"
    );
    let (output, verified) = fixture.json(&json!({"schema_version":1,"operation":{"kind":"verify","context":{"repository":destination,"workspace":null}}}));
    assert!(output.status.success(), "{verified}");
}

struct OwnedChildren(Vec<Option<Child>>);
impl Drop for OwnedChildren {
    fn drop(&mut self) {
        for child in self.0.iter_mut().flatten() {
            if child.try_wait().ok().flatten().is_none() {
                let _ = child.kill();
            }
            let _ = child.wait();
        }
    }
}

#[test]
fn native_tags_and_explicit_metadata_migration_use_real_cli_and_bundle_paths() {
    use izu_engine::{CancellationToken, OperationId, Repository, RepositoryOptions};
    use izu_model::{ObjectKind, encode_metadata};
    use izu_store::{HeadExpectation, PublishOutcome, Store, StoreOptions};
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(
        fixture.root.join("human.txt"),
        b"preserve original tagged source",
    )
    .unwrap();
    let committed = fixture.ok(&["commit", "-m", "tagged source"]);
    let revision = text(&committed, "revision");
    fixture.ok(&[
        "tags",
        "set",
        "release/v1",
        "--revision",
        &revision,
        "--expect-absent",
    ]);
    let tags = fixture.ok(&["tags", "list"]);
    assert_eq!(
        tags["release/v1"]["target"],
        json!({"kind":"revision","revision":revision})
    );
    let expectation = fixture.home.join("exact-tag.json");
    fs::write(
        &expectation,
        serde_json::to_vec(&tags["release/v1"]).unwrap(),
    )
    .unwrap();
    let (stale, body) = fixture.run(&[
        "tags",
        "set",
        "release/v1",
        "--revision",
        &revision,
        "--expect-absent",
    ]);
    assert!(!stale.status.success());
    assert_eq!(body["outcome"]["error"]["code"], "stale_expectation");
    let bundle = fixture.home.join("tagged.izub");
    fixture.ok(&["bundle", "create", bundle.to_str().unwrap()]);
    let restored = fixture.home.join("restored");
    fixture.ok(&[
        "bundle",
        "restore",
        bundle.to_str().unwrap(),
        restored.to_str().unwrap(),
    ]);
    let restored_repo = Repository::open(&restored, RepositoryOptions::default()).unwrap();
    let cancel = CancellationToken::new();
    assert_eq!(
        restored_repo.view(&cancel).unwrap().tags[&izu_model::RefName::new("release/v1").unwrap()]
            .target,
        izu_model::TagTarget::Revision {
            revision: revision.parse().unwrap()
        }
    );
    restored_repo.verify(&cancel).unwrap();
    fixture.ok(&[
        "tags",
        "delete",
        "release/v1",
        "--expect-json",
        expectation.to_str().unwrap(),
    ]);
    // Put a v1 operation into the owned fixture using the original store API.
    // The CLI must not silently migrate or erase the original operation bytes.
    let repo = Repository::open(&fixture.root, RepositoryOptions::default()).unwrap();
    let store = Store::open(repo.metadata_path(), &StoreOptions::default()).unwrap();
    let before = repo.current_operation(&cancel).unwrap();
    let mut legacy = repo.operation(before, &cancel).unwrap();
    legacy.parent = Some(before);
    legacy.view.metadata_version = None;
    let raw = encode_metadata(&legacy, repo.limits()).unwrap();
    let old = OperationId::from_object(store.put(ObjectKind::Operation, &raw, &cancel).unwrap());
    let lease = store.begin(HeadExpectation::At(before), &cancel).unwrap();
    assert!(matches!(
        lease.publish(old, &cancel).unwrap(),
        PublishOutcome::Durable(_)
    ));
    assert_eq!(fixture.ok(&["format", "inspect"])["metadata_version"], 1);
    let (refused, _) = fixture.run(&[
        "tags",
        "set",
        "new",
        "--revision",
        &revision,
        "--expect-absent",
    ]);
    assert!(!refused.status.success());
    let (wrong, _) = fixture.run(&[
        "format",
        "migrate",
        "--to",
        "2",
        "--expect-operation",
        &before.to_string(),
    ]);
    assert!(!wrong.status.success());
    let migration = fixture.ok(&[
        "format",
        "migrate",
        "--to",
        "2",
        "--expect-operation",
        &old.to_string(),
    ]);
    assert_eq!(migration["metadata_version"], 2);
    let current = repo.current_operation(&cancel).unwrap();
    assert_eq!(repo.operation(current, &cancel).unwrap().parent, Some(old));
    assert_eq!(
        store
            .get(old.object_id(), ObjectKind::Operation, &cancel)
            .unwrap(),
        raw
    );
    fixture.ok(&[
        "tags",
        "set",
        "new",
        "--revision",
        &revision,
        "--expect-absent",
    ]);
    assert_eq!(
        fs::read(fixture.root.join("human.txt")).unwrap(),
        b"preserve original tagged source"
    );
    fixture.ok(&["verify"]);
}

struct Fixture {
    temp: Option<tempfile::TempDir>,
    root: PathBuf,
    home: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().expect("fixture");
        let base = temp
            .path()
            .canonicalize()
            .expect("canonical owned fixture directory");
        let root = base.join("project");
        let home = base.join("operator-home");
        fs::create_dir(&root).expect("source directory");
        fs::create_dir(&home).expect("private home");
        fs::write(
            home.join(".gitconfig"),
            "[user]\n name = preserve operator edits\n email = untouched@example.invalid\n",
        )
        .expect("configuration fixture");
        Self {
            temp: Some(temp),
            root,
            home,
        }
    }
    fn command(&self) -> Command {
        let mut command = Command::new(BINARY);
        command
            .current_dir(&self.root)
            .env("HOME", &self.home)
            .env("IZU_AUTHOR_NAME", "izu acceptance")
            .env("IZU_AUTHOR_EMAIL", "acceptance@izu.invalid");
        command
    }
    fn run(&self, args: &[&str]) -> (Output, Value) {
        let output = self
            .command()
            .args(["--json", "--repo"])
            .arg(&self.root)
            .args(args)
            .output()
            .expect("exact CLI binary");
        let value: Value = serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
            panic!(
                "Invalid stdout JSON {error}: stdout={} stderr={}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            )
        });
        assert_eq!(value["schema_version"], 1);
        assert_eq!(
            value.pointer("/build/version").and_then(Value::as_str),
            Some(env!("CARGO_PKG_VERSION"))
        );
        (output, value)
    }
    fn ok(&self, args: &[&str]) -> Value {
        let (output, value) = self.run(args);
        assert!(
            output.status.success(),
            "args={args:?}, response={value}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            value.pointer("/outcome/kind").and_then(Value::as_str),
            Some("ok")
        );
        value["outcome"]["result"]["data"].clone()
    }
    fn json(&self, request: &Value) -> (Output, Value) {
        let mut child = self
            .command()
            .arg("json")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("JSON child");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(
                serde_json::to_string(request)
                    .expect("request JSON")
                    .as_bytes(),
            )
            .expect("send request");
        let output = child.wait_with_output().expect("JSON response");
        let response = serde_json::from_slice(&output.stdout).expect("one JSON response");
        (output, response)
    }
}

#[cfg(unix)]
fn make_fixture_directories_writable(path: &Path) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, OFlags, fchmod, fstat, open, openat, statat};
    use std::os::fd::{AsFd, BorrowedFd};

    fn visit(directory: BorrowedFd<'_>, depth: usize, entries: &mut usize) -> std::io::Result<()> {
        if depth > 128 {
            return Err(std::io::Error::other("fixture cleanup nesting limit"));
        }
        let metadata = fstat(directory)?;
        if FileType::from_raw_mode(metadata.st_mode) != FileType::Directory {
            return Err(std::io::Error::other(
                "fixture cleanup requires a directory",
            ));
        }
        let mode = Mode::from_raw_mode(metadata.st_mode);
        if !mode.contains(Mode::RWXU) {
            // Sealed cache directories prevent unlink on macOS. Only directory
            // FDs are changed; file permissions can be shared by outside links.
            fchmod(directory, mode | Mode::RWXU)?;
        }
        for entry in Dir::read_from(directory)? {
            let entry = entry?;
            let name = entry.file_name();
            if name == c"." || name == c".." {
                continue;
            }
            *entries += 1;
            if *entries > 65_536 {
                return Err(std::io::Error::other("fixture cleanup entry limit"));
            }
            let metadata = statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)?;
            if FileType::from_raw_mode(metadata.st_mode) == FileType::Directory {
                let child = openat(
                    directory,
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )?;
                visit(child.as_fd(), depth + 1, entries)?;
            }
        }
        Ok(())
    }

    let directory = open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    let mut entries = 0;
    visit(directory.as_fd(), 0, &mut entries)
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let Some(temp) = self.temp.take() else {
            return;
        };
        // Disable automatic best-effort deletion before either preserving a
        // failed test or performing cleanup with explicit error/readback checks.
        let path = temp.keep();
        if std::thread::panicking() {
            eprintln!("failed test fixture preserved at {}", path.display());
            return;
        }
        #[cfg(unix)]
        make_fixture_directories_writable(&path).unwrap_or_else(|error| {
            panic!(
                "fixture cleanup preparation failed; retained {}: {error}",
                path.display()
            )
        });
        fs::remove_dir_all(&path).unwrap_or_else(|error| {
            panic!(
                "fixture removal failed; retained {}: {error}",
                path.display()
            )
        });
        match fs::symlink_metadata(&path) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            result => panic!(
                "fixture removal not confirmed for {}: {result:?}",
                path.display()
            ),
        }
    }
}

fn text(value: &Value, key: &str) -> String {
    value[key]
        .as_str()
        .unwrap_or_else(|| panic!("missing {key}: {value}"))
        .to_owned()
}

#[test]
fn native_init_creates_only_the_new_final_directory_without_git() {
    let fixture = Fixture::new();
    let original_config = fs::read(fixture.home.join(".gitconfig")).expect("operator config");
    let destination = fixture.root.join("brand-new");
    assert!(!destination.exists());
    let output = fixture
        .command()
        .env("PATH", "")
        .args(["--json", "init", "brand-new"])
        .output()
        .expect("native new-project init");
    let initialized: Value = serde_json::from_slice(&output.stdout).expect("init receipt");
    assert!(output.status.success(), "{initialized}");
    assert!(destination.join(".izu").is_dir());
    assert_eq!(
        initialized["outcome"]["result"]["data"]["record"]["root"],
        destination.to_str().expect("destination")
    );
    let status = fixture
        .command()
        .current_dir(&destination)
        .env("PATH", "")
        .args(["--json", "status"])
        .output()
        .expect("native new-project status");
    let status: Value = serde_json::from_slice(&status.stdout).expect("status receipt");
    assert_eq!(status["outcome"]["kind"], "ok");
    assert_eq!(
        status["outcome"]["result"]["data"]["head"],
        initialized["outcome"]["result"]["data"]["expected"]["head"]
    );
    let (missing_parent, _) = fixture.run(&["init", "not-created/child"]);
    assert!(!missing_parent.status.success());
    assert!(!fixture.root.join("not-created").exists());
    let legacy = fixture.root.join(".ezy");
    fs::create_dir(&legacy).expect("owned legacy fixture");
    fs::write(legacy.join("retained-metadata"), "keep historical bytes").expect("legacy bytes");
    let (legacy_init, _) = fixture.run(&["init", "."]);
    assert!(!legacy_init.status.success());
    assert!(!fixture.root.join(".izu").exists());
    assert_eq!(
        fs::read_to_string(legacy.join("retained-metadata")).expect("legacy readback"),
        "keep historical bytes"
    );
    assert_eq!(
        fs::read(fixture.home.join(".gitconfig")).expect("config readback"),
        original_config
    );
}

#[test]
fn partial_commit_preserves_unselected_edits_and_global_configuration() {
    let fixture = Fixture::new();
    let original_config = fs::read(fixture.home.join(".gitconfig")).expect("config baseline");
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "selected\n").expect("source");
    fs::write(fixture.root.join("draft.txt"), "keep this edit\n").expect("unselected source");
    let commit = fixture.ok(&["commit", "-m", "selected commit", "--path", "tracked.txt"]);
    assert!(commit["revision"].as_str().is_some());
    assert_eq!(
        fs::read_to_string(fixture.root.join("draft.txt")).expect("unselected edit"),
        "keep this edit\n"
    );
    let status = fixture.ok(&["status"]);
    assert!(
        status["entries"]
            .as_array()
            .expect("entries")
            .iter()
            .any(|entry| entry["path"] == "draft.txt")
    );
    let log = fixture.ok(&["log"]);
    assert_eq!(log[0]["revision"]["description"], "selected commit");
    assert_eq!(
        fs::read(fixture.home.join(".gitconfig")).expect("config readback"),
        original_config
    );
    assert!(!fixture.home.join(".config/izu").exists());
    fixture.ok(&["verify"]);
}

#[test]
fn named_human_flow_is_native_private_checked_and_resumable_without_git() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "base\n").expect("base");
    let initial = fixture.ok(&["commit", "-m", "primary base"]);
    assert_eq!(fixture.ok(&["refs", "list"])["main"], initial["revision"]);

    let launched = fixture.command().env("PATH", "").args(["--json", "start", "feature", "--", "/usr/bin/python3", "-c", "from pathlib import Path; Path('tracked.txt').write_text('private edit\\n'); print(Path.cwd())"]).output().expect("native managed launch with no Git on PATH");
    let launch: Value = serde_json::from_slice(&launched.stdout).expect("managed JSON");
    assert!(launched.status.success(), "{launch}");
    let receipt = &launch["outcome"]["result"]["data"];
    let private = PathBuf::from(text(&receipt["workspace"]["record"], "root"));
    assert!(private.starts_with(fixture.root.join(".izu/workspaces")));
    assert_ne!(private, fixture.root);
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("primary"),
        "base\n"
    );
    assert_eq!(
        fs::read_to_string(private.join("tracked.txt")).expect("private"),
        "private edit\n"
    );
    assert!(receipt["after"]["operation"].is_string());
    assert_eq!(
        fixture.ok(&["--change", "feature", "status"])["workspace_name"],
        "feature"
    );

    let resumed = fixture.ok(&["start", "feature", "--", "/usr/bin/true"]);
    assert_eq!(resumed["workspace"]["id"], receipt["workspace"]["id"]);
    fixture.ok(&["--change", "feature", "run", "--", "/usr/bin/true"]);
    let committed = fixture.ok(&["--change", "feature", "commit", "-m", "intentional feature"]);
    assert_eq!(fixture.ok(&["refs", "list"])["main"], initial["revision"]);
    let (denied, _) = fixture.run(&["run", "--", "/usr/bin/true"]);
    assert_eq!(denied.status.code(), Some(2));

    let landed = fixture.ok(&[
        "--change",
        "feature",
        "land",
        "--current",
        "--check",
        "tests",
        "--",
        "/usr/bin/true",
    ]);
    assert_eq!(
        fixture.ok(&["refs", "list"])["main"],
        landed["land"]["revision"]
    );
    assert_eq!(
        landed["preparation"]["Ready"]["revision"],
        landed["land"]["revision"]
    );
    assert_eq!(
        fixture.ok(&["--change", "feature", "log"])[0]["id"],
        committed["revision"]
    );
    assert_eq!(
        fixture.ok(&["log", "main"])[0]["id"],
        landed["land"]["revision"]
    );
    let primary = fixture.ok(&["status"]);
    assert_eq!(primary["head"], initial["revision"]);
    assert_eq!(primary["main"], landed["land"]["revision"]);
    assert_eq!(primary["at_main"], false);
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("stable primary"),
        "base\n"
    );

    fs::write(fixture.root.join("tracked.txt"), "preserve dirty primary\n").expect("dirty source");
    let (dirty, dirty_receipt) = fixture.run(&["update"]);
    assert!(!dirty.status.success());
    assert_eq!(dirty_receipt["outcome"]["error"]["code"], "conflict");
    assert!(
        dirty_receipt["outcome"]["error"]["next_action"]
            .as_str()
            .expect("dirty next action")
            .contains("Preserve")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("retained dirty source"),
        "preserve dirty primary\n"
    );
    let (diverged, error) = fixture.run(&["commit", "-m", "cannot silently reset main"]);
    assert_eq!(diverged.status.code(), Some(4));
    assert_eq!(error["outcome"]["error"]["code"], "stale_expectation");
    // Only the owned fixture edit is deliberately restored for the clean update.
    fs::write(fixture.root.join("tracked.txt"), "base\n").expect("restore fixture baseline");
    let updated = fixture.ok(&["update"]);
    assert_eq!(updated["head"], landed["land"]["revision"]);
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("updated source"),
        "private edit\n"
    );
    assert!(
        fixture.ok(&["status"])["at_main"]
            .as_bool()
            .expect("current main")
    );
    fixture.ok(&["verify"]);
    fixture.ok(&["change", "close", "feature"]);
    assert!(private.exists(), "close must retain source");
    assert!(!fixture.home.join(".config/izu").exists());
}

fn independent_launches(count: usize) {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "primary stays stable\n").expect("source");
    fixture.ok(&["commit", "-m", "parallel baseline"]);
    let evidence = fixture
        .root
        .parent()
        .expect("owned parent")
        .join("overlap-evidence");
    fs::create_dir(&evidence).expect("owned evidence");
    // Bundled macOS Python 3.9 monotonic_ns has a process-local origin; use the
    // kernel clock so intervals from independent processes share one timeline.
    let script = "from pathlib import Path; import json, sys, time; index=int(sys.argv[2]); start=time.clock_gettime_ns(time.CLOCK_MONOTONIC); Path('private.txt').write_text(str(index)); time.sleep(1); end=time.clock_gettime_ns(time.CLOCK_MONOTONIC); Path(sys.argv[1],str(index)+'.json').write_text(json.dumps({'start':start,'end':end,'cwd':str(Path.cwd())}))";
    let mut children = OwnedChildren(Vec::new());
    for index in 0..count {
        let name = format!("parallel-{index}");
        let child = fixture
            .command()
            .args([
                "--json",
                "start",
                &name,
                "--timeout-seconds",
                "120",
                "--",
                "/usr/bin/python3",
                "-c",
                script,
            ])
            .arg(&evidence)
            .arg(index.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("independent managed CLI");
        children.0.push(Some(child));
    }
    let mut roots = std::collections::BTreeSet::new();
    let mut events = Vec::new();
    for index in 0..count {
        let output = children.0[index]
            .take()
            .expect("owned child")
            .wait_with_output()
            .expect("managed receipt");
        let response: Value =
            serde_json::from_slice(&output.stdout).expect("one parseable managed JSON response");
        assert!(
            output.status.success(),
            "index {index}: {response}, stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(response["outcome"]["kind"], "ok");
        let data = &response["outcome"]["result"]["data"];
        assert_eq!(
            data.pointer("/job/state/Finished/outcome/Exit/code"),
            Some(&json!(0))
        );
        let root = PathBuf::from(text(&data["workspace"]["record"], "root"));
        assert_eq!(
            data.pointer("/job/request/workspace/cwd")
                .and_then(Value::as_str),
            root.to_str()
        );
        assert!(root.starts_with(fixture.root.join(".izu/workspaces")));
        assert_eq!(
            fs::read_to_string(root.join("private.txt")).expect("exact private edit"),
            index.to_string()
        );
        assert!(roots.insert(root.clone()));
        assert!(data["after"]["operation"].is_string());
        let interval: Value = serde_json::from_slice(
            &fs::read(evidence.join(format!("{index}.json"))).expect("actual process interval"),
        )
        .expect("interval JSON");
        assert_eq!(interval["cwd"].as_str(), root.to_str());
        let start = interval["start"].as_u64().expect("start");
        let end = interval["end"].as_u64().expect("end");
        assert!(end > start);
        events.push((start, 1));
        events.push((end, -1));
    }
    events.sort_unstable();
    let mut active = 0;
    let mut peak = 0;
    for (_, change) in events {
        active += change;
        peak = peak.max(active);
    }
    assert!(
        peak >= 2,
        "independent processes were serialized: peak {peak}"
    );
    assert!(
        peak <= 4,
        "shared admission exceeded default four running jobs: peak {peak}"
    );
    assert_eq!(roots.len(), count);
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("primary"),
        "primary stays stable\n"
    );
    assert!(!fixture.root.join("private.txt").exists());
    fixture.ok(&["verify"]);
}

#[test]
fn two_independent_managed_cli_controllers_overlap() {
    independent_launches(2);
}

#[test]
fn thirty_independent_managed_cli_controllers_share_four_job_admission() {
    independent_launches(30);
}

#[test]
fn starting_a_new_check_invalidates_an_old_pass_and_failed_recheck_blocks_land() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "base\n").expect("base");
    let base = fixture.ok(&["commit", "-m", "base"]);
    let flag = fixture
        .root
        .parent()
        .expect("fixture parent")
        .join("check-mode");
    fs::write(&flag, "pass").expect("explicit external check input");
    let script = "from pathlib import Path; import sys; sys.exit(0 if Path(sys.argv[1]).read_text() == 'pass' else 1)";
    let prepared = fixture.ok(&[
        "candidate",
        base["revision"].as_str().expect("revision"),
        "--target",
        "main",
        "--expect",
        base["revision"].as_str().expect("revision"),
        "--check",
        "toggle",
        "--",
        "/usr/bin/python3",
        "-c",
        script,
        flag.to_str().expect("flag"),
    ]);
    let candidate = text(&prepared["Ready"], "candidate");
    fixture.ok(&["check", &candidate, "--check", "toggle"]);
    let pending = fixture.ok(&["check", &candidate, "--check", "toggle", "--start-only"]);
    assert!(pending["attempt"].is_string());
    assert_eq!(pending["pending"]["outcome"]["status"], "pending");
    assert!(pending["pending"]["finished_at_unix_ms"].is_null());
    let (unpassed, _) = fixture.run(&["land", &candidate]);
    assert_eq!(unpassed.status.code(), Some(5));
    fs::write(flag, "fail").expect("explicit check input");
    let (failed, failed_receipt) = fixture.run(&["check", &candidate, "--check", "toggle"]);
    assert_eq!(failed.status.code(), Some(5));
    assert!(
        failed_receipt
            .pointer("/outcome/result/data/evidence")
            .and_then(Value::as_str)
            .is_some()
    );
    let (cannot_land, _) = fixture.run(&["land", &candidate]);
    assert_eq!(cannot_land.status.code(), Some(5));
    assert_eq!(fixture.ok(&["refs", "list"])["main"], base["revision"]);
    fixture.ok(&["verify"]);
}

#[cfg(unix)]
#[test]
fn environment_cli_trust_materialization_and_bound_starting_checks_are_exact() {
    let fixture = Fixture::new();
    let initialized = fixture.ok(&["init", "."]);
    fs::write(fixture.root.join(".gitignore"), "node_modules/\ntarget/\n")
        .expect("ignore prepared files");
    fs::write(fixture.root.join("tracked.txt"), "base\n").expect("source");
    fixture.ok(&["commit", "-m", "source identity"]);
    let started = fixture.ok(&["start", "prepared", "--", "/usr/bin/true"]);
    let private = PathBuf::from(text(&started["workspace"]["record"], "root"));
    let identity = fixture.ok(&["--change", "prepared", "environment", "source-identity"]);
    let parent = fixture.root.parent().expect("owned fixture parent");
    let prepared = parent.join("prepared-input");
    fs::create_dir_all(prepared.join("node_modules/pkg")).expect("prepared dependency");
    fs::create_dir_all(prepared.join("target")).expect("prepared output");
    fs::write(
        prepared.join("node_modules/pkg/data"),
        "prepared dependency",
    )
    .expect("dependency");
    fs::write(prepared.join("target/app"), "prepared output").expect("output");
    let never_execute = parent.join("never-execute-on-discovery");
    let recipe = parent.join("recipe.json");
    fs::write(&recipe, serde_json::to_vec(&json!({
        "schema_version":1,"source_identity":identity["source_identity"],"lockfiles":[],
        "toolchain_identity":"a".repeat(64),"recipe_identity":"b".repeat(64),
        "platform":{"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"abi":"acceptance"},
        "trust_domain":"acceptance","argv":["/usr/bin/python3","-c",format!("from pathlib import Path; Path({:?}).write_text('unauthorized')",never_execute)],
        "dependencies":["node_modules"],"outputs":["target"]
    })).expect("recipe JSON")).expect("explicit recipe");
    let recipe_path = recipe.to_str().expect("recipe path");
    let (untrusted, _) = fixture.run(&[
        "environment",
        "import",
        prepared.to_str().expect("prepared"),
        "--recipe",
        recipe_path,
        "--quiescent",
    ]);
    assert_eq!(untrusted.status.code(), Some(2));
    fixture.ok(&[
        "environment",
        "import",
        prepared.to_str().expect("prepared"),
        "--recipe",
        recipe_path,
        "--trust-recipe",
        "--quiescent",
        "--copy",
    ]);
    fixture.ok(&[
        "environment",
        "status",
        "--recipe",
        recipe_path,
        "--trust-recipe",
    ]);
    let (primary, primary_error) = fixture.run(&[
        "environment",
        "materialize",
        "--recipe",
        recipe_path,
        "--trust-recipe",
    ]);
    assert_eq!(primary.status.code(), Some(2));
    assert_eq!(primary_error["outcome"]["error"]["code"], "invalid_request");
    let materialized = fixture.ok(&[
        "--change",
        "prepared",
        "environment",
        "materialize",
        "--recipe",
        recipe_path,
        "--trust-recipe",
        "--copy",
    ]);
    assert_eq!(materialized["copied_files"], 2);
    assert_eq!(materialized["private_file_inodes"], true);
    assert_eq!(materialized["security_boundary"], false);
    assert_eq!(
        fs::read_to_string(private.join("node_modules/pkg/data")).expect("materialized dependency"),
        "prepared dependency"
    );
    assert!(!fixture.root.join("node_modules").exists());
    assert!(
        !never_execute.exists(),
        "recipe parsing/import/materialization must not execute argv"
    );

    let before_path = parent.join("before-binding.izu-bundle");
    let before = fixture.ok(&["bundle", "create", before_path.to_str().expect("bundle")]);
    let binding = fixture.ok(&["environment", "bind", recipe_path, "--trust-recipe"]);
    assert_eq!(binding["storage"], "durable_local_blob");
    assert_eq!(binding["reachability"], "unreferenced_until_candidate");
    let object_id = text(&binding, "object_id");
    assert_ne!(binding["object_id"], binding["key"]);
    assert_eq!(binding["source_identity"], identity["source_identity"]);
    let mut client = McpClient::new(&fixture);
    client.send(&json!({
        "jsonrpc":"2.0","id":20,"method":"tools/call",
        "params":{"_meta":modern_meta(),"name":"izu_environment_bind",
            "arguments":{"schema_version":1,"operation":{
                "kind":"environment_bind",
                "context":{"repository":fixture.root,"workspace":null},
                "recipe_json":fs::read_to_string(&recipe).expect("exact recipe"),
                "trusted":true
            }}
        }
    }));
    let mcp_binding = client.receive();
    assert_eq!(mcp_binding["result"]["isError"], false, "{mcp_binding}");
    assert_eq!(
        mcp_binding.pointer("/result/structuredContent/outcome/result/data"),
        Some(&binding),
        "CLI and live MCP must return the same native binding receipt"
    );
    drop(client);
    let unrooted_path = parent.join("unreferenced-binding.izu-bundle");
    let unrooted = fixture.ok(&["bundle", "create", unrooted_path.to_str().expect("bundle")]);
    assert_eq!(
        before["verification"]["manifest"]["root_operation"],
        unrooted["verification"]["manifest"]["root_operation"]
    );
    assert_eq!(
        before["verification"]["objects"]["blobs"],
        unrooted["verification"]["objects"]["blobs"]
    );

    let source = text(&started["workspace"]["expected"], "head");
    let check_script = "from pathlib import Path; assert Path('node_modules/pkg/data').read_text() == 'prepared dependency'; assert Path('target/app').read_text() == 'prepared output'; Path('target/app').write_text('check owns writable warm output'); print('observed exact prepared starting files')";
    let preparation = fixture.ok(&[
        "candidate",
        &source,
        "--target",
        "environment-checked",
        "--expect-absent",
        "--check",
        "prepared",
        "--environment",
        &object_id,
        "--",
        "/usr/bin/python3",
        "-c",
        check_script,
    ]);
    let candidate = text(&preparation["Ready"], "candidate");
    assert_eq!(
        fixture.ok(&["candidate-show", &candidate])["checks"][0]["environment"],
        object_id
    );
    let rooted_path = parent.join("rooted-binding.izu-bundle");
    let rooted = fixture.ok(&["bundle", "create", rooted_path.to_str().expect("bundle")]);
    assert_eq!(
        rooted["verification"]["objects"]["blobs"]
            .as_u64()
            .expect("rooted blobs"),
        before["verification"]["objects"]["blobs"]
            .as_u64()
            .expect("baseline blobs")
            + 1
    );
    let cold = parent.join("cold-binding-restored");
    fixture.ok(&[
        "bundle",
        "restore",
        rooted_path.to_str().expect("rooted bundle"),
        cold.to_str().expect("cold restored repository"),
        "--selected-workspace",
        &text(&initialized, "id"),
    ]);
    assert!(
        !cold.join(".izu/environments/v1/artifacts").exists(),
        "cold bundles retain binding descriptors and omit warm payloads"
    );
    let mut cold_client = McpClient::new(&fixture);
    cold_client.send(&json!({
        "jsonrpc":"2.0","id":22,"method":"tools/call",
        "params":{"_meta":modern_meta(),"name":"izu_candidate_show",
            "arguments":{"schema_version":1,"operation":{
                "kind":"candidate_show",
                "context":{"repository":cold,"workspace":null},
                "candidate":candidate
            }}
        }
    }));
    let cold_candidate = cold_client.receive();
    assert_eq!(
        cold_candidate["result"]["isError"], false,
        "{cold_candidate}"
    );
    assert_eq!(
        cold_candidate
            .pointer("/result/structuredContent/outcome/result/data/checks/0/environment"),
        Some(&binding["object_id"])
    );
    cold_client.send(&json!({
        "jsonrpc":"2.0","id":23,"method":"tools/call",
        "params":{"_meta":modern_meta(),"name":"izu_check",
            "arguments":{"schema_version":1,"operation":{
                "kind":"check",
                "context":{"repository":cold,"workspace":null},
                "candidate":candidate,"check":"prepared","timeout_seconds":300
            }}
        }
    }));
    let cold_check = cold_client.receive();
    assert_eq!(cold_check["result"]["isError"], true, "{cold_check}");
    assert_eq!(
        cold_check.pointer("/result/structuredContent/outcome/error/code"),
        Some(&json!("not_found")),
        "cold restore must read the rooted native binding and refuse the omitted cache"
    );
    drop(cold_client);
    let mut client = McpClient::new(&fixture);
    client.send(&json!({
        "jsonrpc":"2.0","id":21,"method":"tools/call",
        "params":{"_meta":modern_meta(),"name":"izu_check",
            "arguments":{"schema_version":1,"operation":{
                "kind":"check",
                "context":{"repository":fixture.root,"workspace":null},
                "candidate":candidate,"check":"prepared","timeout_seconds":300
            }}
        }
    }));
    let mcp_check = client.receive();
    assert_eq!(mcp_check["result"]["isError"], false, "{mcp_check}");
    let checked = mcp_check["result"]["structuredContent"]["outcome"]["result"]["data"].clone();
    drop(client);
    assert_eq!(checked["job"]["request"]["environment_binding"], object_id);
    let proof = &checked["job"]["starting_environment"];
    assert_eq!(proof["key"], binding["key"]);
    assert_eq!(proof["manifest_digest"], binding["manifest_digest"]);
    assert_eq!(proof["source_identity"], binding["source_identity"]);
    assert_eq!(
        proof["workspace"],
        checked["job"]["request"]["workspace"]["id"]
    );
    assert_eq!(proof["scope"], "StartingFileContents");
    assert_eq!(proof["security_boundary"], false);
    assert_eq!(proof["toolchain"]["declared_identity"], "a".repeat(64));
    assert_eq!(proof["platform"]["observed_os"], std::env::consts::OS);
    assert_eq!(
        proof["platform"]["observed_architecture"],
        std::env::consts::ARCH
    );
    assert_eq!(proof["platform"]["declared_abi"], "acceptance");
    let advertised = fixture.ok(&["capabilities"]);
    assert_eq!(advertised["environment_checks"]["scope"], proof["scope"]);
    assert_eq!(
        advertised["environment_checks"]["toolchain_identity"],
        "caller_declared"
    );
    assert_eq!(advertised["environment_checks"]["abi"], "caller_declared");
    let check_root = PathBuf::from(text(&checked["job"]["request"]["workspace"], "cwd"));
    assert_ne!(check_root, fixture.root);
    assert_ne!(check_root, private);
    assert_eq!(
        fs::read_to_string(check_root.join("target/app")).expect("writable private warm output"),
        "check owns writable warm output"
    );
    assert_eq!(
        fs::read_to_string(private.join("target/app")).expect("retained prepared workspace output"),
        "prepared output"
    );

    let cache_parent = fixture.root.join(".izu/environments/v1/artifacts");
    let artifact = cache_parent.join(text(&binding, "key"));
    let retained_artifact = cache_parent.join("fixture-retained-exact-artifact");
    // The controlled missing-cache fixture moves its own sealed directory.
    // Some native macOS runners require owner write permission for this rename;
    // restore the original seal before invoking any product operation.
    let artifact_seal = fs::metadata(&artifact)
        .expect("owned artifact seal")
        .permissions();
    fs::set_permissions(&artifact, fs::Permissions::from_mode(0o700))
        .expect("prepare owned fixture rename");
    fs::rename(&artifact, &retained_artifact).expect("hide only owned cache artifact");
    fs::set_permissions(&retained_artifact, artifact_seal.clone())
        .expect("retain original artifact seal");
    let (missing_cache, missing_receipt) =
        fixture.run(&["check", &candidate, "--check", "prepared"]);
    assert!(!missing_cache.status.success(), "{missing_receipt}");
    assert_eq!(missing_receipt["outcome"]["error"]["code"], "not_found");
    let (old_pass, _) = fixture.run(&["land", &candidate]);
    assert_eq!(
        old_pass.status.code(),
        Some(5),
        "an absent starting environment must not reuse an old pass"
    );
    fs::set_permissions(&retained_artifact, fs::Permissions::from_mode(0o700))
        .expect("prepare owned fixture restoration");
    fs::rename(&retained_artifact, &artifact).expect("restore exact retained fixture artifact");
    fs::set_permissions(&artifact, artifact_seal).expect("restore exact artifact seal");
    let rechecked = fixture.ok(&["check", &candidate, "--check", "prepared"]);
    assert_ne!(checked["evidence"], rechecked["evidence"]);
    fixture.ok(&["land", &candidate]);
    assert!(
        !never_execute.exists(),
        "binding and check launch must not run preparation argv"
    );
    assert!(
        !fixture.root.join("node_modules").exists(),
        "bound checks retain the primary source"
    );
    fixture.ok(&["verify"]);

    // A successful fixture must remove its sealed cache without changing an
    // outside inode's permissions or following a directory symlink.
    use std::os::unix::fs::{PermissionsExt, symlink};
    let outside = Fixture::new();
    let cached_ready = artifact.join("READY");
    let linked_ready = outside.root.join("outside-linked-ready");
    fs::hard_link(&cached_ready, &linked_ready).expect("outside hardlink sentinel");
    let ready_mode = fs::metadata(&linked_ready)
        .expect("sealed READY")
        .permissions()
        .mode();
    assert_eq!(ready_mode & 0o222, 0, "the hardlink sentinel is sealed");
    let outside_directory = rustix::fs::open(
        &outside.root,
        rustix::fs::OFlags::RDONLY
            | rustix::fs::OFlags::DIRECTORY
            | rustix::fs::OFlags::NOFOLLOW
            | rustix::fs::OFlags::CLOEXEC,
        rustix::fs::Mode::empty(),
    )
    .expect("owned outside directory");
    rustix::fs::fchmod(&outside_directory, rustix::fs::Mode::from_raw_mode(0o555))
        .expect("directory-only sentinel seal");
    symlink(&outside.root, fixture.root.join("outside-directory-alias"))
        .expect("outside directory symlink sentinel");
    let removed = fixture
        .root
        .parent()
        .expect("owned fixture parent")
        .to_path_buf();
    drop(fixture);
    assert!(
        matches!(
            fs::symlink_metadata(&removed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "successful fixture cleanup must be observed as NotFound"
    );
    assert_eq!(
        fs::metadata(&linked_ready)
            .expect("retained outside hardlink")
            .permissions()
            .mode(),
        ready_mode,
        "fixture cleanup must never chmod a file through an outside hardlink"
    );
    assert_eq!(
        fs::metadata(&outside.root)
            .expect("retained outside directory")
            .permissions()
            .mode()
            & 0o777,
        0o555,
        "fixture cleanup must not follow a directory symlink"
    );
    drop(outside_directory);
    let outside_removed = outside
        .root
        .parent()
        .expect("owned outside parent")
        .to_path_buf();
    drop(outside);
    assert!(
        matches!(
            fs::symlink_metadata(&outside_removed),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound
        ),
        "outside sentinel fixture cleanup must also be confirmed"
    );
}

#[test]
fn scoped_primary_revert_advances_main_and_preserves_unselected_edits() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "before\n").expect("tracked source");
    fs::write(fixture.root.join("draft.txt"), "draft baseline\n").expect("draft source");
    fixture.ok(&["commit", "-m", "baseline"]);
    fs::write(fixture.root.join("tracked.txt"), "after\n").expect("intentional change");
    let changed = fixture.ok(&["commit", "-m", "target change", "--path", "tracked.txt"]);
    fs::write(fixture.root.join("draft.txt"), "preserve unselected dirt\n")
        .expect("unselected edit");
    let reverted = fixture.ok(&[
        "revert",
        changed["revision"].as_str().expect("changed revision"),
        "--path",
        "tracked.txt",
        "-m",
        "Revert selected change",
    ]);
    assert_eq!(
        fixture.ok(&["refs", "list"])["main"],
        reverted["Applied"]["revision"]
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("reverted selected source"),
        "before\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("draft.txt")).expect("unselected dirt"),
        "preserve unselected dirt\n"
    );
    assert_eq!(
        fixture.ok(&["log"])[0]["revision"]["description"],
        "Revert selected change"
    );
    fixture.ok(&["verify"]);
}

#[cfg(unix)]
#[test]
fn tls_root_fifo_is_rejected_before_any_remote_operation() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    let fifo = fixture
        .root
        .parent()
        .expect("owned parent")
        .join("certificate-fifo");
    assert!(
        Command::new("/usr/bin/mkfifo")
            .arg(&fifo)
            .status()
            .expect("owned FIFO")
            .success()
    );
    let child = fixture
        .command()
        .args([
            "--json",
            "git",
            "fetch",
            "https://example.invalid/owned.git",
            "--tls-root-cert",
        ])
        .arg(&fifo)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("exact CLI input boundary");
    let mut owned = OwnedChildren(vec![Some(child)]);
    let deadline = Instant::now() + Duration::from_secs(2);
    while owned.0[0]
        .as_mut()
        .expect("owned child")
        .try_wait()
        .expect("input-boundary child")
        .is_none()
    {
        assert!(
            Instant::now() < deadline,
            "FIFO input blocked waiting for a writer"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let output = owned.0[0]
        .take()
        .expect("owned child")
        .wait_with_output()
        .expect("bounded response");
    assert_eq!(output.status.code(), Some(2));
    let response: Value = serde_json::from_slice(&output.stdout).expect("machine error");
    assert_eq!(response["outcome"]["error"]["code"], "invalid_request");
    assert_eq!(
        fixture
            .ok(&["jobs", "list"])
            .as_array()
            .expect("no launched jobs")
            .len(),
        0
    );
}

#[cfg(unix)]
#[test]
fn crashed_controller_requires_exact_writer_and_job_stopped_acknowledgements() {
    let fixture = Fixture::new();
    let initial = fixture.ok(&["init", "."]);
    let parent = fixture.root.parent().expect("owned parent");
    let started = parent.join("writer-started");
    let stopped_lease = parent.join("writer-observation.lock");
    // The worker watchdog uses SIGKILL after controller loss. Observe the
    // fixture writer's actual kernel lease release, independent of signal
    // handlers, saved PIDs, or an elapsed quiet period.
    let script = "from pathlib import Path; import fcntl, sys, time; lease=open(sys.argv[2],'w'); fcntl.flock(lease,fcntl.LOCK_EX); Path('unique.txt').write_text('preserved crash edit'); Path(sys.argv[1]).write_text('started'); time.sleep(60)";
    let child = fixture
        .command()
        .args([
            "--json",
            "start",
            "crashed",
            "--timeout-seconds",
            "120",
            "--",
            "/usr/bin/python3",
            "-c",
            script,
        ])
        .arg(&started)
        .arg(&stopped_lease)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("owned launch controller");
    let mut owned = OwnedChildren(vec![Some(child)]);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !started.exists() {
        assert!(Instant::now() < deadline, "managed writer never started");
        std::thread::sleep(Duration::from_millis(10));
    }
    let jobs = fixture.ok(&["--change", "crashed", "jobs", "list"]);
    let job = text(&jobs[0], "id");
    let writer = fixture.ok(&["--change", "crashed", "writer", "status"]);
    let token = text(&writer["intent"], "token");
    let (live, _) = fixture.run(&[
        "--change",
        "crashed",
        "writer",
        "acknowledge-stopped",
        &token,
        "--note",
        "invalid live assertion must be refused",
    ]);
    assert_eq!(live.status.code(), Some(4));
    owned.0[0]
        .as_mut()
        .expect("owned child")
        .kill()
        .expect("crash only owned controller");
    owned.0[0]
        .as_mut()
        .expect("owned child")
        .wait()
        .expect("owned controller reaped");
    let deadline = Instant::now() + Duration::from_secs(15);
    let observation = fs::File::open(&stopped_lease).expect("observed writer lease");
    loop {
        match observation.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {
                assert!(
                    Instant::now() < deadline,
                    "owned worker did not stop its chosen process after controller EOF"
                );
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::Error(error)) => panic!("observe fixture writer: {error}"),
        }
    }
    drop(observation);
    let status = fixture.ok(&["jobs", "status", &job]);
    assert!(
        status["state"].get("OwnershipUnknown").is_some(),
        "{status}"
    );
    let initial_revision = text(&initial["expected"], "head");
    let (blocked, _) = fixture.run(&["--change", "crashed", "restore", &initial_revision]);
    assert_eq!(blocked.status.code(), Some(4));
    let (launch_blocked, _) = fixture.run(&["--change", "crashed", "run", "--", "/usr/bin/true"]);
    assert_eq!(launch_blocked.status.code(), Some(4));
    let wrong = if token == "00".repeat(16) {
        "11".repeat(16)
    } else {
        "00".repeat(16)
    };
    let (mismatch, _) = fixture.run(&[
        "--change",
        "crashed",
        "writer",
        "acknowledge-stopped",
        &wrong,
        "--note",
        "Mismatched fixture token must not clear intent",
    ]);
    assert_eq!(mismatch.status.code(), Some(4));
    assert_eq!(
        fixture.ok(&["--change", "crashed", "writer", "status"])["intent"]["token"],
        token
    );
    fixture.ok(&[
        "--change",
        "crashed",
        "writer",
        "acknowledge-stopped",
        &token,
        "--note",
        "Owned acceptance process released its held kernel lease after controller loss; no external fixture writers remain",
    ]);
    assert!(fixture.ok(&["--change", "crashed", "writer", "status"])["intent"].is_null());
    let recovered = fixture.ok(&[
        "jobs",
        "acknowledge-stopped",
        &job,
        "--note",
        "Exact engine writer was explicitly reconciled after observed fixture stop",
    ]);
    assert!(recovered["state"].get("RecoveredStopped").is_some());
    let workspace = fixture.ok(&["change", "open", "crashed"]);
    let private = PathBuf::from(text(&workspace["record"], "root"));
    assert_eq!(
        fs::read_to_string(private.join("unique.txt")).expect("retained source"),
        "preserved crash edit"
    );
    fixture.ok(&["--change", "crashed", "run", "--", "/usr/bin/true"]);
    fixture.ok(&["verify"]);
    let operations = fixture.ok(&["operations"]);
    assert!(
        operations
            .as_array()
            .expect("operation history")
            .iter()
            .any(|operation| operation["operation"]["description"]
                .as_str()
                .is_some_and(|description| description
                    .contains("Owned acceptance process released its held kernel lease")))
    );
}

#[test]
fn private_workspace_check_and_land_use_exact_native_candidate() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "base\n").expect("source");
    let commit = fixture.ok(&["commit", "-m", "base"]);
    let base = text(&commit, "revision");
    assert_eq!(fixture.ok(&["refs", "list"])["main"], base);
    let private = fixture.root.parent().expect("parent").join("private");
    let started = fixture.ok(&[
        "change",
        "start",
        "--name",
        "private",
        "--path",
        private.to_str().expect("path"),
        "--from",
        &base,
    ]);
    let workspace = text(&started, "id");
    fs::write(private.join("tracked.txt"), "private edit\n").expect("private source");
    let changed = fixture.ok(&["--workspace", &workspace, "commit", "-m", "private change"]);
    let source = text(&changed, "revision");
    let candidate = fixture.ok(&[
        "candidate",
        &source,
        "--target",
        "main",
        "--expect",
        &base,
        "--check",
        "acceptance",
        "--",
        "/usr/bin/true",
    ]);
    let candidate_id = text(&candidate["Ready"], "candidate");
    let (early, early_response) = fixture.run(&["land", &candidate_id]);
    assert_eq!(early.status.code(), Some(5));
    assert_eq!(
        early_response
            .pointer("/outcome/error/code")
            .and_then(Value::as_str),
        Some("missing_check")
    );
    let checked = fixture.ok(&[
        "check",
        &candidate_id,
        "--check",
        "acceptance",
        "--timeout-seconds",
        "10",
    ]);
    assert!(checked["evidence"].as_str().is_some());
    let landed = fixture.ok(&["land", &candidate_id]);
    let refs = fixture.ok(&["refs", "list"]);
    assert_eq!(refs["main"], landed["revision"]);
    let log = fixture.ok(&[
        "log",
        "--from",
        landed["revision"].as_str().expect("landed revision"),
    ]);
    assert!(
        log.as_array()
            .expect("history")
            .iter()
            .any(|record| record["revision"]["description"] == "private change")
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("primary source"),
        "base\n"
    );
    fixture.ok(&["status"]);
    fixture.ok(&["workspace", "close", &workspace]);
    assert!(private.join("tracked.txt").exists());
    fixture.ok(&["verify"]);
}

#[test]
fn stale_json_expectation_and_invalid_input_have_typed_errors() {
    let fixture = Fixture::new();
    let init = fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "new\n").expect("source");
    fixture.ok(&["commit", "-m", "new"]);
    let request = json!({"schema_version":1,"operation":{"kind":"checkpoint","context":{"repository":fixture.root,"workspace":init["id"],"expected":init["expected"]},"selection":{"kind":"all"}}});
    let (output, response) = fixture.json(&request);
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(
        response
            .pointer("/outcome/error/code")
            .and_then(Value::as_str),
        Some("stale_expectation")
    );
    let (output, response) =
        fixture.json(&json!({"schema_version":999,"operation":{"kind":"capabilities"}}));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        response
            .pointer("/outcome/error/code")
            .and_then(Value::as_str),
        Some("unsupported_schema")
    );
    let (output, response) = fixture.json(&json!({"schema_version":1,"operation":{"kind":"show","context":{"repository":fixture.root,"workspace":null},"revision":"not-an-id"}}));
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(
        response
            .pointer("/outcome/error/code")
            .and_then(Value::as_str),
        Some("invalid_request")
    );
    let (output, response) = fixture.run(&["show", "missing-reference"]);
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(response["outcome"]["error"]["code"], "not_found");
}

#[cfg(unix)]
struct McpClient {
    child: Child,
    input: Option<ChildStdin>,
    output: Option<ChildStdout>,
    errors: Option<ChildStderr>,
    stdout: RawCapture,
    stderr: RawCapture,
    sent: RawCapture,
    frame: Vec<u8>,
    replies: VecDeque<Value>,
    started: Instant,
    phase: &'static str,
    events: Vec<Value>,
    drain_stderr: bool,
}

#[cfg(unix)]
const MCP_RESPONSE_DEADLINE: Duration = Duration::from_secs(15);
#[cfg(unix)]
const MCP_CAPTURE_BYTES: usize = 64 * 1024;

#[cfg(unix)]
#[derive(Default)]
struct RawCapture {
    tail: VecDeque<u8>,
    total: u64,
}

#[cfg(unix)]
impl RawCapture {
    fn append(&mut self, bytes: &[u8]) {
        self.total = self.total.saturating_add(bytes.len() as u64);
        for byte in bytes {
            if self.tail.len() == MCP_CAPTURE_BYTES {
                self.tail.pop_front();
            }
            self.tail.push_back(*byte);
        }
    }

    fn diagnostic(&self) -> Value {
        json!({"total_bytes":self.total,"retained_tail_bytes":self.tail.iter().copied().collect::<Vec<_>>(),"truncated":self.total > self.tail.len() as u64})
    }
}

#[cfg(unix)]
impl McpClient {
    fn new(fixture: &Fixture) -> Self {
        let child = fixture
            .command()
            .args(["agent", "serve"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("MCP child");
        Self::from_child(child)
    }

    fn from_child(mut child: Child) -> Self {
        use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
        let input = child.stdin.take();
        let output = child.stdout.take();
        let errors = child.stderr.take();
        // The test owns these freshly created parent pipe endpoints; no reader
        // thread is detached and no bytes disappear into a lines() iterator.
        for fd in [
            input.as_ref().map(|fd| std::os::fd::AsFd::as_fd(fd)),
            output.as_ref().map(|fd| std::os::fd::AsFd::as_fd(fd)),
            errors.as_ref().map(|fd| std::os::fd::AsFd::as_fd(fd)),
        ]
        .into_iter()
        .flatten()
        {
            let flags = fcntl_getfl(fd).expect("test pipe flags");
            fcntl_setfl(fd, flags | OFlags::NONBLOCK).expect("nonblocking owned test pipe");
        }
        Self {
            child,
            input,
            output,
            errors,
            stdout: RawCapture::default(),
            stderr: RawCapture::default(),
            sent: RawCapture::default(),
            frame: Vec::new(),
            replies: VecDeque::new(),
            started: Instant::now(),
            phase: "spawned",
            events: Vec::new(),
            drain_stderr: true,
        }
    }
    fn send(&mut self, value: &Value) {
        self.raw(&format!("{}\n", serde_json::to_string(value).expect("RPC")));
    }
    fn raw(&mut self, text: &str) {
        self.raw_with_drain(text, true);
    }

    fn raw_with_drain(&mut self, text: &str, drain: bool) {
        use rustix::io::{Errno, write};
        assert!(
            text.len() <= izu_api::MAX_REQUEST_BYTES + 4096,
            "bounded test request"
        );
        self.phase = "writing_request";
        self.event("request_started", json!({"requested_bytes":text.len()}));
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut offset = 0;
        while offset < text.len() {
            assert!(
                Instant::now() < deadline,
                "MCP write deadline: {}",
                self.diagnostic("write timeout")
            );
            let end = (offset + 16 * 1024).min(text.len());
            let result = write(
                self.input.as_ref().expect("live stdin"),
                &text.as_bytes()[offset..end],
            );
            match result {
                Ok(0) => panic!("MCP input closed: {}", self.diagnostic("zero write")),
                Ok(length) => {
                    self.sent.append(&text.as_bytes()[offset..offset + length]);
                    offset += length;
                }
                Err(Errno::AGAIN | Errno::INTR) => {
                    let retry = (Instant::now() + Duration::from_millis(10)).min(deadline);
                    if let Err(error) = self.pump(retry, drain, false) {
                        panic!("MCP write failed: {}", self.diagnostic(&error));
                    }
                }
                Err(error) => panic!("MCP write failed: {}", self.diagnostic(&error.to_string())),
            }
        }
        self.event("request_written", json!({"written_bytes":offset}));
    }

    fn receive(&mut self) -> Value {
        self.receive_result()
            .unwrap_or_else(|error| panic!("bounded MCP response deadline: {error}"))
    }

    fn receive_result(&mut self) -> Result<Value, Value> {
        self.phase = "awaiting_response";
        self.event("response_wait_started", json!({"deadline_seconds":15}));
        let deadline = Instant::now() + MCP_RESPONSE_DEADLINE;
        loop {
            if let Some(reply) = self.replies.pop_front() {
                self.event("response_received", json!({"id":reply.get("id")}));
                return Ok(reply);
            }
            if Instant::now() >= deadline {
                let mut failure = self.diagnostic("response timeout");
                failure["native_capture"] = self.capture_deadline();
                return Err(failure);
            }
            if self.child.try_wait().expect("child status").is_some() && self.output.is_none() {
                return Err(self.diagnostic("child exited without a complete response"));
            }
            if let Err(error) = self.pump(deadline, true, false) {
                return Err(self.diagnostic(&error));
            }
        }
    }

    // Capture only after the response bound has failed, while the exact child
    // and all original pipe endpoints are still alive. Cleanup follows capture.
    fn capture_deadline(&mut self) -> Value {
        self.capture_deadline_with_script(
            &Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../../scripts/mcp-deadline-diagnostics.py"),
        )
    }

    fn capture_deadline_with_script(&mut self, script: &Path) -> Value {
        self.capture_deadline_command(script, None)
    }

    fn capture_deadline_command(&mut self, script: &Path, probe: Option<&str>) -> Value {
        use izu_process::{BaseEnvironment, CommandSpec, RunOptions, WorkerLauncher};
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let directory = root
            .join(".artifacts/local/mcp-deadline-captures")
            .join(format!(
                "{}-{}-{}",
                std::process::id(),
                self.child.id(),
                self.started.elapsed().as_nanos()
            ));
        let mut report = json!({"parent":std::process::id(),"child":self.child.id(),
            "before_cleanup":true,"status":"helper_not_started","commands":[],
            "owner_started_unix_ns":std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH).expect("clock after epoch").as_nanos().to_string()});
        fs::create_dir_all(&directory).expect("owned diagnostic directory");
        let save = |value: &Value| {
            let temporary = directory.join("capture-owner.tmp");
            fs::write(&temporary, serde_json::to_vec(value).expect("capture JSON"))
                .expect("capture progress");
            fs::rename(temporary, directory.join("capture.json")).expect("atomic capture progress");
        };
        save(&report);
        let mut command = CommandSpec::new("/usr/bin/python3");
        command
            .arg("-B")
            .arg(script)
            .arg("--parent")
            .arg(std::process::id().to_string())
            .arg("--child")
            .arg(self.child.id().to_string())
            .arg("--output")
            .arg(&directory);
        if let Some(probe) = probe {
            command.arg("--test-probe").arg(probe);
        }
        let worker = WorkerLauncher {
            executable: fs::canonicalize(BINARY).expect("exact Cargo CLI worker"),
            prefix_args: vec!["__process-worker".into()],
        };
        // The original 8 s total capture allowance includes group cleanup.
        // run owns the unreaped worker identity and stops its whole group.
        let options = RunOptions {
            timeout: Duration::from_secs(7),
            cleanup_timeout: Duration::from_secs(1),
            stdout_limit: MCP_CAPTURE_BYTES,
            stderr_limit: MCP_CAPTURE_BYTES,
            base_environment: BaseEnvironment::Inherit,
            ..RunOptions::default()
        };
        let outcome = izu_process::run(&command, &[], &options, &worker, &|| false);
        // No helper can still replace progress after successful group cleanup.
        // Keep helper exceptions/startup output even when no Python report ran.
        match fs::read(directory.join("capture.json"))
            .map_err(|e| e.to_string())
            .and_then(|bytes| serde_json::from_slice(&bytes).map_err(|e| e.to_string()))
        {
            Ok(value) => report = value,
            Err(error) => report["progress_read_error"] = json!(error),
        }
        match outcome {
            Ok(outcome) => {
                fs::write(directory.join("helper.stdout"), &outcome.stdout.bytes)
                    .expect("helper stdout capture");
                fs::write(directory.join("helper.stderr"), &outcome.stderr.bytes)
                    .expect("helper stderr capture");
                report["owner"] = json!({"termination":format!("{:?}",outcome.termination),
                    "exit_code":outcome.exit.and_then(|s|s.code()),"cleanup":format!("{:?}",outcome.cleanup),
                    "cleanup_confirmed":outcome.cleanup.cooperative_stop_succeeded(),
                    "elapsed_seconds":outcome.elapsed.as_secs_f64(),
                    "stdout_dropped_bytes":outcome.stdout.dropped,"stderr_dropped_bytes":outcome.stderr.dropped});
                if outcome.termination != izu_process::Termination::Completed
                    || report["status"] != "completed"
                {
                    if report["status"] == "running" {
                        report["status"] = json!("interrupted");
                    }
                    for row in report["commands"]
                        .as_array_mut()
                        .expect("command progress list")
                    {
                        if matches!(row["state"].as_str(), Some("starting" | "running")) {
                            row["state"] = json!("interrupted");
                        } else if row["state"] == "pending" {
                            row["state"] = json!("not_started");
                        }
                    }
                }
            }
            Err(error) => report["owner"] = json!({"launch_error":error.to_string()}),
        }
        // The helper owns capture.json while its group might still be alive.
        // Keep the owner's result separately so an uncertain cleanup cannot
        // race away the only record of that uncertainty.
        fs::write(
            directory.join("capture-owner-result.json"),
            serde_json::to_vec(&report["owner"]).expect("capture owner JSON"),
        )
        .expect("capture owner result");
        if report["owner"]["cleanup_confirmed"] == true
            || report["owner"]["launch_error"].is_string()
        {
            save(&report);
        }
        json!({"directory":directory,"owner":report["owner"]})
    }

    fn event(&mut self, name: &str, data: Value) {
        if self.events.len() == 32 {
            self.events.remove(0);
        }
        self.events.push(json!({"event":name,"elapsed_seconds":self.started.elapsed().as_secs_f64(),"data":data}));
    }

    fn diagnostic(&mut self, reason: &str) -> Value {
        let status = self
            .child
            .try_wait()
            .map(|status| status.map(|status| status.to_string()))
            .unwrap_or_else(|error| Some(error.to_string()));
        let start = self.frame.len().saturating_sub(MCP_CAPTURE_BYTES);
        json!({"reason":reason,"phase":self.phase,"elapsed_seconds":self.started.elapsed().as_secs_f64(),"child_id":self.child.id(),"child_status":status,"before_cleanup":true,"sent_requests":self.sent.diagnostic(),"stdout":self.stdout.diagnostic(),"partial_frame_bytes":self.frame.len(),"partial_frame_tail_bytes":&self.frame[start..],"stderr":self.stderr.diagnostic(),"events":self.events})
    }

    fn drain_once(&mut self, stdout: bool) -> Result<bool, String> {
        use rustix::io::{Errno, read};
        let mut progress = false;
        let mut bytes = [0_u8; 16 * 1024];
        if stdout && let Some(fd) = &self.output {
            match read(fd, &mut bytes) {
                Ok(0) => {
                    self.output.take();
                }
                Ok(length) => {
                    self.stdout.append(&bytes[..length]);
                    for byte in &bytes[..length] {
                        if *byte == b'\n' {
                            let frame = std::mem::take(&mut self.frame);
                            let reply = serde_json::from_slice(&frame)
                                .map_err(|error| format!("invalid response JSON: {error}"))?;
                            if self.replies.len() >= 16 {
                                return Err("bounded test response queue exhausted".into());
                            }
                            self.replies.push_back(reply);
                        } else {
                            if self.frame.len() == izu_cli::mcp::MAX_RESPONSE_BYTES {
                                return Err("response frame exceeds limit".into());
                            }
                            self.frame.push(*byte);
                        }
                    }
                    progress = true;
                }
                Err(Errno::AGAIN | Errno::INTR) => {}
                Err(error) => return Err(format!("stdout read: {error}")),
            }
        }
        if self.drain_stderr
            && let Some(fd) = &self.errors
        {
            match read(fd, &mut bytes) {
                Ok(0) => {
                    self.errors.take();
                }
                Ok(length) => {
                    self.stderr.append(&bytes[..length]);
                    progress = true;
                }
                Err(Errno::AGAIN | Errno::INTR) => {}
                Err(error) => return Err(format!("stderr read: {error}")),
            }
        }
        Ok(progress)
    }

    fn pump(
        &mut self,
        deadline: Instant,
        drain_stdout: bool,
        write_input: bool,
    ) -> Result<(), String> {
        use rustix::event::{PollFd, PollFlags, Timespec, poll};
        if self.drain_once(drain_stdout)? {
            return Ok(());
        }
        let mut fds = Vec::new();
        if drain_stdout && let Some(fd) = &self.output {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }
        if self.drain_stderr
            && let Some(fd) = &self.errors
        {
            fds.push(PollFd::new(fd, PollFlags::IN));
        }
        if write_input && let Some(fd) = &self.input {
            fds.push(PollFd::new(fd, PollFlags::OUT));
        }
        // Short status-poll intervals also bound cleanup when all stream ends
        // are closed, or intentionally retained without reading in a fixture.
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(Duration::from_millis(50));
        let timeout = Timespec {
            tv_sec: remaining.as_secs() as _,
            tv_nsec: remaining.subsec_nanos() as _,
        };
        match poll(&mut fds, Some(&timeout)) {
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(format!("test transport poll: {error}")),
        }
        self.drain_once(drain_stdout)?;
        Ok(())
    }

    fn wait_exit(&mut self, bound: Duration, drain_stdout: bool) -> Option<ExitStatus> {
        let deadline = Instant::now() + bound;
        while Instant::now() < deadline {
            if let Some(status) = self.child.try_wait().expect("child status") {
                // Preserve bounded trailing stderr before the owner returns.
                for _ in 0..8 {
                    if !self.drain_once(drain_stdout).unwrap_or(false) {
                        break;
                    }
                }
                return Some(status);
            }
            if self.pump(deadline, drain_stdout, false).is_err() {
                break;
            }
        }
        self.child.try_wait().expect("final child status")
    }

    fn partial_response(&mut self) {
        let deadline = Instant::now() + MCP_RESPONSE_DEADLINE;
        while self.frame.is_empty() && self.replies.is_empty() && Instant::now() < deadline {
            self.pump(deadline, true, false)
                .expect("partial response transport");
        }
        assert!(
            !self.frame.is_empty() && self.replies.is_empty(),
            "expected a partially delivered large frame: {}",
            self.diagnostic("partial response setup")
        );
    }
    fn legacy_initialize(&mut self) {
        self.send(&json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"acceptance","version":"1"}}}));
        assert_eq!(
            self.receive()
                .pointer("/result/protocolVersion")
                .and_then(Value::as_str),
            Some("2025-11-25")
        );
        self.send(&json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
    }
    fn close_input(&mut self) {
        self.input.take();
    }
}
#[cfg(unix)]
impl Drop for McpClient {
    fn drop(&mut self) {
        self.close_input();
        if self.wait_exit(Duration::from_secs(3), true).is_none() {
            let _ = self.child.kill();
            // No unbounded wait or detached reader as a cleanup substitute.
            if self.wait_exit(Duration::from_secs(2), true).is_none() {
                eprintln!(
                    "MCP fixture child {} remains unreaped after bounded kill cleanup",
                    self.child.id()
                );
            }
        }
    }
}

#[cfg(unix)]
fn modern_meta() -> Value {
    json!({"io.modelcontextprotocol/protocolVersion":"2026-07-28","io.modelcontextprotocol/clientCapabilities":{},"io.modelcontextprotocol/clientInfo":{"name":"acceptance","version":"1"}})
}

#[cfg(unix)]
fn modern_rpc(id: u64, method: &str) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":{"_meta":modern_meta()}})
}

#[cfg(unix)]
fn diagnostic_bytes(value: &Value) -> Vec<u8> {
    value
        .as_array()
        .expect("retained raw bytes")
        .iter()
        .map(|byte| u8::try_from(byte.as_u64().expect("byte")).expect("byte range"))
        .collect()
}

#[cfg(unix)]
#[test]
fn mcp_timeout_diagnostics_retain_partial_stdout_stderr_request_and_phase() {
    let fixture = Fixture::new();
    // Deliberate negative transport fixture, not the product server. It emits
    // no complete frame, remains alive for the unchanged 15 s deadline, then
    // exits when the owner closes input. No sleeps or detached fixture child.
    let script = "import os,sys\nsys.stdin.buffer.readline(1048576)\nos.write(1,b'{\"jsonrpc\":\"2.0\",\"id\":701,\"res')\nos.write(2,b'intentional-transport-stderr\\n')\nsys.stdin.buffer.read()\n";
    let child = Command::new("/usr/bin/python3")
        .args(["-c", script])
        .current_dir(&fixture.root)
        .env("HOME", &fixture.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("intentional partial transport");
    let mut client = McpClient::from_child(child);
    let request = modern_rpc(701, "server/discover");
    client.send(&request);
    let wait = Instant::now();
    let failure = client
        .receive_result()
        .expect_err("intentional response timeout");
    assert!(wait.elapsed() >= MCP_RESPONSE_DEADLINE);
    assert_eq!(failure["phase"], "awaiting_response");
    assert_eq!(failure["reason"], "response timeout");
    assert!(failure["child_status"].is_null());
    assert_eq!(failure["before_cleanup"], true);
    let capture = Path::new(
        failure["native_capture"]["directory"]
            .as_str()
            .expect("deadline capture path"),
    );
    let report: Value = serde_json::from_slice(
        &fs::read(capture.join("capture.json")).expect("native capture report"),
    )
    .expect("capture JSON");
    assert_eq!(report["child"], client.child.id());
    assert_eq!(report["child_owned_at_start"], true);
    assert!(
        report["commands"]
            .as_array()
            .expect("native commands")
            .iter()
            .any(|row| row["kind"] == "pipe_ownership")
    );
    assert!(
        report["commands"]
            .as_array()
            .expect("native commands")
            .iter()
            .any(|row| row["kind"] == "child_stack")
    );
    assert!(
        client
            .child
            .try_wait()
            .expect("still-owned child")
            .is_none()
    );
    assert_eq!(report["owner"]["cleanup_confirmed"], true);
    // A pending plan is not a capture attempt. Preserve native failures and
    // permission errors, but require real launches while the transport lives.
    for kind in ["pipe_ownership", "child_stack"] {
        let row = report["commands"]
            .as_array()
            .expect("native commands")
            .iter()
            .find(|row| row["kind"] == kind)
            .expect("native capture attempt");
        assert_eq!(row["attempted"], true, "{row}");
        assert!(
            (row["pid"].as_u64().is_some()
                && matches!(row["state"].as_str(), Some("completed" | "timed_out")))
                || (row["state"] == "failed" && row["error"].as_str().is_some()),
            "{row}"
        );
        if cfg!(target_os = "macos") && kind == "child_stack" && row["exit_code"] == 0 {
            let stack =
                fs::read_to_string(capture.join("child_stack.stdout")).expect("native stack bytes");
            assert!(
                stack.contains("read  (in libsystem_kernel.dylib)"),
                "{stack}"
            );
        }
    }

    assert_eq!(
        diagnostic_bytes(&failure["partial_frame_tail_bytes"]),
        b"{\"jsonrpc\":\"2.0\",\"id\":701,\"res"
    );
    assert_eq!(
        diagnostic_bytes(&failure["stderr"]["retained_tail_bytes"]),
        b"intentional-transport-stderr\n"
    );
    let written = diagnostic_bytes(&failure["sent_requests"]["retained_tail_bytes"]);
    assert_eq!(
        serde_json::from_slice::<Value>(&written).expect("exact sent request"),
        request
    );
    assert_eq!(failure["sent_requests"]["truncated"], false);
    client.close_input();
    assert!(
        client
            .wait_exit(Duration::from_secs(3), true)
            .expect("bounded fixture exit")
            .success()
    );
}

#[cfg(unix)]
#[test]
fn mcp_diagnostic_slow_startup_retains_report_and_helper_stderr() {
    let fixture = Fixture::new();
    let child = Command::new("/usr/bin/python3")
        .args(["-B", "-c", "import sys; sys.stdin.buffer.read()"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("owned live transport");
    let mut client = McpClient::from_child(child);
    let wrapper = fixture.root.join("slow-diagnostic.py");
    fs::write(
        &wrapper,
        "import os,time\nos.write(2,b'controlled slow diagnostic startup\\n')\ntime.sleep(30)\n",
    )
    .expect("slow helper fixture");
    let failure = client.capture_deadline_with_script(&wrapper);
    let directory = Path::new(failure["directory"].as_str().expect("capture directory"));
    let report: Value = serde_json::from_slice(
        &fs::read(directory.join("capture.json")).expect("startup timeout still retains report"),
    )
    .expect("capture JSON");
    assert_eq!(report["child"], client.child.id());
    assert_eq!(report["status"], "helper_not_started");
    assert_eq!(report["owner"]["termination"], "TimedOut");
    assert_eq!(report["owner"]["cleanup_confirmed"], true);
    assert!(
        fs::read_to_string(directory.join("helper.stderr"))
            .expect("retained helper stderr")
            .contains("controlled slow diagnostic startup")
    );
    assert!(
        client
            .child
            .try_wait()
            .expect("transport alive before cleanup")
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn mcp_diagnostic_running_capture_is_marked_interrupted_after_watchdog() {
    let fixture = Fixture::new();
    let child = Command::new("/bin/cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("owned live transport");
    let mut client = McpClient::from_child(child);
    let wrapper = fixture.root.join("running-diagnostic.py");
    fs::write(
        &wrapper,
        r#"import json,subprocess,sys,time
from pathlib import Path
output=Path(sys.argv[sys.argv.index('--output')+1])
child=subprocess.Popen(['/bin/sleep','30'])
report=json.loads((output/'capture.json').read_text())
report.update(status='running',commands=[dict(kind='controlled',state='running',pid=child.pid)])
(output/'capture.json').write_text(json.dumps(report))
print('controlled running diagnostic',file=sys.stderr,flush=True)
time.sleep(30)
"#,
    )
    .unwrap();
    let failure = client.capture_deadline_with_script(&wrapper);
    let directory = Path::new(failure["directory"].as_str().unwrap());
    let report: Value =
        serde_json::from_slice(&fs::read(directory.join("capture.json")).unwrap()).unwrap();
    assert_eq!(report["owner"]["termination"], "TimedOut", "{report}");
    assert_eq!(report["owner"]["cleanup_confirmed"], true, "{report}");
    assert_eq!(report["status"], "interrupted", "{report}");
    assert_eq!(report["commands"][0]["state"], "interrupted", "{report}");
    let pid = report["commands"][0]["pid"].as_u64().unwrap();
    let observed = Command::new("/bin/ps")
        .args(["-p", &pid.to_string(), "-o", "pid="])
        .output()
        .unwrap();
    assert!(
        observed.stdout.is_empty(),
        "diagnostic child survived: {observed:?}"
    );
    assert!(client.child.try_wait().unwrap().is_none());
}

#[cfg(unix)]
#[test]
fn mcp_diagnostic_command_timeout_and_failure_retain_progress_and_stop_descendants() {
    let fixture = Fixture::new();
    let child = Command::new("/usr/bin/python3")
        .args(["-B", "-c", "import sys; sys.stdin.buffer.read()"])
        .current_dir(&fixture.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("owned live transport");
    let mut client = McpClient::from_child(child);
    let script =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../scripts/mcp-deadline-diagnostics.py");
    for probe in ["slow-command", "failed-command", "backpressure-command"] {
        let failure = client.capture_deadline_command(&script, Some(probe));
        let directory = Path::new(failure["directory"].as_str().expect("capture directory"));
        let report: Value = serde_json::from_slice(
            &fs::read(directory.join("capture.json")).expect("native command report"),
        )
        .expect("capture JSON");
        // The seven-second owner budget also covers Python startup and real
        // native sibling probes. A slow sibling may be interrupted, but the
        // controlled command's terminal result/output and descendant absence
        // below remain mandatory; partial controlled evidence never passes.
        match report["status"].as_str() {
            Some("completed") => {}
            Some("interrupted") => {
                assert_eq!(report["owner"]["termination"], "TimedOut", "{report}")
            }
            _ => panic!("capture has no terminal state: {report}"),
        }
        assert_eq!(report["owner"]["cleanup_confirmed"], true, "{report}");
        let rows = report["commands"].as_array().expect("command progress");
        for kind in ["pipe_ownership", "child_stack", "parent_stack"] {
            assert!(
                rows.iter()
                    .any(|row| row["kind"] == kind && row["pid"].as_u64().is_some()),
                "{report}"
            );
        }
        let row = rows
            .iter()
            .find(|row| row["kind"] == "parent_stack")
            .expect("controlled native command");
        if probe != "failed-command" {
            assert_eq!(row["state"], "timed_out", "{row}");
            let pid = fs::read_to_string(directory.join("native-descendant.pid"))
                .expect("actual native descendant launched");
            let status = Command::new("/bin/ps")
                .args(["-p", pid.trim(), "-o", "pid="])
                .output()
                .expect("descendant absence observation");
            assert!(
                status.stdout.is_empty(),
                "native descendant survived: {status:?}"
            );
            if probe == "backpressure-command" {
                for stream in ["stdout", "stderr"] {
                    assert_eq!(
                        fs::metadata(directory.join(format!("parent_stack.{stream}")))
                            .expect("bounded native output")
                            .len(),
                        1024 * 1024
                    );
                    assert!(
                        row[format!("{stream}_dropped_bytes")]
                            .as_u64()
                            .expect("native output overflow count")
                            >= 1024 * 1024,
                        "{row}"
                    );
                }
            } else {
                assert!(
                    fs::read_to_string(directory.join("parent_stack.stdout"))
                        .expect("retained command stdout")
                        .contains("controlled slow native command")
                );
            }
        } else {
            assert_eq!(row["state"], "completed");
            assert_eq!(row["exit_code"], 17);
            assert!(
                fs::read_to_string(directory.join("parent_stack.stderr"))
                    .expect("retained command stderr")
                    .contains("controlled native command failure")
            );
        }
        assert!(
            client
                .child
                .try_wait()
                .expect("transport alive before cleanup")
                .is_none()
        );
    }
}

#[cfg(unix)]
#[test]
fn mcp_healthy_exchange_retains_bounded_raw_transport_observations() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    let request = modern_rpc(702, "server/discover");
    let following = serde_json::to_string(&modern_rpc(703, "tools/list")).unwrap();
    let split = following.len() / 2;
    let input = format!("{request}\n{}", &following[..split]);
    // Discovery must arrive while stdin remains open with an incomplete next
    // frame. A reader waiting for that frame must not hold up the first reply.
    client.raw(&input);
    let discovered = client.receive();
    assert_eq!(discovered["id"], 702);
    assert_eq!(discovered["result"]["resultType"], "complete");
    let observed = client.diagnostic("healthy exchange");
    assert!(
        observed["stdout"]["total_bytes"]
            .as_u64()
            .expect("stdout count")
            > 0
    );
    assert_eq!(observed["stderr"]["total_bytes"], 0);
    assert_eq!(
        diagnostic_bytes(&observed["sent_requests"]["retained_tail_bytes"]),
        input.as_bytes()
    );
    client.raw(&format!("{}\n", &following[split..]));
    let listed = client.receive();
    assert_eq!(listed["id"], 703);
    assert!(
        listed["result"]["tools"]
            .as_array()
            .is_some_and(|tools| !tools.is_empty())
    );
    client.close_input();
    assert!(
        client
            .wait_exit(Duration::from_secs(3), true)
            .expect("healthy EOF exit")
            .success()
    );
}

#[cfg(unix)]
#[test]
fn mcp_closed_stdout_terminates_while_stdin_stays_open() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&modern_rpc(703, "server/discover"));
    assert_eq!(client.receive()["id"], 703);
    client.output.take();
    // Actual R4 condition: a further constant request while stdin is retained.
    // The server may observe the pipe error before reading this request.
    let request = format!("{}\n", modern_rpc(704, "server/discover"));
    use rustix::io::write;
    let _ = write(
        client.input.as_ref().expect("open stdin"),
        request.as_bytes(),
    );
    let status = client
        .wait_exit(Duration::from_secs(5), true)
        .unwrap_or_else(|| {
            panic!(
                "closed stdout did not terminate: {}",
                client.diagnostic("R4 condition")
            )
        });
    assert_eq!(status.code(), Some(1));
    assert!(
        client.input.is_some(),
        "input stayed open until observed exit"
    );
    assert!(
        String::from_utf8_lossy(&client.stderr.tail.iter().copied().collect::<Vec<_>>())
            .contains("Transport failure is not rollback")
    );
}

#[cfg(unix)]
#[test]
fn mcp_closed_idle_stdout_is_observed_without_another_request() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&modern_rpc(705, "server/discover"));
    assert_eq!(client.receive()["id"], 705);
    client.output.take();
    assert_eq!(
        client
            .wait_exit(Duration::from_secs(5), true)
            .expect("idle output error shutdown")
            .code(),
        Some(1)
    );
    assert!(client.input.is_some());
}

#[cfg(unix)]
#[test]
fn mcp_eof_exits_with_retained_unread_stdout_and_bounded_queues() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&modern_rpc(706, "server/discover"));
    assert_eq!(client.receive()["id"], 706);
    client.send(&modern_rpc(710, "tools/list"));
    client.partial_response();
    let requests: String = (711..718)
        .map(|id| format!("{}\n", modern_rpc(id, "tools/list")))
        .collect();
    client.raw_with_drain(&requests, false);
    let observed = client.stdout.total;
    client.close_input();
    let deadline = Instant::now();
    let status = client
        .wait_exit(Duration::from_secs(5), false)
        .unwrap_or_else(|| {
            let native = client.capture_deadline();
            panic!(
                "retained output blocked EOF: {}; native capture: {native}",
                client.diagnostic("R5 condition")
            )
        });
    assert_eq!(status.code(), Some(1));
    assert!(deadline.elapsed() < Duration::from_secs(5));
    assert_eq!(
        client.stdout.total, observed,
        "stdout consumer remained paused"
    );
    assert!(client.output.is_some(), "stdout read end was retained");
    assert!(
        String::from_utf8_lossy(&client.stderr.tail.iter().copied().collect::<Vec<_>>())
            .contains("EOF output drain exceeded two seconds")
    );
}

#[cfg(unix)]
#[test]
fn mcp_partial_frame_cancel_finishes_json_and_rejects_inflight_id_reuse() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&modern_rpc(720, "tools/list"));
    client.partial_response();
    let cancel =
        json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":720}});
    client.raw_with_drain(
        &format!(
            "{}\n{}\n{}\n",
            cancel,
            modern_rpc(720, "ping"),
            modern_rpc(721, "ping")
        ),
        false,
    );
    let complete = client.receive();
    assert_eq!(complete["id"], 720);
    assert!(
        complete.pointer("/result/tools").is_some(),
        "a partial JSON frame must finish"
    );
    let mut following = [client.receive(), client.receive()];
    following.sort_by_key(|response| response["id"].as_u64());
    let duplicate = &following[0];
    assert_eq!(duplicate["id"], 720);
    assert_eq!(duplicate["error"]["code"], -32600);
    assert_eq!(following[1]["id"], 721);
    assert!(following[1].get("result").is_some());
    assert!(following[1].get("error").is_none());
    client.send(&modern_rpc(720, "ping"));
    let reused = client.receive();
    assert_eq!(reused["id"], 720, "ID is reusable after full delivery");
    assert!(reused.get("result").is_some());
    assert!(reused.get("error").is_none());
}

#[cfg(unix)]
#[test]
fn mcp_saturated_error_queue_closes_without_blocking_input_or_worker_join() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&modern_rpc(730, "tools/list"));
    client.partial_response();
    client.raw_with_drain(&"{broken}\n".repeat(32), false);
    assert_eq!(
        client
            .wait_exit(Duration::from_secs(5), false)
            .expect("bounded saturated error shutdown")
            .code(),
        Some(1)
    );
    assert!(client.input.is_some());
    assert!(
        String::from_utf8_lossy(&client.stderr.tail.iter().copied().collect::<Vec<_>>())
            .contains("capacity is exhausted")
    );
}

#[cfg(unix)]
#[test]
fn mcp_saturated_request_and_response_queues_close_without_join_deadlock() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&modern_rpc(740, "tools/list"));
    client.partial_response();
    let requests: String = (741..773)
        .map(|id| format!("{}\n", modern_rpc(id, "ping")))
        .collect();
    client.raw_with_drain(&requests, false);
    assert_eq!(
        client
            .wait_exit(Duration::from_secs(5), false)
            .expect("bounded saturated request shutdown")
            .code(),
        Some(1)
    );
    assert!(client.input.is_some());
}

#[cfg(unix)]
#[test]
fn mcp_full_stderr_cannot_block_terminal_transport_exit() {
    let fixture = Fixture::new();
    // One fixture process fills its inherited stderr nonblockingly, restores
    // blocking mode, then execs the exact CLI. No relay or extra live process.
    let script = "import fcntl,os,sys\nf=fcntl.fcntl(2,fcntl.F_GETFL)\nfcntl.fcntl(2,fcntl.F_SETFL,f|os.O_NONBLOCK)\nfor _ in range(4096):\n try: os.write(2,b'x'*4096)\n except BlockingIOError: break\nelse: raise RuntimeError('stderr did not saturate within bounded bytes')\nfcntl.fcntl(2,fcntl.F_SETFL,f)\nos.execv(sys.argv[1],[sys.argv[1],'agent','serve'])\n";
    let child = Command::new("/usr/bin/python3")
        .args(["-c", script, BINARY])
        .current_dir(&fixture.root)
        .env("HOME", &fixture.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("full stderr fixture");
    let mut client = McpClient::from_child(child);
    client.drain_stderr = false;
    client.send(&modern_rpc(780, "server/discover"));
    assert_eq!(client.receive()["id"], 780);
    client.output.take();
    assert_eq!(
        client
            .wait_exit(Duration::from_secs(5), false)
            .expect("full stderr shutdown")
            .code(),
        Some(1)
    );
    assert_eq!(
        client.stderr.total, 0,
        "consumer did not make stderr room before exit"
    );
    client.drain_stderr = true;
    assert!(client.drain_once(false).expect("retained stderr bytes"));
}

#[cfg(unix)]
#[test]
fn mcp_regular_file_stdin_stdout_redirects_remain_usable() {
    let fixture = Fixture::new();
    let input = fixture.root.join("mcp-input.ndjson");
    let output = fixture.root.join("mcp-output.ndjson");
    fs::write(&input, format!("{}\n", modern_rpc(790, "server/discover")))
        .expect("regular request file");
    let child = fixture
        .command()
        .args(["agent", "serve"])
        .stdin(fs::File::open(&input).expect("input file"))
        .stdout(fs::File::create(&output).expect("output file"))
        .stderr(Stdio::piped())
        .spawn()
        .expect("regular redirect CLI");
    let mut client = McpClient::from_child(child);
    assert!(
        client
            .wait_exit(Duration::from_secs(5), false)
            .expect("regular redirect exit")
            .success()
    );
    let bytes = fs::read(output).expect("regular response");
    assert!(bytes.len() < 4096);
    assert_eq!(
        serde_json::from_slice::<Value>(&bytes).expect("NDJSON response")["id"],
        790
    );
}

#[cfg(unix)]
#[test]
fn mcp_returned_durable_receipt_survives_healthy_eof_or_has_exact_loss_provenance() {
    for closed_output in [false, true] {
        let fixture = Fixture::new();
        let initialized = fixture.ok(&["init", "."]);
        fs::write(fixture.root.join("durable.txt"), "durable MCP source\n").expect("source");
        let mut client = McpClient::new(&fixture);
        client.send(&modern_rpc(800, "tools/list"));
        client.partial_response();
        let commit = json!({"jsonrpc":"2.0","id":802,"method":"tools/call","params":{"_meta":modern_meta(),"name":"izu_commit","arguments":{"schema_version":1,"operation":{"kind":"commit","context":{"repository":fixture.root,"workspace":initialized["id"],"expected":initialized["expected"]},"selection":{"kind":"all"},"message":"durable pending MCP receipt","author":{"name":"MCP fixture","email":"fixture@izu.invalid"},"target":null}}}});
        // First output is partially written, second fills the one-frame worker
        // channel, and commit can return while its response send is pending.
        client.raw_with_drain(
            &format!("{}\n{}\n", modern_rpc(801, "tools/list"), commit),
            false,
        );
        let api = izu_api::Api::new(PathBuf::from(BINARY));
        let context = izu_api::ReadContext {
            repository: fixture.root.clone(),
            workspace: None,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let history = api.execute(
                izu_api::Request::new(izu_api::Operation::Log {
                    context: context.clone(),
                    from: None,
                    limit: 8,
                }),
                &izu_api::CancellationToken::new(),
            );
            let value = serde_json::to_value(history).expect("shared facade read");
            if value
                .pointer("/outcome/result/data")
                .and_then(Value::as_array)
                .is_some_and(|records| {
                    records.iter().any(|record| {
                        record
                            .pointer("/revision/description")
                            .and_then(Value::as_str)
                            == Some("durable pending MCP receipt")
                    })
                })
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "commit never became visible: {}",
                client.diagnostic("durable publication setup")
            );
            std::thread::sleep(Duration::from_millis(10));
        }
        let operation = if closed_output {
            client.output.take();
            assert_eq!(
                client
                    .wait_exit(Duration::from_secs(5), false)
                    .expect("output loss cleanup")
                    .code(),
                Some(1)
            );
            let diagnostic: Value =
                serde_json::from_slice(&client.stderr.tail.iter().copied().collect::<Vec<_>>())
                    .expect("bounded loss diagnostic");
            let returned = diagnostic
                .pointer("/undelivered/pending")
                .and_then(Value::as_array)
                .expect("pending receipts")
                .iter()
                .find(|receipt| receipt["request_id"] == 802)
                .expect("exact request");
            assert_eq!(returned["stage"], "returned");
            returned
                .pointer("/receipt/native_ids/~1result~1data~1operation")
                .or_else(|| returned.pointer("/receipt/native_ids/~1error~1operation_id"))
                .and_then(Value::as_str)
                .expect("exact undelivered operation")
                .to_owned()
        } else {
            client.close_input();
            assert_eq!(client.receive()["id"], 800);
            assert_eq!(client.receive()["id"], 801);
            let returned = client.receive();
            assert_eq!(returned["id"], 802);
            assert!(
                matches!(
                    returned
                        .pointer("/result/structuredContent/outcome/kind")
                        .and_then(Value::as_str),
                    Some("ok" | "uncertain")
                ) || returned
                    .pointer("/result/structuredContent/outcome/error/code")
                    .and_then(Value::as_str)
                    == Some("publication_uncertain")
            );
            let operation = returned
                .pointer("/result/structuredContent/outcome/result/data/operation")
                .or_else(|| {
                    returned.pointer("/result/structuredContent/outcome/error/operation_id")
                })
                .and_then(Value::as_str)
                .expect("durable/uncertain receipt operation")
                .to_owned();
            assert!(
                client
                    .wait_exit(Duration::from_secs(3), true)
                    .expect("healthy EOF cleanup")
                    .success()
            );
            operation
        };
        let operations = api.execute(
            izu_api::Request::new(izu_api::Operation::Operations { context, limit: 8 }),
            &izu_api::CancellationToken::new(),
        );
        let value = serde_json::to_value(operations).expect("authoritative operation read");
        assert!(
            value
                .pointer("/outcome/result/data")
                .and_then(Value::as_array)
                .expect("operations")
                .iter()
                .any(|record| record["id"] == operation),
            "diagnostic/receipt ID must identify an actual native operation"
        );
    }
}

#[cfg(target_os = "linux")]
#[test]
fn mcp_linux_packet_stdout_writer_is_rejected_before_request_execution() {
    let fixture = Fixture::new();
    let script = "import os,sys\nr,w=os.pipe2(os.O_DIRECT)\nos.dup2(w,1)\nos.execv(sys.argv[1],[sys.argv[1],'agent','serve'])\n";
    let child = Command::new("/usr/bin/python3")
        .args(["-c", script, BINARY])
        .current_dir(&fixture.root)
        .env("HOME", &fixture.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("packet pipe fixture");
    let mut client = McpClient::from_child(child);
    assert_eq!(
        client
            .wait_exit(Duration::from_secs(5), false)
            .expect("packet pipe refusal")
            .code(),
        Some(3)
    );
    assert_eq!(client.stdout.total, 0);
    assert!(
        client.input.is_some(),
        "input stayed open through packet writer refusal"
    );
    assert!(
        String::from_utf8_lossy(&client.stderr.tail.iter().copied().collect::<Vec<_>>())
            .contains("Packet-mode")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn mcp_linux_packet_stderr_writer_is_rejected_without_blocking_diagnostic_fallback() {
    let fixture = Fixture::new();
    let script = "import os,sys\nr,w=os.pipe2(os.O_DIRECT)\nos.dup2(w,2)\nos.execv(sys.argv[1],[sys.argv[1],'agent','serve'])\n";
    let child = Command::new("/usr/bin/python3")
        .args(["-c", script, BINARY])
        .current_dir(&fixture.root)
        .env("HOME", &fixture.home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("packet stderr fixture");
    let mut client = McpClient::from_child(child);
    client.drain_stderr = false;
    assert_eq!(
        client
            .wait_exit(Duration::from_secs(5), false)
            .expect("packet stderr refusal")
            .code(),
        Some(3)
    );
    assert_eq!(client.stdout.total, 0);
    assert_eq!(
        client.stderr.total, 0,
        "packet diagnostics must not fall back to blocking output"
    );
    assert!(
        client.input.is_some(),
        "input stayed open without a helping output drain"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn mcp_linux_packet_pipe_reader_flags_do_not_attest_writer_packet_mode() {
    let fixture = Fixture::new();
    let script = "import fcntl,json,os\nr,w=os.pipe2(os.O_DIRECT)\ntry:\n rf=fcntl.fcntl(r,fcntl.F_GETFL)\n wf=fcntl.fcntl(w,fcntl.F_GETFL)\n os.write(1,(json.dumps({'reader_direct':bool(rf & os.O_DIRECT),'writer_direct':bool(wf & os.O_DIRECT),'reader_flags':rf,'writer_flags':wf})+'\\n').encode())\nfinally:\n os.close(r)\n os.close(w)\n";
    let child = Command::new("/usr/bin/python3")
        .args(["-c", script])
        .current_dir(&fixture.root)
        .env("HOME", &fixture.home)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("packet reader flag observation");
    let mut client = McpClient::from_child(child);
    let observed = client.receive();
    assert_eq!(observed["reader_direct"], false);
    assert_eq!(observed["writer_direct"], true);
    assert!(
        client
            .wait_exit(Duration::from_secs(3), true)
            .expect("bounded native flag observation")
            .success()
    );
}

#[cfg(unix)]
#[test]
fn mcp_current_and_legacy_protocols_call_the_shared_engine() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    client.send(&json!({"jsonrpc":"2.0","id":2,"method":"server/discover","params":{"_meta":modern_meta()}}));
    let discovered = client.receive();
    assert_eq!(discovered["id"], 2);
    assert_eq!(discovered["result"]["resultType"], "complete");
    client.send(
        &json!({"jsonrpc":"2.0","id":3,"method":"tools/list","params":{"_meta":modern_meta()}}),
    );
    let listed = client.receive();
    let tools = listed["result"]["tools"].as_array().expect("typed tools");
    assert!(tools.iter().any(|tool| tool["name"] == "izu_commit"));
    assert!(
        tools
            .iter()
            .any(|tool| tool["name"] == "izu_environment_bind")
    );
    assert!(!tools.iter().any(|tool| tool["name"] == "izu_agent_run"));
    client.send(&json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"_meta":modern_meta(),"name":"izu_init","arguments":{"schema_version":1,"operation":{"kind":"init","path":fixture.root}}}}));
    let initialized = client.receive();
    assert_eq!(initialized["id"], 4);
    assert_eq!(initialized["result"]["isError"], false);
    assert_eq!(
        initialized
            .pointer("/result/structuredContent/outcome/kind")
            .and_then(Value::as_str),
        Some("ok")
    );
    let engine_init =
        initialized["result"]["structuredContent"]["outcome"]["result"]["data"].clone();
    client.send(&json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"_meta":modern_meta(),"name":"izu_status","arguments":{"schema_version":1,"operation":{"kind":"status","context":{"repository":fixture.root,"workspace":engine_init["id"]}}}}}));
    let status = client.receive();
    assert_eq!(
        status.pointer("/result/structuredContent/outcome/result/data/head"),
        Some(&engine_init["expected"]["head"])
    );
    drop(client);
    let mut legacy = McpClient::new(&fixture);
    legacy.legacy_initialize();
    legacy.send(&json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"izu_status","arguments":{"schema_version":1,"operation":{"kind":"status","context":{"repository":fixture.root,"workspace":engine_init["id"]}}}}}));
    let status = legacy.receive();
    assert_eq!(status["id"], 6);
    assert_eq!(status["result"]["isError"], false);
    assert!(status["result"].get("resultType").is_none());
}

#[cfg(unix)]
#[test]
fn mcp_unknown_protocol_and_invalid_frames_are_recoverable() {
    let fixture = Fixture::new();
    let mut client = McpClient::new(&fixture);
    let mut meta = modern_meta();
    meta["io.modelcontextprotocol/protocolVersion"] = json!("1900-01-01");
    client
        .send(&json!({"jsonrpc":"2.0","id":7,"method":"server/discover","params":{"_meta":meta}}));
    let unsupported = client.receive();
    assert_eq!(
        unsupported.pointer("/error/code").and_then(Value::as_i64),
        Some(-32022)
    );
    client.raw("{broken}\n");
    assert_eq!(client.receive()["error"]["code"], -32700);
    client.raw(&format!("{}\n", "x".repeat(izu_api::MAX_REQUEST_BYTES + 1)));
    assert_eq!(client.receive()["error"]["code"], -32600);
    client.send(&json!({"jsonrpc":"2.0","id":8,"method":"ping","params":{"_meta":modern_meta()}}));
    assert_eq!(client.receive()["id"], 8);
    client.send(&json!({"jsonrpc":"2.0","id":9,"method":"tools/call","params":{"_meta":modern_meta(),"name":"izu_status","arguments":{"schema_version":1,"operation":{"kind":"capabilities"}}}}));
    assert_eq!(client.receive()["error"]["code"], -32602);
}

#[test]
fn native_bundle_restores_the_recorded_snapshot_in_a_new_directory() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "captured\n").expect("source");
    fixture.ok(&["commit", "-m", "captured"]);
    let bundle = fixture
        .root
        .parent()
        .expect("parent")
        .join("backup.izu-bundle");
    fixture.ok(&["bundle", "create", bundle.to_str().expect("bundle path")]);
    fixture.ok(&["bundle", "verify", bundle.to_str().expect("path")]);
    fs::write(fixture.root.join("tracked.txt"), "uncaptured\n").expect("source after snapshot");
    let destination = fixture.root.parent().expect("parent").join("restored");
    let restored = fixture.ok(&[
        "bundle",
        "restore",
        bundle.to_str().expect("bundle"),
        destination.to_str().expect("destination"),
    ]);
    assert_eq!(restored["publication"]["status"], "durable");
    assert_eq!(
        fs::read_to_string(destination.join("tracked.txt")).expect("restored exact source"),
        "captured\n"
    );
    assert_eq!(
        fs::read_to_string(fixture.root.join("tracked.txt")).expect("original source"),
        "uncaptured\n"
    );
}

#[cfg(unix)]
#[test]
fn mcp_eof_cancels_queued_mutations_and_reports_durable_receipt() {
    let fixture = Fixture::new();
    let initialized = fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("tracked.txt"), "exact candidate\n").expect("source");
    let committed = fixture.ok(&["commit", "-m", "durable before shutdown"]);
    let source = text(&committed, "revision");
    let marker = fixture
        .root
        .parent()
        .expect("parent")
        .join("mcp-check-running");
    let script = format!(
        "import pathlib,time; pathlib.Path({}).write_text('running'); time.sleep(60)",
        serde_json::to_string(marker.to_str().expect("marker path")).expect("Python literal")
    );
    let prepared = fixture.ok(&[
        "candidate",
        &source,
        "--target",
        "mcp-target",
        "--expect-absent",
        "--check",
        "slow",
        "--",
        "/usr/bin/python3",
        "-c",
        &script,
    ]);
    let candidate = text(&prepared["Ready"], "candidate");
    let mut client = McpClient::new(&fixture);
    client.legacy_initialize();
    client.send(&json!({"jsonrpc":"2.0","id":20,"method":"tools/call","params":{"name":"izu_check","arguments":{"schema_version":1,"operation":{"kind":"check","context":{"repository":fixture.root,"workspace":initialized["id"]},"candidate":candidate,"check":"slow","timeout_seconds":30}}}}));
    wait_for_path(&marker, Duration::from_secs(10));
    fs::write(
        fixture.root.join("queued.txt"),
        "preserve this working edit\n",
    )
    .expect("unpublished source");
    client.send(&json!({"jsonrpc":"2.0","id":21,"method":"tools/call","params":{"name":"izu_commit","arguments":{"schema_version":1,"operation":{"kind":"commit","context":{"repository":fixture.root,"workspace":initialized["id"],"expected":{"head":committed["revision"],"working_tree":committed["working_tree"]}},"selection":{"kind":"all"},"message":"must not run after EOF","author":{"name":"acceptance","email":"acceptance@izu.invalid"}}}}}));
    client.close_input();
    let mut responses = vec![client.receive(), client.receive()];
    responses.sort_by_key(|response| response["id"].as_u64());
    assert_eq!(responses[0]["id"], 20);
    assert_eq!(responses[1]["id"], 21);
    for response in responses {
        assert_eq!(
            response
                .pointer("/result/structuredContent/outcome/error/code")
                .and_then(Value::as_str),
            Some("cancelled"),
            "receipt={response}"
        );
    }
    let history = fixture.ok(&["log"]);
    let descriptions: Vec<_> = history
        .as_array()
        .expect("history")
        .iter()
        .map(|record| record["revision"]["description"].as_str().unwrap_or(""))
        .collect();
    assert!(descriptions.contains(&"durable before shutdown"));
    assert!(!descriptions.contains(&"must not run after EOF"));
    assert_eq!(
        fs::read_to_string(fixture.root.join("queued.txt")).expect("source preserved"),
        "preserve this working edit\n"
    );
    fixture.ok(&["verify"]);
}

#[cfg(unix)]
#[test]
fn sigint_cancels_managed_run_and_retains_a_truthful_job_receipt() {
    let fixture = Fixture::new();
    let initialized = fixture.ok(&["init", "."]);
    let marker = fixture
        .root
        .parent()
        .expect("parent")
        .join("running-marker");
    let script = format!(
        "import pathlib,time\npathlib.Path({}).write_text('running')\ntime.sleep(60)\n",
        serde_json::to_string(marker.to_str().expect("marker")).expect("quoted marker")
    );
    let script_path = fixture.root.join("run.py");
    fs::write(&script_path, script).expect("run fixture");
    fixture.ok(&["checkpoint"]);
    let workspace = text(&initialized, "id");
    let state = fixture.ok(&["workspace", "open", &workspace]);
    let head = text(&state["expected"], "head");
    let tree = text(&state["expected"], "working_tree");
    let mut child = fixture
        .command()
        .args(["--json", "--workspace", &workspace, "agent", "run", "--cwd"])
        .arg(&fixture.root)
        .args([
            "--expected-head",
            &head,
            "--expected-tree",
            &tree,
            "--timeout-seconds",
            "30",
            "--",
            "/usr/bin/python3",
        ])
        .arg(&script_path)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("managed CLI");
    wait_for_path(&marker, Duration::from_secs(10));
    assert!(child.try_wait().expect("live process").is_none());
    let interrupted = Instant::now();
    assert!(
        Command::new("/bin/kill")
            .args(["-INT", &child.id().to_string()])
            .status()
            .expect("signal owned child")
            .success()
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while child.try_wait().expect("cancellation readback").is_none() {
        if Instant::now() > deadline {
            let _ = child.kill();
            panic!("SIGINT cancellation exceeded deadline");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = child.wait_with_output().expect("managed response");
    assert!(interrupted.elapsed() < Duration::from_secs(8));
    assert_eq!(
        output.status.code(),
        Some(130),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let response: Value = serde_json::from_slice(&output.stdout).expect("cancelled response JSON");
    assert_eq!(
        response
            .pointer("/outcome/error/code")
            .and_then(Value::as_str),
        Some("cancelled")
    );
    assert!(
        response
            .pointer("/outcome/result/data/state/Cancelled")
            .is_some(),
        "receipt={response}"
    );
    fixture.ok(&["status"]);
    fixture.ok(&["verify"]);
}

fn wait_for_path(path: &Path, limit: Duration) {
    let deadline = Instant::now() + limit;
    while !path.exists() {
        assert!(
            Instant::now() < deadline,
            "missing runtime marker {}",
            path.display()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn sparse_workspace_and_checkout_free_queries_work_through_cli_json_and_mcp() {
    let fixture = Fixture::new();
    fixture.ok(&["init", "."]);
    fs::write(fixture.root.join("included"), b"hello").unwrap();
    fs::write(fixture.root.join("omitted"), b"retained").unwrap();
    let commit = fixture.ok(&["commit", "-m", "baseline"]);
    let revision = text(&commit, "revision");
    let format = fixture.ok(&["format", "inspect"]);
    fixture.ok(&[
        "format",
        "migrate",
        "--to",
        "3",
        "--expect-operation",
        &text(&format, "operation"),
    ]);
    let sparse = fixture.temp.as_ref().unwrap().path().join("sparse");
    let workspace = fixture.ok(&[
        "workspace",
        "sparse-start",
        "scoped",
        sparse.to_str().unwrap(),
        "--from",
        &revision,
        "--path",
        "included",
    ]);
    assert!(sparse.join("included").exists());
    assert!(!sparse.join("omitted").exists());
    let file = fixture.ok(&[
        "file", &revision, "omitted", "--offset", "1", "--limit", "3",
    ]);
    assert_eq!(file["bytes"], json!([101, 116, 97]));
    assert_eq!(file["next_offset"], 4);
    let base = fixture.ok(&["merge-base", &revision, &revision]);
    assert_eq!(base["merge_base"], revision);
    let changes = fixture.ok(&["changes", &revision, &revision, "--path", "omitted"]);
    assert_eq!(changes["entries"], json!([]));
    let mut client = McpClient::new(&fixture);
    client.send(
        &json!({"jsonrpc":"2.0","id":901,"method":"tools/call","params":{
        "_meta":modern_meta(),"name":"izu_file_read","arguments":{"schema_version":1,"operation":{
            "kind":"file_read","context":{"repository":fixture.root,"workspace":null},
            "revision":revision,"path":"omitted","offset":0,"limit":64}}}}),
    );
    let reply = client.receive();
    assert_eq!(reply["result"]["isError"], false, "{reply}");
    assert_eq!(
        reply.pointer("/result/structuredContent/outcome/result/data/bytes"),
        Some(&json!(b"retained".to_vec()))
    );
    drop(client);
    let (output, expanded) = fixture.json(&json!({"schema_version":1,"operation":{
        "kind":"workspace_expand","context":{"repository":fixture.root,"workspace":workspace["id"],"expected":workspace["expected"]},
        "selection":{"kind":"all"}}}));
    assert!(output.status.success(), "{expanded}");
    assert_eq!(fs::read(sparse.join("omitted")).unwrap(), b"retained");
    fixture.ok(&["verify"]);
}
