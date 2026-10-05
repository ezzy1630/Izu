use crate::cache::json_bytes;
use crate::{EnvironmentError, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest as _, Sha256};
use std::fmt;

pub const RECIPE_VERSION: u32 = 1;
const HARD_METADATA_BYTES: usize = 64 * 1024 * 1024;
const MAX_RECIPE_BYTES: usize = 1024 * 1024;

/// A SHA-256 identity; values are persisted only as lowercase hexadecimal.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    pub fn of_bytes(bytes: &[u8]) -> Self {
        Self(Sha256::digest(bytes).into())
    }
    pub fn from_hex(value: &str) -> Result<Self> {
        if value.len() != 64 {
            return Err(EnvironmentError::InvalidInput(
                "SHA-256 identity must have 64 lowercase hexadecimal characters".into(),
            ));
        }
        let mut bytes = [0; 32];
        for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
            fn digit(value: u8) -> Option<u8> {
                match value {
                    b'0'..=b'9' => Some(value - b'0'),
                    b'a'..=b'f' => Some(value - b'a' + 10),
                    _ => None,
                }
            }
            let high = digit(pair[0])
                .ok_or_else(|| EnvironmentError::InvalidInput("invalid SHA-256 identity".into()))?;
            let low = digit(pair[1])
                .ok_or_else(|| EnvironmentError::InvalidInput("invalid SHA-256 identity".into()))?;
            bytes[index] = (high << 4) | low;
        }
        Ok(Self(bytes))
    }
    pub(crate) fn from_hasher(hasher: Sha256) -> Self {
        Self(hasher.finalize().into())
    }
    pub(crate) fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}
