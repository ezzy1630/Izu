use crate::{
    Distribution, distribution,
    fixture::{self, FixtureSpec},
    runner::{Run, Runner},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs, io,
    path::{Component, Path, PathBuf},
    thread,
    time::Duration,
};

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Outcome {
    Passed,
    Failed { reason: String },
    Blocked { reason: String },
}

#[derive(Debug, Serialize)]
pub struct Case {
    pub name: String,
    pub outcome: Outcome,
    pub runs: Vec<Run>,
    pub file_observations: Vec<FileObservation>,
    pub managed_overlap: Option<crate::managed::ManagedEvidence>,
}

#[derive(Debug, Serialize)]
pub struct FileObservation {
    pub path: PathBuf,
    pub expected_sha256: String,
    pub expected_bytes: u64,
    pub actual_sha256: Option<String>,
    pub actual_bytes: Option<u64>,
    pub error: Option<String>,
}

pub(crate) fn observe_file(path: PathBuf, expected: &[u8]) -> FileObservation {
    let result = crate::provenance::file_hash(&path);
    FileObservation {
        expected_sha256: format!("{:x}", Sha256::digest(expected)),
        expected_bytes: expected.len() as u64,
        actual_bytes: fs::metadata(&path).ok().map(|metadata| metadata.len()),
        actual_sha256: result.as_ref().ok().cloned(),
        error: result.err().map(|error| error.to_string()),
        path,
    }
}

impl FileObservation {
    pub(crate) fn matched(&self) -> bool {
        self.actual_sha256.as_ref() == Some(&self.expected_sha256)
            && self.actual_bytes == Some(self.expected_bytes)
    }
}

#[derive(Debug, Serialize)]
pub struct Benchmark {
    pub name: String,
    pub implementation: String,
    pub cache_state: String,
    pub accepted_changes: usize,
    pub latency: Option<Distribution>,
    pub runs: Vec<Run>,
    pub outcome: Outcome,
    pub file_observations: Vec<FileObservation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workflow: Option<crate::workflows::WorkflowEvidence>,
    pub fixture_cleanup: Vec<fixture::CleanupObservation>,
}

impl Benchmark {
    pub(crate) fn record_cleanup(&mut self, cleanup: fixture::CleanupObservation) -> bool {
        if !cleanup.removed {
            let message = format!(
                "owned fixture cleanup failed; retained {}: {}",
                cleanup.path.display(),
                cleanup
                    .error
                    .as_deref()
                    .unwrap_or("unknown cleanup failure")
            );
            match &mut self.outcome {
                Outcome::Failed { reason } => {
                    reason.push_str("; ");
                    reason.push_str(&message);
                }
                Outcome::Blocked { reason } => {
                    self.outcome = Outcome::Failed {
                        reason: format!("{reason}; {message}"),
                    }
                }
                Outcome::Passed => self.outcome = Outcome::Failed { reason: message },
            }
        }
        let removed = cleanup.removed;
        self.fixture_cleanup.push(cleanup);
        removed
    }
}

