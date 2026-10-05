use izu_engine::{Identity, Repository, RepositoryOptions, Selection, WorkspaceState};
use izu_environment::*;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

struct Fixture {
    temp: tempfile::TempDir,
    repo: Repository,
    prepared: PathBuf,
    first: WorkspaceState,
    second: WorkspaceState,
}

impl Fixture {
    fn new() -> Self {
        let temp = owned_temp();
        let source = temp.path().join("source");
        fs::create_dir(&source).unwrap();
        fs::write(
            source.join(".izuignore"),
            b"node_modules/\n.next/\ntarget/\nvendor/\n",
        )
        .unwrap();
        fs::write(source.join("package-lock.json"), b"lock-v1").unwrap();
        fs::write(source.join("main.txt"), b"human source\n").unwrap();
        let mut options = RepositoryOptions::default();
        options.store.lock_timeout = Duration::from_millis(100);
        let repo = Repository::init(&source, options).unwrap();
        let cancel = CancellationToken::new();
        let base = repo.workspace(repo.workspace_id(), &cancel).unwrap();
        let receipt = repo
            .commit(
                base.id,
                base.expected,
                Selection::All,
                "environment fixture".into(),
                Identity {
                    name: "test".into(),
                    email: "test@localhost".into(),
                },
                &cancel,
            )
            .unwrap();
        let first = repo
            .fork_workspace(
                "one".into(),
                temp.path().join("one"),
                receipt.revision,
                &cancel,
            )
            .unwrap();
        let second = repo
            .fork_workspace(
                "two".into(),
                temp.path().join("two"),
                receipt.revision,
                &cancel,
            )
            .unwrap();
        let prepared = temp.path().join("prepared");
        fs::create_dir(&prepared).unwrap();
        fs::write(prepared.join("package-lock.json"), b"lock-v1").unwrap();
        fs::create_dir_all(prepared.join("node_modules/pkg")).unwrap();
        fs::create_dir_all(prepared.join("node_modules/.bin")).unwrap();
        fs::create_dir_all(prepared.join(".next/cache")).unwrap();
        fs::write(
            prepared.join("node_modules/pkg/tool"),
            b"dependency-original\n",
        )
        .unwrap();
        fs::write(prepared.join(".next/cache/warm"), b"warm-original\n").unwrap();
        #[cfg(unix)]
        {
            fs::set_permissions(
                prepared.join("node_modules/pkg/tool"),
                fs::Permissions::from_mode(0o755),
            )
            .unwrap();
            symlink("../pkg/tool", prepared.join("node_modules/.bin/tool")).unwrap();
        }
        Self {
            temp,
            repo,
            prepared,
            first,
            second,
        }
    }
    fn recipe(&self) -> Recipe {
        let target = WorkspaceTarget::checked(
            &self.repo,
            self.first.id,
            self.first.expected,
            &CancellationToken::new(),
        )
        .unwrap();
        recipe(target.source_identity())
    }
    fn cache(&self) -> EnvironmentCache {
        EnvironmentCache::open(self.temp.path().join("cache"), test_limits()).unwrap()
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        make_writable(self.temp.path());
    }
}

fn owned_temp() -> tempfile::TempDir {
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target-fixtures");
    fs::create_dir_all(&parent).unwrap();
    tempfile::Builder::new()
        .prefix("izu-environment-")
        .tempdir_in(parent)
        .unwrap()
}
fn make_writable(path: &Path) {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return;
    };
    if metadata.is_symlink() {
        return;
    }
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(
            path,
            fs::Permissions::from_mode(if metadata.is_dir() { 0o700 } else { 0o600 }),
        );
    }
    if metadata.is_dir()
        && let Ok(entries) = fs::read_dir(path)
    {
        for entry in entries.flatten() {
            make_writable(&entry.path());
        }
    }
}
fn test_limits() -> EnvironmentLimits {
    EnvironmentLimits {
        min_free_bytes: 0,
        ..EnvironmentLimits::default()
    }
}
fn recipe(source_identity: Digest) -> Recipe {
    Recipe::trusted(
        RecipeSpec {
            schema_version: 1,
            source_identity,
            lockfiles: vec![LockfileIdentity {
                path: "package-lock.json".into(),
                digest: Digest::of_bytes(b"lock-v1"),
            }],
            toolchain_identity: Digest::of_bytes(b"explicit-toolchain"),
            platform: PlatformIdentity::current("fixture-abi"),
            recipe_identity: Digest::of_bytes(b"fixture-recipe"),
            trust_domain: "local-test".into(),
            argv: vec![
                "explicit-install-tool".into(),
                "token-kept-out-of-manifest".into(),
            ],
            dependencies: vec!["node_modules".into()],
            outputs: vec![".next".into(), "target".into()],
        },
        TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )
    .unwrap()
}
fn import(
    cache: &EnvironmentCache,
    recipe: &Recipe,
    prepared: &Path,
    policy: SharingPolicy,
) -> ImportReceipt {
    cache
        .import_quiescent(
            recipe,
            prepared,
            QuiescenceAcknowledgement::CallerConfirmsNoWriters,
            policy,
            &CancellationToken::new(),
        )
        .unwrap()
}

