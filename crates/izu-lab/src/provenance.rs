use serde::Serialize;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

#[derive(Debug, Serialize)]
pub struct Provenance {
    pub executable: PathBuf,
    pub executable_sha256: String,
    pub source_root: PathBuf,
    pub source_manifest_sha256: String,
    pub source_file_count: usize,
    pub git_head: Option<String>,
    pub git_status: Option<String>,
    pub git_metadata_scope: String,
    pub rust_version: String,
    pub os: String,
    pub os_version: String,
    pub kernel_version: String,
    pub logical_processors: Option<usize>,
    pub arch: String,
    pub filesystem: String,
    pub source_exclusions: Vec<String>,
}

fn read_command(program: &str, args: &[&str], cwd: &Path) -> Option<String> {
    let mut command = izu_process::CommandSpec::new(program);
    if program == "git" {
        command.args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.hooksPath=/dev/null",
            "-c",
            "core.untrackedCache=false",
        ]);
        for name in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_OBJECT_DIRECTORY",
            "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        ] {
            command.env_remove(name);
        }
    }
    command
        .args(args.iter().copied())
        .env("GIT_OPTIONAL_LOCKS", "0")
        .current_dir(cwd);
    command
        .env("TMPDIR", std::env::var_os("TMPDIR")?)
        .env("PATH", std::env::var_os("PATH")?)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1");
    for key in ["CARGO_HOME", "RUSTUP_HOME"] {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
    let options = izu_process::RunOptions {
        stdin_limit: 0,
        stdout_limit: 1024 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(5),
        cleanup_timeout: Duration::from_secs(2),
        output_policy: izu_process::OutputPolicy::Terminate,
        base_environment: izu_process::BaseEnvironment::Clear,
    };
    let worker = izu_process::WorkerLauncher {
        executable: std::env::current_exe().ok()?,
        prefix_args: vec!["__process-worker".into()],
    };
    let output = izu_process::run(&command, &[], &options, &worker, &|| false).ok()?;
    if !matches!(output.termination, izu_process::Termination::Completed)
        || !output.cleanup.cooperative_stop_succeeded()
        || output.exit?.code() != Some(0)
        || output.stdout.dropped > 0
        || !output.stdout.eof
    {
        return None;
    }
    Some(
        String::from_utf8(output.stdout.bytes)
            .ok()?
            .trim()
            .to_owned(),
    )
}

pub fn file_hash(path: &Path) -> io::Result<String> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

/// Reject a changed or unreadable artifact before any product command runs.
pub fn verify_artifact(path: &Path, expected: &str) -> io::Result<()> {
    if expected.len() != 64
        || !expected
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "expected SHA256 must be 64 lowercase hex characters",
        ));
    }
    let actual = file_hash(path)?;
    if actual != expected {
        return Err(io::Error::other(format!(
            "artifact SHA256 mismatch: expected {expected}, observed {actual}; no product command authorized"
        )));
    }
    Ok(())
}

fn source_entries(root: &Path, dir: &Path, entries: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let metadata = fs::symlink_metadata(&path)?;
        if name == ".git"
            || (metadata.is_dir()
                && (name.to_string_lossy().starts_with("target-")
                    || [".artifacts", "target", "node_modules", "lab-results"]
                        .iter()
                        .any(|excluded| name == *excluded)
                    || (dir == root && name == ".izu")))
        {
            continue;
        }
        if path.strip_prefix(root).is_err() {
            return Err(io::Error::other("source path escaped root"));
        }
        if metadata.is_dir() {
            source_entries(root, &path, entries)?;
        } else {
            entries.push(path);
        }
        if entries.len() > 100_000 {
            return Err(io::Error::other("source manifest exceeds 100000 entries"));
        }
    }
    Ok(())
}

