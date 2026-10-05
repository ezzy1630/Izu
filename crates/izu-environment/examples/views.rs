//! Reproducible file-environment benchmark. A fresh artifact/destination is
//! "cold" here; the operating system's page cache is deliberately uncontrolled.
use izu_engine::{Identity, Repository, RepositoryOptions, Selection};
use izu_environment::*;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Instant;

type BenchResult<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() -> BenchResult<()> {
    let mut root = None;
    let mut counts = vec![1_usize, 8, 30];
    let mut payload_mib = 16_u64;
    let mut policies = vec![SharingPolicy::Copy, SharingPolicy::PreferClone];
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        let value = args.next().ok_or("each benchmark flag requires a value")?;
        match arg.as_str() {
            "--root" => root = Some(PathBuf::from(value)),
            "--views" => {
                counts = value
                    .split(',')
                    .map(str::parse)
                    .collect::<std::result::Result<Vec<usize>, _>>()?;
                if counts.is_empty() || counts.iter().any(|count| !matches!(count, 1 | 8 | 30)) {
                    return Err("--views accepts 1,8,30".into());
                }
            }
            "--payload-mib" => {
                payload_mib = value.parse()?;
                if !(1..=512).contains(&payload_mib) {
                    return Err("--payload-mib must be 1..=512".into());
                }
            }
            "--policy" => {
                policies = match value.as_str() {
                    "copy" => vec![SharingPolicy::Copy],
                    "clone" => vec![SharingPolicy::PreferClone],
                    "both" => vec![SharingPolicy::Copy, SharingPolicy::PreferClone],
                    _ => return Err("--policy accepts copy, clone or both".into()),
                }
            }
            _ => return Err(format!("unknown benchmark flag {arg}").into()),
        }
    }
    let root = root.ok_or("pass --root with an absent or empty owned benchmark directory")?;
    match fs::symlink_metadata(&root) {
        Ok(metadata) if metadata.is_dir() && fs::read_dir(&root)?.next().is_none() => {}
        Ok(_) => {
            return Err(
                "benchmark root must be absent or an empty directory; existing data is preserved"
                    .into(),
            );
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => fs::create_dir(&root)?,
        Err(error) => return Err(error.into()),
    }
    let _ = filesystem_available_bytes(&root)?;
    for policy in policies {
        for count in &counts {
            run_case(&root, policy, *count, payload_mib)?;
        }
    }
    Ok(())
}

