#![cfg(unix)]
use izu_process::{
    BaseEnvironment, CommandSpec, GroupSignal, OutputPolicy, OwnedProcess, Phase, RunError,
    RunOptions, Termination, WorkerLauncher, run, run_interactive,
};
use std::{
    ffi::OsString,
    os::unix::ffi::OsStringExt,
    path::PathBuf,
    process::Command,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::{Duration, Instant},
};
fn worker() -> WorkerLauncher {
    WorkerLauncher {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_izu-process-worker")),
        prefix_args: vec![],
    }
}
fn command(mode: &str) -> CommandSpec {
    let mut c = CommandSpec::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
    c.arg(mode);
    c
}
fn opts() -> RunOptions {
    RunOptions {
        timeout: Duration::from_secs(5),
        cleanup_timeout: Duration::from_millis(200),
        ..RunOptions::default()
    }
}
#[test]
fn normal_nonzero_exit_captures_exact_bytes_and_reaps_owned_worker() {
    let r = run(&command("exit7"), b"", &opts(), &worker(), &|| false).unwrap();
    assert_eq!(r.termination, Termination::Completed);
    assert_eq!(r.exit.unwrap().code(), Some(7));
    assert_eq!(r.stdout.bytes, b"normal\n");
    assert!(r.stdout.eof && r.stderr.eof);
    assert!(r.cleanup.cooperative_stop_succeeded());
    assert_eq!(r.cleanup.group_signal, GroupSignal::Sent);
    assert!(r.command_elapsed.is_some());
}
#[test]
fn simultaneous_large_stdin_stdout_stderr_cannot_deadlock() {
    let mut o = opts();
    o.stderr_limit = 3 * 1024 * 1024;
    let input = vec![b'I'; 2 * 1024 * 1024];
    let r = run(&command("flood"), &input, &o, &worker(), &|| false).unwrap();
    assert_eq!(r.termination, Termination::Completed);
    assert_eq!(r.exit.unwrap().code(), Some(0));
    assert_eq!(r.input_written, input.len());
    assert_eq!(r.stdout.bytes, vec![b'O'; 2 * 1024 * 1024]);
    assert_eq!(r.stderr.bytes, vec![b'E'; 2 * 1024 * 1024]);
    assert!(r.stdout.eof && r.stderr.eof);
    assert!(r.cleanup.cooperative_stop_succeeded());
    eprintln!("simultaneous 2MiB input/out/err elapsed {:?}", r.elapsed);
}
#[test]
fn target_exit_with_descendant_retained_pipes_is_bounded() {
    for mode in ["background", "ignore-term"] {
        let r = run(&command(mode), b"", &opts(), &worker(), &|| false).unwrap();
        assert_eq!(r.termination, Termination::Completed);
        assert!(r.exit.unwrap().success());
        assert!(r.cleanup.cooperative_stop_succeeded());
        assert!(r.stdout.eof && r.stderr.eof);
        assert!(r.cleanup_elapsed < opts().cleanup_timeout + Duration::from_millis(500));
        assert!(r.elapsed < opts().timeout + opts().cleanup_timeout + Duration::from_secs(1));
        assert!(r.stdout.bytes.starts_with(b"retained "));
    }
}
#[test]
fn escaped_pipe_writer_never_blocks_completion_and_is_reported() {
    let r = run(&command("escape"), b"", &opts(), &worker(), &|| false).unwrap();
    assert_eq!(r.termination, Termination::Completed);
    assert!(r.cleanup.cooperative_stop_succeeded());
    assert!(!r.stdout.eof);
    assert!(r.cleanup_elapsed < Duration::from_millis(700));
    assert!(r.elapsed < Duration::from_secs(6));
    assert!(r.stdout.bytes.windows(7).any(|x| x == b"escaped"));
    thread::sleep(Duration::from_secs(1));
}
#[test]
fn timeout_and_mid_run_cancellation_stop_owned_groups() {
    let mut o = opts();
    o.timeout = Duration::from_millis(30);
    let r = run(&command("hold"), b"", &o, &worker(), &|| false).unwrap();
    assert_eq!(r.termination, Termination::TimedOut);
    assert!(r.cleanup.cooperative_stop_succeeded());
    let cancelled = AtomicBool::new(false);
    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(30));
            cancelled.store(true, Ordering::Relaxed);
        });
        let r = run(&command("hold"), b"", &opts(), &worker(), &|| {
            cancelled.load(Ordering::Relaxed)
        })
        .unwrap();
        assert_eq!(r.termination, Termination::Cancelled);
        assert!(r.cleanup.cooperative_stop_succeeded());
    });
}
#[test]
fn prelaunch_cancel_limits_and_spawn_failure_are_typed() {
    let invalid = WorkerLauncher {
        executable: PathBuf::from("/does/not/exist"),
        prefix_args: vec![],
    };
    assert!(matches!(
        run(&command("echo"), b"", &opts(), &invalid, &|| true),
        Err(RunError::Cancelled)
    ));
    let mut o = opts();
    o.stdin_limit = 0;
    assert!(matches!(
        run(&command("echo"), b"x", &o, &invalid, &|| false),
        Err(RunError::InputLimit)
    ));
    assert!(matches!(
        run(&command("echo"), b"", &opts(), &invalid, &|| false),
        Err(RunError::WorkerExecutable(_))
    ));
    let r = run(
        &CommandSpec::new("/does/not/exist"),
        b"",
        &opts(),
        &worker(),
        &|| false,
    )
    .unwrap();
    assert!(matches!(
        r.termination,
        Termination::Io {
            phase: Phase::TargetSpawn,
            ..
        }
    ));
    assert!(r.cleanup.cooperative_stop_succeeded());
}
#[test]
fn output_caps_terminate_or_drain_without_corrupt_success() {
    let mut o = opts();
    o.stdout_limit = 128;
    o.stderr_limit = 128;
    let input = vec![b'I'; 2 * 1024 * 1024];
    let r = run(&command("flood"), &input, &o, &worker(), &|| false).unwrap();
    assert!(matches!(r.termination, Termination::OutputLimit(_)));
    assert!(r.stdout.bytes.len() <= 128 && r.stderr.bytes.len() <= 128);
    assert!(r.cleanup.cooperative_stop_succeeded());
    o.output_policy = OutputPolicy::TruncateDrain;
    let r = run(&command("flood"), &input, &o, &worker(), &|| false).unwrap();
    assert_eq!(r.termination, Termination::Completed);
    assert!(r.exit.unwrap().success());
    assert_eq!(r.stdout.bytes.len(), 128);
    assert_eq!(r.stderr.bytes.len(), 128);
    assert_eq!(r.stdout.dropped, 2 * 1024 * 1024 - 128);
    assert_eq!(r.stderr.dropped, 2 * 1024 * 1024 - 128);
    assert!(r.stdout.eof && r.stderr.eof);
}
#[test]
fn raw_argv_environment_and_no_shell_injection() {
    let mut c = command("argv");
    let raw = OsString::from_vec(vec![b'x', 255, b'y']);
    c.arg("$(exit 99); `exit 98`")
        .arg(&raw)
        .env("EXACT_ENV", "visible");
    let mut o = opts();
    o.base_environment = BaseEnvironment::Clear;
    let r = run(&c, b"", &o, &worker(), &|| false).unwrap();
    let mut expected = b"$(exit 99); `exit 98`\0".to_vec();
    expected.extend_from_slice(&[b'x', 255, b'y', 0]);
    assert_eq!(r.stdout.bytes, expected);
    assert_eq!(r.stderr.bytes, b"visible");
    assert!(r.exit.unwrap().success());
}
#[test]
fn interactive_close_predicate_keeps_stdin_open_until_response() {
    let r = run_interactive(
        &command("interactive"),
        b"Q",
        &opts(),
        &worker(),
        &|| false,
        &mut |stdout, _| stdout.windows(9).any(|x| x == b"response\n"),
    )
    .unwrap();
    assert_eq!(r.termination, Termination::Completed);
    assert!(r.exit.unwrap().success());
    assert_eq!(r.stdout.bytes, b"response\nclosed\n");
}
#[test]
fn repeated_cleanup_cannot_signal_after_reap() {
    let mut c = Command::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
    c.arg("hold");
    let mut p = OwnedProcess::spawn(&mut c).unwrap();
    let first = p.stop(Duration::from_secs(1));
    assert!(first.cooperative_stop_succeeded());
    assert_eq!(p.stop(Duration::from_secs(1)), first);
    assert!(p.observe_exit().unwrap());
    drop(p);
}
#[test]
fn drop_is_nonblocking_and_does_not_claim_confirmed_cleanup() {
    use std::io::Read;
    use std::process::Stdio;
    let mut c = Command::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
    c.arg("drop-full-stderr")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut p = OwnedProcess::spawn(&mut c).unwrap();
    let mut stdout = p.take_stdout().unwrap();
    izu_process::set_nonblocking(&stdout).unwrap();
    // The parent deliberately never drains stderr. The child must finish Drop
    // while that pipe is full; waiting for its writer or warning would deadlock.
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut output = Vec::new();
    while Instant::now() < deadline && !output.ends_with(b"dropped\n") {
        let mut bytes = [0; 128];
        match stdout.read(&mut bytes) {
            Ok(0) => break,
            Ok(n) => output.extend_from_slice(&bytes[..n]),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(error) => panic!("fixture stdout: {error}"),
        }
    }
    let cleanup = p.stop(Duration::from_secs(1));
    drop(p);
    assert_eq!(output, b"ready\ndropped\n");
    assert!(cleanup.cooperative_stop_succeeded());
}

