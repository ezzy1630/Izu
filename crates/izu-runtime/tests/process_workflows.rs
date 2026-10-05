#![cfg(unix)]

use std::fs;
use std::net::{Ipv4Addr, TcpListener};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use izu_engine::{Repository, RepositoryOptions, Selection, WorkspaceState};
use izu_model::CancellationToken;
use izu_runtime::*;

struct Fixture {
    _temporary: tempfile::TempDir,
    base: PathBuf,
    repository: Repository,
    cancel: CancellationToken,
}

struct CancelOnDrop(CancellationToken);
impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.cancel();
    }
}
impl Fixture {
    fn new() -> Self {
        Self::with_source(&[])
    }
    fn with_source(files: &[(&str, &[u8])]) -> Self {
        let temporary = owned_tempdir();
        let base = temporary.path().canonicalize().unwrap();
        fs::create_dir(base.join("source")).unwrap();
        fs::write(base.join("source/human.txt"), "human root remains here\n").unwrap();
        for (path, bytes) in files {
            fs::write(base.join("source").join(path), bytes).unwrap();
        }
        let repository =
            Repository::init(base.join("source"), RepositoryOptions::default()).unwrap();
        Self {
            _temporary: temporary,
            base,
            repository,
            cancel: CancellationToken::new(),
        }
    }
    fn workspace(&self, name: &str) -> WorkspaceState {
        let root = self
            .repository
            .workspace(self.repository.workspace_id(), &self.cancel)
            .unwrap();
        self.repository
            .fork_workspace(
                name.into(),
                self.base.join(name),
                root.expected.head,
                &self.cancel,
            )
            .unwrap()
    }
    fn binding(&self, workspace: &WorkspaceState) -> WorkspaceBinding {
        WorkspaceBinding::checked(
            &self.repository,
            workspace.id,
            workspace.expected,
            &self.cancel,
        )
        .unwrap()
    }
    fn config(&self) -> RuntimeConfig {
        RuntimeConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_izu-runtime-worker")))
    }
    fn runtime(&self) -> Runtime {
        Runtime::open(
            self.repository.metadata_path().join("runtime"),
            self.config(),
        )
        .unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self._temporary.disable_cleanup(true);
            eprintln!(
                "IZU_RUNTIME_FAILURE_FIXTURE {}",
                self._temporary.path().display()
            );
            return;
        }
        // Prepared-cache files are immutable in normal use. These disposable
        // fixtures own the entire tree and make it writable only for teardown.
        fn writable(path: &std::path::Path) {
            use std::os::unix::fs::PermissionsExt;
            let Ok(metadata) = fs::symlink_metadata(path) else {
                return;
            };
            if metadata.is_symlink() {
                return;
            }
            let _ = fs::set_permissions(
                path,
                fs::Permissions::from_mode(if metadata.is_dir() { 0o700 } else { 0o600 }),
            );
            if metadata.is_dir()
                && let Ok(entries) = fs::read_dir(path)
            {
                for entry in entries.flatten() {
                    writable(&entry.path());
                }
            }
        }
        writable(&self.repository.metadata_path().join("environments"));
    }
}
fn owned_tempdir() -> tempfile::TempDir {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.artifacts/tmp");
    fs::create_dir_all(&path).unwrap();
    tempfile::tempdir_in(path).unwrap()
}
fn shell(binding: WorkspaceBinding, script: &str) -> RunRequest {
    RunRequest::new(binding, vec!["/bin/sh".into(), "-c".into(), script.into()])
}
fn passed(job: &JobSnapshot) -> bool {
    matches!(&job.state, JobState::Finished { outcome, .. } if outcome.passed())
}

#[test]
fn real_writer_uses_private_cwd_and_overlay_then_close_preserves_source() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("agent");
    let binding = fixture.binding(&workspace);
    let mut runtime = fixture.runtime();
    let mut request = shell(
        binding.clone(),
        "pwd; printf '%s' \"$PORT\"; printf 'original agent work' > result.txt",
    );
    request.env_overlay.insert("PORT".into(), "46123".into());
    let id = runtime.submit(request).unwrap();
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(passed(&job), "{:?}", job.state);
    assert!(
        job.output
            .stdout
            .text_lossy()
            .contains(binding.cwd().to_str().unwrap())
    );
    assert!(job.output.stdout.text_lossy().ends_with("46123"));
    let receipt = runtime
        .close_workspace(
            &fixture.repository,
            workspace.id,
            workspace.expected,
            &fixture.cancel,
        )
        .unwrap();
    assert_eq!(receipt.retained_path, binding.cwd());
    assert!(!receipt.source_files_deleted);
    assert_eq!(
        fs::read_to_string(binding.cwd().join("result.txt")).unwrap(),
        "original agent work"
    );
    assert_eq!(
        fs::read_to_string(fixture.base.join("source/human.txt")).unwrap(),
        "human root remains here\n"
    );
    assert!(
        !fixture
            .repository
            .operation(receipt.close_operation, &fixture.cancel)
            .unwrap()
            .view
            .workspaces
            .contains_key(&workspace.id)
    );
    assert!(
        fixture
            .repository
            .tree(receipt.checkpoint.tree, &fixture.cancel)
            .unwrap()
            .entries
            .keys()
            .any(|path| path.as_str() == "result.txt")
    );
}

#[test]
fn real_concurrency_budget_queues_other_workspaces_and_cancelled_queue_never_writes() {
    let fixture = Fixture::new();
    let first = fixture.workspace("first");
    let second = fixture.workspace("second");
    let mut config = fixture.config();
    config.budget.max_running_jobs = 1;
    let mut runtime =
        Runtime::open(fixture.repository.metadata_path().join("runtime"), config).unwrap();
    let first_id = runtime
        .submit(shell(
            fixture.binding(&first),
            "printf start; sleep 0.2; printf first > done.txt",
        ))
        .unwrap();
    let second_id = runtime
        .submit(shell(fixture.binding(&second), "printf second > done.txt"))
        .unwrap();
    runtime.poll().unwrap();
    assert_eq!(runtime.usage().unwrap().running_jobs, 1);
    assert!(matches!(
        runtime.status(&second_id).unwrap().state,
        JobState::Queued {
            blocked: Some(AdmissionBlock::RunningJobs)
        }
    ));
    runtime.cancel(&second_id).unwrap();
    assert!(passed(&runtime.wait(&first_id, &fixture.cancel).unwrap()));
    assert!(!PathBuf::from(&second.record.root).join("done.txt").exists());
    assert_eq!(runtime.usage().unwrap().running_jobs, 0);
}

#[test]
fn timeout_stops_a_real_descendant_writer_before_return() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("deadline");
    let mut runtime = fixture.runtime();
    let script = "import subprocess,sys,time\nchild = subprocess.Popen([sys.executable,'-c',\"import time; f=open('heartbeat','a');\\nwhile True: f.write('x'); f.flush(); time.sleep(.01)\"])\nopen('child-pid','w').write(str(child.pid))\ntime.sleep(30)";
    let request = RunRequest::new(
        fixture.binding(&workspace),
        vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
    )
    .with_timeout(Duration::from_millis(500))
    .unwrap();
    let id = runtime.submit(request).unwrap();
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(
        matches!(
            job.state,
            JobState::Cancelled {
                reason: CancellationReason::Deadline,
                ..
            }
        ),
        "{:?}",
        job.state
    );
    let heartbeat = PathBuf::from(&workspace.record.root).join("heartbeat");
    let stopped_size = fs::metadata(&heartbeat).unwrap().len();
    assert!(stopped_size > 0);
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::metadata(&heartbeat).unwrap().len(), stopped_size);
}

#[test]
fn explicit_cancel_stops_real_descendant_writes_and_clears_exact_intent() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("cancel");
    let root = PathBuf::from(&workspace.record.root);
    let mut runtime = fixture.runtime();
    let script = "import subprocess,sys,time\nsubprocess.Popen([sys.executable,'-c',\"import time; f=open('heartbeat','a');\\nwhile True: f.write('x'); f.flush(); time.sleep(.01)\"])\ntime.sleep(30)";
    let id = runtime
        .submit(RunRequest::new(
            fixture.binding(&workspace),
            vec!["/usr/bin/python3".into(), "-c".into(), script.into()],
        ))
        .unwrap();
    runtime.poll().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !root.join("heartbeat").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .is_some()
    );
    let job = runtime.cancel(&id).unwrap();
    assert!(
        matches!(
            job.state,
            JobState::Cancelled {
                reason: CancellationReason::Requested,
                ..
            }
        ),
        "{:?}",
        job.state
    );
    let size = fs::metadata(root.join("heartbeat")).unwrap().len();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(fs::metadata(root.join("heartbeat")).unwrap().len(), size);
    assert!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .is_none()
    );
}

