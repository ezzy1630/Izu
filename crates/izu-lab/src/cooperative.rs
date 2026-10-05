//! Cooperative CLI workflows and independent common-target file readbacks.
use crate::{
    distribution,
    fixture::{self, FixtureSpec},
    runner::{Run, Runner},
    suite::{self, Benchmark, Case, Outcome, Scenario},
};
use serde_json::{Value, json};
use std::{fs, io, path::Path, thread};

fn command(mut args: Vec<String>, captures: Value) -> Value {
    if args.iter().any(|arg| arg == "commit" || arg == "candidate") {
        args.extend(
            [
                "--author-name",
                "izu lab fixture",
                "--author-email",
                "fixture@izu.invalid",
            ]
            .map(str::to_owned),
        );
    }
    args.insert(0, "--json".into());
    json!({"kind":"run","args":args,"assertions":{"/outcome/kind":"ok"},"captures":captures})
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}

fn izu_scenario(spec: &FixtureSpec, writers: usize) -> io::Result<Scenario> {
    let mut steps = vec![
        command(args(&["init", "repo"]), json!({})),
        command(
            args(&[
                "--repo",
                "${root}/repo",
                "commit",
                "-m",
                "synthetic common base",
            ]),
            json!({"base":"/outcome/result/data/revision"}),
        ),
        command(
            args(&[
                "--repo",
                "${root}/repo",
                "refs",
                "set",
                "common",
                "${base}",
                "--expect-absent",
            ]),
            json!({}),
        ),
    ];
    let mut concurrent = Vec::new();
    for index in 0..writers {
        steps.push(command(
            vec![
                "--repo".into(),
                "${root}/repo".into(),
                "change".into(),
                "start".into(),
                "--name".into(),
                format!("writer-{index}"),
                "--path".into(),
                format!("${{root}}/writer-{index}"),
                "--from".into(),
                "${base}".into(),
            ],
            json!({format!("ws{index}"):"/outcome/result/data/id"}),
        ));
        steps.push(json!({"kind":"write","path":format!("writer-{index}/unique-{index:02}.txt"),"text":format!("accepted change {index}\n")}));
        concurrent.push(json!({"args":["--json","--repo","${root}/repo","--workspace",format!("${{ws{index}}}"),"commit","-m",format!("accepted writer {index}"),"--author-name","izu lab fixture","--author-email","fixture@izu.invalid"],"captures":{format!("rev{index}"):"/outcome/result/data/revision"}}));
    }
    steps.push(json!({"kind":"parallel","commands":concurrent,"expected_successes":writers,"max_parallel":4}));
    for index in 0..writers {
        let expected = if index == 0 { "${base}" } else { "${current}" };
        let mut candidate = command(
            vec![
                "--repo".into(),
                "${root}/repo".into(),
                "candidate".into(),
                format!("${{rev{index}}}"),
                "--target".into(),
                "common".into(),
                "--expect".into(),
                expected.into(),
                "--check".into(),
                "candidate-file".into(),
            ],
            json!({"candidate":"/outcome/result/data/Ready/candidate"}),
        );
        candidate["args"]
            .as_array_mut()
            .ok_or_else(|| io::Error::other("generated command shape"))?
            .extend([
                json!("--"),
                json!("/bin/sh"),
                json!("-c"),
                json!("test \"$@\""),
                json!("izu-lab-test"),
                json!("-f"),
                json!(format!("unique-{index:02}.txt")),
            ]);
        steps.push(candidate);
        steps.push(command(
            args(&[
                "--repo",
                "${root}/repo",
                "check",
                "${candidate}",
                "--check",
                "candidate-file",
            ]),
            json!({}),
        ));
        steps.push(command(
            args(&["--repo", "${root}/repo", "land", "${candidate}"]),
            json!({"current":"/outcome/result/data/revision"}),
        ));
    }
    steps.push(command(
        args(&["--repo", "${root}/repo", "restore", "${current}"]),
        json!({}),
    ));
    for index in 0..writers {
        steps.push(json!({"kind":"assert_file","path":format!("repo/unique-{index:02}.txt"),"text":format!("accepted change {index}\n")}));
    }
    steps.push(command(
        args(&["--repo", "${root}/repo", "verify"]),
        json!({}),
    ));
    serde_json::from_value(json!({"schema_version":1,"name":format!("cooperative_writers_{writers}"),"fixture":spec,"steps":steps})).map_err(io::Error::other)
}

