use crate::Result;
use izu_model::{CancellationToken, ObjectId, ObjectKind};
use izu_store::Store;
use std::io::Write;

#[derive(Clone, Copy, Debug)]
pub struct ObjectDescriptor {
    pub kind: ObjectKind,
    pub payload_len: u64,
}

/// A source of exact immutable native object payloads. The archive boundary
/// independently checks native identities; implementations need no history API.
pub trait ObjectSource {
    /// Exclude reclamation across the complete archive traversal. Custom sources
    /// without reclamation must keep their immutable snapshot alive themselves.
    fn retain(&self, _cancel: &CancellationToken) -> Result<Option<izu_store::RetentionLease>> {
        Ok(None)
    }

    fn object_info(&self, id: ObjectId, cancel: &CancellationToken) -> Result<ObjectDescriptor>;
    fn read_object_to(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        output: &mut dyn Write,
        cancel: &CancellationToken,
    ) -> Result<u64>;
}

impl ObjectSource for Store {
    fn retain(&self, cancel: &CancellationToken) -> Result<Option<izu_store::RetentionLease>> {
        Ok(Some(Store::retain(self, cancel)?))
    }

    fn object_info(&self, id: ObjectId, cancel: &CancellationToken) -> Result<ObjectDescriptor> {
        let info = self.object_info(id, cancel)?;
        Ok(ObjectDescriptor {
            kind: info.kind,
            payload_len: info.payload_len,
        })
    }

    fn read_object_to(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        mut output: &mut dyn Write,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        Ok(self.read_object_to(id, kind, &mut output, cancel)?)
    }
}

impl ObjectSource for izu_engine::Repository {
    fn retain(&self, cancel: &CancellationToken) -> Result<Option<izu_store::RetentionLease>> {
        Ok(Some(self.retention_lease(cancel)?))
    }

    fn object_info(&self, id: ObjectId, cancel: &CancellationToken) -> Result<ObjectDescriptor> {
        let info = self.object_info(id, cancel)?;
        Ok(ObjectDescriptor {
            kind: info.kind,
            payload_len: info.payload_len,
        })
    }

    fn read_object_to(
        &self,
        id: ObjectId,
        kind: ObjectKind,
        output: &mut dyn Write,
        cancel: &CancellationToken,
    ) -> Result<u64> {
        Ok(self.read_object_to(id, kind, output, cancel)?)
    }
}
