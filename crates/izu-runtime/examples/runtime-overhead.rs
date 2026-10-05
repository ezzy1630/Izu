//! Actual serial launch cost, including runtime admission and durable completion.
//! All fixtures remain in the explicitly supplied, previously absent directory.
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Instant;

use izu_engine::{Repository, RepositoryOptions};
use izu_model::CancellationToken;
use izu_runtime::{JobState, RunRequest, Runtime, RuntimeConfig, WorkspaceBinding};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let arguments: Vec<_> = std::env::args_os().skip(1).collect();
    if arguments.len() != 3 {
        return Err(
            "usage: runtime-overhead ABSENT_ARTIFACT_DIRECTORY WORKER_EXECUTABLE RUNS".into(),
        );
    }
    let artifacts = PathBuf::from(&arguments[0]);
    let worker = PathBuf::from(&arguments[1]).canonicalize()?;
    let count: usize = arguments[2].to_str().ok_or("runs must be UTF-8")?.parse()?;
    if !artifacts.is_absolute() || artifacts.exists() || count == 0 || count > 100 {
        return Err("absolute absent artifact directory and 1..=100 runs required".into());
    }
    std::fs::create_dir(&artifacts)?;
    let source = artifacts.join("source");
    std::fs::create_dir(&source)?;
    std::fs::write(source.join("original.txt"), "isolated runtime benchmark\n")?;
    let repository = Repository::init(&source, RepositoryOptions::default())?;
    let cancel = CancellationToken::new();
    let initial = repository.workspace(repository.workspace_id(), &cancel)?;
    let workspace = repository.fork_workspace(
        "benchmark".into(),
        artifacts.join("workspace"),
        initial.expected.head,
        &cancel,
    )?;
    let binding =
        WorkspaceBinding::checked(&repository, workspace.id, workspace.expected, &cancel)?;
    let mut direct_ms = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let status = Command::new("/usr/bin/true")
            .current_dir(binding.cwd())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if !status.success() {
            return Err("direct fixture command failed".into());
        }
        direct_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let mut runtime = Runtime::open(
        repository.metadata_path().join("runtime"),
        RuntimeConfig::new(worker.clone()),
    )?;
    let mut runtime_ms = Vec::new();
    for _ in 0..count {
        let start = Instant::now();
        let id = runtime.submit(RunRequest::new(
            binding.clone(),
            vec!["/usr/bin/true".into()],
        ))?;
        let job = runtime.wait(&id, &cancel)?;
        if !matches!(job.state, JobState::Finished { outcome, .. } if outcome.passed()) {
            return Err("managed fixture command did not complete successfully".into());
        }
        runtime_ms.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    let close = runtime.close_workspace(&repository, workspace.id, workspace.expected, &cancel)?;
    if close.checkpoint.tree != workspace.expected.working_tree {
        return Err("benchmark source unexpectedly changed".into());
    }
    let metrics = serde_json::json!({
        "schema": 1,
        "platform": std::env::consts::OS,
        "architecture": std::env::consts::ARCH,
        "debug_build": cfg!(debug_assertions),
        "worker": worker,
        "command": ["/usr/bin/true"],
        "serial_runs": count,
        "direct_ms": direct_ms,
        "runtime_ms": runtime_ms,
        "runtime_mean_ms": runtime_ms.iter().sum::<f64>() / count as f64,
        "direct_mean_ms": direct_ms.iter().sum::<f64>() / count as f64,
        "includes": "submit, source validation, writer intent, worker launch, durable result, group cleanup, registry completion",
        "excludes": "initial source creation/fork, runtime open, final workspace checkpoint/close",
        "environment_attestation": "unknown",
        "retained_workspace": close.retained_path,
        "source_tree": close.checkpoint.tree,
        "close_operation": close.close_operation,
    });
    let bytes = serde_json::to_vec_pretty(&metrics)?;
    std::fs::write(artifacts.join("metrics.json"), &bytes)?;
    println!("{}", String::from_utf8(bytes)?);
    Ok(())
}
