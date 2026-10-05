use thiserror::Error;

/// Typed boundary failures distinguish invalid state from resource exhaustion.
#[derive(Debug, Error)]
pub enum ModelError {
    #[error("invalid {kind}: expected {bytes} bytes of lowercase hexadecimal")]
    InvalidId { kind: &'static str, bytes: usize },
    #[error("invalid repository path: {reason}")]
    InvalidPath { reason: &'static str },
    #[error("invalid {kind}: {reason}")]
    InvalidName {
        kind: &'static str,
        reason: &'static str,
    },
    #[error("{resource} exceeds its limit ({actual} > {limit})")]
    LimitExceeded {
        resource: &'static str,
        actual: u64,
        limit: u64,
    },
    #[error("invalid limits: {reason}")]
    InvalidLimits { reason: &'static str },
    #[error("invalid metadata: {reason}")]
    InvalidMetadata { reason: &'static str },
    #[error("metadata JSON is invalid: {0}")]
    Json(#[from] serde_json::Error),
    #[error("metadata is not canonically encoded")]
    NonCanonical,
    #[error("invalid object frame: {reason}")]
    InvalidFrame { reason: &'static str },
    #[error("object payload length differs from its header ({actual} != {expected})")]
    LengthMismatch { expected: u64, actual: u64 },
    #[error("allocation could not be reserved for {resource}")]
    Allocation { resource: &'static str },
    #[error("object content does not match its expected identity")]
    HashMismatch,
    #[error("check evidence does not establish this candidate: {reason}")]
    EvidenceMismatch { reason: &'static str },
    #[error("operation was cancelled")]
    Cancelled,
}

pub(crate) fn check_len(
    resource: &'static str,
    actual: usize,
    limit: usize,
) -> Result<(), ModelError> {
    let actual = u64::try_from(actual).map_err(|_| ModelError::InvalidMetadata {
        reason: "length cannot be represented in the native format",
    })?;
    let limit = u64::try_from(limit).map_err(|_| ModelError::InvalidLimits {
        reason: "configured length cannot be represented in the native format",
    })?;
    check_u64(resource, actual, limit)
}

pub(crate) fn check_u64(resource: &'static str, actual: u64, limit: u64) -> Result<(), ModelError> {
    if actual > limit {
        return Err(ModelError::LimitExceeded {
            resource,
            actual,
            limit,
        });
    }
    Ok(())
}
