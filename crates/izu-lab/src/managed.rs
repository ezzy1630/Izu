//! Independently launched human CLI controllers and measured child overlap.
use crate::{
    runner::Runner,
    suite::{Case, Outcome, observe_file},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    collections::BTreeSet,
    fs, io,
    path::{Path, PathBuf},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Debug, Serialize, Deserialize)]
pub struct Interval {
    cwd: PathBuf,
    start_ns: u128,
    end_ns: u128,
}
#[derive(Debug, Serialize)]
pub struct ManagedEvidence {
    pub logical_writers: usize,
    pub configured_maximum: usize,
    pub observed_maximum: i32,
    pub intervals: Vec<Interval>,
}

fn now() -> io::Result<u128> {
    Ok(SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos())
}

/// Owned benchmark job. Evidence must be beneath the explicit temporary root.
pub fn fixture_child(evidence: &Path, index: usize) -> io::Result<()> {
    if index >= 30 {
        return Err(io::Error::other("fixture writer index exceeds 29"));
    }
    let temp = fs::canonicalize(
        std::env::var_os("TMPDIR").ok_or_else(|| io::Error::other("TMPDIR required"))?,
    )?;
    let evidence = fs::canonicalize(evidence)?;
    if !evidence.starts_with(temp) {
        return Err(io::Error::other(
            "writer evidence escaped controlled temporary root",
        ));
    }
    let cwd = fs::canonicalize(std::env::current_dir()?)?;
    let start_ns = now()?;
    let text = format!("managed private writer {index}\n");
    let mut file = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(format!("managed-{index:02}.txt"))?;
    use std::io::Write;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    thread::sleep(Duration::from_secs(1));
    let interval = Interval {
        cwd,
        start_ns,
        end_ns: now()?,
    };
    let output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(evidence.join(format!("interval-{index:02}.json")))?;
    serde_json::to_writer(output, &interval).map_err(io::Error::other)
}