#[test]
fn native_writer_busy_creates_no_secret_ticket_and_can_retry_after_release() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("busy");
    let mut runtime = fixture.runtime();
    let mut request = shell(fixture.binding(&workspace), "printf complete");
    request
        .env_overlay
        .insert("TOKEN".into(), "secret-must-not-be-staged-when-busy".into());
    let id = runtime.submit(request).unwrap();
    let other = fixture
        .repository
        .lease_workspace(workspace.id, workspace.expected, &fixture.cancel)
        .unwrap();
    assert!(runtime.poll().is_err());
    assert!(matches!(
        runtime.status(&id).unwrap().state,
        JobState::Queued { .. }
    ));
    assert!(
        !runtime
            .registry_path()
            .join("jobs")
            .join(id.as_str())
            .join("ticket.json")
            .exists()
    );
    assert!(
        !fs::read_to_string(runtime.registry_path().join("state.json"))
            .unwrap()
            .contains("secret-must-not-be-staged-when-busy")
    );
    other.finish_stopped().unwrap(); // this fixture lease never launched a writer
    assert!(passed(&runtime.wait(&id, &fixture.cancel).unwrap()));
}

#[test]
fn queued_with_durable_intent_recovers_unknown_and_requires_matching_token() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("queued-intent");
    let mut runtime = fixture.runtime();
    let id = runtime
        .submit(shell(fixture.binding(&workspace), "exit 0"))
        .unwrap();
    let mut snapshot = runtime.status(&id).unwrap();
    drop(runtime);
    let lease = fixture
        .repository
        .lease_workspace(workspace.id, workspace.expected, &fixture.cancel)
        .unwrap();
    let token = lease.intent().token.clone();
    snapshot.writer_intent = Some(token.clone());
    let path = fixture
        .repository
        .metadata_path()
        .join("runtime/state.json");
    let mut state: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    state["jobs"][id.as_str()] = serde_json::to_value(snapshot).unwrap();
    fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
    drop(lease); // crash semantics: drops OS lock, retains durable intent
    let mut recovered = fixture.runtime();
    assert!(matches!(
        recovered.status(&id).unwrap().state,
        JobState::OwnershipUnknown {
            worker_pid: None,
            ..
        }
    ));
    assert_eq!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .unwrap()
            .token,
        token
    );
    recovered
        .acknowledge_stopped(
            &id,
            "Fixture proves no writer was released before the durable launch handshake".into(),
        )
        .unwrap();
    assert!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .is_none()
    );
}

#[test]
fn failed_intent_clear_persists_unknown_and_cannot_acknowledge_another_token() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("clear-failure");
    let mut runtime = fixture.runtime();
    let id = runtime
        .submit(shell(
            fixture.binding(&workspace),
            "sleep 0.1; printf preserved > work.txt",
        ))
        .unwrap();
    runtime.poll().unwrap();
    let intent_path = fixture
        .repository
        .metadata_path()
        .join("workspace-locks")
        .join(format!("live-{}.intent", workspace.id));
    let original = fs::read(&intent_path).unwrap();
    let mut replaced: serde_json::Value = serde_json::from_slice(&original).unwrap();
    replaced["token"] = serde_json::Value::String("00000000000000000000000000000001".into());
    fs::write(&intent_path, serde_json::to_vec(&replaced).unwrap()).unwrap();
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(
        matches!(job.state, JobState::OwnershipUnknown { .. }),
        "{:?}",
        job.state
    );
    let state: serde_json::Value =
        serde_json::from_slice(&fs::read(runtime.registry_path().join("state.json")).unwrap())
            .unwrap();
    let persisted: JobSnapshot =
        serde_json::from_value(state["jobs"][id.as_str()].clone()).unwrap();
    assert!(matches!(persisted.state, JobState::OwnershipUnknown { .. }));
    assert!(matches!(
        runtime.acknowledge_stopped(
            &id,
            "Known group was stopped; unrelated marker must remain".into()
        ),
        Err(RuntimeError::UnsafeClose(_))
    ));
    assert_eq!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .unwrap()
            .token
            .as_str(),
        "00000000000000000000000000000001"
    );
    fs::write(&intent_path, original).unwrap(); // undo this fixture's injected marker replacement
    runtime
        .acknowledge_stopped(
            &id,
            "Owned group stopped and original fixture token restored".into(),
        )
        .unwrap();
}

#[test]
fn parent_crash_preserves_survivor_work_and_blocks_independent_restore_until_token_ack() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("crashed");
    fs::write(
        fixture.base.join("driver-workspace.json"),
        serde_json::to_vec(&workspace.id).unwrap(),
    )
    .unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "crash_driver_process",
            "--ignored",
            "--nocapture",
        ])
        .env("IZU_RUNTIME_CRASH_FIXTURE", &fixture.base)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    let mut driver = FixtureProcess::spawn(&mut command);
    let ready = fixture.base.join("driver-ready.json");
    let root = PathBuf::from(&workspace.record.root);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !ready.exists() || !root.join("survivor-heartbeat").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let snapshot: JobSnapshot = serde_json::from_slice(&fs::read(&ready).unwrap()).unwrap();
    driver.kill_and_reap(); // actual private parent handle, never a saved PID
    let before = fs::metadata(root.join("survivor-heartbeat")).unwrap().len();
    std::thread::sleep(Duration::from_millis(100));
    assert!(fs::metadata(root.join("survivor-heartbeat")).unwrap().len() > before);
    let independent =
        Repository::open(fixture.base.join("source"), RepositoryOptions::default()).unwrap();
    assert!(matches!(
        independent.restore(
            workspace.id,
            workspace.expected,
            workspace.expected.head,
            Selection::All,
            &fixture.cancel
        ),
        Err(izu_engine::EngineError::WorkspaceBusy { .. })
    ));
    let mut recovered = fixture.runtime();
    assert!(matches!(
        recovered.status(&snapshot.id).unwrap().state,
        JobState::OwnershipUnknown { .. }
    ));
    assert!(matches!(
        recovered.close_workspace(
            &independent,
            workspace.id,
            workspace.expected,
            &fixture.cancel
        ),
        Err(RuntimeError::UnsafeClose(_))
    ));
    assert_eq!(
        independent
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .unwrap()
            .token,
        snapshot.writer_intent.unwrap()
    );
    // This deliberately escaped fixture is finite. The runtime claims no
    // containment of another session and never signals its persisted PID.
    let deadline = Instant::now() + Duration::from_secs(10);
    while !root.join("survivor-finished").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let stopped_size = fs::metadata(root.join("survivor-heartbeat")).unwrap().len();
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        fs::metadata(root.join("survivor-heartbeat")).unwrap().len(),
        stopped_size
    );
    recovered
        .acknowledge_stopped(
            &snapshot.id,
            "Finite escaped fixture reported completion; heartbeat remains unchanged".into(),
        )
        .unwrap();
    assert!(
        independent
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .is_none()
    );
    let receipt = recovered
        .close_workspace(
            &independent,
            workspace.id,
            workspace.expected,
            &fixture.cancel,
        )
        .unwrap();
    assert!(receipt.retained_path.join("survivor-heartbeat").is_file());
    assert!(!receipt.source_files_deleted);
}

#[test]
#[ignore = "private crash fixture invoked by parent_crash test"]
fn crash_driver_process() {
    let base = PathBuf::from(
        std::env::var_os("IZU_RUNTIME_CRASH_FIXTURE").expect("private crash fixture path"),
    );
    let id: izu_model::WorkspaceId =
        serde_json::from_slice(&fs::read(base.join("driver-workspace.json")).unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let repository = Repository::open(base.join("source"), RepositoryOptions::default()).unwrap();
    let workspace = repository.workspace(id, &cancel).unwrap();
    let mut runtime = Runtime::open(
        repository.metadata_path().join("runtime"),
        RuntimeConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_izu-runtime-worker"))),
    )
    .unwrap();
    let survivor = "import time; f=open('survivor-heartbeat','a'); deadline=time.monotonic()+3;\nwhile time.monotonic()<deadline: f.write('x'); f.flush(); time.sleep(.01)\nf.close(); open('survivor-finished','w').write('finished')";
    let script = format!(
        "import subprocess,sys,time\nsubprocess.Popen([sys.executable,'-c',{survivor:?}],start_new_session=True,stdin=subprocess.DEVNULL,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)\ntime.sleep(30)"
    );
    let request = RunRequest::new(
        WorkspaceBinding::checked(&repository, id, workspace.expected, &cancel).unwrap(),
        vec!["/usr/bin/python3".into(), "-c".into(), script],
    );
    let job = runtime.submit(request).unwrap();
    runtime.poll().unwrap();
    fs::write(
        base.join("driver-ready.json"),
        serde_json::to_vec(&runtime.status(&job).unwrap()).unwrap(),
    )
    .unwrap();
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn output_is_drained_and_bounded_without_turning_truncation_into_success_evidence() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("output");
    let mut config = fixture.config();
    config.output_bytes_per_stream = 1024;
    let mut runtime =
        Runtime::open(fixture.repository.metadata_path().join("runtime"), config).unwrap();
    let request = RunRequest::new(
        fixture.binding(&workspace),
        vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "import sys; sys.stdout.write('x'*200000); sys.stderr.write('e'*100000); sys.exit(7)"
                .into(),
        ],
    );
    let id = runtime.submit(request).unwrap();
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(matches!(
        job.state,
        JobState::Finished {
            outcome: CommandOutcome::Exit { code: Some(7), .. },
            ..
        }
    ));
    assert_eq!(job.output.stdout.bytes.len(), 1024);
    assert_eq!(job.output.stdout.dropped_bytes, 200000 - 1024);
    assert_eq!(job.output.stderr.bytes.len(), 1024);
    assert_eq!(job.output.stderr.dropped_bytes, 100000 - 1024);
}