pub fn basic_acceptance(runner: &Runner, root: &Path) -> io::Result<Vec<Case>> {
    fs::create_dir(root)?;
    let mut case = Case {
        name: "empty_init_edit_checkpoint_commit_status_history".into(),
        outcome: Outcome::Passed,
        runs: Vec::new(),
        file_observations: Vec::new(),
        managed_overlap: None,
    };
    for args in [vec!["--json", "init", "."], vec!["--json", "status"]] {
        let run = runner.run(
            &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            root,
            None,
        )?;
        let ok = run.succeeded() && json_success(&run.stdout);
        case.runs.push(run);
        if !ok {
            case.outcome = Outcome::Failed {
                reason: "init/status did not return successful structured state".into(),
            };
            return Ok(vec![case]);
        }
    }
    fs::write(root.join("tracked.txt"), "base tracked bytes\n")?;
    fs::write(root.join("loose.txt"), "independent untracked bytes\n")?;
    for args in [
        vec!["--json", "checkpoint"],
        vec![
            "--json",
            "commit",
            "-m",
            "synthetic lab baseline",
            "--path",
            "tracked.txt",
            "--author-name",
            "izu lab fixture",
            "--author-email",
            "fixture@izu.invalid",
        ],
        vec!["--json", "status"],
        vec!["--json", "log"],
        vec!["--json", "verify"],
    ] {
        let run = runner.run(
            &args.into_iter().map(str::to_owned).collect::<Vec<_>>(),
            root,
            None,
        )?;
        let ok = run.succeeded() && json_success(&run.stdout);
        case.runs.push(run);
        if !ok {
            case.outcome = Outcome::Failed {
                reason:
                    "checkpoint/commit/status/log/verify did not return successful structured state"
                        .into(),
            };
            break;
        }
    }
    case.file_observations.push(observe_file(
        root.join("tracked.txt"),
        b"base tracked bytes\n",
    ));
    case.file_observations.push(observe_file(
        root.join("loose.txt"),
        b"independent untracked bytes\n",
    ));
    if case
        .file_observations
        .iter()
        .any(|observation| !observation.matched())
    {
        case.outcome = Outcome::Failed {
            reason: "selective commit changed tracked or unrelated untracked source bytes".into(),
        };
    }
    Ok(vec![case])
}

pub fn standard_acceptance(runner: &Runner, root: &Path) -> io::Result<Vec<Case>> {
    let definitions = [
        include_str!("../scenarios/undo_restore_preserve_tracked_untracked.json"),
        include_str!("../scenarios/private_workspace_root_unchanged.json"),
        include_str!("../scenarios/same_ref_compare_and_swap_race.json"),
        include_str!("../scenarios/30_writers_common_target_integration.json"),
        include_str!("../scenarios/exact_candidate_check_stale_evidence.json"),
        include_str!("../scenarios/conflict_preservation_explicit_resolution.json"),
        include_str!("../scenarios/ignore_tracked_symlink_safety.json"),
        include_str!("../scenarios/json_external_path.json"),
    ];
    definitions
        .into_iter()
        .enumerate()
        .map(|(index, definition)| {
            let scenario: Scenario = serde_json::from_str(definition).map_err(io::Error::other)?;
            run_scenario(runner, &root.join(format!("standard-{index}")), scenario)
        })
        .collect()
}

fn json_success(stdout: &str) -> bool {
    serde_json::from_str::<Value>(stdout)
        .ok()
        .is_some_and(|value| {
            value.pointer("/outcome/kind") == Some(&Value::String("ok".into()))
                && value.get("schema_version") == Some(&Value::from(1))
                && value.pointer("/build/product") == Some(&Value::String("izu".into()))
        })
}

pub fn blocked_coverage() -> Vec<Case> {
    [
        (
            "undo_restore_preserve_tracked_untracked",
            "needs exact revision/operation assertions in supplied real-CLI scenario",
        ),
        (
            "private_workspace_root_unchanged",
            "needs workspace response ID and independently checked source paths",
        ),
        (
            "30_writers_common_target_integration",
            "needs exact candidate/land/ref semantics and integration assertions",
        ),
        (
            "same_ref_compare_and_swap_race",
            "needs two immutable revisions and guarded refs expected-old assertions",
        ),
        (
            "conflict_preservation_explicit_resolution",
            "needs candidate conflict response and explicit resolution path",
        ),
        (
            "exact_candidate_check_stale_evidence",
            "needs real managed child command/check API and stale-evidence rejection",
        ),
        (
            "crash_restart_last_acknowledged_checkpoint",
            "requires supplied test-only fault binary and restart readbacks",
        ),
        (
            "disk_full_failure_retry",
            "requires integrator-owned isolated full-disk image; harness never fills shared disk",
        ),
        (
            "corruption_unknown_format",
            "needs native store path/format contract for precise owned corruption fixture",
        ),
        (
            "ignore_tracked_symlink_safety",
            "needs independent tracked/ignored membership and external sentinel checks",
        ),
        (
            "native_bundle_cold_restore",
            "needs actual native bundle support; unsupported remains blocked",
        ),
        (
            "git_original_izu_localbare_clone",
            "needs actual Git import/export transport commands and clone content/history readbacks",
        ),
        (
            "json_mcp_external_path",
            "needs supplied versioned JSON requests and MCP protocol state assertions",
        ),
    ]
    .into_iter()
    .map(|(name, reason)| Case {
        name: name.into(),
        outcome: Outcome::Blocked {
            reason: reason.into(),
        },
        runs: Vec::new(),
        file_observations: Vec::new(),
        managed_overlap: None,
    })
    .collect()
}