fn imported_binding(fixture: &Fixture, cache: &EnvironmentCache) -> (Recipe, EnvironmentBinding) {
    let recipe = fixture.recipe();
    import(cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let binding = cache.binding(&recipe, &CancellationToken::new()).unwrap();
    (recipe, binding)
}

fn materialize_bound(
    fixture: &Fixture,
    cache: &EnvironmentCache,
    binding: &EnvironmentBinding,
    state: &WorkspaceState,
) {
    let cancel = CancellationToken::new();
    let mut target =
        WorkspaceTarget::checked(&fixture.repo, state.id, state.expected, &cancel).unwrap();
    cache
        .materialize_binding(binding, &mut target, SharingPolicy::Copy, &cancel)
        .unwrap();
}

#[test]
fn native_binding_roundtrip_and_two_leased_views_verify_actual_starting_contents() {
    let fixture = Fixture::new();
    let cache = fixture.cache();
    let (recipe, binding) = imported_binding(&fixture, &cache);
    let encoded = binding.to_json().unwrap();
    assert!(
        !String::from_utf8(encoded.clone())
            .unwrap()
            .contains("token-kept-out-of-manifest")
    );
    assert_eq!(EnvironmentBinding::from_json(&encoded).unwrap(), binding);
    let cancel = CancellationToken::new();
    let blob = fixture
        .repo
        .put_blob(&mut encoded.as_slice(), encoded.len() as u64, &cancel)
        .unwrap();
    assert_ne!(blob.to_string(), binding.key().to_string());
    let mut readback = Vec::new();
    fixture
        .repo
        .read_blob(blob, &mut readback, &cancel)
        .unwrap();
    assert_eq!(EnvironmentBinding::from_json(&readback).unwrap(), binding);
    materialize_bound(&fixture, &cache, &binding, &fixture.first);
    materialize_bound(&fixture, &cache, &binding, &fixture.second);
    let first_lease = fixture
        .repo
        .lease_workspace(fixture.first.id, fixture.first.expected, &cancel)
        .unwrap();
    let second_lease = fixture
        .repo
        .lease_workspace(fixture.second.id, fixture.second.expected, &cancel)
        .unwrap();
    let first = cache
        .verify_starting_environment(&binding, &fixture.repo, &first_lease, &cancel)
        .unwrap();
    let second = cache
        .verify_starting_environment(&binding, &fixture.repo, &second_lease, &cancel)
        .unwrap();
    assert_eq!(
        first.receipt().observed_content_digest,
        second.receipt().observed_content_digest
    );
    assert_eq!(first.receipt().key, recipe.key());
    assert_eq!(first.receipt().source_identity, binding.source_identity());
    assert_eq!(
        first.receipt().scope,
        EnvironmentVerificationScope::StartingFileContents
    );
    assert_eq!(first.receipt().platform.observed_os, std::env::consts::OS);
    assert_eq!(
        first.receipt().platform.observed_architecture,
        std::env::consts::ARCH
    );
    assert_eq!(first.receipt().platform.declared_abi, "fixture-abi");
    assert_eq!(
        first.receipt().toolchain.declared_identity,
        recipe.spec().toolchain_identity
    );
    assert!(!first.receipt().security_boundary);
    assert_eq!(
        serde_json::from_slice::<StartingEnvironmentReceipt>(
            &serde_json::to_vec(first.receipt()).unwrap()
        )
        .unwrap(),
        *first.receipt()
    );
    let cancelled = CancellationToken::new();
    cancelled.cancel();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &first_lease, &cancelled),
        Err(EnvironmentError::Cancelled)
    ));
    drop(first);
    drop(second);
    first_lease.finish_stopped().unwrap();
    second_lease.finish_stopped().unwrap();
}

#[test]
fn binding_schema_key_paths_and_wire_size_are_validated_before_use() {
    let fixture = Fixture::new();
    let cache = fixture.cache();
    let (_, binding) = imported_binding(&fixture, &cache);
    let record: serde_json::Value = serde_json::from_slice(&binding.to_json().unwrap()).unwrap();
    let mut wrong_schema = record.clone();
    wrong_schema["schema_version"] = 2.into();
    assert!(EnvironmentBinding::from_json(&serde_json::to_vec(&wrong_schema).unwrap()).is_err());
    let mut wrong_key = record.clone();
    wrong_key["key"] = Digest::of_bytes(b"unrelated-cache-key").to_string().into();
    assert!(EnvironmentBinding::from_json(&serde_json::to_vec(&wrong_key).unwrap()).is_err());
    let mut escape = record.clone();
    escape["identity"]["dependencies"] = serde_json::json!(["../outside"]);
    assert!(EnvironmentBinding::from_json(&serde_json::to_vec(&escape).unwrap()).is_err());
    let mut wrong_scope = record.clone();
    wrong_scope["scope"] = "ImmutableEnvironment".into();
    assert!(EnvironmentBinding::from_json(&serde_json::to_vec(&wrong_scope).unwrap()).is_err());
    let mut unknown = record;
    unknown["argv"] = serde_json::json!(["never-execute"]);
    assert!(EnvironmentBinding::from_json(&serde_json::to_vec(&unknown).unwrap()).is_err());
    assert!(matches!(
        EnvironmentBinding::from_json(&vec![b' '; MAX_ENVIRONMENT_BINDING_BYTES + 1]),
        Err(EnvironmentError::Limit { .. })
    ));
}

#[test]
fn recipe_rejects_case_folded_and_unicode_namespace_aliases_before_materialization() {
    let base = recipe(Digest::of_bytes(b"namespace-source"));
    for paths in [
        vec!["node_modules", "NODE_MODULES"],
        vec!["vendor/Σ", "VENDOR/ς"],
        vec!["vendor/straße", "vendor/STRASSE"],
        vec!["vendor/café", "vendor/cafe\u{301}"],
        vec!["vendor/DEPS", "Vendor/deps/nested"],
    ] {
        let mut spec = base.spec().clone();
        spec.dependencies = paths.into_iter().map(str::to_owned).collect();
        assert!(
            Recipe::trusted(
                spec,
                TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode
            )
            .is_err()
        );
    }
    let mut duplicate_lock = base.spec().clone();
    duplicate_lock.lockfiles.push(LockfileIdentity {
        path: "PACKAGE-LOCK.JSON".into(),
        digest: Digest::of_bytes(b"lock-v1"),
    });
    assert!(
        Recipe::trusted(
            duplicate_lock,
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode
        )
        .is_err()
    );
    let mut contained_lock = base.spec().clone();
    contained_lock.lockfiles[0].path = "NODE_MODULES/package-lock.json".into();
    assert!(
        Recipe::trusted(
            contained_lock,
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode
        )
        .is_err()
    );
}

