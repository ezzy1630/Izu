//! Cold native archive restore, owned corruption, and real local Git exchange.
use crate::{
    runner::Runner,
    suite::{Case, Outcome, observe_file},
};
use serde_json::Value;
use std::{fs, io, path::Path};

fn case(name: &str) -> Case {
    Case {
        name: name.into(),
        outcome: Outcome::Passed,
        runs: vec![],
        file_observations: vec![],
        managed_overlap: None,
    }
}
fn args(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|part| (*part).to_owned()).collect()
}
fn invoke(
    runner: &Runner,
    case: &mut Case,
    arguments: Vec<String>,
    cwd: &Path,
) -> io::Result<String> {
    let run = runner.run(&arguments, cwd, None)?;
    let successful = run.succeeded();
    let stdout = run.stdout.clone();
    case.runs.push(run);
    if !successful {
        return Err(io::Error::other(
            "external workflow failed; exact command/output recorded",
        ));
    }
    Ok(stdout)
}
fn json(
    runner: &Runner,
    case: &mut Case,
    mut arguments: Vec<String>,
    cwd: &Path,
) -> io::Result<Value> {
    arguments.insert(0, "--json".into());
    let value: Value =
        serde_json::from_str(&invoke(runner, case, arguments, cwd)?).map_err(io::Error::other)?;
    if value.pointer("/outcome/kind").and_then(Value::as_str) != Some("ok") {
        return Err(io::Error::other("external workflow returned typed failure"));
    }
    Ok(value)
}
fn identifier(value: &Value, pointer: &str) -> io::Result<String> {
    value
        .pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other(format!("missing string identifier {pointer}")))
}
fn finish(case: &mut Case, result: io::Result<()>) {
    if let Err(error) = result {
        case.outcome = Outcome::Failed {
            reason: error.to_string(),
        };
    }
    if case
        .file_observations
        .iter()
        .any(|observation| !observation.matched())
    {
        case.outcome = Outcome::Failed {
            reason: "independent external workflow bytes mismatch".into(),
        };
    }
}

pub fn bundle(runner: &Runner, root: &Path) -> io::Result<Case> {
    fs::create_dir_all(root.join("repo"))?;
    fs::write(root.join("repo/source.txt"), "committed archive base\n")?;
    let mut case = case("native_bundle_cold_restore");
    let result = (|| -> io::Result<()> {
        json(runner, &mut case, args(&["init", "repo"]), root)?;
        let commit = json(
            runner,
            &mut case,
            args(&[
                "--repo",
                "repo",
                "commit",
                "-m",
                "archive base",
                "--author-name",
                "izu lab fixture",
                "--author-email",
                "fixture@izu.invalid",
            ]),
            root,
        )?;
        let revision = identifier(&commit, "/outcome/result/data/revision")?;
        fs::write(
            root.join("repo/source.txt"),
            "checkpoint newer than commit\n",
        )?;
        fs::write(
            root.join("repo/uncommitted.txt"),
            "retained uncommitted source\n",
        )?;
        let checkpoint = json(
            runner,
            &mut case,
            args(&["--repo", "repo", "checkpoint"]),
            root,
        )?;
        let tree = identifier(&checkpoint, "/outcome/result/data/tree")?;
        let history = json(runner, &mut case, args(&["--repo", "repo", "log"]), root)?;
        json(
            runner,
            &mut case,
            args(&["--repo", "repo", "bundle", "create", "cold.izubundle"]),
            root,
        )?;
        json(
            runner,
            &mut case,
            args(&["bundle", "verify", "cold.izubundle"]),
            root,
        )?;
        // Only this newly created disposable repository is removed. The artifact
        // is the sole source for restoring acknowledged uncommitted bytes.
        fs::remove_dir_all(root.join("repo"))?;
        json(
            runner,
            &mut case,
            args(&["bundle", "restore", "cold.izubundle", "restored"]),
            root,
        )?;
        let restored = json(
            runner,
            &mut case,
            args(&["--repo", "restored", "inspect"]),
            root,
        )?;
        if identifier(&restored, "/outcome/result/data/expected/head")? != revision
            || identifier(&restored, "/outcome/result/data/expected/working_tree")? != tree
        {
            return Err(io::Error::other(
                "cold restore changed archived revision/checkpoint IDs",
            ));
        }
        let restored_history = json(
            runner,
            &mut case,
            args(&["--repo", "restored", "log"]),
            root,
        )?;
        if history.pointer("/outcome/result/data")
            != restored_history.pointer("/outcome/result/data")
        {
            return Err(io::Error::other(
                "cold restore changed immutable revision history",
            ));
        }
        if fs::symlink_metadata(root.join("repo")).is_ok() {
            return Err(io::Error::other(
                "cold restore recreated historical absolute workspace",
            ));
        }
        json(
            runner,
            &mut case,
            args(&["--repo", "restored", "verify"]),
            root,
        )?;
        Ok(())
    })();
    case.file_observations.push(observe_file(
        root.join("restored/source.txt"),
        b"checkpoint newer than commit\n",
    ));
    case.file_observations.push(observe_file(
        root.join("restored/uncommitted.txt"),
        b"retained uncommitted source\n",
    ));
    finish(&mut case, result);
    Ok(case)
}