#[test]
fn external_port_collision_blocks_launch_and_releases_when_available() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("ports");
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut request = shell(fixture.binding(&workspace), "printf bound > ran.txt");
    request.resources.ports = vec![port];
    let mut runtime = fixture.runtime();
    let id = runtime.submit(request).unwrap();
    runtime.poll().unwrap();
    assert!(
        matches!(runtime.status(&id).unwrap().state, JobState::Queued { blocked: Some(AdmissionBlock::ExternalPortBusy { port: actual }) } if actual == port)
    );
    assert!(
        !PathBuf::from(&workspace.record.root)
            .join("ran.txt")
            .exists()
    );
    drop(listener);
    assert!(passed(&runtime.wait(&id, &fixture.cancel).unwrap()));
    assert!(!runtime.capabilities().race_free_port_handoff);
}

#[test]
fn secrets_are_not_returned_or_retained_and_failed_worker_spawn_expires_ticket() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("secret");
    let worker = fixture.base.join("nonexecutable-worker");
    fs::write(&worker, "explicit but nonexecutable\n").unwrap();
    let mut runtime = Runtime::open(
        fixture.repository.metadata_path().join("runtime"),
        RuntimeConfig::new(worker),
    )
    .unwrap();
    let mut request = shell(fixture.binding(&workspace), "exit 0");
    request
        .env_overlay
        .insert("TOKEN".into(), "never-retain-this-value".into());
    let id = runtime.submit(request).unwrap();
    let before = serde_json::to_string(&runtime.status(&id).unwrap()).unwrap();
    assert!(before.contains("TOKEN"));
    assert!(!before.contains("never-retain-this-value"));
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(matches!(
        job.state,
        JobState::Finished {
            outcome: CommandOutcome::SpawnFailed { .. },
            ..
        }
    ));
    assert!(
        !runtime
            .registry_path()
            .join("jobs")
            .join(id.as_str())
            .join("ticket.json")
            .exists()
    );
    let registry = fs::read_to_string(runtime.registry_path().join("state.json")).unwrap();
    assert!(!registry.contains("never-retain-this-value"));
    drop(runtime);
    let mut runtime = fixture.runtime();
    let mut request = shell(
        fixture.binding(&workspace),
        "test -n \"$TOKEN\"; printf used-token",
    );
    request
        .env_overlay
        .insert("TOKEN".into(), "never-retain-this-value".into());
    let id = runtime.submit(request).unwrap();
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(passed(&job));
    assert_eq!(job.output.stdout.text_lossy(), "used-token");
    assert!(
        !serde_json::to_string(&job)
            .unwrap()
            .contains("never-retain-this-value")
    );
    assert!(
        !runtime
            .registry_path()
            .join("jobs")
            .join(id.as_str())
            .join("ticket.json")
            .exists()
    );
}

#[test]
fn registry_recovery_never_signals_saved_pid_and_refuses_unknown_writer_close() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("recovery");
    let registry_path = fixture.repository.metadata_path().join("runtime");
    let mut runtime = fixture.runtime();
    let id = runtime
        .submit(shell(
            fixture.binding(&workspace),
            "printf retained > work.txt",
        ))
        .unwrap();
    assert!(passed(&runtime.wait(&id, &fixture.cancel).unwrap()));
    let mut snapshot = runtime.status(&id).unwrap();
    drop(runtime);
    // A saved PID can identify this test process. Reopening must not signal it.
    snapshot.state = JobState::Running {
        worker_pid: std::process::id(),
        started_at_unix_ms: snapshot.submitted_at_unix_ms,
        deadline_unix_ms: Some(snapshot.submitted_at_unix_ms + 300000),
    };
    let state_path = registry_path.join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    state["jobs"][id.as_str()] = serde_json::to_value(snapshot).unwrap();
    fs::write(&state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let mut recovered = fixture.runtime();
    assert!(
        matches!(recovered.status(&id).unwrap().state, JobState::OwnershipUnknown { worker_pid: Some(pid), .. } if pid == std::process::id())
    );
    assert!(matches!(
        recovered.cancel(&id),
        Err(RuntimeError::UnsafeClose(_))
    ));
    assert!(matches!(
        recovered.close_workspace(
            &fixture.repository,
            workspace.id,
            workspace.expected,
            &fixture.cancel
        ),
        Err(RuntimeError::UnsafeClose(_))
    ));
    assert_eq!(
        fs::read_to_string(PathBuf::from(&workspace.record.root).join("work.txt")).unwrap(),
        "retained"
    );
    assert_eq!(recovered.usage().unwrap().unknown_writers, 1);
}

#[test]
fn independent_controllers_share_registry_and_replaced_locator_is_reported() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let second = Runtime::open(runtime.registry_path(), fixture.config()).unwrap();
    drop(second);
    let path = runtime.registry_path().to_path_buf();
    fs::rename(&path, path.with_file_name("runtime-retained")).unwrap();
    fs::create_dir(&path).unwrap();
    assert!(matches!(
        runtime.usage(),
        Err(RuntimeError::RegistryLocatorChanged(_))
    ));
    let mut runtime = runtime;
    assert!(matches!(
        runtime.poll(),
        Err(RuntimeError::RegistryLocatorChanged(_))
    ));
    assert!(!path.join("state.json").exists());
}

#[test]
fn replacing_workspace_path_after_child_launch_cannot_redirect_relative_writes() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("cwd");
    let original = PathBuf::from(&workspace.record.root);
    let retained = fixture.base.join("retained-cwd");
    let mut runtime = fixture.runtime();
    let request = RunRequest::new(fixture.binding(&workspace), vec!["/usr/bin/python3".into(), "-c".into(), "import os,time; open('ready','w').write('ready');\nwhile not os.path.exists('continue'): time.sleep(.005)\nopen('relative-write.txt','w').write('pinned')".into()]);
    let id = runtime.submit(request).unwrap();
    runtime.poll().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !original.join("ready").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    fs::rename(&original, &retained).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(retained.join("continue"), "go").unwrap();
    let job = runtime.wait(&id, &fixture.cancel).unwrap();
    assert!(matches!(job.state, JobState::OwnershipUnknown { .. }));
    let intent = fixture
        .repository
        .writer_intent(workspace.id, &fixture.cancel)
        .unwrap()
        .unwrap();
    assert_eq!(job.writer_intent.as_ref(), Some(&intent.token));
    assert_eq!(runtime.usage().unwrap().unknown_writers, 1);
    assert!(matches!(
        runtime.close_workspace(
            &fixture.repository,
            workspace.id,
            workspace.expected,
            &fixture.cancel
        ),
        Err(RuntimeError::UnsafeClose(_))
    ));
    assert_eq!(
        fs::read_to_string(retained.join("relative-write.txt")).unwrap(),
        "pinned"
    );
    assert!(!original.join("relative-write.txt").exists());
    runtime
        .acknowledge_stopped(
            &id,
            "test observed owned group cleanup and retained both source directories".into(),
        )
        .unwrap();
    assert_eq!(runtime.usage().unwrap().unknown_writers, 0);
    assert!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        fs::read_to_string(retained.join("relative-write.txt")).unwrap(),
        "pinned"
    );
    assert!(original.is_dir());
}

#[test]
fn dropping_after_registry_and_source_substitution_keeps_exact_writer_intent() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("drop-substitution");
    let original = PathBuf::from(&workspace.record.root);
    let retained = fixture.base.join("retained-drop-source");
    let mut runtime = fixture.runtime();
    let request = RunRequest::new(
        fixture.binding(&workspace),
        vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "import time; open('ready','w').write('original pinned source'); time.sleep(30)".into(),
        ],
    );
    let id = runtime.submit(request).unwrap();
    runtime.poll().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !original.join("ready").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let token = runtime.status(&id).unwrap().writer_intent.unwrap();
    fs::rename(&original, &retained).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(
        original.join("replacement"),
        "replacement remains untouched",
    )
    .unwrap();
    let registry = runtime.registry_path().to_path_buf();
    let retained_registry = registry.with_file_name("retained-drop-registry");
    fs::rename(&registry, &retained_registry).unwrap();
    fs::create_dir(&registry).unwrap();
    drop(runtime);
    assert_eq!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .unwrap()
            .token,
        token
    );
    let mut recovered = Runtime::open(&retained_registry, fixture.config()).unwrap();
    assert!(matches!(
        recovered.status(&id).unwrap().state,
        JobState::OwnershipUnknown { .. }
    ));
    assert_eq!(recovered.usage().unwrap().running_jobs, 1);
    assert!(matches!(
        fixture.repository.restore(
            workspace.id,
            workspace.expected,
            workspace.expected.head,
            Selection::All,
            &fixture.cancel
        ),
        Err(izu_engine::EngineError::WorkspaceBusy { .. })
    ));
    assert_eq!(
        fs::read_to_string(retained.join("ready")).unwrap(),
        "original pinned source"
    );
    assert_eq!(
        fs::read_to_string(original.join("replacement")).unwrap(),
        "replacement remains untouched"
    );
    assert!(!original.join("ready").exists());
    assert!(!registry.join("state.json").exists());
    recovered
        .acknowledge_stopped(
            &id,
            "test observed fallback group shutdown and retained both substituted directories"
                .into(),
        )
        .unwrap();
    assert_eq!(recovered.usage().unwrap().unknown_writers, 0);
}