#[test]
fn recipe_rejects_aliased_ancestor_spellings_across_roots_and_lockfiles() {
    let base = recipe(Digest::of_bytes(b"ancestor-source"));
    for paths in [
        vec!["vendor/one", "VENDOR/two"],
        vec!["vendor/Σ/one", "vendor/ς/two"],
        vec!["vendor/straße/one", "vendor/STRASSE/two"],
        vec!["vendor/café/one", "vendor/cafe\u{301}/two"],
    ] {
        let mut spec = base.spec().clone();
        spec.dependencies = paths.into_iter().map(str::to_owned).collect();
        assert!(
            Recipe::trusted(
                spec,
                TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
            )
            .is_err()
        );
    }
    for lockfiles in [
        vec!["VENDOR/lock.json"],
        vec!["vendor/one.lock", "VENDOR/two.lock"],
    ] {
        let mut spec = base.spec().clone();
        spec.dependencies = vec!["vendor/deps".into()];
        spec.outputs = vec!["vendor/build".into()];
        spec.lockfiles = lockfiles
            .into_iter()
            .map(|path| LockfileIdentity {
                path: path.into(),
                digest: Digest::of_bytes(b"lock"),
            })
            .collect();
        assert!(
            Recipe::trusted(
                spec,
                TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
            )
            .is_err()
        );
    }
    let mut consistent = base.spec().clone();
    consistent.dependencies = vec!["vendor/one".into(), "vendor/two".into()];
    consistent.outputs = vec!["vendor/build".into()];
    consistent.lockfiles[0].path = "vendor/lock.json".into();
    assert!(
        Recipe::trusted(
            consistent,
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
        )
        .is_ok()
    );
}

#[cfg(target_os = "linux")]
#[test]
fn known_volatile_cache_is_refused_before_namespace_creation() {
    use izu_platform::Directory;

    let volatile = tempfile::Builder::new()
        .prefix("izu-environment-volatile-cache-")
        .tempdir_in("/dev/shm")
        .unwrap();
    let directory = Directory::open(volatile.path()).unwrap();
    assert_eq!(
        directory
            .require_persistent_filesystem()
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::Unsupported,
    );
    directory.sync().unwrap();
    let path = volatile.path().join("cache");
    match EnvironmentCache::open(&path, test_limits()) {
        Err(EnvironmentError::Io { source, .. }) => {
            assert_eq!(source.kind(), std::io::ErrorKind::Unsupported);
            assert!(!path.exists());
        }
        Err(error) => panic!("unexpected refusal: {error}"),
        Ok(cache) => {
            let prepared = volatile.path().join("prepared");
            fs::create_dir_all(prepared.join("deps")).unwrap();
            fs::write(prepared.join("deps/bytes"), b"prepared bytes").unwrap();
            let recipe = Recipe::trusted(
                RecipeSpec {
                    schema_version: 1,
                    source_identity: Digest::of_bytes(b"source"),
                    lockfiles: Vec::new(),
                    toolchain_identity: Digest::of_bytes(b"toolchain"),
                    platform: PlatformIdentity::current("test"),
                    recipe_identity: Digest::of_bytes(b"recipe"),
                    trust_domain: "volatile-regression".into(),
                    argv: vec!["never-executed".into()],
                    dependencies: vec!["deps".into()],
                    outputs: Vec::new(),
                },
                TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
            )
            .unwrap();
            let receipt = import(&cache, &recipe, &prepared, SharingPolicy::Copy);
            make_writable(volatile.path());
            panic!("known volatile cache acknowledged publication: {receipt:?}");
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn known_volatile_private_workspace_is_refused_before_registration_or_environment_writes() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let volatile = tempfile::Builder::new()
        .prefix("izu-environment-volatile-view-")
        .tempdir_in("/dev/shm")
        .unwrap();
    let cancel = CancellationToken::new();
    let requested_root = volatile.path().join("view");
    let operation_before = fixture.repo.current_operation(&cancel).unwrap();
    let view_before = fixture.repo.view(&cancel).unwrap();
    let binding_before = cache.binding(&recipe, &cancel).unwrap();
    let protected_files = [
        fixture.repo.root_path().join("main.txt"),
        fixture.repo.root_path().join("package-lock.json"),
        fixture.repo.root_path().join(".izuignore"),
        fixture.prepared.join("package-lock.json"),
        fixture.prepared.join("node_modules/pkg/tool"),
        fixture.prepared.join(".next/cache/warm"),
    ];
    let contents_before: Vec<_> = protected_files
        .iter()
        .map(|path| fs::read(path).unwrap())
        .collect();

    // Source publication validates persistence before the workspace marker or
    // registration exists, so a known volatile target never reaches environments.
    let result = fixture.repo.fork_workspace(
        "volatile-view".into(),
        &requested_root,
        fixture.first.expected.head,
        &cancel,
    );
    assert!(
        matches!(&result, Err(izu_engine::EngineError::Io { path, source })
            if path == &requested_root
                && source.kind() == std::io::ErrorKind::Unsupported
                && source.to_string() == "known volatile filesystem cannot acknowledge persistent storage"),
        "known volatile workspace was not refused at its source boundary: {result:?}",
    );
    assert_eq!(
        fixture.repo.current_operation(&cancel).unwrap(),
        operation_before
    );
    assert_eq!(fixture.repo.view(&cancel).unwrap(), view_before);
    assert!(fs::symlink_metadata(&requested_root).unwrap().is_dir());
    assert!(fs::read_dir(&requested_root).unwrap().next().is_none());
    for path in [".izu", ".izu-recovery/environment-staging"] {
        assert_eq!(
            fs::symlink_metadata(requested_root.join(path))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound,
        );
    }
    for path in recipe.declared_paths() {
        assert_eq!(
            fs::symlink_metadata(requested_root.join(path))
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::NotFound,
        );
    }
    for (path, expected) in protected_files.iter().zip(contents_before) {
        assert_eq!(fs::read(path).unwrap(), expected);
    }
    assert_eq!(cache.binding(&recipe, &cancel).unwrap(), binding_before);
}

#[test]
fn bound_manifest_missing_cache_and_source_mismatch_preserve_empty_destinations() {
    let fixture = Fixture::new();
    let cache = fixture.cache();
    let (_, binding) = imported_binding(&fixture, &cache);
    let mut value: serde_json::Value = serde_json::from_slice(&binding.to_json().unwrap()).unwrap();
    value["manifest_digest"] = Digest::of_bytes(b"wrong-manifest").to_string().into();
    let wrong = EnvironmentBinding::from_json(&serde_json::to_vec(&value).unwrap()).unwrap();
    let cancel = CancellationToken::new();
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &cancel,
    )
    .unwrap();
    assert!(matches!(
        cache.materialize_binding(&wrong, &mut target, SharingPolicy::Copy, &cancel),
        Err(EnvironmentError::Corrupt(_))
    ));
    let missing =
        EnvironmentCache::open(fixture.temp.path().join("missing-cache"), test_limits()).unwrap();
    assert!(matches!(
        missing.materialize_binding(&binding, &mut target, SharingPolicy::Copy, &cancel),
        Err(EnvironmentError::Missing(_))
    ));
    assert!(!target.root_path().join("node_modules").exists());
    assert!(
        !target
            .root_path()
            .join(".izu-recovery/environment-staging")
            .exists()
    );
    drop(target);
    fs::write(
        Path::new(&fixture.first.record.root).join("main.txt"),
        b"preserve changed source\n",
    )
    .unwrap();
    let mut changed = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &cancel,
    )
    .unwrap();
    assert!(matches!(
        cache.materialize_binding(&binding, &mut changed, SharingPolicy::Copy, &cancel),
        Err(EnvironmentError::SourceMismatch)
    ));
    assert_eq!(
        fs::read(changed.root_path().join("main.txt")).unwrap(),
        b"preserve changed source\n"
    );
    assert!(!changed.root_path().join("node_modules").exists());
}

