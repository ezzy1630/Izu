//! Durable CLI workflows, with measured phases and independent output readbacks.
use crate::{
    distribution,
    fixture::{self, FixtureSpec},
    provenance,
    runner::{Run, Runner},
    suite::{Benchmark, Outcome, observe_file},
};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    fs, io,
    path::{Path, PathBuf},
};

const AUTHOR: [&str; 4] = [
    "--author-name",
    "izu lab fixture",
    "--author-email",
    "fixture@izu.invalid",
];
const OUTPUT_LIMIT: u64 = 32 * 1024 * 1024;
const BUILD_SCRIPT: &str = r#"set -eu
test ! -e deps/liblab_dependency.rlib
test ! -e build/app
mkdir -p deps build
"$1" --edition=2024 --crate-name lab_dependency --crate-type rlib dependency.rs -o deps/liblab_dependency.rlib
"$1" --edition=2024 --crate-name lab_app main.rs --extern lab_dependency=deps/liblab_dependency.rlib -o build/app
actual="$(build/app)"
test "$actual" = "$2"
printf '%s\n' "$actual"
"#;
const REUSE_SCRIPT: &str = r#"set -eu
test -f deps/liblab_dependency.rlib
test -f build/app
"$1" --edition=2024 --crate-name lab_app main.rs --extern lab_dependency=deps/liblab_dependency.rlib -o build/app
actual="$(build/app)"
test "$actual" = "$2"
printf '%s\n' "$actual"
"#;
const MAIN: &str = "fn main() { println!(\"{}\", lab_dependency::value()); }\n";
const IGNORE: &str = "deps/\nbuild/\n";

/// Explicitly selected executable bytes; no PATH or rustup selection is performed.
pub struct PinnedCompiler {
    executable: PathBuf,
    sha256: String,
}

impl PinnedCompiler {
    pub fn selected(path: Option<&Path>, digest: Option<&str>) -> io::Result<Option<Self>> {
        match (path, digest) {
            (None, None) => Ok(None),
            (Some(path), Some(digest)) => {
                let executable = fs::canonicalize(path)?;
                if !fs::metadata(&executable)?.is_file() || executable.to_str().is_none() {
                    return Err(io::Error::other(
                        "selected rustc must be a UTF-8 regular executable",
                    ));
                }
                provenance::verify_artifact(&executable, digest)?;
                Ok(Some(Self {
                    executable,
                    sha256: digest.into(),
                }))
            }
            _ => Err(io::Error::other(
                "supply --rustc and --expected-rustc-sha256 together",
            )),
        }
    }

    pub fn verify(&self) -> io::Result<()> {
        provenance::verify_artifact(&self.executable, &self.sha256)
    }

    fn argument(&self) -> io::Result<String> {
        self.executable
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| io::Error::other("selected rustc path is not UTF-8"))
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct CompilerEvidence {
    pub executable: PathBuf,
    pub sha256: String,
    pub verbose_version: String,
    pub host: String,
    pub identity: String,
    pub pin_scope: String,
}

#[derive(Debug, Serialize)]
pub struct WorkflowEvidence {
    pub timing_scope: String,
    pub compiler: Option<CompilerEvidence>,
    pub samples: Vec<WorkflowSample>,
}

#[derive(Debug, Serialize)]
pub struct Phase {
    pub name: String,
    pub run_index: usize,
    pub measured: bool,
}

#[derive(Debug, Serialize)]
pub struct ArtifactIdentity {
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
}

#[derive(Default, Debug, Serialize)]
pub struct WorkflowSample {
    pub sample: usize,
    pub phases: Vec<Phase>,
    pub recipe: Option<Value>,
    pub source_identity: Option<String>,
    pub cache_key: Option<String>,
    pub manifest_digest: Option<String>,
    pub binding: Option<String>,
    pub candidate: Option<String>,
    pub result_revision: Option<String>,
    pub starting_environment_receipts: Vec<Value>,
    pub artifacts: Vec<ArtifactIdentity>,
    pub stale_rejections: Vec<Value>,
    pub completed: bool,
}

fn benchmark(name: &str, cache: &str, timing: &str) -> Benchmark {
    Benchmark {
        name: name.into(),
        implementation: "izu".into(),
        cache_state: cache.into(),
        accepted_changes: 0,
        latency: None,
        runs: vec![],
        outcome: Outcome::Passed,
        file_observations: vec![],
        workflow: Some(WorkflowEvidence {
            timing_scope: timing.into(),
            compiler: None,
            samples: vec![],
        }),
        fixture_cleanup: vec![],
    }
}

fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}
fn authored(mut arguments: Vec<String>) -> Vec<String> {
    arguments.extend(AUTHOR.map(str::to_owned));
    arguments
}
fn string(value: &Value, pointer: &str) -> io::Result<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("missing string at {pointer}")))
}
fn require(value: &Value, pointer: &str, expected: Value) -> io::Result<()> {
    if value.pointer(pointer) == Some(&expected) {
        Ok(())
    } else {
        Err(io::Error::other(format!(
            "workflow assertion mismatch at {pointer}: expected {expected}, observed {:?}",
            value.pointer(pointer)
        )))
    }
}
fn data(value: &Value) -> io::Result<&Value> {
    value
        .pointer("/outcome/result/data")
        .ok_or_else(|| io::Error::other("missing result data"))
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn bounded_file(path: &Path) -> io::Result<Vec<u8>> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > OUTPUT_LIMIT {
        return Err(io::Error::other(
            "compiled output must be a nonempty regular file at most 32 MiB",
        ));
    }
    fs::read(path)
}

