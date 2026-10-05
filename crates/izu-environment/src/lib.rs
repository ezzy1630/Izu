#![forbid(unsafe_code)]
//! Optional prepared environments for original izu workspaces.
//!
//! Artifacts belong to a cooperative trust domain. Descriptor-relative traversal,
//! verified manifests and distinct file inodes prevent accidental aliasing and
//! path escape; they do not isolate hostile code running as the same user.

mod binding;
mod cache;
mod error;
mod filesystem;
mod materialize;
mod recipe;

pub use binding::{
    DeclaredEnvironmentToolchain, EnvironmentBinding, EnvironmentVerificationScope,
    MAX_ENVIRONMENT_BINDING_BYTES, ObservedEnvironmentPlatform, StartingEnvironmentProof,
    StartingEnvironmentReceipt,
};
pub use cache::{ArtifactSummary, CacheStatus, EnvironmentCache, ImportReceipt, RetentionPolicy};
pub use error::{EnvironmentError, Result};
pub use filesystem::{MaterializationMode, SharingPolicy, filesystem_available_bytes};
pub use izu_model::CancellationToken;
pub use materialize::{MaterializationReport, WorkspaceTarget};
pub use recipe::{
    Digest, EnvironmentLimits, LockfileIdentity, PlatformIdentity, QuiescenceAcknowledgement,
    Recipe, RecipeSpec, TrustAcknowledgement,
};