#[test]
fn replacing_source_after_completion_cannot_close_unrecorded_original_work() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("completed-source");
    let original = PathBuf::from(&workspace.record.root);
    let retained = fixture.base.join("retained-completed-source");
    let mut runtime = fixture.runtime();
    let id = runtime
        .submit(shell(
            fixture.binding(&workspace),
            "printf 'unique completed source' > result.txt",
        ))
        .unwrap();
    assert!(passed(&runtime.wait(&id, &fixture.cancel).unwrap()));
    fs::rename(&original, &retained).unwrap();
    fs::create_dir(&original).unwrap();
    fs::write(
        original.join(".izu"),
        fs::read(retained.join(".izu")).unwrap(),
    )
    .unwrap();
    fs::write(original.join("replacement.txt"), "unrelated replacement").unwrap();
    let close = runtime.close_workspace(
        &fixture.repository,
        workspace.id,
        workspace.expected,
        &fixture.cancel,
    );
    assert!(
        matches!(close, Err(RuntimeError::StaleWorkspace(_))),
        "{close:?}"
    );
    assert_eq!(
        fixture
            .repository
            .workspace(workspace.id, &fixture.cancel)
            .unwrap()
            .expected,
        workspace.expected
    );
    assert_eq!(
        fs::read_to_string(retained.join("result.txt")).unwrap(),
        "unique completed source"
    );
    assert_eq!(
        fs::read_to_string(original.join("replacement.txt")).unwrap(),
        "unrelated replacement"
    );
    assert_eq!(runtime.usage().unwrap().running_jobs, 0);
    assert_eq!(runtime.usage().unwrap().unknown_writers, 0);
}

#[test]
fn thirty_private_writers_preserve_all_work_with_four_running_at_once() {
    let fixture = Fixture::new();
    let mut runtime = fixture.runtime();
    let mut jobs = Vec::new();
    for index in 0..30 {
        let workspace = fixture.workspace(&format!("agent-{index}"));
        let id = runtime
            .submit(shell(
                fixture.binding(&workspace),
                &format!("printf 'writer {index}' > result.txt; printf 'evidence {index}'"),
            ))
            .unwrap();
        jobs.push((workspace, id));
    }
    let mut peak = 0;
    while runtime
        .jobs()
        .unwrap()
        .iter()
        .any(|job| !job.state.is_terminal())
    {
        runtime.poll().unwrap();
        peak = peak.max(runtime.usage().unwrap().running_jobs);
        assert!(peak <= 4);
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(peak, 4);
    for (index, (workspace, id)) in jobs.iter().enumerate() {
        assert!(passed(&runtime.status(id).unwrap()));
        let receipt = runtime
            .close_workspace(
                &fixture.repository,
                workspace.id,
                workspace.expected,
                &fixture.cancel,
            )
            .unwrap();
        assert_eq!(
            fs::read_to_string(receipt.retained_path.join("result.txt")).unwrap(),
            format!("writer {index}")
        );
    }
    assert_eq!(
        fs::read_to_string(fixture.base.join("source/human.txt")).unwrap(),
        "human root remains here\n"
    );
}

fn candidate_with_check(
    fixture: &Fixture,
    argv: Vec<String>,
    environment: Option<izu_model::ObjectId>,
) -> izu_model::CandidateId {
    let root = fixture
        .repository
        .workspace(fixture.repository.workspace_id(), &fixture.cancel)
        .unwrap();
    let tree = fixture
        .repository
        .capture(root.id, Selection::All, &fixture.cancel)
        .unwrap()
        .tree;
    fixture
        .repository
        .prepare_candidate(
            "checked-target".parse().unwrap(),
            None,
            vec![root.expected.head],
            tree,
            vec![izu_model::CheckSpec {
                name: "selected".into(),
                argv,
                environment,
            }],
            izu_model::Identity {
                name: "Runtime fixture".into(),
                email: "runtime@example.test".into(),
            },
            &fixture.cancel,
        )
        .unwrap()
}
fn current_check(fixture: &Fixture, candidate: izu_model::CandidateId) -> izu_model::CheckEvidence {
    let view = fixture.repository.view(&fixture.cancel).unwrap();
    let ids = view.evidence.get(&candidate).unwrap();
    assert_eq!(ids.len(), 1);
    fixture
        .repository
        .evidence(*ids.iter().next().unwrap(), &fixture.cancel)
        .unwrap()
}
#[test]
fn failed_rerun_supersedes_previous_pass_before_land() {
    let fixture = Fixture::new();
    let flag = fixture.base.join("fail-rerun");
    let candidate = candidate_with_check(
        &fixture,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"if test -f "$1"; then exit 7; fi"#.into(),
            "fixture".into(),
            flag.to_str().unwrap().into(),
        ],
        None,
    );
    let mut runtime = fixture.runtime();
    let first = run_check(
        &mut runtime,
        &fixture.repository,
        candidate,
        "selected",
        Duration::from_secs(5),
        &fixture.cancel,
    )
    .unwrap();
    let passed_evidence = fixture
        .repository
        .evidence(first.evidence, &fixture.cancel)
        .unwrap();
    assert_eq!(passed_evidence.outcome, izu_model::CheckOutcome::Passed);
    fs::write(flag, "fail second run").unwrap();
    let second = run_check(
        &mut runtime,
        &fixture.repository,
        candidate,
        "selected",
        Duration::from_secs(5),
        &fixture.cancel,
    )
    .unwrap();
    assert!(!passed(&second.job));
    let failed = current_check(&fixture, candidate);
    assert_eq!(
        failed.outcome,
        izu_model::CheckOutcome::Failed { exit_code: Some(7) }
    );
    assert_ne!(failed.attempt, passed_evidence.attempt);
    assert!(failed.finished_at_unix_ms.is_some());
    assert!(matches!(
        fixture.repository.land(candidate, &fixture.cancel),
        Err(izu_engine::EngineError::MissingCheck(_))
    ));
    assert_eq!(
        fixture
            .repository
            .evidence(first.evidence, &fixture.cancel)
            .unwrap(),
        passed_evidence
    );
}
#[test]
fn source_mutating_rerun_records_failure_and_cannot_reuse_previous_pass() {
    let fixture = Fixture::new();
    let flag = fixture.base.join("mutate-rerun");
    let candidate = candidate_with_check(
        &fixture,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"if test -f "$1"; then printf changed > human.txt; fi"#.into(),
            "fixture".into(),
            flag.to_str().unwrap().into(),
        ],
        None,
    );
    let mut runtime = fixture.runtime();
    let first = run_check(
        &mut runtime,
        &fixture.repository,
        candidate,
        "selected",
        Duration::from_secs(5),
        &fixture.cancel,
    )
    .unwrap();
    let previous = current_check(&fixture, candidate);
    fs::write(flag, "mutate second run").unwrap();
    assert!(matches!(
        run_check(
            &mut runtime,
            &fixture.repository,
            candidate,
            "selected",
            Duration::from_secs(5),
            &fixture.cancel
        ),
        Err(RuntimeError::StaleWorkspace(_))
    ));
    let failed = current_check(&fixture, candidate);
    assert_eq!(
        failed.outcome,
        izu_model::CheckOutcome::Failed { exit_code: None }
    );
    assert_ne!(previous.attempt, failed.attempt);
    assert!(matches!(
        fixture.repository.land(candidate, &fixture.cancel),
        Err(izu_engine::EngineError::MissingCheck(_))
    ));
    assert_eq!(
        fixture
            .repository
            .evidence(first.evidence, &fixture.cancel)
            .unwrap(),
        previous
    );
    assert_eq!(
        fs::read_to_string(fixture.base.join("source/human.txt")).unwrap(),
        "human root remains here\n"
    );
}
#[test]
fn active_check_cancellation_records_terminal_attempt_and_stops_writer() {
    let fixture = Fixture::new();
    let ready = fixture.base.join("check-ready");
    let candidate = candidate_with_check(
        &fixture,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"printf ready > "$1"; sleep 30"#.into(),
            "fixture".into(),
            ready.to_str().unwrap().into(),
        ],
        None,
    );
    let cancel = CancellationToken::new();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !ready.exists() {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(5));
            }
            cancel.cancel();
        });
        let mut runtime = fixture.runtime();
        let execution = run_check(
            &mut runtime,
            &fixture.repository,
            candidate,
            "selected",
            Duration::from_secs(30),
            &cancel,
        )
        .unwrap();
        assert!(matches!(execution.job.state, JobState::Cancelled { .. }));
        assert_eq!(
            current_check(&fixture, candidate).outcome,
            izu_model::CheckOutcome::Cancelled
        );
        assert!(
            fixture
                .repository
                .writer_intent(execution.workspace_close.workspace, &fixture.cancel)
                .unwrap()
                .is_none()
        );
    });
    assert!(matches!(
        fixture.repository.land(candidate, &fixture.cancel),
        Err(izu_engine::EngineError::MissingCheck(_))
    ));
}
#[test]
fn malformed_environment_binding_keeps_selected_attempt_pending_before_any_command() {
    let fixture = Fixture::new();
    let candidate = candidate_with_check(
        &fixture,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf unwanted > wrote.txt".into(),
        ],
        Some(
            fixture
                .repository
                .put_object(
                    izu_model::ObjectKind::Blob,
                    b"unverified fixture environment",
                    &fixture.cancel,
                )
                .unwrap(),
        ),
    );
    let mut runtime = fixture.runtime();
    assert!(matches!(
        run_check(
            &mut runtime,
            &fixture.repository,
            candidate,
            "selected",
            Duration::from_secs(5),
            &fixture.cancel
        ),
        Err(RuntimeError::Environment(_))
    ));
    let pending = current_check(&fixture, candidate);
    assert_eq!(pending.outcome, izu_model::CheckOutcome::Pending);
    assert!(pending.finished_at_unix_ms.is_none());
    assert!(runtime.jobs().unwrap().is_empty());
}