#[test]
fn hidden_facade_worker_mode_uses_the_same_boundary() {
    let launcher = WorkerLauncher {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_izu-process-test-child")),
        prefix_args: vec!["__process-worker".into()],
    };
    let r = run(
        &command("echo"),
        b"facade bytes\0",
        &opts(),
        &launcher,
        &|| false,
    )
    .unwrap();
    assert_eq!(r.stdout.bytes, b"facade bytes\0");
    assert!(r.cleanup.cooperative_stop_succeeded());
    assert_eq!(r.termination, Termination::Completed);
}
#[test]
fn wrong_worker_protocol_is_rejected_and_its_owned_group_stopped() {
    let launcher = WorkerLauncher {
        executable: PathBuf::from(env!("CARGO_BIN_EXE_izu-process-test-child")),
        prefix_args: vec!["wrong-worker".into()],
    };
    let r = run(&command("echo"), b"", &opts(), &launcher, &|| false).unwrap();
    assert_eq!(r.termination, Termination::Protocol);
    assert!(r.cleanup.cooperative_stop_succeeded());
}
#[test]
fn direct_leader_observation_preserves_group_identity_until_cleanup() {
    let mut c = Command::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
    c.arg("background");
    let mut p = OwnedProcess::spawn(&mut c).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !p.observe_exit().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let report = p.stop(Duration::from_secs(1));
    assert_eq!(report.group_signal, GroupSignal::Sent);
    assert!(report.leader_reaped);
}
#[test]
fn direct_exited_empty_group_never_ignores_permission_error() {
    let mut c = Command::new("/usr/bin/true");
    let mut p = OwnedProcess::spawn(&mut c).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !p.observe_exit().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let report = p.stop(Duration::from_secs(1));
    assert!(report.leader_reaped);
    #[cfg(target_os = "macos")]
    assert_eq!(
        report.group_signal,
        GroupSignal::Uncertain(Some(rustix::io::Errno::PERM.raw_os_error()))
    );
    #[cfg(target_os = "linux")]
    assert!(matches!(
        report.group_signal,
        GroupSignal::Sent | GroupSignal::AlreadyAbsent
    ));
    assert_eq!(p.stop(Duration::from_secs(1)), report);
}