fn benchmark_case(
    name: &str,
    implementation: &str,
    cache_state: &str,
    accepted_changes: usize,
    runs: Vec<Run>,
) -> Benchmark {
    let observed_success = runs.iter().all(|run| {
        run.succeeded()
            && (implementation != "izu" || name == "startup" || json_success(&run.stdout))
    });
    let outcome = if observed_success {
        Outcome::Passed
    } else {
        Outcome::Failed {
            reason: "one or more real executable invocations failed".into(),
        }
    };
    let latency = distribution(runs.iter().map(|run| run.elapsed_ns).collect());
    Benchmark {
        name: name.into(),
        implementation: implementation.into(),
        cache_state: cache_state.into(),
        accepted_changes: if observed_success {
            accepted_changes
        } else {
            0
        },
        latency,
        runs,
        outcome,
        file_observations: vec![],
        workflow: None,
        fixture_cleanup: vec![],
    }
}

fn sample_runs(runner: &Runner, args: &[String], cwd: &Path, count: usize) -> io::Result<Vec<Run>> {
    let mut samples = Vec::new();
    for _ in 0..count {
        let run = runner.run(args, cwd, None)?;
        let passed = run.succeeded();
        samples.push(run);
        if !passed {
            break;
        }
    }
    Ok(samples)
}

