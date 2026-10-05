use crate::cache::{json_bytes, verify_identity_lockfiles};
use crate::error::{check, io};
use crate::filesystem::*;
use crate::recipe::{ArtifactRequest, PersistedIdentity};
use crate::{
    CancellationToken, Digest, EnvironmentCache, EnvironmentError, MaterializationReport, Recipe,
    Result, SharingPolicy, WorkspaceTarget,
};
use izu_engine::{Repository, Selection, WorkspaceWriterLease};
use izu_model::WorkspaceId;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
use std::fs::File;
#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

pub const MAX_ENVIRONMENT_BINDING_BYTES: usize = 1024 * 1024;
const BINDING_VERSION: u32 = 1;

/// File contents observed before a command starts. Warm outputs remain writable
/// during execution; this scope never attests an immutable execution environment.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum EnvironmentVerificationScope {
    StartingFileContents,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BindingRecord {
    schema_version: u32,
    scope: EnvironmentVerificationScope,
    identity: PersistedIdentity,
    key: Digest,
    manifest_digest: Digest,
}

/// Bounded data to store as a native engine Blob. The Blob's native object ID is
/// distinct from this binding's cache key. Parsing does not execute preparation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EnvironmentBinding {
    record: BindingRecord,
}

impl EnvironmentBinding {
    pub fn from_json(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_ENVIRONMENT_BINDING_BYTES {
            return Err(EnvironmentError::Limit {
                resource: "environment binding bytes",
                limit: MAX_ENVIRONMENT_BINDING_BYTES as u64,
            });
        }
        let mut record: BindingRecord = serde_json::from_slice(bytes)?;
        if record.schema_version != BINDING_VERSION {
            return Err(EnvironmentError::InvalidInput(
                "unsupported environment binding schema".into(),
            ));
        }
        record.identity.normalize_and_validate()?;
        if record.identity.key()? != record.key {
            return Err(EnvironmentError::InvalidInput(
                "environment binding key does not match recipe identity".into(),
            ));
        }
        Ok(Self { record })
    }
    pub fn to_json(&self) -> Result<Vec<u8>> {
        json_bytes(&self.record, MAX_ENVIRONMENT_BINDING_BYTES)
    }
    pub fn key(&self) -> Digest {
        self.record.key
    }
    pub fn manifest_digest(&self) -> Digest {
        self.record.manifest_digest
    }
    pub fn source_identity(&self) -> Digest {
        self.record.identity.source_identity
    }
    pub(crate) fn request(&self) -> ArtifactRequest<'_> {
        ArtifactRequest::new(&self.record.identity, self.key())
    }
}