#[test]
fn leased_starting_verifier_rejects_private_tamper_extra_warm_entries_and_aliases() {
    let fixture = Fixture::new();
    let cache = fixture.cache();
    let (recipe, binding) = imported_binding(&fixture, &cache);
    materialize_bound(&fixture, &cache, &binding, &fixture.first);
    let cancel = CancellationToken::new();
    let lease = fixture
        .repo
        .lease_workspace(fixture.first.id, fixture.first.expected, &cancel)
        .unwrap();
    let proof = cache
        .verify_starting_environment(&binding, &fixture.repo, &lease, &cancel)
        .unwrap();
    let original_receipt = proof.receipt().clone();
    let root = Path::new(&fixture.first.record.root);
    let tool = root.join("node_modules/pkg/tool");
    fs::write(&tool, b"dependency-tampered\n").unwrap();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &lease, &cancel),
        Err(EnvironmentError::StartingEnvironmentMismatch(_))
    ));
    fs::write(&tool, b"dependency-original\n").unwrap();
    fs::write(
        root.join(".next/cache/untracked"),
        b"preserve unexpected warm data",
    )
    .unwrap();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &lease, &cancel),
        Err(EnvironmentError::StartingEnvironmentMismatch(_))
    ));
    assert_eq!(
        fs::read(root.join(".next/cache/untracked")).unwrap(),
        b"preserve unexpected warm data"
    );
    fs::remove_file(root.join(".next/cache/untracked")).unwrap();
    let alias = fixture.temp.path().join("owned-test-alias");
    fs::hard_link(&tool, &alias).unwrap();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &lease, &cancel),
        Err(EnvironmentError::StartingEnvironmentMismatch(_))
    ));
    fs::remove_file(alias).unwrap();
    fs::write(root.join(".next/cache/warm"), b"new private warm output\n").unwrap();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &lease, &cancel),
        Err(EnvironmentError::StartingEnvironmentMismatch(_))
    ));
    assert_eq!(proof.receipt(), &original_receipt);
    assert_eq!(
        fs::read(fixture.prepared.join(".next/cache/warm")).unwrap(),
        b"warm-original\n"
    );
    assert!(matches!(
        cache.status(&recipe, &cancel).unwrap(),
        CacheStatus::Ready(_)
    ));
    drop(proof);
    lease.finish_stopped().unwrap();
}

