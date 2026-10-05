use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::error::check_u64;
use crate::{ModelError, ObjectKind};

pub(crate) const HARD_METADATA_BYTES: u64 = 64 * 1024 * 1024;
pub(crate) const HARD_PATH_BYTES: usize = 4096;
pub(crate) const HARD_SYMLINK_BYTES: usize = 4096;
pub(crate) const HARD_NAME_BYTES: usize = 255;

/// Resource policy is separate from object identity. Lower limits are supported;
/// metadata has a hard ceiling because decoding must allocate its full payload.
#[derive(Clone, Debug)]
pub struct Limits {
    pub max_blob_bytes: u64,
    pub max_metadata_bytes: u64,
    pub max_tree_entries: usize,
    pub max_revisions_per_change: usize,
    pub max_revision_parents: usize,
    pub max_workspaces: usize,
    pub max_refs: usize,
    pub max_changes: usize,
    pub max_candidates: usize,
    pub max_evidence: usize,
    pub max_text_bytes: usize,
    pub max_path_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_blob_bytes: 64 * 1024 * 1024 * 1024,
            max_metadata_bytes: 16 * 1024 * 1024,
            max_tree_entries: 250_000,
            max_revisions_per_change: 1024,
            max_revision_parents: 64,
            max_workspaces: 10_000,
            max_refs: 100_000,
            max_changes: 250_000,
            max_candidates: 100_000,
            max_evidence: 250_000,
            max_text_bytes: 1024 * 1024,
            max_path_bytes: HARD_PATH_BYTES,
        }
    }
}

impl Limits {
    pub(crate) fn decoding_ceiling() -> Self {
        // Used by serde constructors, after the native decoder has bounded the
        // bytes. The caller's actual policy is checked by decode_metadata.
        Self {
            max_metadata_bytes: HARD_METADATA_BYTES,
            max_tree_entries: HARD_METADATA_BYTES as usize,
            max_revisions_per_change: HARD_METADATA_BYTES as usize,
            max_revision_parents: HARD_METADATA_BYTES as usize,
            max_workspaces: HARD_METADATA_BYTES as usize,
            max_refs: HARD_METADATA_BYTES as usize,
            max_changes: HARD_METADATA_BYTES as usize,
            max_candidates: HARD_METADATA_BYTES as usize,
            max_evidence: HARD_METADATA_BYTES as usize,
            max_text_bytes: HARD_METADATA_BYTES as usize,
            ..Self::default()
        }
    }

    pub fn validate(&self) -> Result<(), ModelError> {
        if self.max_metadata_bytes == 0 || self.max_metadata_bytes > HARD_METADATA_BYTES {
            return Err(ModelError::InvalidLimits {
                reason: "metadata limit must be 1..=67108864 bytes",
            });
        }
        if self.max_path_bytes == 0 || self.max_path_bytes > HARD_PATH_BYTES {
            return Err(ModelError::InvalidLimits {
                reason: "path limit must be 1..=4096 bytes",
            });
        }
        if u64::try_from(self.max_text_bytes).map_or(true, |n| n > HARD_METADATA_BYTES) {
            return Err(ModelError::InvalidLimits {
                reason: "text limit exceeds the metadata ceiling",
            });
        }
        Ok(())
    }

    pub fn max_payload(&self, kind: ObjectKind) -> u64 {
        match kind {
            ObjectKind::Blob => self.max_blob_bytes,
            ObjectKind::Tree
            | ObjectKind::Revision
            | ObjectKind::Operation
            | ObjectKind::Candidate
            | ObjectKind::Evidence => self.max_metadata_bytes,
        }
    }

    pub fn check_payload(&self, kind: ObjectKind, length: u64) -> Result<(), ModelError> {
        self.validate()?;
        length
            .checked_add(crate::FRAME_HEADER_LEN as u64)
            .ok_or(ModelError::InvalidFrame {
                reason: "total object frame length overflow",
            })?;
        check_u64("object payload bytes", length, self.max_payload(kind))
    }

    /// A length is checked before converting to a platform allocation size.
    pub fn metadata_len(&self, length: u64) -> Result<usize, ModelError> {
        self.validate()?;
        check_u64("metadata bytes", length, self.max_metadata_bytes)?;
        usize::try_from(length).map_err(|_| ModelError::Allocation {
            resource: "metadata bytes",
        })
    }
}

/// Metadata validation at a persistence or transport boundary.
pub trait Validate {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError>;
}

/// Clones share a cancellation signal; cancellation is monotonic.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
    pub fn check(&self) -> Result<(), ModelError> {
        if self.is_cancelled() {
            Err(ModelError::Cancelled)
        } else {
            Ok(())
        }
    }
    pub fn as_atomic(&self) -> &AtomicBool {
        &self.0
    }
}