struct Sample<'a> {
    runner: &'a Runner,
    root: &'a Path,
    row: &'a mut Benchmark,
    evidence: WorkflowSample,
}
impl<'a> Sample<'a> {
    fn new(runner: &'a Runner, root: &'a Path, row: &'a mut Benchmark, index: usize) -> Self {
        Self {
            runner,
            root,
            row,
            evidence: WorkflowSample {
                sample: index + 1,
                ..WorkflowSample::default()
            },
        }
    }
    fn record(&mut self, phase: &str, measured: bool, run: Run) -> io::Result<Value> {
        let successful = run.succeeded();
        let parsed = serde_json::from_str::<Value>(&run.stdout);
        let run_index = self.row.runs.len();
        self.row.runs.push(run);
        self.evidence.phases.push(Phase {
            name: phase.into(),
            run_index,
            measured,
        });
        let value = parsed.map_err(io::Error::other)?;
        require(&value, "/schema_version", json!(1))?;
        require(&value, "/build/product", json!("izu"))?;
        if !successful {
            return Err(io::Error::other(format!(
                "{phase} invocation failed; exact command/output retained"
            )));
        }
        require(&value, "/outcome/kind", json!("ok"))?;
        Ok(value)
    }
    fn json(
        &mut self,
        phase: &str,
        measured: bool,
        mut arguments: Vec<String>,
    ) -> io::Result<Value> {
        arguments.insert(0, "--json".into());
        let run = self.runner.run(&arguments, self.root, None)?;
        self.record(phase, measured, run)
    }
    fn repo(
        &mut self,
        phase: &str,
        measured: bool,
        mut arguments: Vec<String>,
    ) -> io::Result<Value> {
        let mut prefixed = args(&["--repo", "repo"]);
        prefixed.append(&mut arguments);
        self.json(phase, measured, prefixed)
    }
    fn private(
        &mut self,
        phase: &str,
        measured: bool,
        workspace: &str,
        mut arguments: Vec<String>,
    ) -> io::Result<Value> {
        let mut prefixed = args(&["--workspace", workspace]);
        prefixed.append(&mut arguments);
        self.repo(phase, measured, prefixed)
    }
    fn rejection(
        &mut self,
        phase: &str,
        arguments: Vec<String>,
        code: &str,
        exit: i32,
    ) -> io::Result<()> {
        let mut prefixed = args(&["--json", "--repo", "repo"]);
        prefixed.extend(arguments);
        let run = self.runner.run(&prefixed, self.root, None)?;
        let complete = run.exit_code == Some(exit)
            && !run.timed_out
            && !run.output_truncated
            && run.output_capture_error.is_none()
            && run.command_elapsed_ns.is_some();
        let parsed = serde_json::from_str::<Value>(&run.stdout);
        let run_index = self.row.runs.len();
        self.row.runs.push(run);
        self.evidence.phases.push(Phase {
            name: phase.into(),
            run_index,
            measured: false,
        });
        let value = parsed.map_err(io::Error::other)?;
        require(&value, "/schema_version", json!(1))?;
        require(&value, "/build/product", json!("izu"))?;
        require(&value, "/outcome/kind", json!("error"))?;
        require(&value, "/outcome/error/code", json!(code))?;
        if !complete {
            return Err(io::Error::other(
                "expected rejection had wrong exit or incomplete process/output evidence",
            ));
        }
        self.evidence.stale_rejections.push(value);
        Ok(())
    }
    fn observe(&mut self, path: PathBuf, expected: &[u8]) -> io::Result<()> {
        let observation = observe_file(path, expected);
        let matched = observation.matched();
        self.row.file_observations.push(observation);
        if matched {
            Ok(())
        } else {
            Err(io::Error::other("independent workflow byte/hash mismatch"))
        }
    }
    fn artifact(&mut self, path: PathBuf) -> io::Result<Vec<u8>> {
        let bytes = bounded_file(&path)?;
        self.evidence.artifacts.push(ArtifactIdentity {
            path,
            sha256: hash(&bytes),
            bytes: bytes.len() as u64,
        });
        Ok(bytes)
    }
    fn finish(mut self, mut result: io::Result<()>, accepted: usize) -> bool {
        let measured = self
            .evidence
            .phases
            .iter()
            .filter(|phase| phase.measured)
            .map(|phase| self.row.runs[phase.run_index].elapsed_ns)
            .try_fold(0u64, u64::checked_add);
        if measured.is_none() {
            result = Err(io::Error::other(
                "workflow measured phase duration overflow",
            ));
        }
        self.evidence.completed = result.is_ok();
        if let Err(error) = result {
            self.row.outcome = Outcome::Failed {
                reason: error.to_string(),
            };
        } else {
            self.row.accepted_changes += accepted;
        }
        let timings = self
            .row
            .latency
            .take()
            .map(|d| d.samples_ns)
            .unwrap_or_default();
        let mut timings = timings;
        if let Some(elapsed) =
            measured.filter(|_| self.evidence.phases.iter().any(|phase| phase.measured))
        {
            timings.push(elapsed);
        }
        self.row.latency = distribution(timings);
        let passed = self.evidence.completed;
        if let Some(workflow) = self.row.workflow.as_mut() {
            workflow.samples.push(self.evidence);
        }
        passed
    }
}