pub fn acceptance(runner: &Runner, root: &Path, writers: usize) -> io::Result<Case> {
    if ![2, 30].contains(&writers) {
        return Err(io::Error::other(
            "managed acceptance supports 2 or 30 writers",
        ));
    }
    let repository = root.join("repo");
    let evidence = root.join("intervals");
    fs::create_dir_all(&repository)?;
    fs::create_dir_all(&evidence)?;
    fs::write(repository.join("source.txt"), b"managed root base\n")?;
    let mut case = Case {
        name: format!("managed_independent_cli_{writers}"),
        outcome: Outcome::Passed,
        runs: vec![],
        file_observations: vec![],
        managed_overlap: None,
    };
    let result = (|| -> io::Result<()> {
        let init = runner.run(&["--json".into(), "init".into(), "repo".into()], root, None)?;
        let success = init.succeeded();
        case.runs.push(init);
        if !success {
            return Err(io::Error::other("managed fixture init failed"));
        }
        let commit = runner.run(
            &[
                "--json".into(),
                "--repo".into(),
                repository.to_string_lossy().into_owned(),
                "commit".into(),
                "-m".into(),
                "managed synthetic baseline".into(),
                "--author-name".into(),
                "izu lab fixture".into(),
                "--author-email".into(),
                "fixture@izu.invalid".into(),
            ],
            root,
            None,
        )?;
        let success = commit.succeeded();
        case.runs.push(commit);
        if !success {
            return Err(io::Error::other("managed fixture commit failed"));
        }
        let job_executable = std::env::current_exe()?;
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(writers));
        let jobs = (0..writers)
            .map(|index| {
                let mut runner = runner.clone();
                runner.measure_allocation = false;
                let barrier = barrier.clone();
                let repository = repository.clone();
                let evidence = evidence.clone();
                let job_executable = job_executable.clone();
                let cwd = root.to_owned();
                thread::spawn(move || {
                    barrier.wait();
                    runner.run(
                        &[
                            "--json".into(),
                            "--repo".into(),
                            repository.to_string_lossy().into_owned(),
                            "start".into(),
                            format!("managed-{index}"),
                            "--".into(),
                            job_executable.to_string_lossy().into_owned(),
                            "__lab-writer".into(),
                            evidence.to_string_lossy().into_owned(),
                            index.to_string(),
                        ],
                        &cwd,
                        None,
                    )
                })
            })
            .collect::<Vec<_>>();
        let results = jobs.into_iter().map(|job| job.join()).collect::<Vec<_>>();
        let mut failures = Vec::new();
        let mut roots = BTreeSet::new();
        let mut events = Vec::new();
        let mut intervals = Vec::new();
        for (index, result) in results.into_iter().enumerate() {
            let run = match result {
                Ok(Ok(run)) => run,
                other => {
                    failures.push(format!("controller {index}: {other:?}"));
                    continue;
                }
            };
            let successful = run.succeeded();
            let parsed = serde_json::from_str::<Value>(&run.stdout);
            case.runs.push(run);
            let validated = (|| -> io::Result<()> {
                let value = parsed.map_err(io::Error::other)?;
                if !successful
                    || value.pointer("/outcome/kind").and_then(Value::as_str) != Some("ok")
                    || value
                        .pointer("/outcome/result/data/job/state/Finished/outcome/Exit/code")
                        .and_then(Value::as_i64)
                        != Some(0)
                {
                    return Err(io::Error::other(
                        "managed controller did not acknowledge successful completed job",
                    ));
                }
                let workspace = value
                    .pointer("/outcome/result/data/workspace/record/root")
                    .and_then(Value::as_str)
                    .ok_or_else(|| io::Error::other("missing managed workspace root"))?;
                let binding_cwd = value
                    .pointer("/outcome/result/data/job/request/workspace/cwd")
                    .and_then(Value::as_str)
                    .ok_or_else(|| io::Error::other("missing job cwd binding"))?;
                if fs::canonicalize(binding_cwd)? != fs::canonicalize(workspace)? {
                    return Err(io::Error::other(
                        "job cwd binding differs from registered private workspace",
                    ));
                }
                for field in ["id", "head", "tree"] {
                    let pointer = match field {
                        "id" => "/outcome/result/data/workspace/id",
                        "head" => "/outcome/result/data/before/head",
                        _ => "/outcome/result/data/before/tree",
                    };
                    let expected =
                        value
                            .pointer(pointer)
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                io::Error::other(
                                    "missing exact managed checkpoint/workspace binding",
                                )
                            })?;
                    if value
                        .pointer(&format!(
                            "/outcome/result/data/job/request/workspace/{field}"
                        ))
                        .and_then(Value::as_str)
                        != Some(expected)
                    {
                        return Err(io::Error::other(
                            "managed job executed another workspace/checkpoint binding",
                        ));
                    }
                }
                let before = value
                    .pointer("/outcome/result/data/before/tree")
                    .and_then(Value::as_str)
                    .ok_or_else(|| io::Error::other("missing pre-job checkpoint"))?;
                let after = value
                    .pointer("/outcome/result/data/after/tree")
                    .and_then(Value::as_str)
                    .ok_or_else(|| io::Error::other("missing post-job checkpoint"))?;
                if before == after {
                    return Err(io::Error::other(
                        "completed private write did not produce a changed checkpoint",
                    ));
                }
                let workspace = fs::canonicalize(workspace)?;
                if workspace == repository
                    || !workspace.starts_with(root)
                    || !roots.insert(workspace.clone())
                {
                    return Err(io::Error::other(
                        "managed writers did not receive distinct owned private roots",
                    ));
                }
                let interval: Interval = serde_json::from_slice(&fs::read(
                    evidence.join(format!("interval-{index:02}.json")),
                )?)
                .map_err(io::Error::other)?;
                if interval.cwd != workspace || interval.end_ns <= interval.start_ns {
                    return Err(io::Error::other(
                        "managed child interval or actual cwd mismatched receipt",
                    ));
                }
                case.file_observations.push(observe_file(
                    workspace.join(format!("managed-{index:02}.txt")),
                    format!("managed private writer {index}\n").as_bytes(),
                ));
                for other in 0..writers {
                    if other != index
                        && fs::symlink_metadata(workspace.join(format!("managed-{other:02}.txt")))
                            .is_ok()
                    {
                        return Err(io::Error::other(
                            "private workspace contains another writer's output",
                        ));
                    }
                }
                events.push((interval.start_ns, 1i32));
                events.push((interval.end_ns, -1i32));
                intervals.push(interval);
                Ok(())
            })();
            if let Err(error) = validated {
                failures.push(format!("controller {index}: {error}"));
            }
        }
        events.sort();
        let mut active = 0;
        let mut maximum = 0;
        for (_, delta) in events {
            active += delta;
            maximum = maximum.max(active);
            if !(0..=4).contains(&active) {
                failures.push(format!(
                    "measured global active children exceeded budget: {active}"
                ));
            }
        }
        case.managed_overlap = Some(ManagedEvidence {
            logical_writers: writers,
            configured_maximum: 4,
            observed_maximum: maximum,
            intervals,
        });
        if maximum < 2 {
            failures.push("independent managed children never overlapped".into());
        }
        for index in 0..writers {
            if fs::symlink_metadata(repository.join(format!("managed-{index:02}.txt"))).is_ok() {
                failures.push("managed write escaped into primary checkout".into());
            }
        }
        if failures.is_empty() {
            Ok(())
        } else {
            Err(io::Error::other(failures.join("; ")))
        }
    })();
    case.file_observations.push(observe_file(
        repository.join("source.txt"),
        b"managed root base\n",
    ));
    if let Err(error) = result {
        case.outcome = Outcome::Failed {
            reason: error.to_string(),
        };
    }
    if case.file_observations.iter().any(|file| !file.matched()) {
        case.outcome = Outcome::Failed {
            reason: "managed independent source readback failed".into(),
        };
    }
    Ok(case)
}