#[test]
fn raw_invalid_inputs_are_rejected_before_any_worker_launch() {
    let invalid = WorkerLauncher {
        executable: PathBuf::from("/does/not/exist"),
        prefix_args: vec![],
    };
    let mut c = command("argv");
    c.arg(OsString::from_vec(vec![0]));
    assert!(matches!(
        run(&c, b"", &opts(), &invalid, &|| false),
        Err(RunError::Configuration)
    ));
    let mut c = command("argv");
    c.args = vec![OsString::new(); 4097];
    assert!(matches!(
        run(&c, b"", &opts(), &invalid, &|| false),
        Err(RunError::TicketLimit)
    ));
    let mut c = command("argv");
    c.env("", "invalid");
    assert!(matches!(
        run(&c, b"", &opts(), &invalid, &|| false),
        Err(RunError::Configuration)
    ));
}
#[test]
fn working_directory_and_inherited_environment_removal_are_exact() {
    let dir = tempfile::tempdir().unwrap();
    let mut c = command("state");
    c.current_dir(dir.path());
    let mut o = opts();
    o.base_environment = BaseEnvironment::Inherit;
    let r = run(&c, b"", &o, &worker(), &|| false).unwrap();
    assert_eq!(
        r.stdout.bytes,
        dir.path()
            .canonicalize()
            .unwrap()
            .as_os_str()
            .as_encoded_bytes()
    );
    assert_eq!(
        r.stderr.bytes,
        std::env::var_os("PATH")
            .unwrap_or_default()
            .as_encoded_bytes()
    );
    c.env_remove("PATH");
    let r = run(&c, b"", &o, &worker(), &|| false).unwrap();
    assert!(r.stderr.bytes.is_empty());
}

