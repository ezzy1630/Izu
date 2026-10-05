#![forbid(unsafe_code)]
use clap::Parser;
use izu_lab::{
    cooperative,
    fixture::FixtureSpec,
    provenance,
    suite::{self, Outcome},
};
use std::{fs, io, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(about = "Standalone real local Git worktree baseline; no izu behavior claim")]
struct Args {
    #[arg(long, default_value = "/usr/bin/git")]
    git: PathBuf,
    #[arg(long)]
    source_root: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    scratch_dir: PathBuf,
    #[arg(long, default_value_t = 512)]
    files: usize,
    #[arg(long, default_value_t = 4096)]
    bytes_per_file: usize,
    #[arg(long, default_value_t = 1729)]
    seed: u64,
    #[arg(long, default_value_t = 3)]
    samples: usize,
    #[arg(long)]
    measure_rss: bool,
}

fn execute(args: Args) -> io::Result<bool> {
    let spec = FixtureSpec {
        seed: args.seed,
        files: args.files,
        bytes_per_file: args.bytes_per_file,
    };
    spec.validate()?;
    let parent = izu_lab::runner::validate_storage(&args.scratch_dir, &args.output)?;
    let temp = tempfile::Builder::new()
        .prefix("izu-git-baseline-")
        .tempdir_in(parent)?;
    let mut runner =
        suite::default_runner(fs::canonicalize(&args.git)?, 30, std::env::current_exe()?)?;
    runner.measure_rss = args.measure_rss;
    let identity = provenance::capture(&runner.executable, &args.source_root, temp.path())?;
    let benchmarks = cooperative::git_baseline(&runner, temp.path(), &spec, args.samples)?;
    let after = provenance::capture(&runner.executable, &args.source_root, temp.path())?;
    let source_unchanged = identity.source_manifest_sha256 == after.source_manifest_sha256;
    let artifact_unchanged = identity.executable_sha256 == after.executable_sha256;
    let worker_unchanged =
        provenance::file_hash(&runner.worker.executable)? == runner.worker_sha256;
    let complete = worker_unchanged
        && source_unchanged
        && artifact_unchanged
        && benchmarks
            .iter()
            .all(|benchmark| matches!(benchmark.outcome, Outcome::Passed));
    temp.close()?;
    let output = fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&args.output)?;
    serde_json::to_writer_pretty(output,&serde_json::json!({"schema_version":1,"mode":"git_baseline","provenance":identity,"fixture":spec,"samples":args.samples,"max_parallel_writers":4,"source_manifest_after":after.source_manifest_sha256,"source_unchanged":source_unchanged,"artifact_unchanged":artifact_unchanged,"worker_unchanged":worker_unchanged,"benchmarks":benchmarks,"scratch_cleanup":"owned temporary fixtures removed"})).map_err(io::Error::other)?;
    println!(
        "{}: {}",
        if complete {
            "Git baseline recorded"
        } else {
            "Git baseline failed"
        },
        args.output.display()
    );
    Ok(complete)
}
fn main() -> ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__process-worker")) {
        return match izu_process::worker_main() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(1)
            }
        };
    }
    match execute(Args::parse()) {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(2),
        Err(error) => {
            eprintln!("git-baseline: {error}");
            ExitCode::from(1)
        }
    }
}