#[test]
fn leased_starting_verifier_refuses_source_path_replacement_and_preserves_original() {
    let fixture = Fixture::new();
    let cache = fixture.cache();
    let (_, binding) = imported_binding(&fixture, &cache);
    materialize_bound(&fixture, &cache, &binding, &fixture.first);
    let cancel = CancellationToken::new();
    let lease = fixture
        .repo
        .lease_workspace(fixture.first.id, fixture.first.expected, &cancel)
        .unwrap();
    let original = fixture.temp.path().join("retained-original-workspace");
    fs::rename(&fixture.first.record.root, &original).unwrap();
    fs::create_dir(&fixture.first.record.root).unwrap();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &lease, &cancel),
        Err(EnvironmentError::Workspace(_))
    ));
    assert_eq!(
        fs::read(original.join("node_modules/pkg/tool")).unwrap(),
        b"dependency-original\n"
    );
    assert!(
        fs::read_dir(&fixture.first.record.root)
            .unwrap()
            .next()
            .is_none()
    );
    lease.finish_stopped().unwrap();
}

#[test]
fn leased_starting_verifier_refuses_replaced_cache_locator() {
    let fixture = Fixture::new();
    let cache = fixture.cache();
    let (_, binding) = imported_binding(&fixture, &cache);
    materialize_bound(&fixture, &cache, &binding, &fixture.first);
    let cancel = CancellationToken::new();
    let lease = fixture
        .repo
        .lease_workspace(fixture.first.id, fixture.first.expected, &cancel)
        .unwrap();
    let original = fixture.temp.path().join("retained-original-cache");
    fs::rename(cache.root_path(), &original).unwrap();
    fs::create_dir(cache.root_path()).unwrap();
    assert!(matches!(
        cache.verify_starting_environment(&binding, &fixture.repo, &lease, &cancel),
        Err(EnvironmentError::NamespaceChanged(_))
    ));
    assert!(
        original
            .join("artifacts")
            .join(binding.key().to_string())
            .join("READY")
            .is_file()
    );
    lease.finish_stopped().unwrap();
}

#[test]
fn import_two_private_views_and_mutation_do_not_alias() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    let imported = import(
        &cache,
        &recipe,
        &fixture.prepared,
        SharingPolicy::PreferClone,
    );
    assert!(!imported.reused);
    let cancel = CancellationToken::new();
    for state in [&fixture.first, &fixture.second] {
        let mut target =
            WorkspaceTarget::checked(&fixture.repo, state.id, state.expected, &cancel).unwrap();
        let report = cache
            .materialize(&recipe, &mut target, SharingPolicy::PreferClone, &cancel)
            .unwrap();
        assert_eq!(report.published_paths.len(), 3);
        assert_eq!(report.cloned_files + report.copied_files, 2);
        assert!(report.private_file_inodes);
        assert!(!report.security_boundary);
        assert!(report.retained_stage.is_none());
        if std::env::var_os("IZU_REQUIRE_CLONE").is_some() {
            assert_eq!(report.mode, MaterializationMode::Clone);
            assert_eq!(report.copied_files, 0);
        }
    }
    let first = Path::new(&fixture.first.record.root);
    let second = Path::new(&fixture.second.record.root);
    let cached = cache
        .root_path()
        .join("artifacts")
        .join(recipe.key().to_string())
        .join("payload/0/pkg/tool");
    #[cfg(unix)]
    {
        let a = fs::metadata(first.join("node_modules/pkg/tool")).unwrap();
        let b = fs::metadata(second.join("node_modules/pkg/tool")).unwrap();
        let c = fs::metadata(&cached).unwrap();
        assert_ne!(a.ino(), b.ino());
        assert_ne!(a.ino(), c.ino());
        assert_eq!(a.nlink(), 1);
        assert_eq!(b.nlink(), 1);
        assert_eq!(c.nlink(), 1);
    }
    fs::write(first.join("node_modules/pkg/tool"), b"private mutation\n").unwrap();
    fs::write(first.join(".next/cache/warm"), b"private build\n").unwrap();
    fs::write(first.join("target/new-output"), b"private target\n").unwrap();
    assert_eq!(
        fs::read(second.join("node_modules/pkg/tool")).unwrap(),
        b"dependency-original\n"
    );
    assert_eq!(
        fs::read(second.join(".next/cache/warm")).unwrap(),
        b"warm-original\n"
    );
    assert!(!second.join("target/new-output").exists());
    assert_eq!(fs::read(&cached).unwrap(), b"dependency-original\n");
    assert_eq!(
        fs::read(fixture.prepared.join("node_modules/pkg/tool")).unwrap(),
        b"dependency-original\n"
    );
    assert!(matches!(
        cache.status(&recipe, &cancel).unwrap(),
        CacheStatus::Ready(_)
    ));
    #[cfg(unix)]
    assert_eq!(
        fs::read_link(second.join("node_modules/.bin/tool")).unwrap(),
        Path::new("../pkg/tool")
    );
    let manifest = fs::read_to_string(
        cache
            .root_path()
            .join("artifacts")
            .join(recipe.key().to_string())
            .join("manifest.json"),
    )
    .unwrap();
    assert!(!manifest.contains("token-kept-out-of-manifest"));
    assert!(!manifest.contains("explicit-install-tool"));
}

#[test]
fn explicit_copy_reports_copy_and_preserves_executable_bits() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    let imported = import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    assert_eq!(imported.mode, MaterializationMode::Copy);
    assert_eq!(imported.cloned_files, 0);
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &CancellationToken::new(),
    )
    .unwrap();
    let report = cache
        .materialize(
            &recipe,
            &mut target,
            SharingPolicy::Copy,
            &CancellationToken::new(),
        )
        .unwrap();
    assert_eq!(report.mode, MaterializationMode::Copy);
    assert_eq!(report.copied_files, 2);
    #[cfg(unix)]
    assert_eq!(
        fs::metadata(target.root_path().join("node_modules/pkg/tool"))
            .unwrap()
            .mode()
            & 0o777,
        0o755
    );
    let reused = import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    assert!(reused.reused);
    assert_eq!(reused.mode, MaterializationMode::Empty);
}