fn start_private(sample: &mut Sample<'_>, name: &str, from: &str) -> io::Result<(String, PathBuf)> {
    let path = sample.root.join(name);
    let result = sample.repo(
        "private_workspace_setup",
        false,
        vec![
            "change".into(),
            "start".into(),
            "--name".into(),
            name.into(),
            "--path".into(),
            path.to_string_lossy().into_owned(),
            "--from".into(),
            from.into(),
        ],
    )?;
    let workspace = string(data(&result)?, "/id")?;
    let actual = PathBuf::from(string(data(&result)?, "/record/root")?);
    if fs::canonicalize(&actual)? != fs::canonicalize(&path)? {
        return Err(io::Error::other(
            "private workspace receipt names another directory",
        ));
    }
    Ok((workspace, path))
}
fn commit(sample: &mut Sample<'_>, workspace: Option<&str>, message: &str) -> io::Result<String> {
    let arguments = authored(args(&["commit", "-m", message]));
    let value = match workspace {
        Some(workspace) => sample.private("authored_commit", false, workspace, arguments)?,
        None => sample.repo("baseline_commit", false, arguments)?,
    };
    string(data(&value)?, "/revision")
}
fn deadline(runner: &Runner) -> String {
    runner.timeout.as_secs().to_string()
}
fn candidate(
    sample: &mut Sample<'_>,
    revision: &str,
    expected: &str,
    binding: Option<&str>,
    argv: Vec<String>,
    measured: bool,
) -> io::Result<(String, String, String)> {
    let mut arguments = authored(args(&[
        "candidate",
        revision,
        "--target",
        "review",
        "--expect",
        expected,
        "--check",
        "workflow",
    ]));
    if let Some(binding) = binding {
        arguments.extend(args(&["--environment", binding]));
    }
    arguments.push("--".into());
    arguments.extend(argv);
    let value = sample.repo("candidate_preparation", measured, arguments)?;
    let value = data(&value)?;
    Ok((
        string(value, "/Ready/candidate")?,
        string(value, "/Ready/revision")?,
        string(value, "/Ready/tree")?,
    ))
}
fn check(sample: &mut Sample<'_>, candidate: &str, measured: bool) -> io::Result<Value> {
    let timeout = deadline(sample.runner);
    let value = sample.repo(
        "exact_candidate_check",
        measured,
        args(&[
            "check",
            candidate,
            "--check",
            "workflow",
            "--timeout-seconds",
            &timeout,
        ]),
    )?;
    let value = data(&value)?.clone();
    require(&value, "/job/state/Finished/outcome/Exit/code", json!(0))?;
    require(
        &value,
        "/job/state/Finished/outcome/Exit/signal",
        Value::Null,
    )?;
    require(
        &value,
        "/job/request/timeout_ms",
        json!(sample.runner.timeout.as_millis() as u64),
    )?;
    Ok(value)
}
fn land(
    sample: &mut Sample<'_>,
    candidate: &str,
    checked: &Value,
    measured: bool,
) -> io::Result<String> {
    let value = sample.repo("guarded_land", measured, args(&["land", candidate]))?;
    require(data(&value)?, "/candidate", json!(candidate))?;
    require(
        data(&value)?,
        "/evidence",
        json!([string(checked, "/evidence")?]),
    )?;
    string(data(&value)?, "/revision")
}

fn merge_text(left: bool, right: bool) -> String {
    (0..80)
        .map(|line| {
            if line == 10 && left {
                "left independently authored edit\n".into()
            } else if line == 60 && right {
                "right independently authored edit\n".into()
            } else {
                format!("baseline merge line {line:02}\n")
            }
        })
        .collect()
}

