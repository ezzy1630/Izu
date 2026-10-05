use crate::{BundleError, Result};
use izu_model::Limits;

/// Resource policy is independent of immutable object identity. Every consumer
/// can lower these budgets without changing the language-independent format.
#[derive(Clone, Debug)]
pub struct BundleOptions {
    pub model_limits: Limits,
    pub max_objects: u64,
    pub max_total_bytes: u64,
    pub max_manifest_bytes: u64,
    pub max_references: u64,
    pub max_graph_bytes: u64,
    pub max_depth: usize,
    #[cfg(feature = "fault-injection")]
    pub fault_hook: Option<FaultHook>,
}

impl Default for BundleOptions {
    fn default() -> Self {
        Self {
            model_limits: Limits::default(),
            max_objects: 1_000_000,
            max_total_bytes: 64 * 1024 * 1024 * 1024,
            max_manifest_bytes: 64 * 1024,
            max_references: 4_000_000,
            max_graph_bytes: 512 * 1024 * 1024,
            max_depth: 100_000,
            #[cfg(feature = "fault-injection")]
            fault_hook: None,
        }
    }
}

impl BundleOptions {
    pub(crate) fn validate(&self) -> Result<()> {
        self.model_limits.validate()?;
        if self.max_objects == 0
            || self.max_total_bytes == 0
            || self.max_manifest_bytes == 0
            || self.max_references == 0
            || self.max_graph_bytes == 0
            || self.max_depth == 0
        {
            return Err(BundleError::Invalid("resource budgets must be positive"));
        }
        if self.max_manifest_bytes > self.model_limits.max_metadata_bytes {
            return Err(BundleError::Invalid(
                "manifest budget exceeds metadata budget",
            ));
        }
        Ok(())
    }

    pub(crate) fn boundary(&self, boundary: DurableBoundary) -> Result<()> {
        #[cfg(feature = "fault-injection")]
        if let Some(hook) = &self.fault_hook {
            (hook.0)(boundary)
                .map_err(|error| crate::error::io("injected bundle boundary", error))?;
        }
        let _ = boundary;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DurableBoundary {
    OutputCreated,
    DataWritten,
    FileSynced,
    BeforePublication,
    Published,
    ParentSynced,
    RestoreStageCreated,
    RestoreObjectsImported,
    RestoreStaged,
    RestoreBeforePublication,
    RestorePublished,
    RestoreParentSynced,
}

#[cfg(feature = "fault-injection")]
#[derive(Clone)]
pub struct FaultHook(
    pub std::sync::Arc<dyn Fn(DurableBoundary) -> std::io::Result<()> + Send + Sync>,
);

#[cfg(feature = "fault-injection")]
impl std::fmt::Debug for FaultHook {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("FaultHook(..)")
    }
}