#[test]
fn missing_nested_parent_does_not_change_prepublication_source_identity() {
    let fixture = Fixture::new();
    let mut spec = fixture.recipe().spec().clone();
    spec.outputs.push("new-parent/target".into());
    let recipe = Recipe::trusted(
        spec,
        TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )
    .unwrap();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &CancellationToken::new(),
    )
    .unwrap();
    cache
        .materialize(
            &recipe,
            &mut target,
            SharingPolicy::Copy,
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(target.root_path().join("new-parent/target").is_dir());
    assert!(
        fs::read_dir(target.root_path().join("new-parent/target"))
            .unwrap()
            .next()
            .is_none()
    );
}

#[test]
fn existing_untracked_destination_is_preserved_without_staging() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let root = Path::new(&fixture.first.record.root);
    fs::create_dir(root.join("node_modules")).unwrap();
    fs::write(root.join("node_modules/human"), b"unique manual dependency").unwrap();
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &CancellationToken::new(),
    )
    .unwrap();
    assert!(matches!(
        cache.materialize(
            &recipe,
            &mut target,
            SharingPolicy::PreferClone,
            &CancellationToken::new()
        ),
        Err(EnvironmentError::DestinationNotEmpty(_))
    ));
    assert_eq!(
        fs::read(root.join("node_modules/human")).unwrap(),
        b"unique manual dependency"
    );
    assert!(!root.join(".izu-recovery/environment-staging").exists());
    assert_eq!(fs::read(root.join("main.txt")).unwrap(), b"human source\n");
}

#[test]
fn empty_destination_directories_are_allowed() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    fs::create_dir(Path::new(&fixture.first.record.root).join("node_modules")).unwrap();
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &CancellationToken::new(),
    )
    .unwrap();
    cache
        .materialize(
            &recipe,
            &mut target,
            SharingPolicy::Copy,
            &CancellationToken::new(),
        )
        .unwrap();
    assert!(target.root_path().join("node_modules/pkg/tool").is_file());
}

#[cfg(unix)]
#[test]
fn cache_digest_and_same_content_inode_replacements_are_rejected() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let payload = cache
        .root_path()
        .join("artifacts")
        .join(recipe.key().to_string())
        .join("payload/0/pkg");
    let file = payload.join("tool");
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&file, b"dependency-corrupt!\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()),
        Err(EnvironmentError::Corrupt(_))
    ));
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();
    fs::write(&file, b"dependency-original\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()).unwrap(),
        CacheStatus::Ready(_)
    ));
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o755)).unwrap();
    fs::rename(&file, payload.join("old-tool")).unwrap();
    fs::write(&file, b"dependency-original\n").unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o555)).unwrap();
    fs::remove_file(payload.join("old-tool")).unwrap();
    fs::set_permissions(&payload, fs::Permissions::from_mode(0o555)).unwrap();
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()),
        Err(EnvironmentError::Corrupt(_))
    ));
    assert!(
        cache
            .import_quiescent(
                &recipe,
                &fixture.prepared,
                QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                SharingPolicy::Copy,
                &CancellationToken::new()
            )
            .is_err()
    );
}

#[test]
fn recipe_key_changes_for_each_declared_identity_and_command() {
    let original = recipe(Digest::of_bytes(b"source"));
    let spec = original.spec().clone();
    let mutations: [fn(&mut RecipeSpec); 7] = [
        |s| s.source_identity = Digest::of_bytes(b"other-source"),
        |s| s.lockfiles[0].digest = Digest::of_bytes(b"other-lock"),
        |s| s.toolchain_identity = Digest::of_bytes(b"other-toolchain"),
        |s| s.platform.abi = "other-abi".into(),
        |s| s.recipe_identity = Digest::of_bytes(b"other-recipe"),
        |s| s.trust_domain = "other-domain".into(),
        |s| s.argv.push("other-arg".into()),
    ];
    for mutate in mutations {
        let mut spec = spec.clone();
        mutate(&mut spec);
        let changed = Recipe::trusted(
            spec,
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
        )
        .unwrap();
        assert_ne!(original.key(), changed.key());
    }
    let mut overlap = spec.clone();
    overlap.outputs.push("node_modules/subdir".into());
    assert!(
        Recipe::trusted(
            overlap,
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode
        )
        .is_err()
    );
    let mut secret_domain = spec;
    secret_domain.trust_domain = "https://user:secret@host".into();
    assert!(
        Recipe::trusted(
            secret_domain,
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode
        )
        .is_err()
    );
    assert!(
        Recipe::from_json(
            &vec![b' '; 1024 * 1024 + 1],
            TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode
        )
        .is_err()
    );
}

