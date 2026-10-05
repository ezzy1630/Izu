use izu_lab::{
    runner::{Runner, allocated_bytes},
    suite,
};
use std::{
    path::{Path, PathBuf},
    thread,
    time::Duration,
};
fn runner(executable: &str, timeout: Duration, output_limit: usize, measure_rss: bool) -> Runner {
    let mut runner = suite::default_runner(
        PathBuf::from(executable),
        1,
        PathBuf::from(env!("CARGO_BIN_EXE_izu-lab")),
    )
    .expect("fresh helper identity");
    runner.timeout = timeout;
    runner.output_limit = output_limit;
    runner.measure_rss = measure_rss;
    runner
}

#[test]
fn mismatched_artifact_is_rejected_before_creating_a_fixture_or_running_a_command() {
    let temp = tempfile::tempdir().expect("owned scratch");
    let artifact = temp.path().join("artifact");
    std::fs::write(&artifact, b"untrusted artifact bytes").expect("fixture");
    let output = temp.path().join("report.json");
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_izu-lab"))
        .args([
            "--executable",
            artifact.to_str().expect("UTF8 temp path"),
            "--expected-sha256",
            &"0".repeat(64),
            "--source-root",
            temp.path().to_str().expect("UTF8 temp path"),
            "--output",
            output.to_str().expect("UTF8 temp path"),
            "--scratch-dir",
            temp.path().to_str().expect("UTF8 temp path"),
            "acceptance",
        ])
        .output()
        .expect("fresh harness");
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("artifact SHA256 mismatch"));
    assert!(!output.exists());
    assert_eq!(
        std::fs::read_dir(temp.path()).expect("owned files").count(),
        1
    );
}

#[cfg(unix)]
#[test]
fn bounded_child_drains_large_output_and_preserves_failure() {
    let temp = tempfile::tempdir().expect("owned scratch");
    let runner = runner("/bin/sh", Duration::from_secs(2), 128, false);
    let run = runner.run(&["-c".into(), "i=0; while [ $i -lt 1000 ]; do printf abcdefghijklmnopqrstuvwxyz; i=$((i+1)); done; exit 7".into()], temp.path(), None).expect("execution");
    assert_eq!(run.exit_code, Some(7));
    assert!(run.output_truncated);
    assert_eq!(run.stdout.len(), 128);
    assert!(!run.succeeded());
}
#[cfg(unix)]
#[test]
fn deadline_reaps_only_owned_child() {
    let temp = tempfile::tempdir().expect("owned scratch");
    let runner = runner("/bin/sh", Duration::from_millis(25), 128, false);
    let run = runner
        .run(
            &["-c".into(), "while :; do :; done".into()],
            temp.path(),
            None,
        )
        .expect("execution");
    assert!(run.timed_out);
    assert!(!run.succeeded());
    assert!(run.elapsed_ns < 1_000_000_000);
}
#[cfg(unix)]
#[test]
fn allocation_does_not_follow_external_symlink() {
    let temp = tempfile::tempdir().expect("owned scratch");
    std::os::unix::fs::symlink("/", temp.path().join("outside")).expect("link");
    assert!(
        allocated_bytes(temp.path())
            .expect("allocated bytes")
            .expect("Unix")
            < 1_000_000
    );
}
#[cfg(unix)]
#[test]
fn exited_leader_cannot_leave_inherited_pipe_writer_running() {
    let temp = tempfile::tempdir().expect("owned scratch");
    let runner = runner("/bin/sh", Duration::from_secs(2), 128, false);
    let run = runner
        .run(
            &[
                "-c".into(),
                "(sleep 0.15; printf escaped > escaped.txt) & printf acknowledged; exit 0".into(),
            ],
            temp.path(),
            None,
        )
        .expect("group contained");
    assert_eq!(run.stdout, "acknowledged");
    assert!(run.succeeded());
    thread::sleep(Duration::from_millis(250));
    assert!(!temp.path().join("escaped.txt").exists());
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn supported_time_wrapper_records_real_peak_rss() {
    assert!(
        Path::new("/usr/bin/time").exists(),
        "RSS acceptance requires supported system time utility"
    );
    let temp = tempfile::tempdir().expect("owned scratch");
    let runner = runner("/usr/bin/true", Duration::from_secs(2), 4096, true);
    let run = runner
        .run(&[], temp.path(), None)
        .expect("owned wrapped execution");
    assert!(run.succeeded());
    assert!(run.maximum_rss_bytes.is_some_and(|bytes| bytes > 0));
    assert_eq!(
        run.measurement_wrapper,
        Some(PathBuf::from("/usr/bin/time"))
    );
}