fn environment_fixture() -> Fixture {
    Fixture::with_source(&[
        (".izuignore", b"dependencies/\nwarm/\n"),
        ("fixture.lock", b"locked inputs\n"),
    ])
}
fn prepared_environment(
    fixture: &Fixture,
) -> (
    izu_environment::EnvironmentCache,
    izu_environment::EnvironmentBinding,
    izu_model::ObjectId,
) {
    use izu_environment::*;
    let prepared = fixture.base.join("prepared-environment");
    fs::create_dir(&prepared).unwrap();
    fs::create_dir(prepared.join("dependencies")).unwrap();
    fs::create_dir(prepared.join("warm")).unwrap();
    fs::write(prepared.join("fixture.lock"), b"locked inputs\n").unwrap();
    fs::write(
        prepared.join("dependencies/library"),
        b"prepared dependency\n",
    )
    .unwrap();
    fs::write(prepared.join("warm/cache"), b"prepared warm output\n").unwrap();
    let tree = fixture
        .repository
        .capture(
            fixture.repository.workspace_id(),
            Selection::All,
            &fixture.cancel,
        )
        .unwrap()
        .tree;
    let recipe = Recipe::trusted(
        RecipeSpec {
            schema_version: 1,
            source_identity: Digest::from_hex(&tree.to_string()).unwrap(),
            lockfiles: vec![LockfileIdentity {
                path: "fixture.lock".into(),
                digest: Digest::of_bytes(b"locked inputs\n"),
            }],
            toolchain_identity: Digest::of_bytes(b"declared fixture toolchain"),
            platform: PlatformIdentity::current("declared-fixture-abi"),
            recipe_identity: Digest::of_bytes(b"explicit fixture recipe"),
            trust_domain: "runtime-owned-fixture".into(),
            argv: vec!["this-preparation-recipe-must-never-execute".into()],
            dependencies: vec!["dependencies".into()],
            outputs: vec!["warm".into()],
        },
        TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )
    .unwrap();
    let cache = EnvironmentCache::open(
        fixture.repository.metadata_path().join("environments/v1"),
        EnvironmentLimits::default(),
    )
    .unwrap();
    cache
        .import_quiescent(
            &recipe,
            &prepared,
            QuiescenceAcknowledgement::CallerConfirmsNoWriters,
            SharingPolicy::Copy,
            &fixture.cancel,
        )
        .unwrap();
    let binding = cache.binding(&recipe, &fixture.cancel).unwrap();
    let id = fixture
        .repository
        .put_object(
            izu_model::ObjectKind::Blob,
            &binding.to_json().unwrap(),
            &fixture.cancel,
        )
        .unwrap();
    (cache, binding, id)
}

#[test]
fn bound_check_verifies_actual_starting_files_and_records_exact_native_blob() {
    use izu_environment::{Digest, EnvironmentVerificationScope};
    let fixture = environment_fixture();
    let (_cache, environment, environment_id) = prepared_environment(&fixture);
    let marker = fixture.base.join("selected-environment-command");
    let candidate = candidate_with_check(
        &fixture,
        vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"test "$(cat dependencies/library)" = 'prepared dependency' && printf 'command changed warm output' > warm/cache && printf observed > "$1""#.into(),
            "fixture".into(),
            marker.to_str().unwrap().into(),
        ],
        Some(environment_id),
    );
    let mut runtime = fixture.runtime();
    let execution = run_check(
        &mut runtime,
        &fixture.repository,
        candidate,
        "selected",
        Duration::from_secs(5),
        &fixture.cancel,
    )
    .unwrap();
    assert!(passed(&execution.job));
    assert_eq!(fs::read_to_string(marker).unwrap(), "observed");
    let receipt = execution.job.starting_environment.as_ref().unwrap();
    assert_eq!(receipt.key, environment.key());
    assert_eq!(receipt.manifest_digest, environment.manifest_digest());
    assert_eq!(receipt.source_identity, environment.source_identity());
    assert_eq!(receipt.workspace, execution.workspace_close.workspace);
    assert_eq!(
        receipt.scope,
        EnvironmentVerificationScope::StartingFileContents
    );
    assert!(!receipt.security_boundary);
    assert_eq!(receipt.platform.observed_os, std::env::consts::OS);
    assert_eq!(
        receipt.platform.observed_architecture,
        std::env::consts::ARCH
    );
    assert_eq!(receipt.platform.declared_abi, "declared-fixture-abi");
    assert_eq!(
        receipt.toolchain.declared_identity,
        Digest::of_bytes(b"declared fixture toolchain")
    );
    assert_eq!(
        execution.job.request.environment_binding,
        Some(environment_id)
    );
    let evidence = current_check(&fixture, candidate);
    assert_eq!(evidence.inputs.environment, Some(environment_id));
    assert_eq!(evidence.outcome, izu_model::CheckOutcome::Passed);
    assert!(evidence.finished_at_unix_ms.is_some());
    assert_eq!(
        fixture
            .repository
            .evidence(execution.evidence, &fixture.cancel)
            .unwrap(),
        evidence
    );
    let persisted = fixture.runtime().status(&execution.job.id).unwrap();
    assert_eq!(persisted.starting_environment.as_ref(), Some(receipt));
    assert_eq!(persisted.request.environment_binding, Some(environment_id));
    assert!(passed(&persisted));
    assert_eq!(runtime.usage().unwrap().running_jobs, 0);
    assert!(
        fixture
            .repository
            .writer_intent(execution.workspace_close.workspace, &fixture.cancel)
            .unwrap()
            .is_none()
    );
    assert!(!execution.workspace_close.source_files_deleted);
    assert!(
        !fixture
            .repository
            .operation(execution.workspace_close.close_operation, &fixture.cancel)
            .unwrap()
            .view
            .workspaces
            .contains_key(&execution.workspace_close.workspace)
    );
    assert_eq!(
        fs::read_to_string(execution.workspace_close.retained_path.join("warm/cache")).unwrap(),
        "command changed warm output"
    );
    assert_eq!(
        fs::read_to_string(fixture.base.join("source/human.txt")).unwrap(),
        "human root remains here\n"
    );
}

#[test]
fn tampered_private_dependency_prevents_selected_argv_before_ticket_or_writer() {
    use izu_environment::{EnvironmentError, SharingPolicy, WorkspaceTarget};
    let fixture = environment_fixture();
    let (cache, environment, environment_id) = prepared_environment(&fixture);
    let candidate =
        candidate_with_check(&fixture, vec!["/usr/bin/true".into()], Some(environment_id));
    let result = fixture
        .repository
        .candidate(candidate, &fixture.cancel)
        .unwrap()
        .result;
    let workspace = fixture
        .repository
        .fork_workspace(
            "tampered-environment".into(),
            fixture.base.join("tampered-environment"),
            result,
            &fixture.cancel,
        )
        .unwrap();
    {
        let mut target = WorkspaceTarget::checked(
            &fixture.repository,
            workspace.id,
            workspace.expected,
            &fixture.cancel,
        )
        .unwrap();
        cache
            .materialize_binding(
                &environment,
                &mut target,
                SharingPolicy::Copy,
                &fixture.cancel,
            )
            .unwrap();
    }
    let root = PathBuf::from(&workspace.record.root);
    fs::write(root.join("dependencies/library"), b"tampered dependency\n").unwrap();
    let marker = fixture.base.join("unwanted-command");
    let mut request = RunRequest::new(
        fixture.binding(&workspace),
        vec![
            "/bin/sh".into(),
            "-c".into(),
            r#"printf unwanted > "$1""#.into(),
            "fixture".into(),
            marker.to_str().unwrap().into(),
        ],
    );
    request.environment_binding = Some(environment_id);
    request
        .env_overlay
        .insert("TOKEN".into(), "must-not-reach-ticket".into());
    let mut runtime = fixture.runtime();
    let id = runtime.submit(request).unwrap();
    assert!(matches!(
        runtime.wait(&id, &fixture.cancel),
        Err(RuntimeError::Environment(
            EnvironmentError::StartingEnvironmentMismatch(_)
        ))
    ));
    let job = runtime.status(&id).unwrap();
    assert!(matches!(
        job.state,
        JobState::Finished {
            outcome: CommandOutcome::SpawnFailed { .. },
            ..
        }
    ));
    assert!(job.starting_environment.is_none());
    assert!(!marker.exists());
    assert!(
        !runtime
            .registry_path()
            .join("jobs")
            .join(id.as_str())
            .join("ticket.json")
            .exists()
    );
    assert!(
        !serde_json::to_string(&job)
            .unwrap()
            .contains("must-not-reach-ticket")
    );
    assert!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .is_none()
    );
    assert!(
        fixture
            .repository
            .workspace(workspace.id, &fixture.cancel)
            .is_ok()
    );
    assert_eq!(
        fs::read(root.join("dependencies/library")).unwrap(),
        b"tampered dependency\n"
    );
}