pub fn merge(
    runner: &Runner,
    scratch: &Path,
    spec: &FixtureSpec,
    samples: usize,
) -> io::Result<Benchmark> {
    let mut row = benchmark(
        "merge",
        "independent divergent fixtures; OS caches uncontrolled",
        "durable CLI divergent merge candidate preparation, including executable startup and native candidate publication; setup, exact check, guarded land and independent readbacks excluded from latency and retained as phases; not a pure merge algorithm benchmark",
    );
    if spec
        .files
        .checked_mul(spec.bytes_per_file)
        .and_then(|bytes| bytes.checked_mul(4))
        .is_none_or(|bytes| bytes > 128 * 1024 * 1024)
    {
        row.outcome = Outcome::Blocked {
            reason: "merge workspace copies exceed 128 MiB per-case payload safety budget".into(),
        };
        return Ok(row);
    }
    for index in 0..samples {
        let temp = tempfile::Builder::new()
            .prefix("merge-")
            .tempdir_in(scratch)?;
        fixture::create(&temp.path().join("repo"), spec)?;
        fs::write(temp.path().join("repo/merge.txt"), merge_text(false, false))?;
        let mut sample = Sample::new(runner, temp.path(), &mut row, index);
        let result = (|| -> io::Result<()> {
            sample.json("init", false, args(&["init", "repo"]))?;
            let base = commit(&mut sample, None, "divergent merge base")?;
            let (left, left_path) = start_private(&mut sample, "left", &base)?;
            let (right, right_path) = start_private(&mut sample, "right", &base)?;
            fs::write(left_path.join("merge.txt"), merge_text(true, false))?;
            fs::write(right_path.join("merge.txt"), merge_text(false, true))?;
            let left_revision = commit(&mut sample, Some(&left), "left source edit")?;
            let right_revision = commit(&mut sample, Some(&right), "right source edit")?;
            sample.repo(
                "target_setup",
                false,
                args(&["refs", "set", "review", &right_revision, "--expect-absent"]),
            )?;
            let (candidate, revision, tree) = candidate(
                &mut sample,
                &left_revision,
                &right_revision,
                None,
                args(&[
                    "/bin/sh",
                    "-c",
                    "test \"$(sed -n '11p' merge.txt)\" = 'left independently authored edit' && test \"$(sed -n '61p' merge.txt)\" = 'right independently authored edit'",
                ]),
                true,
            )?;
            let shown =
                sample.repo("merge_revision_readback", false, args(&["show", &revision]))?;
            require(
                data(&shown)?,
                "/parents",
                json!([right_revision, left_revision]),
            )?;
            require(data(&shown)?, "/tree", json!(tree))?;
            let checked = check(&mut sample, &candidate, false)?;
            let check_path = PathBuf::from(string(&checked, "/job/request/workspace/cwd")?);
            sample.observe(
                check_path.join("merge.txt"),
                merge_text(true, true).as_bytes(),
            )?;
            let landed = land(&mut sample, &candidate, &checked, false)?;
            if landed != revision {
                return Err(io::Error::other("land changed exact merge result revision"));
            }
            sample.repo("restore_merged_result", false, args(&["restore", &landed]))?;
            sample.observe(
                temp.path().join("repo/merge.txt"),
                merge_text(true, true).as_bytes(),
            )?;
            sample.observe(
                left_path.join("merge.txt"),
                merge_text(true, false).as_bytes(),
            )?;
            sample.observe(
                right_path.join("merge.txt"),
                merge_text(false, true).as_bytes(),
            )?;
            for file in 0..spec.files {
                sample.observe(
                    temp.path().join("repo").join(fixture::file_path(file)),
                    &fixture::file_bytes(spec, file),
                )?;
            }
            sample.observe(temp.path().join("repo/.gitignore"), b"build/\n*.tmp\n")?;
            sample.repo("verify", false, args(&["verify"]))?;
            sample.evidence.candidate = Some(candidate);
            sample.evidence.result_revision = Some(landed);
            sample.evidence.source_identity = Some(tree);
            Ok(())
        })();
        let passed = sample.finish(result, 2);
        let removed = row.record_cleanup(fixture::cleanup_owned(temp));
        if !passed || !removed {
            break;
        }
    }
    Ok(row)
}

fn compiler_probe(
    runner: &Runner,
    scratch: &Path,
    compiler: &PinnedCompiler,
) -> io::Result<(CompilerEvidence, Run)> {
    compiler.verify()?;
    let mut probe = runner.clone();
    probe.executable = compiler.executable.clone();
    let run = probe.run(&args(&["-vV"]), scratch, None)?;
    if !run.succeeded() || !run.stdout.starts_with("rustc ") {
        return Err(io::Error::other(
            "selected pinned compiler -vV probe failed",
        ));
    }
    let host = run
        .stdout
        .lines()
        .find_map(|line| line.strip_prefix("host: "))
        .filter(|host| !host.is_empty())
        .ok_or_else(|| io::Error::other("compiler -vV has no host"))?
        .to_owned();
    let verbose_version = run.stdout.trim().to_owned();
    let identity = hash(
        &serde_json::to_vec(
            &json!({"compiler_sha256":compiler.sha256,"verbose_version":verbose_version}),
        )
        .map_err(io::Error::other)?,
    );
    compiler.verify()?;
    Ok((CompilerEvidence {
        executable: compiler.executable.clone(), sha256: compiler.sha256.clone(),
        verbose_version, host, identity,
        pin_scope: "selected compiler executable bytes and actual -vV; standard library and system linker bytes are not pinned; no registry or network resolution".into(),
    }, run))
}