fn run_case(root: &Path, policy: SharingPolicy, count: usize, payload_mib: u64) -> BenchResult<()> {
    let mode = if policy == SharingPolicy::Copy {
        "copy"
    } else {
        "prefer-clone"
    };
    let case = root.join(format!("{mode}-{count}"));
    fs::create_dir(&case)?;
    let source = case.join("source");
    fs::create_dir(&source)?;
    fs::write(
        source.join(".izuignore"),
        b"node_modules/\n.next/\ntarget/\n",
    )?;
    fs::write(source.join("fixture.lock"), b"environment-benchmark-v1")?;
    fs::write(source.join("source.txt"), b"original izu source fixture\n")?;
    let repo = Repository::init(&source, RepositoryOptions::default())?;
    let cancel = CancellationToken::new();
    let state = repo.workspace(repo.workspace_id(), &cancel)?;
    let committed = repo.commit(
        state.id,
        state.expected,
        Selection::All,
        "prepared environment benchmark fixture".into(),
        Identity {
            name: "benchmark".into(),
            email: "benchmark@localhost".into(),
        },
        &cancel,
    )?;
    let prepared = case.join("prepared");
    fs::create_dir(&prepared)?;
    fs::create_dir(prepared.join("node_modules"))?;
    fs::create_dir_all(prepared.join(".next/cache"))?;
    fs::write(prepared.join("fixture.lock"), b"environment-benchmark-v1")?;
    let mut expected = Vec::new();
    expected.try_reserve_exact(33)?;
    let per_file = usize::try_from(
        payload_mib
            .checked_mul(1024 * 1024)
            .ok_or("payload overflow")?
            / 32,
    )?;
    for index in 0..32 {
        let path = format!("node_modules/dependency-{index:02}.bin");
        let digest = write_data(&prepared.join(&path), per_file, index as u64 + 1)?;
        expected.push((path, digest));
    }
    expected.push((
        ".next/cache/warm.bin".into(),
        write_data(&prepared.join(".next/cache/warm.bin"), 1024 * 1024, 79)?,
    ));
    let first = repo.fork_workspace(
        "view-0".into(),
        case.join("view-0"),
        committed.revision,
        &cancel,
    )?;
    let first_target = WorkspaceTarget::checked(&repo, first.id, first.expected, &cancel)?;
    let recipe = Recipe::trusted(
        RecipeSpec {
            schema_version: 1,
            source_identity: first_target.source_identity(),
            lockfiles: vec![LockfileIdentity {
                path: "fixture.lock".into(),
                digest: Digest::of_bytes(b"environment-benchmark-v1"),
            }],
            toolchain_identity: Digest::of_bytes(
                b"fixture-generator-v1; no package toolchain executed",
            ),
            platform: PlatformIdentity::current("file-fixture-v1"),
            recipe_identity: Digest::of_bytes(b"explicit quiescent binary fixture import-v1"),
            trust_domain: "owned-local-benchmark".into(),
            argv: vec!["fixture-import-only-no-execution".into()],
            dependencies: vec!["node_modules".into()],
            outputs: vec![".next".into(), "target".into()],
        },
        TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )?;
    drop(first_target);
    let cache = EnvironmentCache::open(
        case.join("cache"),
        EnvironmentLimits {
            min_free_bytes: 0,
            ..EnvironmentLimits::default()
        },
    )?;
    let import_start = Instant::now();
    let imported = cache.import_quiescent(
        &recipe,
        &prepared,
        QuiescenceAcknowledgement::CallerConfirmsNoWriters,
        policy,
        &cancel,
    )?;
    let import_ms = milliseconds(import_start)?;
    let free_before = filesystem_available_bytes(&case)?;
    let setup_start = Instant::now();
    let mut views = Vec::new();
    views.try_reserve_exact(count)?;
    let mut cloned = 0_u64;
    let mut copied = 0_u64;
    let mut logical = 0_u64;
    let mut allocated_sum = 0_u64;
    for index in 0..count {
        let state = if index == 0 {
            first.clone()
        } else {
            repo.fork_workspace(
                format!("view-{index}"),
                case.join(format!("view-{index}")),
                committed.revision,
                &cancel,
            )?
        };
        let mut target = WorkspaceTarget::checked(&repo, state.id, state.expected, &cancel)?;
        let result = cache.materialize(&recipe, &mut target, policy, &cancel)?;
        cloned = cloned
            .checked_add(result.cloned_files)
            .ok_or("clone count overflow")?;
        copied = copied
            .checked_add(result.copied_files)
            .ok_or("copy count overflow")?;
        logical = logical
            .checked_add(result.logical_bytes)
            .ok_or("logical bytes overflow")?;
        allocated_sum = allocated_sum
            .checked_add(result.allocated_file_bytes_sum)
            .ok_or("allocated sum overflow")?;
        verify_view(target.root_path(), &expected)?;
        views.push(state);
        drop(target);
    }
    let setup_ms = milliseconds(setup_start)?;
    let free_after = filesystem_available_bytes(&case)?;
    let resume_start = Instant::now();
    for state in &views {
        let reopened = Repository::open(&state.record.root, RepositoryOptions::default())?;
        let current = reopened.workspace(state.id, &cancel)?;
        let target = WorkspaceTarget::checked(&reopened, state.id, current.expected, &cancel)?;
        if target.source_identity() != recipe.spec().source_identity {
            return Err("source identity changed on resume".into());
        }
        if !matches!(cache.status(&recipe, &cancel)?, CacheStatus::Ready(_)) {
            return Err("artifact not ready on resume".into());
        }
        verify_view(target.root_path(), &expected)?;
    }
    let resume_ms = milliseconds(resume_start)?;
    #[cfg(unix)]
    {
        let mut inodes = std::collections::BTreeSet::new();
        for state in &views {
            for (path, _) in &expected {
                let metadata = fs::metadata(Path::new(&state.record.root).join(path))?;
                if metadata.nlink() != 1 || !inodes.insert((metadata.dev(), metadata.ino())) {
                    return Err("writable files share an inode".into());
                }
            }
        }
    }
    let changed = Path::new(&views[0].record.root).join(&expected[0].0);
    fs::write(&changed, b"private first-view mutation\n")?;
    for state in views.iter().skip(1) {
        verify_view(Path::new(&state.record.root), &expected)?;
    }
    if hash_path(&prepared.join(&expected[0].0))? != expected[0].1
        || !matches!(cache.status(&recipe, &cancel)?, CacheStatus::Ready(_))
    {
        return Err("private mutation damaged preparation/cache".into());
    }
    println!(
        "{}",
        serde_json::to_string(&json!({
            "policy": policy, "views": count, "dependency_payload_mib": payload_mib, "warm_output_mib": 1,
            "import_wall_ms": import_ms, "view_setup_wall_ms": setup_ms, "resume_wall_ms": resume_ms,
            "resume_operations": "repository reopen, source guard/capture, complete cache verification, full dependency/output hash readback",
            "cloned_files": cloned, "copied_files": copied, "logical_view_bytes": logical, "allocated_file_bytes_sum": allocated_sum,
            "volume_available_bytes_before_views": free_before, "volume_available_bytes_after_views": free_after,
            "volume_free_bytes_delta": i128::from(free_before)-i128::from(free_after),
            "free_space_delta_requires_exclusive_isolated_volume": true, "allocated_file_sum_counts_unique_cow_extents": false,
            "private_mutation_isolated": true, "all_writable_file_inodes_unique": true,
            "key": recipe.key(), "manifest_digest": imported.artifact.manifest_digest,
            "cold_means_absent_artifact_and_destinations": true, "os_page_cache_controlled": false,
            "fixture_only_no_dependency_installer_or_db": true, "os": std::env::consts::OS, "architecture": std::env::consts::ARCH,
            "retained_root": case
        }))?
    );
    Ok(())
}

fn milliseconds(started: Instant) -> BenchResult<u64> {
    Ok(u64::try_from(started.elapsed().as_millis())?)
}
fn write_data(path: &Path, length: usize, mut seed: u64) -> BenchResult<Digest> {
    let mut data = Vec::new();
    data.try_reserve_exact(length)?;
    data.resize(length, 0);
    for byte in &mut data {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        *byte = seed as u8;
    }
    let digest = Digest::of_bytes(&data);
    let mut file = File::create(path)?;
    file.write_all(&data)?;
    file.sync_all()?;
    Ok(digest)
}
fn hash_path(path: &Path) -> BenchResult<Digest> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 65536];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    let digest: [u8; 32] = hasher.finalize().into();
    let mut hex = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write as _;
        write!(&mut hex, "{byte:02x}")?;
    }
    Ok(Digest::from_hex(&hex)?)
}
fn verify_view(root: &Path, expected: &[(String, Digest)]) -> BenchResult<()> {
    for (path, digest) in expected {
        if hash_path(&root.join(path))? != *digest {
            return Err(format!("view content differs: {path}").into());
        }
    }
    if !root.join("target").is_dir() {
        return Err("private empty output was not materialized".into());
    }
    Ok(())
}
