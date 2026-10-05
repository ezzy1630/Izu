use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum EnvironmentError {
    #[error("invalid environment recipe: {0}")]
    InvalidRecipe(String),
    #[error("environment input is invalid: {0}")]
    InvalidInput(String),
    #[error("environment operation was cancelled")]
    Cancelled,
    #[error("environment {resource} exceeds limit {limit}")]
    Limit { resource: &'static str, limit: u64 },
    #[error("environment allocation failed: {0}")]
    Allocation(&'static str),
    #[error("environment filesystem error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("environment lock {operation} failed at {path}: {source}")]
    LockIo {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("environment cache is busy")]
    Busy,
    #[error("environment artifact is absent for key {0}")]
    Missing(String),
    #[error("environment artifact failed verification: {0}")]
    Corrupt(String),
    #[error("environment directory locator no longer identifies the pinned directory: {0}")]
    NamespaceChanged(PathBuf),
    #[error("private starting environment failed verification: {0}")]
    StartingEnvironmentMismatch(String),
    #[error("environment publication identity is uncertain at {path}: {source}")]
    PublicationIdentityUncertain {
        path: PathBuf,
        #[source]
        source: Box<EnvironmentError>,
    },
    #[error("prepared input changed during import: {0}")]
    InputChanged(String),
    #[error("environment destination must be absent or an empty directory: {0}")]
    DestinationNotEmpty(String),
    #[error("environment source identity differs from the locked workspace")]
    SourceMismatch,
    #[error("environment lockfile identity differs: {0}")]
    LockfileMismatch(String),
    #[error("environment preparation incomplete at staging locator {stage}: {source}")]
    PreparationIncomplete {
        stage: PathBuf,
        #[source]
        source: Box<EnvironmentError>,
    },
    #[error(
        "environment materialization incomplete; {published:?} were published, staging locators {stages:?}: {source}"
    )]
    MaterializationIncomplete {
        published: Vec<String>,
        stages: Vec<PathBuf>,
        #[source]
        source: Box<EnvironmentError>,
    },
    #[error("environment publication visible but durability is uncertain at {path}: {source}")]
    DurabilityUncertain {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("managed workspace error: {0}")]
    Workspace(#[from] izu_engine::EngineError),
    #[error("environment metadata is invalid: {0}")]
    Json(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, EnvironmentError>;

pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> EnvironmentError {
    EnvironmentError::Io {
        path: path.into(),
        source,
    }
}

pub(crate) fn check(cancel: &izu_model::CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(EnvironmentError::Cancelled)
    } else {
        Ok(())
    }
}