fn dependency(value: u64) -> String {
    format!("pub const VALUE: u64 = {value};\npub fn value() -> u64 {{ VALUE }}\n")
}
fn lock(value: u64) -> String {
    format!(
        "lab_dependency {value}\nsource-sha256 {}\n",
        hash(dependency(value).as_bytes())
    )
}
fn build_argv(compiler: &PinnedCompiler, value: u64, reuse: bool) -> io::Result<Vec<String>> {
    Ok(vec![
        "/bin/sh".into(),
        "-c".into(),
        if reuse { REUSE_SCRIPT } else { BUILD_SCRIPT }.into(),
        "izu-lab-offline-rust".into(),
        compiler.argument()?,
        value.to_string(),
    ])
}
fn managed_build(
    sample: &mut Sample<'_>,
    workspace: &str,
    compiler: &PinnedCompiler,
    value: u64,
    reuse: bool,
) -> io::Result<()> {
    compiler.verify()?;
    let timeout = deadline(sample.runner);
    let mut arguments = args(&["run", "--timeout-seconds", &timeout, "--"]);
    let argv = build_argv(compiler, value, reuse)?;
    arguments.extend(argv.clone());
    let result = sample.private(
        if reuse {
            "warm_managed_application_build"
        } else {
            "cold_managed_dependency_and_application_build"
        },
        true,
        workspace,
        arguments,
    )?;
    compiler.verify()?;
    let result = data(&result)?;
    require(result, "/job/state/Finished/outcome/Exit/code", json!(0))?;
    require(result, "/job/request/argv", json!(argv))?;
    require(result, "/job/request/workspace/id", json!(workspace))?;
    require(
        result,
        "/job/request/timeout_ms",
        json!(sample.runner.timeout.as_millis() as u64),
    )?;
    require(
        result,
        "/job/output/stdout/bytes",
        json!(format!("{value}\n").into_bytes()),
    )?;
    Ok(())
}

struct Prepared {
    workspace: String,
    path: PathBuf,
    revision: String,
    target_revision: String,
    source: String,
    key: String,
    manifest: String,
    binding: String,
    library: Vec<u8>,
    application: Vec<u8>,
}

fn prepare(
    sample: &mut Sample<'_>,
    compiler: &PinnedCompiler,
    evidence: &CompilerEvidence,
    workspace: String,
    path: PathBuf,
    revision: String,
    value: u64,
) -> io::Result<Prepared> {
    let source = sample.private(
        "source_identity",
        false,
        &workspace,
        args(&["environment", "source-identity"]),
    )?;
    let source = string(data(&source)?, "/source_identity")?;
    let argv = build_argv(compiler, value, false)?;
    let recipe = json!({
        "schema_version":1, "source_identity":source,
        "lockfiles":[{"path":"dependency.lock","digest":hash(lock(value).as_bytes())}],
        "toolchain_identity":evidence.identity,
        "platform":{"os":std::env::consts::OS,"architecture":std::env::consts::ARCH,"abi":evidence.host},
        "recipe_identity":hash(BUILD_SCRIPT.as_bytes()), "trust_domain":"izu-lab-offline-rust",
        "argv":argv, "dependencies":["deps"], "outputs":["build"],
    });
    let recipe_path = sample.root.join(format!("recipe-{value}.json"));
    let recipe_bytes = serde_json::to_vec_pretty(&recipe).map_err(io::Error::other)?;
    fs::write(&recipe_path, &recipe_bytes)?;
    sample.observe(recipe_path.clone(), &recipe_bytes)?;
    let recipe_arg = recipe_path.to_string_lossy();
    let before = sample.repo(
        "absent_cache_readback",
        false,
        args(&[
            "environment",
            "status",
            "--recipe",
            &recipe_arg,
            "--trust-recipe",
        ]),
    )?;
    let missing_key = string(data(&before)?, "/Missing/key")?;
    let imported = sample.repo(
        "immutable_environment_import",
        true,
        args(&[
            "environment",
            "import",
            &path.to_string_lossy(),
            "--recipe",
            &recipe_arg,
            "--trust-recipe",
            "--quiescent",
        ]),
    )?;
    require(data(&imported)?, "/reused", json!(false))?;
    let key = string(data(&imported)?, "/artifact/key")?;
    if key != missing_key {
        return Err(io::Error::other("import changed absent artifact key"));
    }
    let manifest = string(data(&imported)?, "/artifact/manifest_digest")?;
    let ready = sample.repo(
        "verified_cache_readback",
        false,
        args(&[
            "environment",
            "status",
            "--recipe",
            &recipe_arg,
            "--trust-recipe",
        ]),
    )?;
    require(data(&ready)?, "/Ready/key", json!(key))?;
    require(data(&ready)?, "/Ready/manifest_digest", json!(manifest))?;
    let bound = sample.repo(
        "native_environment_binding",
        true,
        args(&["environment", "bind", &recipe_arg, "--trust-recipe"]),
    )?;
    require(data(&bound)?, "/key", json!(key))?;
    require(data(&bound)?, "/manifest_digest", json!(manifest))?;
    require(data(&bound)?, "/source_identity", json!(source))?;
    let binding = string(data(&bound)?, "/object_id")?;
    let library = sample.artifact(path.join("deps/liblab_dependency.rlib"))?;
    let application = sample.artifact(path.join("build/app"))?;
    let cache = sample
        .root
        .join("repo/.izu/environments/v1/artifacts")
        .join(&key);
    sample.observe(cache.join("payload/0/liblab_dependency.rlib"), &library)?;
    sample.observe(cache.join("payload/1/app"), &application)?;
    sample.evidence.recipe = Some(recipe);
    sample.evidence.source_identity = Some(source.clone());
    sample.evidence.cache_key = Some(key.clone());
    sample.evidence.manifest_digest = Some(manifest.clone());
    sample.evidence.binding = Some(binding.clone());
    Ok(Prepared {
        workspace,
        path,
        revision,
        target_revision: String::new(),
        source,
        key,
        manifest,
        binding,
        library,
        application,
    })
}