impl Serialize for Digest {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}
impl<'de> Deserialize<'de> for Digest {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let value = String::deserialize(deserializer)?;
        Self::from_hex(&value).map_err(serde::de::Error::custom)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlatformIdentity {
    pub os: String,
    pub architecture: String,
    pub abi: String,
}

impl PlatformIdentity {
    pub fn current(abi: impl Into<String>) -> Self {
        Self {
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            abi: abi.into(),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LockfileIdentity {
    pub path: String,
    pub digest: Digest,
}

/// Explicit input. `argv` is execution intent, never an instruction to execute on
/// parse/import/materialize. No raw argv or environment values are persisted.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecipeSpec {
    pub schema_version: u32,
    pub source_identity: Digest,
    pub lockfiles: Vec<LockfileIdentity>,
    pub toolchain_identity: Digest,
    pub platform: PlatformIdentity,
    pub recipe_identity: Digest,
    pub trust_domain: String,
    pub argv: Vec<String>,
    pub dependencies: Vec<String>,
    pub outputs: Vec<String>,
}

/// The caller explicitly vouches for the recipe and prepared code. This is a
/// cooperative trust contract, not a permission, signature or sandbox claim.
#[derive(Clone, Copy, Debug)]
pub enum TrustAcknowledgement {
    ExplicitlyTrustRecipeAndPreparedCode,
}

/// Import cannot discover unknown writers. The caller stops processes/watchers
/// and database writers before passing this acknowledgement.
#[derive(Clone, Copy, Debug)]
pub enum QuiescenceAcknowledgement {
    CallerConfirmsNoWriters,
}

#[derive(Clone, Debug)]
pub struct Recipe {
    spec: RecipeSpec,
    identity: PersistedIdentity,
    key: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PersistedIdentity {
    pub schema_version: u32,
    pub source_identity: Digest,
    pub lockfiles: Vec<LockfileIdentity>,
    pub toolchain_identity: Digest,
    pub platform: PlatformIdentity,
    pub recipe_identity: Digest,
    pub trust_domain: String,
    pub argv_digest: Digest,
    pub dependencies: Vec<String>,
    pub outputs: Vec<String>,
}

#[derive(Clone, Copy)]
pub(crate) struct ArtifactRequest<'a> {
    identity: &'a PersistedIdentity,
    key: Digest,
}

impl<'a> ArtifactRequest<'a> {
    pub(crate) fn new(identity: &'a PersistedIdentity, key: Digest) -> Self {
        Self { identity, key }
    }
    pub(crate) fn key(self) -> Digest {
        self.key
    }
    pub(crate) fn identity(self) -> &'a PersistedIdentity {
        self.identity
    }
    pub(crate) fn declared_paths(self) -> impl Iterator<Item = &'a str> {
        self.identity
            .dependencies
            .iter()
            .chain(&self.identity.outputs)
            .map(String::as_str)
    }
}

impl PersistedIdentity {
    pub(crate) fn normalize_and_validate(&mut self) -> Result<()> {
        if self.schema_version != RECIPE_VERSION {
            return Err(EnvironmentError::InvalidRecipe(
                "unsupported schema version".into(),
            ));
        }
        slug(&self.trust_domain, "trust domain")?;
        slug(&self.platform.os, "platform OS")?;
        slug(&self.platform.architecture, "platform architecture")?;
        slug(&self.platform.abi, "platform ABI")?;
        if self.dependencies.len().saturating_add(self.outputs.len()) == 0
            || self.dependencies.len().saturating_add(self.outputs.len()) > 128
        {
            return Err(EnvironmentError::InvalidRecipe(
                "declare between 1 and 128 dependency/output directories".into(),
            ));
        }
        self.dependencies.sort();
        self.outputs.sort();
        let mut paths = Vec::new();
        paths
            .try_reserve(self.dependencies.len() + self.outputs.len())
            .map_err(|_| EnvironmentError::Allocation("declared paths"))?;
        for path in self.dependencies.iter().chain(&self.outputs) {
            validate_path(path)?;
            paths.push(namespace_key(path)?);
        }
        paths.sort_unstable();
        for pair in paths.windows(2) {
            if pair[0] == pair[1]
                || pair[1]
                    .strip_prefix(pair[0].as_str())
                    .is_some_and(|suffix| suffix.starts_with('/'))
            {
                return Err(EnvironmentError::InvalidRecipe(
                    "declared paths overlap".into(),
                ));
            }
        }
        if self.lockfiles.len() > 128 {
            return Err(EnvironmentError::InvalidRecipe(
                "at most 128 lockfiles may be declared".into(),
            ));
        }
        self.lockfiles.sort_by(|a, b| a.path.cmp(&b.path));
        let mut lock_keys = Vec::new();
        lock_keys
            .try_reserve(self.lockfiles.len())
            .map_err(|_| EnvironmentError::Allocation("lockfile namespace keys"))?;
        for file in &self.lockfiles {
            validate_path(&file.path)?;
            let key = namespace_key(&file.path)?;
            if paths.iter().any(|path| {
                key == *path
                    || key
                        .strip_prefix(path.as_str())
                        .is_some_and(|suffix| suffix.starts_with('/'))
            }) {
                return Err(EnvironmentError::InvalidRecipe(
                    "lockfiles cannot lie in materialized directories".into(),
                ));
            }
            lock_keys.push(key);
        }
        lock_keys.sort_unstable();
        if lock_keys.windows(2).any(|pair| pair[0] == pair[1]) {
            return Err(EnvironmentError::InvalidRecipe(
                "duplicate lockfile path".into(),
            ));
        }
        validate_ancestor_spelling(self)
    }
    pub(crate) fn key(&self) -> Result<Digest> {
        Ok(Digest::of_bytes(&json_bytes(self, MAX_RECIPE_BYTES * 2)?))
    }
}

fn validate_ancestor_spelling(identity: &PersistedIdentity) -> Result<()> {
    let mut paths = Vec::new();
    paths
        .try_reserve(
            identity.dependencies.len() + identity.outputs.len() + identity.lockfiles.len(),
        )
        .map_err(|_| EnvironmentError::Allocation("ancestor namespace keys"))?;
    for path in identity
        .dependencies
        .iter()
        .chain(&identity.outputs)
        .map(String::as_str)
        .chain(identity.lockfiles.iter().map(|file| file.path.as_str()))
    {
        paths.push((namespace_key(path)?, path));
    }
    paths.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    // Common canonical ancestors are adjacent after sorting. Compare components
    // without allocating every full prefix of a deeply nested path.
    for pair in paths.windows(2) {
        let left = pair[0].0.split('/').zip(pair[0].1.split('/'));
        let right = pair[1].0.split('/').zip(pair[1].1.split('/'));
        for ((left_key, left_spelling), (right_key, right_spelling)) in left.zip(right) {
            if left_key != right_key {
                break;
            }
            if left_spelling != right_spelling {
                return Err(EnvironmentError::InvalidRecipe(
                    "declared paths use aliased ancestor spellings".into(),
                ));
            }
        }
    }
    Ok(())
}

fn namespace_key(path: &str) -> Result<String> {
    izu_model::RepoPath::new(path)
        .and_then(|path| path.namespace_key())
        .map_err(|error| EnvironmentError::InvalidRecipe(error.to_string()))
}

impl Recipe {
    pub fn from_json(bytes: &[u8], trust: TrustAcknowledgement) -> Result<Self> {
        if bytes.len() > MAX_RECIPE_BYTES {
            return Err(EnvironmentError::Limit {
                resource: "recipe bytes",
                limit: MAX_RECIPE_BYTES as u64,
            });
        }
        Self::trusted(serde_json::from_slice(bytes)?, trust)
    }

