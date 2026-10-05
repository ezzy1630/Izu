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