fn bound_check(
    sample: &mut Sample<'_>,
    compiler: &PinnedCompiler,
    prepared: &Prepared,
    expected: &str,
    value: u64,
) -> io::Result<(String, Value)> {
    let (candidate, revision, tree) = candidate(
        sample,
        &prepared.revision,
        expected,
        Some(&prepared.binding),
        build_argv(compiler, value, true)?,
        true,
    )?;
    if tree != prepared.source {
        return Err(io::Error::other(
            "environment recipe source differs from exact candidate result tree",
        ));
    }
    sample.rejection(
        "unpassed_candidate_refusal",
        args(&["land", &candidate]),
        "missing_check",
        5,
    )?;
    compiler.verify()?;
    let checked = check(sample, &candidate, true)?;
    compiler.verify()?;
    validate_starting_receipt(&checked, prepared, &revision, &tree)?;
    require(
        &checked,
        "/job/request/argv",
        json!(build_argv(compiler, value, true)?),
    )?;
    require(
        &checked,
        "/job/output/stdout/bytes",
        json!(format!("{value}\n").into_bytes()),
    )?;
    require(&checked, "/job/output/stdout/dropped_bytes", json!(0))?;
    require(&checked, "/job/output/stderr/dropped_bytes", json!(0))?;
    let receipt = checked
        .pointer("/job/starting_environment")
        .cloned()
        .ok_or_else(|| io::Error::other("bound check has no starting-file receipt"))?;
    sample.evidence.starting_environment_receipts.push(receipt);
    let check_path = PathBuf::from(string(&checked, "/job/request/workspace/cwd")?);
    sample.observe(
        check_path.join("deps/liblab_dependency.rlib"),
        &prepared.library,
    )?;
    sample.artifact(check_path.join("build/app"))?;
    sample.observe(
        check_path.join("dependency.rs"),
        dependency(value).as_bytes(),
    )?;
    sample.observe(check_path.join("dependency.lock"), lock(value).as_bytes())?;
    sample.observe(check_path.join("main.rs"), MAIN.as_bytes())?;
    sample.evidence.candidate = Some(candidate.clone());
    sample.evidence.result_revision = Some(revision);
    Ok((candidate, checked))
}

fn validate_starting_receipt(
    checked: &Value,
    prepared: &Prepared,
    revision: &str,
    tree: &str,
) -> io::Result<()> {
    require(
        checked,
        "/job/request/environment_binding",
        json!(prepared.binding),
    )?;
    require(checked, "/job/request/workspace/head", json!(revision))?;
    require(checked, "/job/request/workspace/tree", json!(tree))?;
    for (pointer, expected) in [
        ("/job/starting_environment/key", prepared.key.as_str()),
        (
            "/job/starting_environment/manifest_digest",
            prepared.manifest.as_str(),
        ),
        (
            "/job/starting_environment/source_identity",
            prepared.source.as_str(),
        ),
        ("/job/starting_environment/scope", "StartingFileContents"),
    ] {
        require(checked, pointer, json!(expected))?;
    }
    let workspace = string(checked, "/job/request/workspace/id")?;
    require(
        checked,
        "/job/starting_environment/workspace",
        json!(workspace),
    )?;
    require(checked, "/workspace_close/workspace", json!(workspace))?;
    require(
        checked,
        "/job/starting_environment/security_boundary",
        json!(false),
    )?;
    require(
        checked,
        "/job/starting_environment/platform/observed_os",
        json!(std::env::consts::OS),
    )?;
    require(
        checked,
        "/job/starting_environment/platform/observed_architecture",
        json!(std::env::consts::ARCH),
    )?;
    string(checked, "/job/starting_environment/observed_content_digest")?;
    Ok(())
}

fn cold(
    sample: &mut Sample<'_>,
    compiler: &PinnedCompiler,
    evidence: &CompilerEvidence,
) -> io::Result<Prepared> {
    let repository = sample.root.join("repo");
    fs::create_dir(&repository)?;
    for (name, text) in [
        ("dependency.rs", dependency(41)),
        ("dependency.lock", lock(41)),
        ("main.rs", MAIN.into()),
        (".gitignore", IGNORE.into()),
        ("workload.txt", "baseline workload\n".into()),
    ] {
        fs::write(repository.join(name), text)?;
    }
    sample.json("init", false, args(&["init", "repo"]))?;
    let base = commit(sample, None, "offline Rust compiler baseline")?;
    sample.repo(
        "target_setup",
        false,
        args(&["refs", "set", "review", &base, "--expect-absent"]),
    )?;
    let (workspace, path) = start_private(sample, "cold", &base)?;
    fs::write(
        path.join("workload.txt"),
        "prepared offline Rust workload\n",
    )?;
    managed_build(sample, &workspace, compiler, 41, false)?;
    let revision = commit(sample, Some(&workspace), "offline Rust workload")?;
    let mut prepared = prepare(sample, compiler, evidence, workspace, path, revision, 41)?;
    let (candidate, checked) = bound_check(sample, compiler, &prepared, &base, 41)?;
    let landed = land(sample, &candidate, &checked, false)?;
    if sample.evidence.result_revision.as_deref() != Some(landed.as_str()) {
        return Err(io::Error::other("cold check/land changed source revision"));
    }
    prepared.target_revision = landed;
    observe_dependency_inputs(sample, &prepared.path, 41)?;
    observe_dependency_inputs(sample, &repository, 41)?;
    sample.observe(repository.join("workload.txt"), b"baseline workload\n")?;
    Ok(prepared)
}