#[test]
fn parent_disconnect_stops_the_workers_independent_cooperative_group() {
    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("heartbeat");
    let mut c = Command::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
    c.arg("orphan-runner")
        .arg(env!("CARGO_BIN_EXE_izu-process-worker"))
        .arg(&heartbeat);
    let mut parent = OwnedProcess::spawn(&mut c).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !heartbeat.exists() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(5));
    }
    assert!(
        parent
            .stop(Duration::from_secs(1))
            .cooperative_stop_succeeded()
    );
    // The worker owns a different group, so killing this parent group alone
    // cannot stop the heartbeat. Socket EOF must activate worker self-cleanup.
    thread::sleep(Duration::from_millis(30));
    let stopped = std::fs::metadata(&heartbeat).unwrap().len();
    thread::sleep(Duration::from_millis(100));
    assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), stopped);
}

#[test]
fn post_reap_group_probe_can_observe_actual_absence() {
    for mode in ["hold", "background"] {
        let mut c = Command::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
        c.arg(mode);
        let mut p = OwnedProcess::spawn(&mut c).unwrap();
        let pid = rustix::process::Pid::from_raw(i32::try_from(p.id()).unwrap()).unwrap();
        if mode == "background" {
            let deadline = Instant::now() + Duration::from_secs(5);
            while !p.observe_exit().unwrap() {
                assert!(Instant::now() < deadline);
                thread::sleep(Duration::from_millis(1));
            }
        }
        let report = p.stop(Duration::from_secs(1));
        assert!(report.leader_reaped);
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let result = rustix::process::test_kill_process_group(pid);
            if result == Err(rustix::io::Errno::SRCH) {
                break;
            }
            if Instant::now() >= deadline {
                panic!("group absence not observed for {mode}: {result:?}");
            }
            thread::sleep(Duration::from_millis(1));
        }
    }
}

#[test]
fn same_group_background_writer_is_quiescent_before_cleanup_succeeds() {
    let dir = tempfile::tempdir().unwrap();
    let heartbeat = dir.path().join("heartbeat");
    let mut spec = command("background-writer");
    spec.arg(&heartbeat);
    let result = run(&spec, b"", &opts(), &worker(), &|| false).unwrap();
    assert_eq!(
        result.cleanup.group_quiescence,
        izu_process::GroupQuiescence::AbsentObserved
    );
    assert!(result.cleanup.cooperative_stop_succeeded());
    assert!(result.exit.unwrap().success());
    let stopped = std::fs::metadata(&heartbeat).unwrap().len();
    assert!(stopped > 0);
    thread::sleep(Duration::from_millis(100));
    assert_eq!(std::fs::metadata(&heartbeat).unwrap().len(), stopped);
}

#[test]
fn independently_observed_absence_proves_self_killed_worker_stopped() {
    let mut c = Command::new(env!("CARGO_BIN_EXE_izu-process-test-child"));
    c.arg("self-kill-worker");
    let mut p = OwnedProcess::spawn(&mut c).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !p.observe_exit().unwrap() {
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(1));
    }
    let report = p.stop(Duration::from_secs(1));
    assert!(report.leader_reaped);
    assert_eq!(
        report.group_quiescence,
        izu_process::GroupQuiescence::AbsentObserved
    );
    #[cfg(target_os = "macos")]
    assert_eq!(
        report.group_signal,
        GroupSignal::Uncertain(Some(rustix::io::Errno::PERM.raw_os_error()))
    );
    assert!(
        report.cooperative_stop_succeeded(),
        "stopped-state proof must not depend on parent signal delivery: {report:?}"
    );
}