pub fn benchmark(
    runner: &Runner,
    git: &Runner,
    root: &Path,
    spec: &FixtureSpec,
    samples: usize,
    cooperative_samples: usize,
    compiler: Option<&crate::workflows::PinnedCompiler>,
) -> io::Result<Vec<Benchmark>> {
    if samples == 0 || samples > 1000 {
        return Err(io::Error::other("samples requires 1..1000"));
    }
    let izu_root = root.join("izu");
    let git_root = root.join("git");
    fixture::create(&izu_root, spec)?;
    fixture::create(&git_root, spec)?;
    let mut results = Vec::new();
    let strings = |args: &[&str]| args.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    results.push(benchmark_case(
        "startup",
        "izu",
        "OS cache uncontrolled; first invocation retained",
        0,
        sample_runs(runner, &strings(&["--version"]), &izu_root, samples)?,
    ));
    let init = runner.run(&strings(&["--json", "init", "."]), &izu_root, None)?;
    let initialized = init.succeeded() && json_success(&init.stdout);
    results.push(benchmark_case("init", "izu", "new fixture", 0, vec![init]));
    let init_git = git.run(&strings(&["init", "-q"]), &git_root, None)?;
    if !init_git.succeeded() {
        return Err(io::Error::other("Git fixture init failed"));
    }
    for (key, value) in [
        ("core.preloadindex", "true"),
        ("core.untrackedCache", "true"),
        ("core.fsmonitor", "false"),
        ("user.name", "izu lab fixture"),
        ("user.email", "fixture@example.invalid"),
    ] {
        let run = git.run(&strings(&["config", key, value]), &git_root, None)?;
        if !run.succeeded() {
            return Err(io::Error::other(
                "Git fixture optimization configuration failed",
            ));
        }
    }
    results.push(benchmark_case(
        "startup",
        "git",
        "OS cache uncontrolled; first invocation retained",
        0,
        sample_runs(git, &strings(&["--version"]), &git_root, samples)?,
    ));
    if initialized {
        results.push(benchmark_case(
            "full_capture",
            "izu",
            "first tree capture; OS cache uncontrolled",
            spec.files + 1,
            vec![runner.run(&strings(&["--json", "checkpoint"]), &izu_root, None)?],
        ));
    } else {
        results.push(Benchmark {
            name: "checkpoint_diff".into(),
            implementation: "izu".into(),
            cache_state: "unavailable".into(),
            accepted_changes: 0,
            latency: None,
            runs: vec![],
            outcome: Outcome::Blocked {
                reason: "real CLI init failed".into(),
            },
            file_observations: vec![],
            workflow: None,
            fixture_cleanup: vec![],
        });
    }
    results.push(benchmark_case(
        "full_capture",
        "git",
        "first index capture; OS cache uncontrolled",
        spec.files + 1,
        vec![git.run(&strings(&["add", "-A"]), &git_root, None)?],
    ));
    // Git index/tree capture is compared with izu checkpoints, never Git commit count.
    for (implementation, executable, cwd, command) in [
        ("izu", runner, &izu_root, strings(&["--json", "checkpoint"])),
        ("git", git, &git_root, strings(&["add", "-A"])),
    ] {
        if implementation == "izu" && !initialized {
            continue;
        }
        let runs = sample_runs(executable, &command, cwd, samples)?;
        results.push(benchmark_case(
            "noop_capture",
            implementation,
            "warm repeated fixture",
            0,
            runs,
        ));
        let mut runs = Vec::new();
        for index in 0..samples {
            fs::write(
                cwd.join("src/lab_changed.rs"),
                format!("fn changed() {{ let sequence = {index}; }}\n"),
            )?;
            let run = executable.run(&command, cwd, None)?;
            let passed = run.succeeded();
            runs.push(run);
            if !passed {
                break;
            }
        }
        results.push(benchmark_case(
            "changed_file_capture",
            implementation,
            "warm; one source file changed each sample",
            samples,
            runs,
        ));
        let args = if implementation == "izu" {
            strings(&["--json", "diff"])
        } else {
            strings(&["diff", "--cached", "--stat"])
        };
        results.push(benchmark_case(
            "diff",
            implementation,
            "warm captured fixture; own semantics",
            0,
            sample_runs(executable, &args, cwd, samples)?,
        ));
    }
    results.push(crate::workflows::merge(runner, root, spec, samples)?);
    results.extend(crate::workflows::dependencies(
        runner, root, samples, compiler,
    )?);
    results.extend(crate::cooperative::benchmark(
        runner,
        git,
        root,
        spec,
        cooperative_samples,
    )?);
    Ok(results)
}

