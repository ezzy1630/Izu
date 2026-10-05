#![forbid(unsafe_code)]
use clap::{Parser, Subcommand, ValueEnum};
use izu_lab::{
    fixture::FixtureSpec,
    provenance,
    suite::{self, Outcome, Scenario},
};
use serde::Serialize;
use std::{fs, io, path::PathBuf, process::ExitCode};

#[derive(Parser)]
#[command(about = "Evidence from an exact izu executable in owned disposable fixtures")]
struct Args {
    #[arg(long)]
    executable: PathBuf,
    #[arg(long)]
    expected_sha256: String,
    #[arg(long)]
    source_root: PathBuf,
    #[arg(long)]
    output: PathBuf,
    #[arg(long)]
    scratch_dir: PathBuf,
    #[arg(long, default_value_t = 30)]
    timeout_seconds: u64,
    #[arg(long)]
    measure_rss: bool,
    #[command(subcommand)]
    command: Mode,
}

#[derive(Clone, Copy, Serialize, ValueEnum)]
#[serde(rename_all = "snake_case")]
enum AcceptanceScope {
    Full,
    CliWorkflows,
}

#[derive(Subcommand)]
enum Mode {
    Acceptance {
        #[arg(long,value_enum,default_value_t=AcceptanceScope::Full)]
        scope: AcceptanceScope,
        #[arg(long)]
        scenario: Vec<PathBuf>,
    },
    Benchmark {
        #[arg(long, default_value = "/usr/bin/git")]
        git: PathBuf,
        #[arg(long, default_value_t = 7)]
        samples: usize,
        #[arg(long, default_value_t = 3)]
        cooperative_samples: usize,
        #[arg(long, default_value_t = 512)]
        files: usize,
        #[arg(long, default_value_t = 4096)]
        bytes_per_file: usize,
        #[arg(long, default_value_t = 1729)]
        seed: u64,
        #[arg(long, requires = "expected_rustc_sha256")]
        rustc: Option<PathBuf>,
        #[arg(long, requires = "rustc")]
        expected_rustc_sha256: Option<String>,
    },
}

#[derive(Serialize)]
struct Report {
    schema_version: u32,
    provenance: provenance::Provenance,
    git_provenance: Option<provenance::Provenance>,
    fixture: Option<FixtureSpec>,
    timeout_seconds: u64,
    output_limit_bytes: usize,
    scratch_root: PathBuf,
    scratch_cleanup: String,
    scratch_cleanup_evidence: Option<izu_lab::fixture::CleanupObservation>,
    acceptance: Vec<suite::Case>,
    benchmarks: Vec<suite::Benchmark>,
    source_manifest_after: Option<String>,
    source_unchanged: bool,
    worker_unchanged: bool,
    requested_samples: Option<usize>,
    requested_cooperative_samples: Option<usize>,
    acceptance_scope: Option<AcceptanceScope>,
    required_cases: Vec<String>,
    excluded_scope_cases: Vec<String>,
    scope_complete: bool,
    full_coverage_complete: bool,
}