fn observe_dependency_inputs(sample: &mut Sample<'_>, path: &Path, value: u64) -> io::Result<()> {
    sample.observe(path.join("dependency.rs"), dependency(value).as_bytes())?;
    sample.observe(path.join("dependency.lock"), lock(value).as_bytes())?;
    sample.observe(path.join("main.rs"), MAIN.as_bytes())?;
    sample.observe(path.join(".gitignore"), IGNORE.as_bytes())
}

fn warm(sample: &mut Sample<'_>, compiler: &PinnedCompiler, prepared: &Prepared) -> io::Result<()> {
    let (workspace, path) = start_private(sample, "warm", &prepared.revision)?;
    if path == prepared.path || workspace == prepared.workspace {
        return Err(io::Error::other(
            "warm workflow reused the preparation workspace",
        ));
    }
    let recipe_path = sample.root.join("recipe-41.json");
    let materialized = sample.private(
        "verified_private_materialization",
        true,
        &workspace,
        args(&[
            "environment",
            "materialize",
            "--recipe",
            &recipe_path.to_string_lossy(),
            "--trust-recipe",
        ]),
    )?;
    require(data(&materialized)?, "/key", json!(prepared.key))?;
    require(
        data(&materialized)?,
        "/manifest_digest",
        json!(prepared.manifest),
    )?;
    require(data(&materialized)?, "/workspace", json!(workspace))?;
    require(data(&materialized)?, "/private_file_inodes", json!(true))?;
    sample.observe(path.join("deps/liblab_dependency.rlib"), &prepared.library)?;
    sample.observe(path.join("build/app"), &prepared.application)?;
    managed_build(sample, &workspace, compiler, 41, true)?;
    sample.observe(path.join("deps/liblab_dependency.rlib"), &prepared.library)?;
    sample.artifact(path.join("build/app"))?;
    let (_, checked) = bound_check(sample, compiler, prepared, &prepared.target_revision, 41)?;
    require(&checked, "/job/output/stdout/bytes", json!(b"41\n"))?;
    sample.observe(
        prepared.path.join("deps/liblab_dependency.rlib"),
        &prepared.library,
    )?;
    sample.observe(prepared.path.join("build/app"), &prepared.application)?;
    observe_dependency_inputs(sample, &path, 41)?;
    sample.evidence.recipe =
        Some(serde_json::from_slice(&fs::read(recipe_path)?).map_err(io::Error::other)?);
    sample.evidence.source_identity = Some(prepared.source.clone());
    sample.evidence.cache_key = Some(prepared.key.clone());
    sample.evidence.manifest_digest = Some(prepared.manifest.clone());
    sample.evidence.binding = Some(prepared.binding.clone());
    Ok(())
}

fn changed(
    sample: &mut Sample<'_>,
    compiler: &PinnedCompiler,
    evidence: &CompilerEvidence,
    previous: &Prepared,
) -> io::Result<()> {
    let (source_workspace, source_path) =
        start_private(sample, "changed-inputs", &previous.revision)?;
    fs::write(source_path.join("dependency.rs"), dependency(42))?;
    fs::write(source_path.join("dependency.lock"), lock(42))?;
    let revision = commit(sample, Some(&source_workspace), "changed dependency input")?;
    let (workspace, path) = start_private(sample, "changed-build", &revision)?;
    managed_build(sample, &workspace, compiler, 42, false)?;
    let prepared = prepare(sample, compiler, evidence, workspace, path, revision, 42)?;
    if prepared.key == previous.key
        || prepared.source == previous.source
        || prepared.binding == previous.binding
        || prepared.library == previous.library
        || prepared.application == previous.application
    {
        return Err(io::Error::other(
            "changed dependency reused stale key/source/binding or compiled output",
        ));
    }
    let (stale, _, _) = candidate(
        sample,
        &prepared.revision,
        &previous.target_revision,
        Some(&previous.binding),
        build_argv(compiler, 42, true)?,
        false,
    )?;
    let timeout = deadline(sample.runner);
    sample.rejection(
        "stale_environment_check_refusal",
        args(&[
            "check",
            &stale,
            "--check",
            "workflow",
            "--timeout-seconds",
            &timeout,
        ]),
        "stale_expectation",
        4,
    )?;
    sample.rejection(
        "stale_environment_land_refusal",
        args(&["land", &stale]),
        "missing_check",
        5,
    )?;
    let (candidate, checked) =
        bound_check(sample, compiler, &prepared, &previous.target_revision, 42)?;
    require(&checked, "/job/output/stdout/bytes", json!(b"42\n"))?;
    let landed = land(sample, &candidate, &checked, false)?;
    if sample.evidence.result_revision.as_deref() != Some(landed.as_str()) {
        return Err(io::Error::other(
            "changed check/land changed source revision",
        ));
    }
    observe_dependency_inputs(sample, &prepared.path, 42)?;
    observe_dependency_inputs(sample, &previous.path, 41)?;
    sample.observe(
        previous.path.join("deps/liblab_dependency.rlib"),
        &previous.library,
    )?;
    sample.observe(previous.path.join("build/app"), &previous.application)?;
    let previous_cache = sample
        .root
        .join("repo/.izu/environments/v1/artifacts")
        .join(&previous.key);
    sample.observe(
        previous_cache.join("payload/0/liblab_dependency.rlib"),
        &previous.library,
    )?;
    sample.observe(previous_cache.join("payload/1/app"), &previous.application)?;
    observe_dependency_inputs(sample, &sample.root.join("repo"), 41)?;
    sample.repo("verify", false, args(&["verify"]))?;
    Ok(())
}