/// The running OS/architecture are observed; ABI remains a caller declaration.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ObservedEnvironmentPlatform {
    pub observed_os: String,
    pub observed_architecture: String,
    pub declared_abi: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct DeclaredEnvironmentToolchain {
    pub declared_identity: Digest,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct StartingEnvironmentReceipt {
    pub key: Digest,
    pub manifest_digest: Digest,
    pub source_identity: Digest,
    pub workspace: WorkspaceId,
    pub observed_content_digest: Digest,
    pub entries: u64,
    pub logical_bytes: u64,
    pub scope: EnvironmentVerificationScope,
    pub platform: ObservedEnvironmentPlatform,
    pub toolchain: DeclaredEnvironmentToolchain,
    pub security_boundary: bool,
}

/// Only the verifier can construct this proof. It owns the cache's shared lease;
/// callers can retain it through process-group reap without retaining inventories.
#[must_use]
pub struct StartingEnvironmentProof {
    _lease: File,
    receipt: StartingEnvironmentReceipt,
}
impl StartingEnvironmentProof {
    pub fn receipt(&self) -> &StartingEnvironmentReceipt {
        &self.receipt
    }
}

impl EnvironmentCache {
    pub fn binding(
        &self,
        recipe: &Recipe,
        cancel: &CancellationToken,
    ) -> Result<EnvironmentBinding> {
        let artifact = self.borrow_artifact(recipe, cancel)?;
        let binding = EnvironmentBinding {
            record: BindingRecord {
                schema_version: BINDING_VERSION,
                scope: EnvironmentVerificationScope::StartingFileContents,
                identity: recipe.identity().clone(),
                key: recipe.key(),
                manifest_digest: artifact.summary.manifest_digest,
            },
        };
        // Refuse definitions whose encoded binding exceeds the wire limit.
        binding.to_json()?;
        self.verify_artifact_binding(&artifact)?;
        Ok(binding)
    }

    pub fn materialize_binding(
        &self,
        binding: &EnvironmentBinding,
        target: &mut WorkspaceTarget<'_>,
        policy: SharingPolicy,
        cancel: &CancellationToken,
    ) -> Result<MaterializationReport> {
        self.materialize_request(
            binding.request(),
            Some(binding.manifest_digest()),
            target,
            policy,
            cancel,
        )
    }

    pub fn verify_starting_environment(
        &self,
        binding: &EnvironmentBinding,
        repository: &Repository,
        lease: &WorkspaceWriterLease,
        cancel: &CancellationToken,
    ) -> Result<StartingEnvironmentProof> {
        check(cancel)?;
        let captured = repository.capture_leased(lease, Selection::All, cancel)?;
        if Digest::from_hex(&captured.tree.to_string())? != binding.source_identity() {
            return Err(EnvironmentError::SourceMismatch);
        }
        let identity = &binding.record.identity;
        verify_identity_lockfiles(lease.source_directory(), identity, &self.limits, cancel)?;
        let artifact = self.borrow_request(binding.request(), cancel)?;
        if artifact.summary.manifest_digest != binding.manifest_digest() {
            return Err(EnvironmentError::Corrupt(
                "binding manifest digest differs from verified artifact".into(),
            ));
        }
        let inventory = collect_declared(
            lease.source_directory(),
            identity,
            &self.limits,
            cancel,
            false,
        )?;
        if inventory.len() != artifact.manifest.entries.len() {
            return Err(EnvironmentError::StartingEnvironmentMismatch(
                "declared root entry count differs".into(),
            ));
        }
        let paths: Vec<_> = identity
            .dependencies
            .iter()
            .chain(&identity.outputs)
            .collect();
        let mut content = Sha256::new();
        content.update(b"izu-starting-file-contents-v1\0");
        let mut logical = 0_u64;
        for (observed, expected) in inventory.iter().zip(&artifact.manifest.entries) {
            check(cancel)?;
            if observed.root != expected.root || observed.path != expected.path {
                return Err(EnvironmentError::StartingEnvironmentMismatch(
                    "declared root entry name/order differs".into(),
                ));
            }
            content.update(observed.root.to_le_bytes());
            hash_text(&mut content, &observed.path);
            match (&observed.kind, &expected.node) {
                (InventoryKind::Directory { mode }, ManifestKind::Directory { mode: original })
                    if *mode == (*original | 0o700) =>
                {
                    content.update(b"d");
                    content.update(mode.to_le_bytes());
                }
                (
                    InventoryKind::File { mode, bytes },
                    ManifestKind::File {
                        mode: original,
                        bytes: expected_bytes,
                        digest,
                    },
                ) if *mode == (*original | 0o200) && bytes == expected_bytes => {
                    let path = paths.get(observed.root as usize).ok_or_else(|| {
                        EnvironmentError::Corrupt("manifest root index exceeds recipe".into())
                    })?;
                    let directory = open_directory(lease.source_directory(), path)?;
                    let mut file = open_regular(&directory, &observed.path)?;
                    let metadata = file.metadata().map_err(|error| io(&observed.path, error))?;
                    let before = stamp(&metadata)?;
                    if observed.stamp.as_ref() != Some(&before)
                        || before.identity == expected.identity
                    {
                        return Err(EnvironmentError::StartingEnvironmentMismatch(format!(
                            "private file identity differs or aliases artifact: {}",
                            observed.path
                        )));
                    }
                    #[cfg(unix)]
                    if metadata.nlink() != 1 {
                        return Err(EnvironmentError::StartingEnvironmentMismatch(format!(
                            "private file has a hard-link alias: {}",
                            observed.path
                        )));
                    }
                    let (actual, actual_bytes) =
                        hash_file(&mut file, self.limits.max_file_bytes, cancel)?;
                    if actual != *digest
                        || actual_bytes != *expected_bytes
                        || stamp(&file.metadata().map_err(|error| io(&observed.path, error))?)?
                            != before
                    {
                        return Err(EnvironmentError::StartingEnvironmentMismatch(format!(
                            "private file digest changed: {}",
                            observed.path
                        )));
                    }
                    logical = logical
                        .checked_add(actual_bytes)
                        .ok_or(EnvironmentError::Limit {
                            resource: "starting environment bytes",
                            limit: self.limits.max_artifact_bytes,
                        })?;
                    content.update(b"f");
                    content.update(mode.to_le_bytes());
                    content.update(actual_bytes.to_le_bytes());
                    content.update(actual.bytes());
                }
                (InventoryKind::Symlink { target }, ManifestKind::Symlink { target: expected })
                    if target == expected =>
                {
                    content.update(b"l");
                    hash_text(&mut content, target);
                }
                _ => {
                    return Err(EnvironmentError::StartingEnvironmentMismatch(format!(
                        "private entry type/mode/size/target differs: {}",
                        observed.path
                    )));
                }
            }
        }
        if logical != artifact.summary.logical_bytes {
            return Err(EnvironmentError::StartingEnvironmentMismatch(
                "private byte total differs".into(),
            ));
        }
        if collect_declared(
            lease.source_directory(),
            identity,
            &self.limits,
            cancel,
            false,
        )? != inventory
        {
            return Err(EnvironmentError::StartingEnvironmentMismatch(
                "private inventory changed during verification".into(),
            ));
        }
        verify_identity_lockfiles(lease.source_directory(), identity, &self.limits, cancel)?;
        let after = repository.capture_leased(lease, Selection::All, cancel)?;
        if after.tree != captured.tree || after.expected != captured.expected {
            return Err(EnvironmentError::SourceMismatch);
        }
        self.verify_artifact_binding(&artifact)?;
        Ok(StartingEnvironmentProof {
            _lease: artifact._lease,
            receipt: StartingEnvironmentReceipt {
                key: binding.key(),
                manifest_digest: binding.manifest_digest(),
                source_identity: binding.source_identity(),
                workspace: lease.state().id,
                observed_content_digest: Digest::from_hasher(content),
                entries: inventory.len() as u64,
                logical_bytes: logical,
                scope: EnvironmentVerificationScope::StartingFileContents,
                platform: ObservedEnvironmentPlatform {
                    observed_os: std::env::consts::OS.into(),
                    observed_architecture: std::env::consts::ARCH.into(),
                    declared_abi: identity.platform.abi.clone(),
                },
                toolchain: DeclaredEnvironmentToolchain {
                    declared_identity: identity.toolchain_identity,
                },
                security_boundary: false,
            },
        })
    }
}

fn hash_text(content: &mut Sha256, text: &str) {
    content.update((text.len() as u64).to_le_bytes());
    content.update(text.as_bytes());
}