fn git_case(runner: &Runner, root: &Path, spec: &FixtureSpec, writers: usize) -> io::Result<Case> {
    let repository = root.join("repo");
    fixture::create(&repository, spec)?;
    let mut case = Case {
        name: format!("git_cooperative_writers_{writers}"),
        outcome: Outcome::Passed,
        runs: vec![],
        file_observations: vec![],
        managed_overlap: None,
    };
    let result = (|| {
        invoke(
            runner,
            &mut case.runs,
            args(&["init", "-q", "--initial-branch=main"]),
            &repository,
        )?;
        for (name, value) in [
            ("user.name", "izu lab fixture"),
            ("user.email", "fixture@izu.invalid"),
            ("core.preloadindex", "true"),
            ("core.untrackedCache", "true"),
            ("core.fsmonitor", "false"),
            ("core.autocrlf", "false"),
        ] {
            invoke(
                runner,
                &mut case.runs,
                args(&["config", name, value]),
                &repository,
            )?;
        }
        invoke(runner, &mut case.runs, args(&["add", "-A"]), &repository)?;
        invoke(
            runner,
            &mut case.runs,
            args(&["commit", "-qm", "synthetic common base"]),
            &repository,
        )?;
        let mut paths = Vec::new();
        for index in 0..writers {
            let path = root.join(format!("writer-{index}"));
            invoke(
                runner,
                &mut case.runs,
                vec![
                    "worktree".into(),
                    "add".into(),
                    "-q".into(),
                    "-b".into(),
                    format!("writer-{index}"),
                    path.to_string_lossy().into_owned(),
                    "HEAD".into(),
                ],
                &repository,
            )?;
            fs::write(
                path.join(format!("unique-{index:02}.txt")),
                format!("accepted change {index}\n"),
            )?;
            paths.push(path);
        }
        let mut writer_runner = runner.clone();
        writer_runner.measure_allocation = false;
        let mut concurrent = Vec::new();
        for (batch_index, batch) in paths.chunks(4).enumerate() {
            let barrier = std::sync::Arc::new(std::sync::Barrier::new(batch.len()));
            concurrent.extend(thread::scope(|scope| {
                let runner = &writer_runner;
                let handles = batch
                    .iter()
                    .enumerate()
                    .map(|(index, path)| {
                        let barrier = barrier.clone();
                        scope.spawn(move || -> io::Result<Vec<Run>> {
                            barrier.wait();
                            let add = runner.run(&args(&["add", "-A"]), path, None)?;
                            if !add.succeeded() {
                                return Ok(vec![add]);
                            }
                            let commit = runner.run(
                                &[
                                    "commit".into(),
                                    "-qm".into(),
                                    format!("accepted writer {}", batch_index * 4 + index),
                                ],
                                path,
                                None,
                            )?;
                            Ok(vec![add, commit])
                        })
                    })
                    .collect::<Vec<_>>();
                handles
                    .into_iter()
                    .map(|handle| {
                        handle
                            .join()
                            .map_err(|_| io::Error::other("Git fixture worker panicked"))
                            .and_then(|result| result)
                    })
                    .collect::<Vec<_>>()
            }));
        }
        for result in concurrent {
            let runs = result?;
            let success = runs.iter().all(Run::succeeded) && runs.len() == 2;
            case.runs.extend(runs);
            if !success {
                return Err(io::Error::other("concurrent Git writer failed"));
            }
        }
        for index in 0..writers {
            let run = runner.run(
                &[
                    "merge".into(),
                    "--no-edit".into(),
                    format!("writer-{index}"),
                ],
                &repository,
                None,
            )?;
            let success = run.succeeded();
            case.runs.push(run);
            if !success {
                return Err(io::Error::other("Git integration failed"));
            }
        }
        Ok(())
    })();
    if let Err(error) = result {
        case.outcome = Outcome::Failed {
            reason: error.to_string(),
        };
    }
    for index in 0..writers {
        case.file_observations.push(suite::observe_file(
            repository.join(format!("unique-{index:02}.txt")),
            format!("accepted change {index}\n").as_bytes(),
        ));
    }
    if case
        .file_observations
        .iter()
        .any(|observation| !observation.matched())
    {
        case.outcome = Outcome::Failed {
            reason: "independent merged Git file readback failed".into(),
        };
    }
    Ok(case)
}