pub fn dependencies(
    runner: &Runner,
    scratch: &Path,
    samples: usize,
    compiler: Option<&PinnedCompiler>,
) -> io::Result<Vec<Benchmark>> {
    let mut rows = vec![
        benchmark(
            "cold_dependency_cache",
            "absent dependency/output paths and cache key in each independent fixture; OS caches uncontrolled",
            "sum of durable managed dependency+application build, immutable import, native binding, candidate preparation and environment-bound check CLI timings; setup/readbacks/land excluded",
        ),
        benchmark(
            "warm_dependency_cache",
            "verified immutable artifact reused in a second private workspace; dependency compilation omitted, application rebuilt and executed; OS caches uncontrolled",
            "sum of verified private materialization, durable managed application rebuild, candidate preparation and environment-bound check CLI timings; setup/readbacks excluded",
        ),
        benchmark(
            "changed_dependency_cache",
            "changed dependency source and lockfile; new source/key/binding and compiled outputs required; old artifact retained; OS caches uncontrolled",
            "sum of durable fresh dependency+application build, immutable import, native binding, candidate preparation and environment-bound check CLI timings; input/setup/stale refusal/readbacks/land excluded",
        ),
    ];
    let Some(compiler) = compiler else {
        for row in &mut rows {
            row.outcome = Outcome::Blocked { reason: "real offline build requires paired --rustc and --expected-rustc-sha256; no guessed compiler or mock substituted".into() };
        }
        return Ok(rows);
    };
    let (evidence, probe) = compiler_probe(runner, scratch, compiler)?;
    for row in &mut rows {
        if let Some(workflow) = row.workflow.as_mut() {
            workflow.compiler = Some(evidence.clone());
        }
    }
    rows[0].runs.push(probe);
    for index in 0..samples {
        let temp = tempfile::Builder::new()
            .prefix("dependency-")
            .tempdir_in(scratch)?;
        let mut sample = Sample::new(runner, temp.path(), &mut rows[0], index);
        let prepared = cold(&mut sample, compiler, &evidence);
        let prepared = match prepared {
            Ok(prepared) => {
                sample.finish(Ok(()), 1);
                prepared
            }
            Err(error) => {
                sample.finish(Err(error), 0);
                for row in &mut rows[1..] {
                    row.outcome = Outcome::Blocked {
                        reason: format!(
                            "independent sample {} cold preparation failed; actual failure retained in cold row",
                            index + 1
                        ),
                    };
                }
                let cleanup = fixture::cleanup_owned(temp);
                for row in &mut rows {
                    row.record_cleanup(cleanup.clone());
                }
                break;
            }
        };
        let mut sample = Sample::new(runner, temp.path(), &mut rows[1], index);
        let result = warm(&mut sample, compiler, &prepared);
        let warm_passed = sample.finish(result, 0);
        let mut sample = Sample::new(runner, temp.path(), &mut rows[2], index);
        let result = changed(&mut sample, compiler, &evidence, &prepared);
        let changed_passed = sample.finish(result, 1);
        let cleanup = fixture::cleanup_owned(temp);
        let removed = cleanup.removed;
        for row in &mut rows {
            row.record_cleanup(cleanup.clone());
        }
        if !warm_passed || !changed_passed || !removed {
            break;
        }
    }
    compiler.verify()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn compiler_pair_rejects_incomplete_and_untrusted_bytes() {
        let temp = tempfile::tempdir().expect("owned fixture");
        let compiler = temp.path().join("compiler");
        fs::write(&compiler, b"selected bytes").expect("fixture");
        assert!(PinnedCompiler::selected(Some(&compiler), None).is_err());
        assert!(PinnedCompiler::selected(None, Some(&hash(b"selected bytes"))).is_err());
        assert!(PinnedCompiler::selected(Some(&compiler), Some("not a digest")).is_err());
        assert!(PinnedCompiler::selected(Some(&compiler), Some(&hash(b"other bytes"))).is_err());
        let selected = PinnedCompiler::selected(Some(&compiler), Some(&hash(b"selected bytes")))
            .expect("valid pin")
            .expect("compiler");
        fs::write(&compiler, b"replaced bytes").expect("replacement");
        assert!(selected.verify().is_err());
    }
}