#[test]
fn oversized_native_environment_blob_is_bounded_before_any_command() {
    let fixture = Fixture::new();
    let blob = vec![b'x'; izu_environment::MAX_ENVIRONMENT_BINDING_BYTES + 1];
    let environment = fixture
        .repository
        .put_object(izu_model::ObjectKind::Blob, &blob, &fixture.cancel)
        .unwrap();
    let candidate = candidate_with_check(&fixture, vec!["/usr/bin/true".into()], Some(environment));
    let mut runtime = fixture.runtime();
    assert!(matches!(
        run_check(
            &mut runtime,
            &fixture.repository,
            candidate,
            "selected",
            Duration::from_secs(5),
            &fixture.cancel
        ),
        Err(RuntimeError::Environment(
            izu_environment::EnvironmentError::Limit {
                resource: "environment binding bytes",
                ..
            }
        ))
    ));
    assert_eq!(
        current_check(&fixture, candidate).outcome,
        izu_model::CheckOutcome::Pending
    );
    assert!(runtime.jobs().unwrap().is_empty());
}

#[test]
fn source_root_substitution_cannot_pass_or_clear_writer_intent() {
    let fixture = Fixture::new();
    let flag = fixture.base.join("replace-check-root");
    let marker = fixture.base.join("replacement-path");
    let candidate = candidate_with_check(
        &fixture,
        vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            "import os,pathlib,sys\nif pathlib.Path(sys.argv[1]).exists():\n p=pathlib.Path.cwd(); old=pathlib.Path(str(p)+'-retained'); os.rename(p,old); p.mkdir(); (p/'human.txt').write_bytes((old/'human.txt').read_bytes()); (old/'human.txt').write_text('original private directory changed'); pathlib.Path(sys.argv[2]).write_text(str(p))".into(),
            flag.to_str().unwrap().into(),
            marker.to_str().unwrap().into(),
        ],
        None,
    );
    let mut runtime = fixture.runtime();
    let first = run_check(
        &mut runtime,
        &fixture.repository,
        candidate,
        "selected",
        Duration::from_secs(5),
        &fixture.cancel,
    )
    .unwrap();
    let previous = current_check(&fixture, candidate);
    fs::write(flag, "replace after launch").unwrap();
    assert!(matches!(
        run_check(
            &mut runtime,
            &fixture.repository,
            candidate,
            "selected",
            Duration::from_secs(5),
            &fixture.cancel
        ),
        Err(RuntimeError::UnsafeClose(_)) | Err(RuntimeError::StaleWorkspace(_))
    ));
    let job = runtime.jobs().unwrap().pop().unwrap();
    assert!(matches!(job.state, JobState::OwnershipUnknown { .. }));
    let replacement = PathBuf::from(fs::read_to_string(marker).unwrap());
    let retained = PathBuf::from(format!("{}-retained", replacement.display()));
    assert_eq!(
        fs::read_to_string(retained.join("human.txt")).unwrap(),
        "original private directory changed"
    );
    assert_eq!(
        fs::read_to_string(replacement.join("human.txt")).unwrap(),
        "human root remains here\n"
    );
    let workspace = fixture
        .repository
        .workspace(job.request.workspace.id(), &fixture.cancel)
        .unwrap();
    assert_eq!(PathBuf::from(&workspace.record.root), replacement);
    let intent = fixture
        .repository
        .writer_intent(workspace.id, &fixture.cancel)
        .unwrap()
        .unwrap();
    assert_eq!(job.writer_intent.as_ref(), Some(&intent.token));
    assert!(matches!(
        fixture.repository.restore(
            workspace.id,
            workspace.expected,
            workspace.expected.head,
            Selection::All,
            &fixture.cancel
        ),
        Err(izu_engine::EngineError::WorkspaceBusy { .. })
    ));
    let failed = current_check(&fixture, candidate);
    assert_eq!(
        failed.outcome,
        izu_model::CheckOutcome::Failed { exit_code: None }
    );
    assert_ne!(failed.attempt, previous.attempt);
    assert_eq!(
        fixture
            .repository
            .evidence(first.evidence, &fixture.cancel)
            .unwrap(),
        previous
    );
    assert!(matches!(
        fixture.repository.land(candidate, &fixture.cancel),
        Err(izu_engine::EngineError::MissingCheck(_))
    ));
    assert_eq!(
        fs::read_to_string(fixture.base.join("source/human.txt")).unwrap(),
        "human root remains here\n"
    );
    runtime
        .acknowledge_stopped(
            &job.id,
            "test observed owned group cleanup; both substituted source directories retained"
                .into(),
        )
        .unwrap();
}
#[test]
fn live_foreign_jobs_keep_shared_reservations_and_cannot_be_cancelled_by_another_controller() {
    let fixture = Fixture::new();
    let first = fixture.workspace("foreign-one");
    let second = fixture.workspace("foreign-two");
    let mut config = fixture.config();
    config.budget.max_running_jobs = 1;
    let path = fixture.repository.metadata_path().join("runtime");
    let mut owner = Runtime::open(&path, config.clone()).unwrap();
    let first_id = owner
        .submit(shell(
            fixture.binding(&first),
            "sleep 1; printf first > result.txt",
        ))
        .unwrap();
    owner.poll().unwrap();
    let mut observer = Runtime::open(&path, config.clone()).unwrap();
    assert!(matches!(
        observer.status(&first_id).unwrap().state,
        JobState::Running { .. }
    ));
    let second_id = observer
        .submit(shell(
            fixture.binding(&second),
            "printf second > result.txt",
        ))
        .unwrap();
    observer.poll().unwrap();
    assert!(matches!(
        observer.status(&second_id).unwrap().state,
        JobState::Queued {
            blocked: Some(AdmissionBlock::RunningJobs)
        }
    ));
    assert!(matches!(
        observer.cancel(&first_id),
        Err(RuntimeError::Busy(_))
    ));
    assert_eq!(observer.usage().unwrap().running_jobs, 1);
    assert_eq!(observer.usage().unwrap().unknown_writers, 0);
    assert!(passed(&owner.wait(&first_id, &fixture.cancel).unwrap()));
    assert!(passed(&observer.wait(&second_id, &fixture.cancel).unwrap()));
    config.budget.max_running_jobs = 2;
    assert!(matches!(
        Runtime::open(&path, config),
        Err(RuntimeError::Invalid(_))
    ));
}