fn invoke(
    runner: &Runner,
    runs: &mut Vec<Run>,
    arguments: Vec<String>,
    cwd: &Path,
) -> io::Result<()> {
    let run = runner.run(&arguments, cwd, None)?;
    let success = run.succeeded();
    runs.push(run);
    if success {
        Ok(())
    } else {
        Err(io::Error::other(
            "Git workflow invocation failed; see captured output",
        ))
    }
}

fn writer_span(runs: &[Run]) -> Option<u64> {
    let selected = runs
        .iter()
        .filter(|run| {
            run.args
                .iter()
                .any(|arg| arg.starts_with("accepted writer "))
                || (run.args == ["add", "-A"]
                    && run
                        .cwd
                        .file_name()
                        .is_some_and(|name| name.to_string_lossy().starts_with("writer-")))
        })
        .collect::<Vec<_>>();
    let start = selected.iter().map(|run| run.started_monotonic_ns).min()?;
    let end = selected
        .iter()
        .map(|run| {
            run.started_monotonic_ns
                .saturating_add(run.launcher_elapsed_ns)
        })
        .max()?;
    Some(end.saturating_sub(start))
}

fn aggregate_samples(samples: Vec<Benchmark>) -> Vec<Benchmark> {
    let mut groups: Vec<Benchmark> = Vec::new();
    for sample in samples {
        if let Some(group) = groups.iter_mut().find(|group| {
            group.name == sample.name && group.implementation == sample.implementation
        }) {
            let mut raw = group
                .latency
                .take()
                .map(|d| d.samples_ns)
                .unwrap_or_default();
            raw.extend(sample.latency.map(|d| d.samples_ns).unwrap_or_default());
            group.latency = distribution(raw);
            group.accepted_changes += sample.accepted_changes;
            group.runs.extend(sample.runs);
            group.file_observations.extend(sample.file_observations);
            group.fixture_cleanup.extend(sample.fixture_cleanup);
            if !matches!(sample.outcome, Outcome::Passed) {
                group.outcome = sample.outcome;
            }
        } else {
            let mut sample = sample;
            sample.cache_state = "independent materialized fixtures; max 4 concurrent writers; whole writer workflow wall span; OS caches uncontrolled".into();
            groups.push(sample);
        }
    }
    groups
}