fn execute(args: Args) -> io::Result<bool> {
    if args.timeout_seconds == 0 || args.timeout_seconds > 300 {
        return Err(io::Error::other("timeout requires 1..300 seconds"));
    }
    let executable = fs::canonicalize(&args.executable)?;
    provenance::verify_artifact(&executable, &args.expected_sha256)?;
    let compiler = match &args.command {
        Mode::Benchmark {
            rustc,
            expected_rustc_sha256,
            ..
        } => izu_lab::workflows::PinnedCompiler::selected(
            rustc.as_deref(),
            expected_rustc_sha256.as_deref(),
        )?,
        Mode::Acceptance { .. } => None,
    };
    let scratch_parent = izu_lab::runner::validate_storage(&args.scratch_dir, &args.output)?;
    let scratch = tempfile::Builder::new()
        .prefix("izu-lab-")
        .tempdir_in(scratch_parent)?;
    let worker = std::env::current_exe()?;
    let mut runner = suite::default_runner(executable, args.timeout_seconds, worker.clone())?;
    runner.measure_rss = args.measure_rss;
    let mut report = Report {
        schema_version: 1,
        provenance: provenance::capture(&runner.executable, &args.source_root, scratch.path())?,
        git_provenance: None,
        fixture: None,
        timeout_seconds: args.timeout_seconds,
        output_limit_bytes: runner.output_limit,
        scratch_root: scratch.path().to_owned(),
        scratch_cleanup: "pending".into(),
        scratch_cleanup_evidence: None,
        acceptance: vec![],
        benchmarks: vec![],
        source_manifest_after: None,
        source_unchanged: false,
        worker_unchanged: false,
        requested_samples: None,
        requested_cooperative_samples: None,
        acceptance_scope: None,
        required_cases: vec![],
        excluded_scope_cases: vec![],
        scope_complete: false,
        full_coverage_complete: false,
    };
    match args.command {
        Mode::Acceptance { scenario, scope } => {
            report.acceptance_scope = Some(scope);
            report.acceptance = suite::basic_acceptance(&runner, &scratch.path().join("basic"))?;
            report
                .acceptance
                .extend(suite::standard_acceptance(&runner, scratch.path())?);
            report.acceptance.push(izu_lab::mcp::acceptance(
                &runner,
                &scratch.path().join("mcp"),
            )?);
            report.acceptance.push(izu_lab::external::bundle(
                &runner,
                &scratch.path().join("bundle"),
            )?);
            report.acceptance.push(izu_lab::external::corruption(
                &runner,
                &scratch.path().join("corruption"),
            )?);
            report.acceptance.push(izu_lab::external::git(
                &runner,
                &scratch.path().join("git-exchange"),
            )?);
            for writers in [2, 30] {
                report.acceptance.push(izu_lab::managed::acceptance(
                    &runner,
                    &scratch.path().join(format!("managed-{writers}")),
                    writers,
                )?);
            }
            let standard_names = report
                .acceptance
                .iter()
                .map(|case| case.name.clone())
                .collect::<std::collections::BTreeSet<_>>();
            for (index, path) in scenario.iter().enumerate() {
                let bytes = fs::read(path)?;
                if bytes.len() > 2 * 1024 * 1024 {
                    return Err(io::Error::other("scenario exceeds 2 MiB"));
                }
                let scenario: Scenario =
                    serde_json::from_slice(&bytes).map_err(io::Error::other)?;
                report.acceptance.push(suite::run_scenario(
                    &runner,
                    &scratch.path().join(format!("scenario-{index}")),
                    scenario,
                )?);
            }
            report.required_cases = report
                .acceptance
                .iter()
                .map(|case| case.name.clone())
                .collect();
            for blocked in suite::blocked_coverage() {
                if !standard_names.contains(&blocked.name) {
                    let storage_fault = matches!(
                        blocked.name.as_str(),
                        "crash_restart_last_acknowledged_checkpoint" | "disk_full_failure_retry"
                    );
                    if matches!(scope, AcceptanceScope::CliWorkflows) && storage_fault {
                        report.excluded_scope_cases.push(blocked.name.clone());
                    } else {
                        report.required_cases.push(blocked.name.clone());
                    }
                    report.acceptance.push(blocked);
                }
            }
        }
        Mode::Benchmark {
            git,
            samples,
            cooperative_samples,
            files,
            bytes_per_file,
            seed,
            rustc: _,
            expected_rustc_sha256: _,
        } => {
            report.requested_samples = Some(samples);
            report.requested_cooperative_samples = Some(cooperative_samples);
            let spec = FixtureSpec {
                seed,
                files,
                bytes_per_file,
            };
            spec.validate()?;
            let mut git =
                suite::default_runner(fs::canonicalize(git)?, args.timeout_seconds, worker)?;
            git.measure_rss = args.measure_rss;
            report.git_provenance = Some(provenance::capture(
                &git.executable,
                &args.source_root,
                scratch.path(),
            )?);
            report.benchmarks = suite::benchmark(
                &runner,
                &git,
                scratch.path(),
                &spec,
                samples,
                cooperative_samples,
                compiler.as_ref(),
            )?;
            report.fixture = Some(spec);
        }
    }
    provenance::verify_artifact(&runner.executable, &args.expected_sha256)?;
    if let Some(compiler) = &compiler {
        compiler.verify()?;
    }
    let after = provenance::capture(&runner.executable, &args.source_root, scratch.path())?;
    report.source_unchanged =
        after.source_manifest_sha256 == report.provenance.source_manifest_sha256;
    report.source_manifest_after = Some(after.source_manifest_sha256);
    report.worker_unchanged =
        provenance::file_hash(&runner.worker.executable)? == runner.worker_sha256;
    let identity_unchanged = report.source_unchanged && report.worker_unchanged;
    report.full_coverage_complete = report.acceptance_scope.is_some()
        && identity_unchanged
        && report
            .acceptance
            .iter()
            .all(|case| matches!(case.outcome, Outcome::Passed));
    report.scope_complete = identity_unchanged
        && report
            .acceptance
            .iter()
            .filter(|case| report.required_cases.contains(&case.name))
            .all(|case| matches!(case.outcome, Outcome::Passed))
        && report
            .benchmarks
            .iter()
            .all(|case| matches!(case.outcome, Outcome::Passed));
    let cleanup = izu_lab::fixture::cleanup_owned(scratch);
    if cleanup.removed {
        report.scratch_cleanup = "owned temporary fixture removed".into();
    } else {
        report.scratch_cleanup = format!(
            "failed; owned fixture retained at {}: {}",
            cleanup.path.display(),
            cleanup
                .error
                .as_deref()
                .unwrap_or("unknown cleanup failure")
        );
        report.scope_complete = false;
        report.full_coverage_complete = false;
    }
    report.scratch_cleanup_evidence = Some(cleanup);
    let complete = report.scope_complete;
    let output = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args.output)?;
    serde_json::to_writer_pretty(output, &report).map_err(io::Error::other)?;
    println!(
        "{}: {}",
        if complete {
            if matches!(report.acceptance_scope, Some(AcceptanceScope::CliWorkflows)) {
                "cli-workflows scope complete; full coverage remains incomplete"
            } else {
                "complete"
            }
        } else {
            "requested scope incomplete (failed or required blocked cases)"
        },
        args.output.display()
    );
    Ok(complete)
}

fn main() -> ExitCode {
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("__lab-writer")) {
        let arguments = std::env::args_os().skip(2).collect::<Vec<_>>();
        let result = (|| -> io::Result<()> {
            if arguments.len() != 2 {
                return Err(io::Error::other("writer requires evidence path and index"));
            }
            let index = arguments[1]
                .to_str()
                .ok_or_else(|| io::Error::other("invalid writer index"))?
                .parse()
                .map_err(io::Error::other)?;
            izu_lab::managed::fixture_child(&PathBuf::from(&arguments[0]), index)
        })();
        return match result {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("{error}");
                ExitCode::from(1)
            }
        };
    }

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
            eprintln!("izu-lab: {error}");
            ExitCode::from(1)
        }
    }
}