struct FixtureProcess(Option<std::process::Child>);
impl FixtureProcess {
    fn spawn(command: &mut std::process::Command) -> Self {
        Self(Some(command.spawn().unwrap()))
    }
    fn reap_if_finished(&mut self) -> Option<std::process::ExitStatus> {
        let status = self.0.as_mut()?.try_wait().unwrap();
        if status.is_some() {
            self.0.take();
        }
        status
    }
    fn kill_and_reap(&mut self) {
        if let Some(mut child) = self.0.take() {
            child.kill().unwrap();
            child.wait().unwrap();
        }
    }
}
impl Drop for FixtureProcess {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}
#[test]
fn thirty_independent_process_controllers_share_four_job_budget_and_private_source() {
    let fixture = Fixture::new();
    fs::create_dir(fixture.base.join("controllers")).unwrap();
    let mut workspaces = Vec::new();
    let mut drivers = Vec::new();
    for index in 0..30 {
        let workspace = fixture.workspace(&format!("independent-{index}"));
        fs::write(
            fixture
                .base
                .join("controllers")
                .join(format!("workspace-{index}.json")),
            serde_json::to_vec(&workspace.id).unwrap(),
        )
        .unwrap();
        workspaces.push(workspace);
    }
    // Prepare every source before the barrier participants exist. Controller
    // startup deadlines measure controller startup, not sequential source forks.
    for index in 0..30 {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "independent_controller_process",
                "--ignored",
                "--nocapture",
            ])
            .env("IZU_RUNTIME_CONTROLLER_FIXTURE", &fixture.base)
            .env("IZU_RUNTIME_CONTROLLER_INDEX", index.to_string())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit());
        drivers.push(FixtureProcess::spawn(&mut command));
    }
    let deadline = Instant::now() + Duration::from_secs(30);
    while (0..30).any(|index| {
        !fixture
            .base
            .join("controllers")
            .join(format!("ready-{index}"))
            .exists()
    }) {
        assert!(Instant::now() < deadline, "independent controller startup");
        std::thread::sleep(Duration::from_millis(10));
    }
    fs::write(fixture.base.join("controllers/go"), "go").unwrap();
    let mut observer = fixture.runtime();
    let mut peak_reservations = 0;
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut finished = 0;
    let mut reaped = [false; 30];
    while finished < 30 {
        observer.poll().unwrap();
        let usage = observer.usage().unwrap();
        assert!(usage.running_jobs <= 4, "{usage:?}");
        assert_eq!(
            usage.unknown_writers, 0,
            "a live foreign controller was recovered"
        );
        peak_reservations = peak_reservations.max(usage.running_jobs);
        for (index, driver) in drivers.iter_mut().enumerate() {
            if !reaped[index]
                && let Some(status) = driver.reap_if_finished()
            {
                assert!(status.success(), "controller {index}: {status}");
                reaped[index] = true;
                finished += 1;
            }
        }
        assert!(
            Instant::now() < deadline,
            "independent controllers timed out"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut edges = Vec::new();
    for (index, workspace) in workspaces.iter().enumerate() {
        let interval: [u64; 2] = serde_json::from_slice(
            &fs::read(
                fixture
                    .base
                    .join("controllers")
                    .join(format!("interval-{index}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        edges.push((interval[0], 1_i32));
        edges.push((interval[1], -1_i32));
        assert_eq!(
            fs::read_to_string(PathBuf::from(&workspace.record.root).join("result.txt")).unwrap(),
            format!("private writer {index}")
        );
        let job: JobSnapshot = serde_json::from_slice(
            &fs::read(
                fixture
                    .base
                    .join("controllers")
                    .join(format!("result-{index}.json")),
            )
            .unwrap(),
        )
        .unwrap();
        assert!(passed(&job), "{:?}", job.state);
    }
    edges.sort_unstable();
    let mut running = 0_i32;
    let mut actual_peak = 0_i32;
    for (_, delta) in edges {
        running += delta;
        actual_peak = actual_peak.max(running);
        assert!(running <= 4);
    }
    assert!(
        actual_peak >= 2,
        "independent commands did not actually overlap"
    );
    assert_eq!(running, 0);
    assert_eq!(peak_reservations, 4);
    assert_eq!(
        fs::read_to_string(fixture.base.join("source/human.txt")).unwrap(),
        "human root remains here\n"
    );
}
#[test]
#[ignore = "private independent controller fixture invoked by the parent test"]
fn independent_controller_process() {
    let base = PathBuf::from(std::env::var_os("IZU_RUNTIME_CONTROLLER_FIXTURE").unwrap());
    let index: u32 = std::env::var("IZU_RUNTIME_CONTROLLER_INDEX")
        .unwrap()
        .parse()
        .unwrap();
    let artifacts = base.join("controllers");
    let id: izu_model::WorkspaceId = serde_json::from_slice(
        &fs::read(artifacts.join(format!("workspace-{index}.json"))).unwrap(),
    )
    .unwrap();
    let repository = Repository::open(base.join("source"), RepositoryOptions::default()).unwrap();
    let cancel = CancellationToken::new();
    let workspace = repository.workspace(id, &cancel).unwrap();
    let mut runtime = Runtime::open(
        repository.metadata_path().join("runtime"),
        RuntimeConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_izu-runtime-worker"))),
    )
    .unwrap();
    fs::write(artifacts.join(format!("ready-{index}")), "ready").unwrap();
    let deadline = Instant::now() + Duration::from_secs(30);
    while !artifacts.join("go").exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let program = "import json,pathlib,sys,time\nstart=time.time_ns()\npathlib.Path('result.txt').write_text('private writer '+sys.argv[2])\ntime.sleep(2)\npathlib.Path(sys.argv[1]).write_text(json.dumps([start,time.time_ns()]))";
    let mut request = RunRequest::new(
        WorkspaceBinding::checked(&repository, id, workspace.expected, &cancel).unwrap(),
        vec![
            "/usr/bin/python3".into(),
            "-c".into(),
            program.into(),
            artifacts
                .join(format!("interval-{index}.json"))
                .to_str()
                .unwrap()
                .into(),
            index.to_string(),
        ],
    );
    request.environment = EnvironmentPolicy::Clear;
    request
        .env_overlay
        .insert("TMPDIR".into(), std::env::var("TMPDIR").unwrap());
    request
        .env_overlay
        .insert("PYTHONDONTWRITEBYTECODE".into(), "1".into());
    let job = runtime.submit(request).unwrap();
    let result = runtime.wait(&job, &cancel).unwrap();
    assert!(passed(&result), "{:?}", result.state);
    runtime
        .close_workspace(&repository, id, workspace.expected, &cancel)
        .unwrap();
    fs::write(
        artifacts.join(format!("result-{index}.json")),
        serde_json::to_vec(&result).unwrap(),
    )
    .unwrap();
}

#[test]
fn source_operation_admission_blocks_close_without_publication_and_cancels() {
    use std::sync::mpsc;
    let fixture = Fixture::new();
    let workspace = fixture.workspace("admission-close");
    let runtime = fixture.runtime();
    let permits: Vec<_> = (0..4)
        .map(|_| runtime.source_operation_permit(&fixture.cancel).unwrap())
        .collect();
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    let cancel = CancellationToken::new();
    std::thread::scope(|scope| {
        let _cancel_on_exit = CancelOnDrop(cancel.clone());
        let (entered_tx, entered_rx) = mpsc::channel();
        let (result_tx, result_rx) = mpsc::channel();
        let fixture = &fixture;
        let token = cancel.clone();
        let workspace = &workspace;
        scope.spawn(move || {
            let mut controller = fixture.runtime();
            entered_tx.send(()).unwrap();
            result_tx
                .send(controller.close_workspace(
                    &fixture.repository,
                    workspace.id,
                    workspace.expected,
                    &token,
                ))
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(200)),
            Err(mpsc::RecvTimeoutError::Timeout)
        ));
        assert_eq!(
            fixture
                .repository
                .current_operation(&fixture.cancel)
                .unwrap(),
            before
        );
        assert_eq!(
            fixture
                .repository
                .workspace(workspace.id, &fixture.cancel)
                .unwrap()
                .expected,
            workspace.expected
        );
        assert!(runtime.jobs().unwrap().is_empty());
        cancel.cancel();
        let error = result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, RuntimeError::Cancelled), "{error:?}");
        assert_eq!(error.uncertain_operation(), None);
    });
    assert_eq!(
        fixture
            .repository
            .current_operation(&fixture.cancel)
            .unwrap(),
        before
    );
    drop(permits);
    let mut controller = fixture.runtime();
    let closed = controller
        .close_workspace(
            &fixture.repository,
            workspace.id,
            workspace.expected,
            &fixture.cancel,
        )
        .unwrap();
    assert_eq!(
        fixture
            .repository
            .current_operation(&fixture.cancel)
            .unwrap(),
        closed.close_operation
    );
    assert!(closed.retained_path.exists());
}

#[test]
fn source_operation_grants_honor_job_budget_ceiling_and_cancel_before_return() {
    let fixture = Fixture::new();
    for budget in [1, 9] {
        let mut config = fixture.config();
        config.budget.max_running_jobs = budget;
        let runtime = Runtime::open(
            fixture
                .repository
                .metadata_path()
                .join(format!("source-budget-{budget}")),
            config,
        )
        .unwrap();
        let permits: Vec<_> = (0..budget.min(4))
            .map(|_| runtime.source_operation_permit(&fixture.cancel).unwrap())
            .collect();
        let cancel = CancellationToken::new();
        std::thread::scope(|scope| {
            let _cancel_on_exit = CancelOnDrop(cancel.clone());
            let (entered_tx, entered_rx) = std::sync::mpsc::channel();
            let (result_tx, result_rx) = std::sync::mpsc::channel();
            let runtime = &runtime;
            let token = cancel.clone();
            scope.spawn(move || {
                entered_tx.send(()).unwrap();
                result_tx
                    .send(runtime.source_operation_permit(&token))
                    .unwrap();
            });
            entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(matches!(
                result_rx.recv_timeout(Duration::from_millis(100)),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout)
            ));
            cancel.cancel();
            assert!(matches!(
                result_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                Err(RuntimeError::Cancelled)
            ));
        });
        drop(permits);
        let cancelled = CancellationToken::new();
        cancelled.cancel();
        assert!(matches!(
            runtime.source_operation_permit(&cancelled),
            Err(RuntimeError::Cancelled)
        ));
        let permit = runtime.source_operation_permit(&fixture.cancel).unwrap();
        assert_eq!(runtime.budget().max_running_jobs, budget);
        drop(permit);
    }
}

#[test]
fn source_operation_admission_refuses_non_private_and_symlink_slots() {
    use std::os::unix::fs::{PermissionsExt, symlink};
    let fixture = Fixture::new();
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    for symlink_slot in [false, true] {
        let runtime = Runtime::open(
            fixture
                .repository
                .metadata_path()
                .join(format!("source-unsafe-{symlink_slot}")),
            fixture.config(),
        )
        .unwrap();
        let path = runtime.registry_path().join("source-0.lock");
        if symlink_slot {
            // No slot is live in this owned corruption fixture.
            fs::rename(
                &path,
                runtime.registry_path().join("retained-source-0.lock"),
            )
            .unwrap();
            symlink("source-1.lock", &path).unwrap();
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let error = runtime
            .source_operation_permit(&fixture.cancel)
            .unwrap_err();
        assert!(
            matches!(error, RuntimeError::Invalid(_) | RuntimeError::Io { .. }),
            "{error:?}"
        );
        assert_eq!(
            fixture
                .repository
                .current_operation(&fixture.cancel)
                .unwrap(),
            before
        );
    }
}

#[test]
fn queued_source_operation_refuses_replaced_registry_binding() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    let permits: Vec<_> = (0..4)
        .map(|_| runtime.source_operation_permit(&fixture.cancel).unwrap())
        .collect();
    let cancel = CancellationToken::new();
    std::thread::scope(|scope| {
        let _cancel_on_exit = CancelOnDrop(cancel.clone());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let runtime = &runtime;
        let token = cancel.clone();
        scope.spawn(move || {
            entered_tx.send(()).unwrap();
            result_tx
                .send(runtime.source_operation_permit(&token))
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        fs::rename(
            runtime.registry_path(),
            fixture.repository.metadata_path().join("retained-runtime"),
        )
        .unwrap();
        fs::create_dir(runtime.registry_path()).unwrap();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
            Err(RuntimeError::RegistryLocatorChanged(_))
        ));
        assert_eq!(
            fixture
                .repository
                .current_operation(&fixture.cancel)
                .unwrap(),
            before
        );
    });
    drop(permits);
}

#[test]
fn source_operation_permits_are_released_by_actual_controller_death() {
    let fixture = Fixture::new();
    let runtime = fixture.runtime();
    let before = fixture
        .repository
        .current_operation(&fixture.cancel)
        .unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "source_admission_holder_process",
            "--ignored",
            "--nocapture",
        ])
        .env("IZU_SOURCE_ADMISSION_FIXTURE", &fixture.base)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    let mut holder = FixtureProcess::spawn(&mut command);
    let deadline = Instant::now() + Duration::from_secs(5);
    while !fixture.base.join("source-permit-holder-ready").exists() {
        assert!(Instant::now() < deadline, "permit holder startup");
        assert!(
            holder.reap_if_finished().is_none(),
            "holder exited before ready"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
    let waiter_cancel = CancellationToken::new();
    std::thread::scope(|scope| {
        let _cancel_on_exit = CancelOnDrop(waiter_cancel.clone());
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (result_tx, result_rx) = std::sync::mpsc::channel();
        let runtime = &runtime;
        let cancel = &waiter_cancel;
        scope.spawn(move || {
            entered_tx.send(()).unwrap();
            result_tx
                .send(runtime.source_operation_permit(cancel))
                .unwrap();
        });
        entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            result_rx.recv_timeout(Duration::from_millis(100)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        ));
        assert!(runtime.jobs().unwrap().is_empty());
        holder.kill_and_reap();
        let permit = result_rx
            .recv_timeout(Duration::from_secs(5))
            .unwrap()
            .unwrap();
        assert_eq!(
            fixture
                .repository
                .current_operation(&fixture.cancel)
                .unwrap(),
            before
        );
        for index in 0..4 {
            assert!(
                runtime
                    .registry_path()
                    .join(format!("source-{index}.lock"))
                    .is_file(),
                "coordination names must remain linked"
            );
        }
        drop(permit);
    });
}