pub fn benchmark(
    runner: &Runner,
    git: &Runner,
    scratch: &Path,
    spec: &FixtureSpec,
    samples: usize,
) -> io::Result<Vec<Benchmark>> {
    if !(1..=100).contains(&samples) {
        return Err(io::Error::other("cooperative samples requires 1..100"));
    }
    let mut benchmarks = Vec::new();
    for writers in [1, 8, 30] {
        if spec
            .files
            .checked_mul(spec.bytes_per_file)
            .and_then(|size| size.checked_mul(writers + 2))
            .is_none_or(|size| size > 128 * 1024 * 1024)
        {
            benchmarks.push(Benchmark {
                name: format!("cooperative_writers_{writers}"),
                implementation: "izu_and_git".into(),
                cache_state: "unavailable".into(),
                accepted_changes: 0,
                latency: None,
                runs: vec![],
                outcome: Outcome::Blocked {
                    reason: "workspace copies exceed 128 MiB per-case payload safety budget".into(),
                },
                file_observations: vec![],
                workflow: None,
                fixture_cleanup: vec![],
            });
            continue;
        }
        for implementation in ["izu", "git"] {
            for sample in 0..samples {
                let temp = tempfile::Builder::new()
                    .prefix("cooperative-")
                    .tempdir_in(scratch)?;
                let case_root = temp.path().join("case");
                let mut case = if implementation == "izu" {
                    suite::run_scenario(runner, &case_root, izu_scenario(spec, writers)?)?
                } else {
                    git_case(git, &case_root, spec, writers)?
                };
                for index in 0..spec.files {
                    case.file_observations.push(suite::observe_file(
                        case_root.join("repo").join(fixture::file_path(index)),
                        &fixture::file_bytes(spec, index),
                    ));
                }
                case.file_observations.push(suite::observe_file(
                    case_root.join("repo/.gitignore"),
                    b"build/\n*.tmp\n",
                ));
                if case
                    .file_observations
                    .iter()
                    .any(|observation| !observation.matched())
                {
                    case.outcome = Outcome::Failed {
                        reason: "independent source/binary fixture readback mismatch".into(),
                    };
                }
                let success = matches!(case.outcome, Outcome::Passed);
                let latency = writer_span(&case.runs).and_then(|span| distribution(vec![span]));
                benchmarks.push(Benchmark {
                name: format!("cooperative_writers_{writers}"), implementation: implementation.into(),
                cache_state: format!("independent sample {}; max 4 concurrent writers; whole add/capture+commit span; OS caches uncontrolled",sample+1),
                accepted_changes: if success { writers } else { 0 }, latency, runs: case.runs, outcome: case.outcome, file_observations: case.file_observations, workflow: None, fixture_cleanup: vec![],
            });
                let removed = benchmarks
                    .last_mut()
                    .ok_or_else(|| io::Error::other("missing completed cooperative row"))?
                    .record_cleanup(fixture::cleanup_owned(temp));
                if !success || !removed {
                    break;
                }
            }
        }
    }
    Ok(aggregate_samples(benchmarks))
}

/// A standalone genuine Git baseline can be recorded before izu is buildable.
pub fn git_baseline(
    runner: &Runner,
    scratch: &Path,
    spec: &FixtureSpec,
    samples: usize,
) -> io::Result<Vec<Benchmark>> {
    if !(1..=100).contains(&samples) {
        return Err(io::Error::other("cooperative samples requires 1..100"));
    }
    let mut results = Vec::new();
    for writers in [1, 8, 30] {
        if spec
            .files
            .checked_mul(spec.bytes_per_file)
            .and_then(|size| size.checked_mul(writers + 2))
            .is_none_or(|size| size > 128 * 1024 * 1024)
        {
            return Err(io::Error::other(
                "Git worktree copies exceed 128 MiB per-case payload budget",
            ));
        }
        for sample in 0..samples {
            let temp = tempfile::Builder::new()
                .prefix("git-cooperative-")
                .tempdir_in(scratch)?;
            let case_root = temp.path().join("case");
            let mut case = git_case(runner, &case_root, spec, writers)?;
            for index in 0..spec.files {
                case.file_observations.push(suite::observe_file(
                    case_root.join("repo").join(fixture::file_path(index)),
                    &fixture::file_bytes(spec, index),
                ));
            }
            if case
                .file_observations
                .iter()
                .any(|observation| !observation.matched())
            {
                case.outcome = Outcome::Failed {
                    reason: "independent Git baseline readback mismatch".into(),
                };
            }
            let successful = matches!(case.outcome, Outcome::Passed);
            let latency = writer_span(&case.runs).and_then(|span| distribution(vec![span]));
            results.push(Benchmark {
            name: format!("cooperative_writers_{writers}"),
            implementation: "git".into(),
            cache_state: format!("independent sample {}; max 4 concurrent writers; whole add-A+commit span; OS cache uncontrolled",sample+1),
            accepted_changes: if successful { writers } else { 0 },
            latency,
            runs: case.runs,
            outcome: case.outcome,
            file_observations: case.file_observations,
            workflow: None,
            fixture_cleanup: vec![],
        });
            let removed = results
                .last_mut()
                .ok_or_else(|| io::Error::other("missing completed Git baseline row"))?
                .record_cleanup(fixture::cleanup_owned(temp));
            if !successful || !removed {
                break;
            }
        }
    }
    Ok(aggregate_samples(results))
}