fn source_manifest(source_root: &Path) -> io::Result<(String, usize)> {
    let mut entries = Vec::new();
    source_entries(source_root, source_root, &mut entries)?;
    // Full encoded paths share the same root prefix. Byte ordering therefore
    // matches relative-path ordering, including a.rs before a/nested.rs.
    entries.sort_by(|left, right| {
        left.as_os_str()
            .as_encoded_bytes()
            .cmp(right.as_os_str().as_encoded_bytes())
    });
    let mut manifest = Sha256::new();
    for path in &entries {
        let relative = path.strip_prefix(source_root).map_err(io::Error::other)?;
        let name = relative.as_os_str().as_encoded_bytes();
        manifest.update((name.len() as u64).to_le_bytes());
        manifest.update(name);
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            manifest.update(b"symlink\0");
            manifest.update(fs::read_link(path)?.as_os_str().as_encoded_bytes());
        } else {
            manifest.update(b"file\0");
            manifest.update(file_hash(path)?.as_bytes());
        }
    }
    Ok((format!("{:x}", manifest.finalize()), entries.len()))
}

pub fn capture(executable: &Path, source: &Path, scratch: &Path) -> io::Result<Provenance> {
    let executable = fs::canonicalize(executable)?;
    let source_root = fs::canonicalize(source)?;
    let (source_manifest_sha256, source_file_count) = source_manifest(&source_root)?;
    let filesystem = if cfg!(target_os = "macos") {
        let usage = read_command("/bin/df", &["-P", &scratch.to_string_lossy()], scratch);
        let mounts = read_command("/sbin/mount", &[], scratch);
        usage.and_then(|usage| {
            let mountpoint = usage
                .lines()
                .last()?
                .split_whitespace()
                .skip(5)
                .collect::<Vec<_>>()
                .join(" ");
            mounts?.lines().find_map(|line| {
                let (_, tail) = line.split_once(" on ")?;
                let (path, options) = tail.rsplit_once(" (")?;
                (path == mountpoint).then(|| {
                    format!(
                        "{}; mount={}; options={}",
                        options.split(',').next().unwrap_or("unknown"),
                        mountpoint,
                        options.trim_end_matches(')')
                    )
                })
            })
        })
    } else {
        read_command(
            "stat",
            &["-f", "-c", "%T", &scratch.to_string_lossy()],
            scratch,
        )
    }
    .unwrap_or_else(|| "unavailable".to_owned());
    let git_root = read_command("git", &["rev-parse", "--show-toplevel"], &source_root)
        .and_then(|path| fs::canonicalize(path).ok());
    let source_git = git_root.as_ref() == Some(&source_root);
    let git_head = source_git
        .then(|| read_command("git", &["rev-parse", "HEAD"], &source_root))
        .flatten();
    let git_status = source_git
        .then(|| {
            read_command(
                "git",
                &["status", "--porcelain=v1", "--untracked-files=normal"],
                &source_root,
            )
        })
        .flatten();
    Ok(Provenance {
        executable_sha256: file_hash(&executable)?,
        executable,
        source_manifest_sha256,
        source_file_count,
        git_head,
        git_status,
        git_metadata_scope: if source_git {
            "source_root"
        } else if git_root.is_some() {
            "ancestor_ignored"
        } else {
            "unavailable"
        }
        .into(),
        rust_version: read_command("rustc", &["--version"], &source_root)
            .unwrap_or_else(|| "unavailable".to_owned()),
        os: std::env::consts::OS.to_owned(),
        os_version: if cfg!(target_os = "macos") {
            read_command("/usr/bin/sw_vers", &["-productVersion"], &source_root)
        } else {
            read_command("uname", &["-v"], &source_root)
        }
        .unwrap_or_else(|| "unavailable".into()),
        kernel_version: read_command("uname", &["-r"], &source_root)
            .unwrap_or_else(|| "unavailable".into()),
        logical_processors: std::thread::available_parallelism().ok().map(usize::from),
        arch: std::env::consts::ARCH.to_owned(),
        source_root,
        filesystem,
        source_exclusions: vec![
            "root .izu".into(),
            "any target-* and .artifacts directories".into(),
            "any .git; target, node_modules, lab-results directories".into(),
        ],
    })
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn source_manifest_uses_canonical_byte_order_across_component_boundaries() {
        let temp = tempfile::tempdir().expect("owned source fixture");
        let root = temp.path();
        fs::create_dir(root.join("a")).expect("nested directory");
        fs::write(root.join("a.rs"), b"top\n").expect("top-level source");
        fs::write(root.join("a/nested.rs"), b"nested\n").expect("nested source");
        fs::write(root.join("é.rs"), b"unicode\n").expect("Unicode source name");
        symlink("a/nested.rs", root.join("z-link")).expect("relative symbolic link");

        // Independently framed in Python from literal names, contents and link
        // target bytes; Path component sorting puts a/nested.rs before a.rs.
        let (digest, count) = source_manifest(root).expect("source manifest");
        assert_eq!(count, 4);
        assert_eq!(
            digest,
            "4eedc9f907ca3e849f30443cf267139d17465fda26869ac6f85e8eb16eb54375"
        );
    }

    #[test]
    fn source_manifest_records_directory_and_broken_links_without_following() {
        let temp = tempfile::tempdir().expect("owned source fixture");
        let root = temp.path();
        fs::create_dir(root.join("a")).expect("nested directory");
        fs::write(root.join("a.rs"), b"top\n").expect("top-level source");
        fs::write(root.join("a/nested.rs"), b"nested\n").expect("nested source");
        fs::write(root.join("é.rs"), b"unicode\n").expect("Unicode source name");
        for excluded in [
            ".git",
            ".artifacts",
            "target",
            "target-cache",
            "node_modules",
            "lab-results",
            ".izu",
        ] {
            let directory = root.join(excluded);
            fs::create_dir(&directory).expect("excluded directory");
            fs::write(
                directory.join("excluded.rs"),
                b"excluded source must not be hashed\n",
            )
            .expect("excluded source");
        }
        for (name, target) in [
            ("z-link", "a/nested.rs"),
            ("dir-link", "a"),
            ("broken-link", "missing"),
            ("target-symbolic", "a"),
        ] {
            symlink(target, root.join(name)).expect("source symbolic link");
        }
        let manifest = source_manifest(root).expect("source manifest");
        assert_eq!(manifest.1, 7);
        assert_eq!(
            manifest.0,
            "6f50c8231a8273b9069a9e4dfef03dfbdd6b220032d5560977f067aa43ccdfa4"
        );
        fs::remove_file(root.join(".git/excluded.rs")).expect("owned excluded file");
        fs::remove_dir(root.join(".git")).expect("owned excluded directory");
        symlink("a", root.join(".git")).expect("excluded Git directory link");
        assert_eq!(source_manifest(root).expect("source manifest"), manifest);
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::ffi::OsStrExt;

            // TMPDIR can be an APFS/VirtioFS bind even in Linux. Raw names
            // require the approved guest-native /dev/shm fixture; no fallback.
            let raw_temp = tempfile::Builder::new()
                .prefix("izu-provenance-raw-")
                .tempdir_in("/dev/shm")
                .expect("owned guest-native raw filename fixture");
            let raw_root = raw_temp.path();
            fs::create_dir(raw_root.join("a")).expect("native nested directory");
            for name in ["a.rs", "a/nested.rs", "é.rs"] {
                fs::copy(root.join(name), raw_root.join(name)).expect("native fixture source");
            }
            for name in ["z-link", "dir-link", "broken-link", "target-symbolic"] {
                let target = fs::read_link(root.join(name)).expect("literal fixture link");
                symlink(target, raw_root.join(name)).expect("native fixture link");
            }
            for (name, contents) in [
                (&b"\x80.rs"[..], &b"raw\n"[..]),
                (&b"\xc2\xa0.rs"[..], &b"unicode-byte-order\n"[..]),
            ] {
                fs::write(raw_root.join(std::ffi::OsStr::from_bytes(name)), contents)
                    .expect("raw-byte source filename");
            }
            let raw_manifest = source_manifest(raw_root).expect("raw-byte source manifest");
            assert_eq!(raw_manifest.1, 9);
            assert_eq!(
                raw_manifest.0,
                "dfb4e04383b09da7ade9b88660ad784bfc56d6f85effd219dbefc07481b98ac6"
            );
        }
    }
}