pub fn corruption(runner: &Runner, root: &Path) -> io::Result<Case> {
    fs::create_dir_all(root.join("repo"))?;
    fs::write(
        root.join("repo/sentinel.txt"),
        "corruption preserves source\n",
    )?;
    let mut case = case("corruption_unknown_format");
    let result = (|| -> io::Result<()> {
        json(runner, &mut case, args(&["init", "repo"]), root)?;
        json(
            runner,
            &mut case,
            args(&["--repo", "repo", "checkpoint"]),
            root,
        )?;
        let format = root.join("repo/.izu/FORMAT");
        let original = fs::read(&format)?;
        fs::write(&format, b"IZU-STORE 999999\n")?;
        rejection(
            runner,
            &mut case,
            args(&["--json", "--repo", "repo", "status"]),
            root,
            "unsupported_feature",
            3,
        )?;
        fs::write(format, original)?;
        let head = root.join("repo/.izu/HEAD");
        let original = fs::read(&head)?;
        if original.is_empty() {
            return Err(io::Error::other("native HEAD has no bytes to corrupt"));
        }
        let mut changed = original.clone();
        let index = changed.len() - 1;
        changed[index] ^= 0x80;
        fs::write(&head, changed)?;
        rejection(
            runner,
            &mut case,
            args(&["--json", "--repo", "repo", "status"]),
            root,
            "corrupt_data",
            7,
        )?;
        fs::write(head, original)?;
        // Damage a referenced immutable object independently of the control marker.
        let head = fs::read(root.join("repo/.izu/HEAD"))?;
        let operation = head
            .get(8..40)
            .ok_or_else(|| io::Error::other("native HEAD has no operation identity"))?
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let object = root
            .join("repo/.izu/objects")
            .join(&operation[..2])
            .join(&operation[2..]);
        let original = fs::read(&object)?;
        let mut changed = original.clone();
        let last = changed
            .last_mut()
            .ok_or_else(|| io::Error::other("native object has no frame bytes to corrupt"))?;
        *last ^= 0x80;
        fs::write(&object, changed)?;
        rejection(
            runner,
            &mut case,
            args(&["--json", "--repo", "repo", "verify"]),
            root,
            "corrupt_data",
            7,
        )?;
        fs::write(object, original)?;
        json(runner, &mut case, args(&["--repo", "repo", "verify"]), root)?;
        Ok(())
    })();
    case.file_observations.push(observe_file(
        root.join("repo/sentinel.txt"),
        b"corruption preserves source\n",
    ));
    finish(&mut case, result);
    Ok(case)
}
fn rejection(
    runner: &Runner,
    case: &mut Case,
    arguments: Vec<String>,
    cwd: &Path,
    expected_code: &str,
    expected_exit: i32,
) -> io::Result<()> {
    let run = runner.run(&arguments, cwd, None)?;
    let value = serde_json::from_str::<Value>(&run.stdout).map_err(io::Error::other);
    let complete = !run.timed_out
        && !run.output_truncated
        && run.output_capture_error.is_none()
        && run.command_elapsed_ns.is_some()
        && run.exit_code == Some(expected_exit);
    case.runs.push(run);
    let value = value?;
    if !complete
        || value.pointer("/outcome/kind").and_then(Value::as_str) != Some("error")
        || value.pointer("/outcome/error/code").and_then(Value::as_str) != Some(expected_code)
    {
        return Err(io::Error::other(format!(
            "native state was not explicitly rejected as {expected_code} with exit {expected_exit}"
        )));
    }
    Ok(())
}