#[test]
fn stale_lock_source_primary_and_active_writer_are_rejected() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let cancel = CancellationToken::new();
    let primary = fixture
        .repo
        .workspace(fixture.repo.workspace_id(), &cancel)
        .unwrap();
    assert!(
        WorkspaceTarget::checked(&fixture.repo, primary.id, primary.expected, &cancel).is_err()
    );
    let lease = fixture
        .repo
        .lease_workspace(fixture.first.id, fixture.first.expected, &cancel)
        .unwrap();
    assert!(matches!(
        WorkspaceTarget::checked(
            &fixture.repo,
            fixture.first.id,
            fixture.first.expected,
            &cancel
        ),
        Err(EnvironmentError::Workspace(
            izu_engine::EngineError::WorkspaceBusy { .. }
        ))
    ));
    lease.finish_stopped().unwrap();
    fs::write(
        Path::new(&fixture.first.record.root).join("package-lock.json"),
        b"other-lock",
    )
    .unwrap();
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &cancel,
    )
    .unwrap();
    assert!(matches!(
        cache.materialize(&recipe, &mut target, SharingPolicy::Copy, &cancel),
        Err(EnvironmentError::SourceMismatch)
    ));
    let mut altered = recipe.spec().clone();
    altered.source_identity = target.source_identity();
    let altered = Recipe::trusted(
        altered,
        TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )
    .unwrap();
    assert!(matches!(
        cache.materialize(&altered, &mut target, SharingPolicy::Copy, &cancel),
        Err(EnvironmentError::LockfileMismatch(_))
    ));
}

#[cfg(unix)]
#[test]
fn prepared_symlink_parents_and_escaping_targets_are_refused() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    fs::remove_file(fixture.prepared.join("node_modules/.bin/tool")).unwrap();
    symlink(
        "../../../outside",
        fixture.prepared.join("node_modules/.bin/tool"),
    )
    .unwrap();
    assert!(
        cache
            .import_quiescent(
                &recipe,
                &fixture.prepared,
                QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                SharingPolicy::Copy,
                &CancellationToken::new()
            )
            .is_err()
    );
    fs::remove_file(fixture.prepared.join("node_modules/.bin/tool")).unwrap();
    symlink(
        "../pkg/tool",
        fixture.prepared.join("node_modules/.bin/tool"),
    )
    .unwrap();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("unique"), b"untouched").unwrap();
    fs::rename(
        fixture.prepared.join("node_modules"),
        fixture.prepared.join("original-deps"),
    )
    .unwrap();
    symlink(&outside, fixture.prepared.join("node_modules")).unwrap();
    assert!(
        cache
            .import_quiescent(
                &recipe,
                &fixture.prepared,
                QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                SharingPolicy::Copy,
                &CancellationToken::new()
            )
            .is_err()
    );
    assert_eq!(fs::read(outside.join("unique")).unwrap(), b"untouched");
}

#[cfg(unix)]
#[test]
fn destination_symlink_parent_cannot_escape_workspace() {
    let fixture = Fixture::new();
    let mut spec = fixture.recipe().spec().clone();
    spec.dependencies = vec!["vendor/node_modules".into()];
    fs::create_dir(fixture.prepared.join("vendor")).unwrap();
    fs::rename(
        fixture.prepared.join("node_modules"),
        fixture.prepared.join("vendor/node_modules"),
    )
    .unwrap();
    let outside = fixture.temp.path().join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("unique"), b"preserved").unwrap();
    symlink(
        &outside,
        Path::new(&fixture.first.record.root).join("vendor"),
    )
    .unwrap();
    let mut target = WorkspaceTarget::checked(
        &fixture.repo,
        fixture.first.id,
        fixture.first.expected,
        &CancellationToken::new(),
    )
    .unwrap();
    spec.source_identity = target.source_identity();
    let recipe = Recipe::trusted(
        spec,
        TrustAcknowledgement::ExplicitlyTrustRecipeAndPreparedCode,
    )
    .unwrap();
    let cache = fixture.cache();
    import(&cache, &recipe, &fixture.prepared, SharingPolicy::Copy);
    let result = cache.materialize(
        &recipe,
        &mut target,
        SharingPolicy::Copy,
        &CancellationToken::new(),
    );
    assert!(
        matches!(result, Err(EnvironmentError::DestinationNotEmpty(_))),
        "symlink destination was not refused by destination validation: {result:?}",
    );
    assert!(!outside.join("node_modules").exists());
    assert_eq!(fs::read(outside.join("unique")).unwrap(), b"preserved");
}

#[test]
fn bounded_input_cancel_disk_pressure_and_incomplete_stages() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let cache = fixture.cache();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        cache.import_quiescent(
            &recipe,
            &fixture.prepared,
            QuiescenceAcknowledgement::CallerConfirmsNoWriters,
            SharingPolicy::Copy,
            &cancel
        ),
        Err(EnvironmentError::Cancelled)
    ));
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()).unwrap(),
        CacheStatus::Missing { .. }
    ));
    let small = EnvironmentCache::open(
        fixture.temp.path().join("small"),
        EnvironmentLimits {
            max_file_bytes: 1,
            ..test_limits()
        },
    )
    .unwrap();
    assert!(matches!(
        small.import_quiescent(
            &recipe,
            &fixture.prepared,
            QuiescenceAcknowledgement::CallerConfirmsNoWriters,
            SharingPolicy::Copy,
            &CancellationToken::new()
        ),
        Err(EnvironmentError::Limit { .. })
    ));
    let pressure = EnvironmentCache::open(
        fixture.temp.path().join("pressure"),
        EnvironmentLimits {
            min_free_bytes: u64::MAX / 2,
            ..test_limits()
        },
    )
    .unwrap();
    assert!(matches!(
        pressure.import_quiescent(
            &recipe,
            &fixture.prepared,
            QuiescenceAcknowledgement::CallerConfirmsNoWriters,
            SharingPolicy::Copy,
            &CancellationToken::new()
        ),
        Err(EnvironmentError::Limit { .. })
    ));
    let incomplete = EnvironmentCache::open(
        fixture.temp.path().join("incomplete"),
        EnvironmentLimits {
            max_manifest_bytes: 64,
            ..test_limits()
        },
    )
    .unwrap();
    assert!(matches!(
        incomplete.import_quiescent(
            &recipe,
            &fixture.prepared,
            QuiescenceAcknowledgement::CallerConfirmsNoWriters,
            SharingPolicy::Copy,
            &CancellationToken::new()
        ),
        Err(EnvironmentError::PreparationIncomplete { .. })
    ));
    assert!(matches!(
        incomplete
            .status(&recipe, &CancellationToken::new())
            .unwrap(),
        CacheStatus::Incomplete {
            retained_stages: 1,
            ..
        }
    ));
    assert_eq!(
        fs::read(fixture.prepared.join("node_modules/pkg/tool")).unwrap(),
        b"dependency-original\n"
    );
}