/// Recipes extend acceptance without embedding engine internals or guessed IDs.
#[derive(Deserialize)]
pub struct Scenario {
    pub schema_version: u32,
    pub name: String,
    pub steps: Vec<Step>,
    #[serde(default)]
    pub fixture: Option<FixtureSpec>,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Step {
    Write {
        path: PathBuf,
        text: String,
    },
    AssertFile {
        path: PathBuf,
        text: String,
    },
    AssertAbsent {
        path: PathBuf,
    },
    Symlink {
        path: PathBuf,
        target: PathBuf,
    },
    Run {
        args: Vec<String>,
        #[serde(default)]
        cwd: PathBuf,
        #[serde(default)]
        stdin: Option<String>,
        #[serde(default)]
        expected_exit: i32,
        #[serde(default)]
        assertions: BTreeMap<String, Value>,
        #[serde(default)]
        one_of_assertions: BTreeMap<String, Vec<Value>>,
        #[serde(default)]
        captures: BTreeMap<String, String>,
    },
    Parallel {
        commands: Vec<CommandSpec>,
        expected_successes: usize,
        #[serde(default = "default_parallel_limit")]
        max_parallel: usize,
        #[serde(default)]
        expected_failure_code: Option<String>,
    },
}

#[derive(Deserialize)]
pub struct CommandSpec {
    pub args: Vec<String>,
    #[serde(default)]
    pub cwd: PathBuf,
    #[serde(default)]
    pub stdin: Option<String>,
    #[serde(default)]
    pub captures: BTreeMap<String, String>,
}

fn default_parallel_limit() -> usize {
    30
}

fn substitute(input: &str, variables: &BTreeMap<String, String>) -> io::Result<String> {
    let mut result = input.to_owned();
    for (key, value) in variables {
        result = result.replace(&format!("${{{key}}}"), value);
    }
    if result.contains("${") {
        return Err(io::Error::other(format!(
            "unresolved scenario variable: {result}"
        )));
    }
    Ok(result)
}

fn json_observation(json: &Value, pointer: &str) -> Option<Value> {
    if let Some((array, child)) = pointer.split_once("/*/") {
        json.pointer(array)?
            .as_array()?
            .iter()
            .map(|item| item.pointer(&format!("/{child}")).cloned())
            .collect::<Option<Vec<_>>>()
            .map(Value::Array)
    } else {
        json.pointer(pointer).cloned()
    }
}

fn fixture_path(root: &Path, relative: &Path) -> io::Result<PathBuf> {
    if relative
        .components()
        .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
    {
        return Err(io::Error::other(
            "scenario path must stay relative to owned fixture",
        ));
    }
    let joined = root.join(relative);
    let mut parent = joined.as_path();
    while fs::symlink_metadata(parent).is_err() {
        parent = parent
            .parent()
            .ok_or_else(|| io::Error::other("missing fixture parent"))?;
    }
    if !fs::canonicalize(parent)?.starts_with(fs::canonicalize(root)?) {
        return Err(io::Error::other("scenario symlink escapes owned fixture"));
    }
    Ok(joined)
}

pub fn run_scenario(runner: &Runner, root: &Path, scenario: Scenario) -> io::Result<Case> {
    if scenario.schema_version != 1
        || scenario.steps.is_empty()
        || scenario.steps.len() > 1000
        || !scenario
            .steps
            .iter()
            .any(|step| matches!(step, Step::Run { .. } | Step::Parallel { .. }))
    {
        return Err(io::Error::other(
            "scenario schema must be 1 and <=1000 steps",
        ));
    }
    fs::create_dir(root)?;
    if let Some(spec) = &scenario.fixture {
        fixture::create(&root.join("repo"), spec)?;
    }
    let mut vars = BTreeMap::from([("root".into(), root.to_string_lossy().into_owned())]);
    let mut case = Case {
        name: scenario.name,
        outcome: Outcome::Passed,
        runs: Vec::new(),
        file_observations: Vec::new(),
        managed_overlap: None,
    };
    let result: io::Result<()> = (|| {
        for step in scenario.steps {
            match step {
                Step::Write { path, text } => {
                    let path = fixture_path(root, &path)?;
                    if let Some(parent) = path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::write(path, substitute(&text, &vars)?)?;
                }
                Step::AssertFile { path, text } => {
                    let observation = observe_file(
                        fixture_path(root, &path)?,
                        substitute(&text, &vars)?.as_bytes(),
                    );
                    let matched = observation.matched();
                    case.file_observations.push(observation);
                    if !matched {
                        return Err(io::Error::other(format!(
                            "independent file mismatch: {}",
                            path.display()
                        )));
                    }
                }
                Step::AssertAbsent { path } => {
                    match fs::symlink_metadata(fixture_path(root, &path)?) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => (),
                        _ => {
                            return Err(io::Error::other(format!(
                                "expected absent owned path: {}",
                                path.display()
                            )));
                        }
                    }
                }
                Step::Symlink { path, target } => {
                    let path = fixture_path(root, &path)?;
                    let target = fixture_path(root, &target)?;
                    #[cfg(unix)]
                    std::os::unix::fs::symlink(target, path)?;
                    #[cfg(not(unix))]
                    {
                        let _ = (path, target);
                        return Err(io::Error::other("symlink scenario requires Unix"));
                    }
                }
                Step::Run {
                    args,
                    cwd,
                    stdin,
                    expected_exit,
                    assertions,
                    one_of_assertions,
                    captures,
                } => {
                    let args = args
                        .iter()
                        .map(|arg| substitute(arg, &vars))
                        .collect::<io::Result<Vec<_>>>()?;
                    let input = stdin.map(|s| substitute(&s, &vars)).transpose()?;
                    let run = runner.run(
                        &args,
                        &fixture_path(root, &cwd)?,
                        input.as_deref().map(str::as_bytes),
                    )?;
                    let result = (|| {
                        if run.exit_code != Some(expected_exit)
                            || run.timed_out
                            || run.output_truncated
                            || run.output_capture_error.is_some()
                        {
                            return Err(io::Error::other(
                                "unexpected exit, timeout or incomplete output",
                            ));
                        }
                        if !assertions.is_empty()
                            || !one_of_assertions.is_empty()
                            || !captures.is_empty()
                        {
                            let json: Value =
                                serde_json::from_str(&run.stdout).map_err(io::Error::other)?;
                            for (pointer, expected) in assertions {
                                let expected = if let Value::String(s) = expected {
                                    Value::String(substitute(&s, &vars)?)
                                } else {
                                    expected
                                };
                                if json_observation(&json, &pointer) != Some(expected.clone()) {
                                    return Err(io::Error::other(format!(
                                        "JSON assertion mismatch at {pointer}: expected {expected}, observed {:?}",
                                        json.pointer(&pointer)
                                    )));
                                }
                            }
                            for (name, pointer) in captures {
                                let value = json
                                    .pointer(&pointer)
                                    .and_then(Value::as_str)
                                    .ok_or_else(|| {
                                        io::Error::other(format!(
                                            "capture {pointer} is missing/non-string"
                                        ))
                                    })?;
                                vars.insert(name, value.into());
                            }
                            for (pointer, alternatives) in one_of_assertions {
                                let alternatives = alternatives
                                    .into_iter()
                                    .map(|value| {
                                        if let Value::String(s) = value {
                                            Ok(Value::String(substitute(&s, &vars)?))
                                        } else {
                                            Ok(value)
                                        }
                                    })
                                    .collect::<io::Result<Vec<_>>>()?;
                                if !alternatives
                                    .iter()
                                    .any(|value| json.pointer(&pointer) == Some(value))
                                {
                                    return Err(io::Error::other(format!(
                                        "JSON value at {pointer} did not match any explicit alternative"
                                    )));
                                }
                            }
                        }
                        Ok(())
                    })();
                    case.runs.push(run);
                    result?;
                }
                Step::Parallel {
                    commands,
                    expected_successes,
                    max_parallel,
                    expected_failure_code,
                } => {
                    if commands.is_empty()
                        || commands.len() > 30
                        || max_parallel == 0
                        || max_parallel > 30
                    {
                        return Err(io::Error::other("parallel fixture requires 1..30 commands"));
                    }
                    let mut prepared = Vec::new();
                    for command in commands {
                        let cwd = fixture_path(root, &command.cwd)?;
                        let args = command
                            .args
                            .iter()
                            .map(|arg| substitute(arg, &vars))
                            .collect::<io::Result<Vec<_>>>()?;
                        let stdin = command.stdin.map(|s| substitute(&s, &vars)).transpose()?;
                        prepared.push((cwd, args, stdin, command.captures));
                    }
                    let mut results = Vec::new();
                    let mut remaining = prepared.into_iter();
                    loop {
                        let batch = remaining.by_ref().take(max_parallel).collect::<Vec<_>>();
                        if batch.is_empty() {
                            break;
                        }
                        let barrier = std::sync::Arc::new(std::sync::Barrier::new(batch.len()));
                        let jobs = batch
                            .into_iter()
                            .map(|(cwd, args, stdin, captures)| {
                                let mut runner = runner.clone();
                                runner.measure_allocation = false;
                                let barrier = barrier.clone();
                                (
                                    thread::spawn(move || {
                                        barrier.wait();
                                        runner.run(&args, &cwd, stdin.as_deref().map(str::as_bytes))
                                    }),
                                    captures,
                                )
                            })
                            .collect::<Vec<_>>();
                        // Join every owned worker before inspecting responses or returning errors.
                        results.extend(
                            jobs.into_iter()
                                .map(|(job, captures)| (job.join(), captures)),
                        );
                    }
                    let mut successes = 0;
                    let mut failures = Vec::new();
                    for (result, captures) in results {
                        let run = match result {
                            Ok(Ok(run)) => run,
                            other => {
                                failures.push(format!("owned harness worker failed: {other:?}"));
                                continue;
                            }
                        };
                        if run.succeeded() && json_success(&run.stdout) {
                            successes += 1;
                            let json: Value =
                                serde_json::from_str(&run.stdout).map_err(io::Error::other)?;
                            for (name, pointer) in captures {
                                if let Some(value) = json.pointer(&pointer).and_then(Value::as_str)
                                {
                                    vars.insert(name, value.to_owned());
                                } else {
                                    failures.push(format!(
                                        "parallel capture {pointer} missing or non-string"
                                    ));
                                }
                            }
                        } else {
                            let code =
                                serde_json::from_str::<Value>(&run.stdout)
                                    .ok()
                                    .and_then(|value| {
                                        value
                                            .pointer("/outcome/error/code")
                                            .and_then(Value::as_str)
                                            .map(str::to_owned)
                                    });
                            if run.timed_out
                                || run.output_truncated
                                || run.output_capture_error.is_some()
                                || expected_failure_code.is_none()
                                || code != expected_failure_code
                            {
                                failures.push("parallel failure was not the explicitly expected typed rejection".into());
                            }
                        }
                        case.runs.push(run);
                    }
                    if !failures.is_empty() {
                        return Err(io::Error::other(failures.join("; ")));
                    }
                    if successes != expected_successes {
                        return Err(io::Error::other(format!(
                            "expected {expected_successes} successful processes, observed {successes}"
                        )));
                    }
                }
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        case.outcome = Outcome::Failed {
            reason: error.to_string(),
        };
    }
    Ok(case)
}

pub fn default_runner(
    executable: PathBuf,
    timeout_seconds: u64,
    worker_executable: PathBuf,
) -> io::Result<Runner> {
    let worker_sha256 = crate::provenance::file_hash(&worker_executable)?;
    Ok(Runner {
        executable,
        timeout: Duration::from_secs(timeout_seconds),
        output_limit: 2 * 1024 * 1024,
        measure_rss: false,
        measure_allocation: true,
        worker: izu_process::WorkerLauncher {
            executable: worker_executable,
            prefix_args: vec!["__process-worker".into()],
        },
        worker_sha256,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_parent_and_absolute_fixture_paths() {
        let temp = tempfile::tempdir().expect("scratch");
        assert!(fixture_path(temp.path(), Path::new("../outside")).is_err());
        assert!(fixture_path(temp.path(), Path::new("/outside")).is_err());
    }
    #[cfg(unix)]
    #[test]
    fn rejects_existing_external_symlink_before_write() {
        let temp = tempfile::tempdir().expect("scratch");
        let other = tempfile::tempdir().expect("outside");
        std::os::unix::fs::symlink(other.path(), temp.path().join("escape")).expect("link");
        assert!(fixture_path(temp.path(), Path::new("escape/overwritten")).is_err());
        std::os::unix::fs::symlink(other.path().join("absent"), temp.path().join("dangling"))
            .expect("dangling link");
        assert!(fixture_path(temp.path(), Path::new("dangling")).is_err());
    }
}
