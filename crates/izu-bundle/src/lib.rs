#![forbid(unsafe_code)]
//! Complete, versioned native archives. Source capture and all history mutations
//! belong to the shared engine; this crate owns the streaming archive boundary.

#[cfg(all(feature = "fault-injection", not(debug_assertions)))]
compile_error!("izu-bundle fault injection is restricted to development builds");

mod codec;
mod error;
mod files;
mod graph;
mod options;
mod owned;
mod restore;
mod source;

pub use codec::{BundleManifest, BundleScope, Inspection, ObjectCounts, VerificationReport};
pub use error::{BundleError, Result};
pub use files::{BundleReceipt, Publication, create, inspect, verify};
#[cfg(feature = "fault-injection")]
pub use options::FaultHook;
pub use options::{BundleOptions, DurableBoundary};
pub use restore::{RestorationReceipt, RestoreOptions, restore};
pub use source::{ObjectDescriptor, ObjectSource};