pub fn git(runner: &Runner, root: &Path) -> io::Result<Case> {
    fs::create_dir_all(root.join("original"))?;
    fs::create_dir(root.join("native"))?;
    let mut git = runner.clone();
    git.executable = fs::canonicalize("/usr/bin/git")?;
    let mut case = case("git_original_izu_localbare_clone");
    let result = (|| -> io::Result<()> {
        invoke(
            &git,
            &mut case,
            args(&[
                "init",
                "-q",
                "--initial-branch=main",
                "--object-format=sha1",
                "original",
            ]),
            root,
        )?;
        invoke(
            &git,
            &mut case,
            args(&[
                "-C",
                "original",
                "config",
                "user.name",
                "Original Fixture Author",
            ]),
            root,
        )?;
        invoke(
            &git,
            &mut case,
            args(&[
                "-C",
                "original",
                "config",
                "user.email",
                "original@izu.invalid",
            ]),
            root,
        )?;
        fs::write(root.join("original/source.txt"), "original Git source\n")?;
        invoke(
            &git,
            &mut case,
            args(&["-C", "original", "add", "-A"]),
            root,
        )?;
        invoke(
            &git,
            &mut case,
            args(&["-C", "original", "commit", "-qm", "original commit"]),
            root,
        )?;
        let original = invoke(
            &git,
            &mut case,
            args(&["-C", "original", "rev-parse", "HEAD"]),
            root,
        )?
        .trim()
        .to_owned();
        json(runner, &mut case, args(&["init", "native"]), root)?;
        json(
            runner,
            &mut case,
            args(&[
                "--repo",
                "native",
                "git",
                "import",
                "original",
                "--native-ref",
                "imported",
                "--git-ref",
                "refs/heads/main",
                "--expect-absent",
            ]),
            root,
        )?;
        let refs = json(
            runner,
            &mut case,
            args(&["--repo", "native", "refs", "list"]),
            root,
        )?;
        let revision = identifier(&refs, "/outcome/result/data/imported")?;
        let writer = json(
            runner,
            &mut case,
            vec![
                "--repo".into(),
                "native".into(),
                "change".into(),
                "start".into(),
                "--name".into(),
                "git-writer".into(),
                "--path".into(),
                root.join("writer").to_string_lossy().into_owned(),
                "--from".into(),
                revision,
            ],
            root,
        )?;
        let workspace = identifier(&writer, "/outcome/result/data/id")?;
        fs::write(root.join("writer/source.txt"), "native authored edit\n")?;
        fs::write(root.join("writer/unique.txt"), "native added source\n")?;
        let commit = json(
            runner,
            &mut case,
            vec![
                "--repo".into(),
                "native".into(),
                "--workspace".into(),
                workspace,
                "commit".into(),
                "-m".into(),
                "native edit".into(),
                "--author-name".into(),
                "Native Fixture Author".into(),
                "--author-email".into(),
                "native@izu.invalid".into(),
            ],
            root,
        )?;
        let revision = identifier(&commit, "/outcome/result/data/revision")?;
        json(
            runner,
            &mut case,
            vec![
                "--repo".into(),
                "native".into(),
                "refs".into(),
                "set".into(),
                "published".into(),
                revision,
                "--expect-absent".into(),
            ],
            root,
        )?;
        invoke(
            &git,
            &mut case,
            args(&[
                "init",
                "--bare",
                "-q",
                "--initial-branch=main",
                "--object-format=sha1",
                "remote.git",
            ]),
            root,
        )?;
        json(
            runner,
            &mut case,
            args(&[
                "--repo",
                "native",
                "git",
                "push",
                "remote.git",
                "--native-ref",
                "published",
                "--git-ref",
                "refs/heads/main",
                "--expect-absent",
                "--committer-name",
                "Export Fixture Committer",
                "--committer-email",
                "export@izu.invalid",
                "--timestamp",
                "1700000000",
            ]),
            root,
        )?;
        invoke(
            &git,
            &mut case,
            args(&["clone", "-q", "remote.git", "clone"]),
            root,
        )?;
        if invoke(
            &git,
            &mut case,
            args(&["-C", "clone", "rev-list", "--count", "HEAD"]),
            root,
        )?
        .trim()
            != "2"
            || invoke(
                &git,
                &mut case,
                args(&["-C", "clone", "rev-parse", "HEAD^"]),
                root,
            )?
            .trim()
                != original
        {
            return Err(io::Error::other(
                "ordinary clone did not preserve original Git object/history parent",
            ));
        }
        if invoke(
            &git,
            &mut case,
            args(&["-C", "clone", "show", "-s", "--format=%an <%ae>", "HEAD"]),
            root,
        )?
        .trim()
            != "Native Fixture Author <native@izu.invalid>"
            || invoke(
                &git,
                &mut case,
                args(&["-C", "clone", "show", "-s", "--format=%cn <%ce>", "HEAD"]),
                root,
            )?
            .trim()
                != "Export Fixture Committer <export@izu.invalid>"
        {
            return Err(io::Error::other(
                "native author/export committer provenance changed",
            ));
        }
        invoke(
            &git,
            &mut case,
            args(&["-C", "clone", "fsck", "--full"]),
            root,
        )?;
        json(
            runner,
            &mut case,
            args(&["--repo", "native", "verify"]),
            root,
        )?;
        Ok(())
    })();
    for (path, bytes) in [
        ("clone/source.txt", b"native authored edit\n".as_slice()),
        ("clone/unique.txt", b"native added source\n".as_slice()),
        ("original/source.txt", b"original Git source\n".as_slice()),
    ] {
        case.file_observations
            .push(observe_file(root.join(path), bytes));
    }
    finish(&mut case, result);
    Ok(case)
}
