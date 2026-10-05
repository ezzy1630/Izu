use std::io::{self, Write};

use izu_engine::Repository;
use izu_environment::{
    EnvironmentBinding, EnvironmentCache, EnvironmentError, EnvironmentLimits,
    MAX_ENVIRONMENT_BINDING_BYTES,
};
use izu_model::{CancellationToken, ObjectId};

use crate::RuntimeError;

/// Native Blob identity and prepared-cache identity are deliberately distinct.
pub(crate) fn read_binding(
    repository: &Repository,
    id: ObjectId,
    cancel: &CancellationToken,
) -> Result<EnvironmentBinding, RuntimeError> {
    let mut output = BindingBytes::default();
    if let Err(error) = repository.read_blob(id, &mut output, cancel) {
        if output.limit_exceeded {
            return Err(EnvironmentError::Limit {
                resource: "environment binding bytes",
                limit: MAX_ENVIRONMENT_BINDING_BYTES as u64,
            }
            .into());
        }
        return Err(error.into());
    }
    Ok(EnvironmentBinding::from_json(&output.bytes)?)
}

pub(crate) fn open_cache(repository: &Repository) -> Result<EnvironmentCache, RuntimeError> {
    Ok(EnvironmentCache::open(
        repository.metadata_path().join("environments/v1"),
        EnvironmentLimits::default(),
    )?)
}

#[derive(Default)]
struct BindingBytes {
    bytes: Vec<u8>,
    limit_exceeded: bool,
}
impl Write for BindingBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_ENVIRONMENT_BINDING_BYTES.saturating_sub(self.bytes.len()) {
            self.limit_exceeded = true;
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "environment binding exceeds the wire limit",
            ));
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|error| io::Error::other(error.to_string()))?;
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