#[test]
fn cancellation_during_copy_retains_incomplete_stage_and_input() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let data = vec![29_u8; 256 * 1024];
    for index in 0..50 {
        fs::write(
            fixture
                .prepared
                .join("node_modules")
                .join(format!("cancel-{index}")),
            &data,
        )
        .unwrap();
    }
    let cache = fixture.cache();
    let cancel = CancellationToken::new();
    let result = std::thread::scope(|scope| {
        let handle = scope.spawn(|| {
            cache.import_quiescent(
                &recipe,
                &fixture.prepared,
                QuiescenceAcknowledgement::CallerConfirmsNoWriters,
                SharingPolicy::Copy,
                &cancel,
            )
        });
        let started = Instant::now();
        let mut observed = false;
        while started.elapsed() < Duration::from_secs(5) {
            for entry in fs::read_dir(cache.root_path().join("artifacts"))
                .unwrap()
                .flatten()
            {
                if entry.path().join("BUILDING").exists() {
                    observed = true;
                    break;
                }
            }
            if observed {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        cancel.cancel();
        let result = handle.join().unwrap();
        assert!(observed);
        result
    });
    assert!(
        matches!(result, Err(EnvironmentError::PreparationIncomplete { source, .. }) if matches!(*source, EnvironmentError::Cancelled))
    );
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()).unwrap(),
        CacheStatus::Incomplete {
            retained_stages: 1,
            ..
        }
    ));
    assert_eq!(
        fs::read(fixture.prepared.join("node_modules/cancel-0")).unwrap(),
        data
    );
}

#[test]
fn concurrent_importers_publish_one_verified_artifact() {
    let fixture = Fixture::new();
    let recipe = fixture.recipe();
    let path = fixture.temp.path().join("cache");
    let receipts = std::thread::scope(|scope| {
        let mut handles = Vec::new();
        for _ in 0..2 {
            let recipe = &recipe;
            let prepared = &fixture.prepared;
            let path = &path;
            handles.push(scope.spawn(move || {
                import(
                    &EnvironmentCache::open(path, test_limits()).unwrap(),
                    recipe,
                    prepared,
                    SharingPolicy::Copy,
                )
            }));
        }
        handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(receipts.iter().filter(|receipt| !receipt.reused).count(), 1);
    assert_eq!(receipts[0].artifact, receipts[1].artifact);
    assert!(matches!(
        EnvironmentCache::open(&path, test_limits())
            .unwrap()
            .status(&recipe, &CancellationToken::new())
            .unwrap(),
        CacheStatus::Ready(_)
    ));
}

#[test]
#[ignore = "invoked by killed_preparation_never_becomes_ready"]
fn child_import() {
    let root = PathBuf::from(std::env::var_os("IZU_ENV_CHILD_ROOT").unwrap());
    let recipe = recipe(Digest::of_bytes(b"child-source"));
    import(
        &EnvironmentCache::open(root.join("cache"), test_limits()).unwrap(),
        &recipe,
        &root.join("prepared"),
        SharingPolicy::Copy,
    );
}

#[test]
fn killed_preparation_never_becomes_ready() {
    let temp = owned_temp();
    let prepared = temp.path().join("prepared");
    fs::create_dir_all(prepared.join("node_modules")).unwrap();
    fs::write(prepared.join("package-lock.json"), b"lock-v1").unwrap();
    let data = vec![73_u8; 256 * 1024];
    for index in 0..100 {
        fs::write(
            prepared.join("node_modules").join(format!("dep-{index}")),
            &data,
        )
        .unwrap();
    }
    let recipe = recipe(Digest::of_bytes(b"child-source"));
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "child_import", "--ignored", "--nocapture"])
        .env("IZU_ENV_CHILD_ROOT", temp.path())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let started = Instant::now();
    let artifacts = temp.path().join("cache/artifacts");
    let mut observed = false;
    while started.elapsed() < Duration::from_secs(5) {
        if let Ok(entries) = fs::read_dir(&artifacts) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".build-"))
                    && entry.path().join("BUILDING").exists()
                {
                    observed = true;
                    break;
                }
            }
        }
        if observed {
            break;
        }
        if child.try_wait().unwrap().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    if !observed {
        let _ = child.kill();
        let _ = child.wait();
    }
    assert!(observed, "child did not expose its retained building state");
    child.kill().unwrap();
    child.wait().unwrap();
    let cache = EnvironmentCache::open(temp.path().join("cache"), test_limits()).unwrap();
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()).unwrap(),
        CacheStatus::Incomplete {
            retained_stages: 1,
            ..
        }
    ));
    let receipt = import(&cache, &recipe, &prepared, SharingPolicy::Copy);
    assert!(!receipt.reused);
    assert!(matches!(
        cache.status(&recipe, &CancellationToken::new()).unwrap(),
        CacheStatus::Ready(_)
    ));
    assert!(fs::read_dir(&artifacts).unwrap().flatten().any(|entry| {
        entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with(".build-"))
    }));
    make_writable(temp.path());
}