    pub fn trusted(spec: RecipeSpec, _: TrustAcknowledgement) -> Result<Self> {
        if spec.argv.is_empty() || spec.argv.len() > 256 || spec.argv[0].is_empty() {
            return Err(EnvironmentError::InvalidRecipe(
                "explicit argv must have 1..=256 arguments and a nonempty program".into(),
            ));
        }
        let mut argument_bytes = 0_usize;
        for argument in &spec.argv {
            if argument.contains('\0') {
                return Err(EnvironmentError::InvalidRecipe(
                    "argv contains a NUL byte".into(),
                ));
            }
            argument_bytes = argument_bytes
                .checked_add(argument.len())
                .ok_or(EnvironmentError::Allocation("argv"))?;
            if argument_bytes > 64 * 1024 {
                return Err(EnvironmentError::Limit {
                    resource: "argv bytes",
                    limit: 64 * 1024,
                });
            }
        }
        let argv_digest = Digest::of_bytes(&json_bytes(&spec.argv, 512 * 1024)?);
        let mut identity = PersistedIdentity {
            schema_version: spec.schema_version,
            source_identity: spec.source_identity,
            lockfiles: spec.lockfiles,
            toolchain_identity: spec.toolchain_identity,
            platform: spec.platform,
            recipe_identity: spec.recipe_identity,
            trust_domain: spec.trust_domain,
            argv_digest,
            dependencies: spec.dependencies,
            outputs: spec.outputs,
        };
        identity.normalize_and_validate()?;
        let spec = RecipeSpec {
            schema_version: identity.schema_version,
            source_identity: identity.source_identity,
            lockfiles: identity.lockfiles.clone(),
            toolchain_identity: identity.toolchain_identity,
            platform: identity.platform.clone(),
            recipe_identity: identity.recipe_identity,
            trust_domain: identity.trust_domain.clone(),
            argv: spec.argv,
            dependencies: identity.dependencies.clone(),
            outputs: identity.outputs.clone(),
        };
        let key = identity.key()?;
        Ok(Self {
            spec,
            identity,
            key,
        })
    }

    pub fn key(&self) -> Digest {
        self.key
    }
    pub fn spec(&self) -> &RecipeSpec {
        &self.spec
    }
    pub fn declared_paths(&self) -> impl Iterator<Item = &str> {
        self.spec
            .dependencies
            .iter()
            .chain(&self.spec.outputs)
            .map(String::as_str)
    }
    pub(crate) fn identity(&self) -> &PersistedIdentity {
        &self.identity
    }
    pub(crate) fn request(&self) -> ArtifactRequest<'_> {
        ArtifactRequest::new(&self.identity, self.key)
    }
}

fn slug(value: &str, field: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        return Err(EnvironmentError::InvalidRecipe(format!(
            "{field} must be a bounded non-secret identifier using letters, digits, dot, underscore or hyphen"
        )));
    }
    Ok(())
}

pub(crate) fn validate_path(value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > 4096
        || value.starts_with('/')
        || value.contains('\0')
        || value.contains('\\')
    {
        return Err(EnvironmentError::InvalidRecipe(
            "paths must be bounded relative UTF-8 paths".into(),
        ));
    }
    for component in value.split('/') {
        if component.is_empty()
            || component == "."
            || component == ".."
            || component.len() > 255
            || component.eq_ignore_ascii_case(".izu")
            || component.eq_ignore_ascii_case(".git")
            || component.eq_ignore_ascii_case(".izu-recovery")
        {
            return Err(EnvironmentError::InvalidRecipe(
                "path contains an unsupported or reserved component".into(),
            ));
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct EnvironmentLimits {
    pub max_entries: usize,
    pub max_depth: usize,
    pub max_file_bytes: u64,
    pub max_artifact_bytes: u64,
    pub max_cache_bytes: u64,
    pub max_manifest_bytes: usize,
    pub min_free_bytes: u64,
    pub lock_timeout: std::time::Duration,
}

impl Default for EnvironmentLimits {
    fn default() -> Self {
        Self {
            max_entries: 1_000_000,
            max_depth: 128,
            max_file_bytes: 32 * 1024 * 1024 * 1024,
            max_artifact_bytes: 64 * 1024 * 1024 * 1024,
            max_cache_bytes: 256 * 1024 * 1024 * 1024,
            max_manifest_bytes: 32 * 1024 * 1024,
            min_free_bytes: 256 * 1024 * 1024,
            lock_timeout: std::time::Duration::from_secs(30),
        }
    }
}

impl EnvironmentLimits {
    pub(crate) fn validate(&self) -> Result<()> {
        if self.max_entries == 0
            || self.max_entries > 4_000_000
            || self.max_depth == 0
            || self.max_depth > 256
            || self.max_manifest_bytes == 0
            || self.max_manifest_bytes > HARD_METADATA_BYTES
        {
            return Err(EnvironmentError::InvalidInput(
                "limits exceed metadata/traversal hard ceilings".into(),
            ));
        }
        if self.max_artifact_bytes > self.max_cache_bytes
            || self.lock_timeout > std::time::Duration::from_secs(60)
        {
            return Err(EnvironmentError::InvalidInput(
                "artifact limit must fit cache quota; lock timeout must be at most 60 seconds"
                    .into(),
            ));
        }
        Ok(())
    }
}