#[test]
#[ignore = "owned child process for source admission crash release"]
fn source_admission_holder_process() {
    let base = PathBuf::from(std::env::var_os("IZU_SOURCE_ADMISSION_FIXTURE").unwrap());
    let runtime = Runtime::open(
        base.join("source/.izu/runtime"),
        RuntimeConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_izu-runtime-worker"))),
    )
    .unwrap();
    let permits: Vec<_> = (0..4)
        .map(|_| {
            runtime
                .source_operation_permit(&CancellationToken::new())
                .unwrap()
        })
        .collect();
    fs::write(base.join("source-permit-holder-ready"), "ready").unwrap();
    std::hint::black_box(&permits);
    loop {
        std::thread::park();
    }
}

#[test]
fn crash_during_starting_before_token_keeps_reservations_and_requires_engine_token_ack() {
    let fixture = Fixture::new();
    let workspace = fixture.workspace("starting-gap");
    fs::write(
        fixture.base.join("starting-workspace.json"),
        serde_json::to_vec(&workspace.id).unwrap(),
    )
    .unwrap();
    let mut command = std::process::Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "starting_before_token_driver_process",
            "--ignored",
            "--nocapture",
        ])
        .env("IZU_RUNTIME_STARTING_FIXTURE", &fixture.base)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());
    let mut driver = FixtureProcess::spawn(&mut command);
    let ready = fixture.base.join("starting-ready.json");
    let deadline = Instant::now() + Duration::from_secs(30);
    while !ready.exists() {
        assert!(Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(10));
    }
    let (id, token): (JobId, izu_engine::WriterToken) =
        serde_json::from_slice(&fs::read(ready).unwrap()).unwrap();
    driver.kill_and_reap();
    let mut recovered = fixture.runtime();
    let snapshot = recovered.status(&id).unwrap();
    assert!(matches!(
        snapshot.state,
        JobState::OwnershipUnknown {
            worker_pid: None,
            ..
        }
    ));
    assert!(snapshot.writer_intent.is_none());
    assert_eq!(recovered.usage().unwrap().running_jobs, 1);
    assert_eq!(recovered.usage().unwrap().unknown_writers, 1);
    assert!(matches!(
        fixture.repository.restore(
            workspace.id,
            workspace.expected,
            workspace.expected.head,
            Selection::All,
            &fixture.cancel
        ),
        Err(izu_engine::EngineError::WorkspaceBusy { .. })
    ));
    assert!(matches!(
        recovered.acknowledge_stopped(
            &id,
            "Helper was killed before selected command launch".into()
        ),
        Err(RuntimeError::UnsafeClose(_))
    ));
    assert_eq!(
        fixture
            .repository
            .writer_intent(workspace.id, &fixture.cancel)
            .unwrap()
            .unwrap()
            .token,
        token
    );
    // Fixture explicitly knows it never released a selected process. The exact
    // engine token must be acknowledged; runtime cannot adopt it by workspace.
    fixture
        .repository
        .acknowledge_writer_stopped(workspace.id, &token, &fixture.cancel)
        .unwrap();
    recovered
        .acknowledge_stopped(
            &id,
            "Exact engine intent reconciled after killing pre-launch helper".into(),
        )
        .unwrap();
    assert_eq!(recovered.usage().unwrap().running_jobs, 0);
    assert!(
        !PathBuf::from(&workspace.record.root)
            .join("unreleased.txt")
            .exists()
    );
}
#[test]
#[ignore = "private pre-token crash transition fixture invoked by its parent test"]
fn starting_before_token_driver_process() {
    let base = PathBuf::from(std::env::var_os("IZU_RUNTIME_STARTING_FIXTURE").unwrap());
    let id: izu_model::WorkspaceId =
        serde_json::from_slice(&fs::read(base.join("starting-workspace.json")).unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let repository = Repository::open(base.join("source"), RepositoryOptions::default()).unwrap();
    let workspace = repository.workspace(id, &cancel).unwrap();
    let mut runtime = Runtime::open(
        repository.metadata_path().join("runtime"),
        RuntimeConfig::new(PathBuf::from(env!("CARGO_BIN_EXE_izu-runtime-worker"))),
    )
    .unwrap();
    let job = runtime
        .submit(shell(
            WorkspaceBinding::checked(&repository, id, workspace.expected, &cancel).unwrap(),
            "printf unreleased > unreleased.txt",
        ))
        .unwrap();
    let state_path = runtime.registry_path().join("state.json");
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(&state_path).unwrap()).unwrap();
    let mut snapshot = runtime.status(&job).unwrap();
    snapshot.state = JobState::Starting {
        reserved_at_unix_ms: snapshot.submitted_at_unix_ms,
    };
    state["jobs"][job.as_str()] = serde_json::to_value(snapshot).unwrap();
    fs::write(state_path, serde_json::to_vec(&state).unwrap()).unwrap();
    let lease = repository
        .lease_workspace(id, workspace.expected, &cancel)
        .unwrap();
    fs::write(
        base.join("starting-ready.json"),
        serde_json::to_vec(&(job, lease.intent().token.clone())).unwrap(),
    )
    .unwrap();
    // No runtime token publication, worker spawn or release handshake occurs.
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

#[test]
fn failed_first_launch_does_not_strand_other_admitted_workspace() {
    let fixture = Fixture::new();
    let first = fixture.workspace("blocked-first");
    let second = fixture.workspace("ready-second");
    let mut runtime = fixture.runtime();
    let first_id = runtime
        .submit(shell(fixture.binding(&first), "printf first > result.txt"))
        .unwrap();
    let second_id = runtime
        .submit(shell(
            fixture.binding(&second),
            "printf second > result.txt",
        ))
        .unwrap();
    let lease = fixture
        .repository
        .lease_workspace(first.id, first.expected, &fixture.cancel)
        .unwrap();
    assert!(runtime.poll().is_err());
    assert!(matches!(
        runtime.status(&first_id).unwrap().state,
        JobState::Queued { .. }
    ));
    assert!(matches!(
        runtime.status(&second_id).unwrap().state,
        JobState::Running { .. }
    ));
    lease.finish_stopped().unwrap();
    assert!(passed(&runtime.wait(&second_id, &fixture.cancel).unwrap()));
    assert!(passed(&runtime.wait(&first_id, &fixture.cancel).unwrap()));
    assert_eq!(runtime.usage().unwrap().running_jobs, 0);
}
