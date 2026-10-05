use crate::error::io;
use crate::graph::{Graph, Node};
use crate::{BundleError, BundleOptions, ObjectSource, Result};
use izu_model::{
    CancellationToken, FRAME_HEADER_LEN, Limits, MetadataObject, ModelError, ObjectHasher,
    ObjectId, ObjectKind, OperationId, Validate, decode_metadata, decode_object_metadata,
    encode_metadata, frame_header, parse_frame_header,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};

pub(crate) const BUNDLE_MAGIC: &[u8; 8] = b"IZUBND1\0";
pub(crate) const TRAILER_MAGIC: &[u8; 8] = b"IZUEND1\0";
pub(crate) const HEADER_LEN: usize = 16;
const CHUNK_BYTES: usize = 64 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BundleScope {
    RootClosure,
}

/// A declaration of the selected immutable snapshot. It makes no claim about
/// source edits made after that root, running processes, databases, or caches.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BundleManifest {
    pub bundle_schema: u16,
    pub native_schema: u16,
    pub root_operation: OperationId,
    pub scope: BundleScope,
    pub object_count: u64,
    pub payload_bytes: u64,
}

impl Validate for BundleManifest {
    fn validate(&self, _: &Limits) -> std::result::Result<(), ModelError> {
        if self.bundle_schema != 1 || self.native_schema != 1 {
            return Err(ModelError::InvalidMetadata {
                reason: "unsupported bundle or native schema",
            });
        }
        if self.object_count == 0 {
            return Err(ModelError::InvalidMetadata {
                reason: "bundle must contain its operation root",
            });
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct Inspection {
    pub manifest: BundleManifest,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct ObjectCounts {
    pub blobs: u64,
    pub trees: u64,
    pub revisions: u64,
    pub operations: u64,
    pub candidates: u64,
    pub evidence: u64,
}

impl ObjectCounts {
    fn count(&mut self, kind: ObjectKind) -> Result<()> {
        let count = match kind {
            ObjectKind::Blob => &mut self.blobs,
            ObjectKind::Tree => &mut self.trees,
            ObjectKind::Revision => &mut self.revisions,
            ObjectKind::Operation => &mut self.operations,
            ObjectKind::Candidate => &mut self.candidates,
            ObjectKind::Evidence => &mut self.evidence,
        };
        *count = count
            .checked_add(1)
            .ok_or(BundleError::Limit("kind counts"))?;
        Ok(())
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct VerificationReport {
    pub manifest: BundleManifest,
    pub objects: ObjectCounts,
    pub bundle_bytes: u64,
    pub references: u64,
    pub maximum_depth: usize,
    pub sha256: String,
}

pub(crate) struct Verified {
    pub report: VerificationReport,
    pub graph: Graph,
}

pub(crate) fn manifest(root: OperationId, graph: &Graph) -> Result<BundleManifest> {
    Ok(BundleManifest {
        bundle_schema: 1,
        native_schema: 1,
        root_operation: root,
        scope: BundleScope::RootClosure,
        object_count: u64::try_from(graph.nodes.len())
            .map_err(|_| BundleError::Limit("object count"))?,
        payload_bytes: graph.payload_bytes,
    })
}

pub(crate) fn write_bundle<S: ObjectSource + ?Sized, W: Write>(
    source: &S,
    graph: &Graph,
    manifest: &BundleManifest,
    output: W,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<VerificationReport> {
    let bytes = encode_metadata(manifest, &options.model_limits)?;
    check_manifest(manifest, bytes.len(), options)?;
    let mut writer = BundleWriter {
        output,
        hash: Sha256::new(),
        written: 0,
        options,
        cancel,
    };
    let mut header = [0_u8; HEADER_LEN];
    header[..8].copy_from_slice(BUNDLE_MAGIC);
    header[8..10].copy_from_slice(&1_u16.to_be_bytes());
    header[10..12].copy_from_slice(&0_u16.to_be_bytes());
    let manifest_len =
        u32::try_from(bytes.len()).map_err(|_| BundleError::Limit("manifest bytes"))?;
    header[12..16].copy_from_slice(&manifest_len.to_be_bytes());
    writer.put(&header, true)?;
    writer.put(&bytes, true)?;
    let mut counts = ObjectCounts::default();
    for id in graph.sorted_ids()? {
        cancel.check()?;
        let node = graph
            .nodes
            .get(&id)
            .ok_or(BundleError::Invalid("object ordering lost its node"))?;
        writer.put(id.as_bytes(), true)?;
        writer.put(
            &frame_header(node.kind, node.payload_len, &options.model_limits)?,
            true,
        )?;
        let mut object_output = ObjectOutput {
            writer: &mut writer,
            hasher: ObjectHasher::new(node.kind, node.payload_len, &options.model_limits)?,
            failure: None,
        };
        let result = source.read_object_to(id, node.kind, &mut object_output, cancel);
        if let Some(error) = object_output.failure.take() {
            return Err(error);
        }
        let length = result?;
        if length != node.payload_len {
            return Err(BundleError::Invalid("source object length changed"));
        }
        if object_output.hasher.finish()? != id {
            return Err(ModelError::HashMismatch.into());
        }
        counts.count(node.kind)?;
    }
    let digest: [u8; 32] = writer.hash.clone().finalize().into();
    writer.put(TRAILER_MAGIC, false)?;
    writer.put(&digest, false)?;
    writer
        .output
        .flush()
        .map_err(|error| io("flush bundle output", error))?;
    Ok(VerificationReport {
        manifest: manifest.clone(),
        objects: counts,
        bundle_bytes: writer.written,
        references: graph.reference_count,
        maximum_depth: graph.validate(manifest.root_operation, options, cancel)?,
        sha256: hex_digest(&digest),
    })
}

pub(crate) fn inspect_reader<R: Read>(
    input: R,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<Inspection> {
    let mut reader = BundleReader::new(input, options, cancel);
    Ok(Inspection {
        manifest: reader.manifest()?,
    })
}

pub(crate) fn verify_reader<R: Read>(
    input: R,
    options: &BundleOptions,
    cancel: &CancellationToken,
) -> Result<Verified> {
    let mut reader = BundleReader::new(input, options, cancel);
    let manifest = reader.manifest()?;
    let mut graph = Graph::new();
    let mut counts = ObjectCounts::default();
    let mut chunk = [0_u8; CHUNK_BYTES];
    for _ in 0..manifest.object_count {
        cancel.check()?;
        let mut id = [0_u8; 32];
        reader.exact(&mut id, true)?;
        let id = ObjectId::from_bytes(id);
        if graph.nodes.contains_key(&id) {
            return Err(BundleError::DuplicateObject(id));
        }
        let mut header = [0_u8; FRAME_HEADER_LEN];
        reader.exact(&mut header, true)?;
        let (kind, payload_len) = parse_frame_header(&header, &options.model_limits)?;
        let payload_offset = reader.read;
        reader.check_available(payload_len)?;
        let mut hasher = ObjectHasher::new(kind, payload_len, &options.model_limits)?;
        let node = if kind == ObjectKind::Blob {
            let mut remaining = payload_len;
            while remaining > 0 {
                let amount = usize::try_from(remaining.min(CHUNK_BYTES as u64))
                    .map_err(|_| BundleError::Limit("blob chunk"))?;
                reader.exact(&mut chunk[..amount], true)?;
                hasher.update(&chunk[..amount])?;
                remaining -= u64::try_from(amount).map_err(|_| BundleError::Limit("blob chunk"))?;
            }
            Node::blob(payload_len, payload_offset)
        } else {
            let length = options.model_limits.metadata_len(payload_len)?;
            let mut bytes = Vec::new();
            bytes
                .try_reserve_exact(length)
                .map_err(|_| BundleError::Allocation("metadata payload"))?;
            bytes.resize(length, 0);
            reader.exact(&mut bytes, true)?;
            hasher.update(&bytes)?;
            let object = decode_object_metadata(kind, &bytes, &options.model_limits)?;
            let node = Node::metadata(
                kind,
                payload_len,
                payload_offset,
                &object,
                options,
                graph.remaining_bytes(options)?,
            )?;
            if id == manifest.root_operation.object_id()
                && let MetadataObject::Operation(operation) = object
            {
                graph.retain_root(operation, payload_len, options)?;
            }
            node
        };
        if hasher.finish()? != id {
            return Err(ModelError::HashMismatch.into());
        }
        graph.insert(id, node, options)?;
        counts.count(kind)?;
    }
    if graph.payload_bytes != manifest.payload_bytes {
        return Err(BundleError::Invalid(
            "manifest payload total differs from its records",
        ));
    }
    let digest: [u8; 32] = reader.hash.clone().finalize().into();
    let mut trailer = [0_u8; 8];
    reader.exact(&mut trailer, false)?;
    if &trailer != TRAILER_MAGIC {
        return Err(BundleError::Invalid("missing bundle trailer"));
    }
    let mut claimed_digest = [0_u8; 32];
    reader.exact(&mut claimed_digest, false)?;
    if digest != claimed_digest {
        return Err(BundleError::TrailerMismatch);
    }
    reader.require_eof()?;
    let maximum_depth = graph.validate(manifest.root_operation, options, cancel)?;
    Ok(Verified {
        report: VerificationReport {
            manifest,
            objects: counts,
            bundle_bytes: reader.read,
            references: graph.reference_count,
            maximum_depth,
            sha256: hex_digest(&digest),
        },
        graph,
    })
}

fn check_manifest(manifest: &BundleManifest, length: usize, options: &BundleOptions) -> Result<()> {
    if u64::try_from(length).map_or(true, |length| length > options.max_manifest_bytes) {
        return Err(BundleError::Limit("manifest bytes"));
    }
    if manifest.object_count > options.max_objects {
        return Err(BundleError::Limit("objects"));
    }
    let overhead = manifest
        .object_count
        .checked_mul((32 + FRAME_HEADER_LEN) as u64)
        .and_then(|n| n.checked_add(HEADER_LEN as u64))
        .and_then(|n| n.checked_add(u64::try_from(length).ok()?))
        .and_then(|n| n.checked_add(40))
        .ok_or(BundleError::Limit("bundle bytes"))?;
    let total = overhead
        .checked_add(manifest.payload_bytes)
        .ok_or(BundleError::Limit("bundle bytes"))?;
    if total > options.max_total_bytes {
        return Err(BundleError::Limit("bundle bytes"));
    }
    Ok(())
}

fn hex_digest(bytes: &[u8; 32]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(64);
    for byte in bytes {
        value.push(char::from(HEX[usize::from(byte >> 4)]));
        value.push(char::from(HEX[usize::from(byte & 15)]));
    }
    value
}

struct BundleWriter<'a, W> {
    output: W,
    hash: Sha256,
    written: u64,
    options: &'a BundleOptions,
    cancel: &'a CancellationToken,
}

impl<W: Write> BundleWriter<'_, W> {
    fn put(&mut self, bytes: &[u8], digest: bool) -> Result<()> {
        self.cancel.check()?;
        let written = self
            .written
            .checked_add(
                u64::try_from(bytes.len()).map_err(|_| BundleError::Limit("bundle bytes"))?,
            )
            .ok_or(BundleError::Limit("bundle bytes"))?;
        if written > self.options.max_total_bytes {
            return Err(BundleError::Limit("bundle bytes"));
        }
        self.output
            .write_all(bytes)
            .map_err(|error| io("write bundle", error))?;
        if digest {
            self.hash.update(bytes);
        }
        self.written = written;
        Ok(())
    }
}

struct ObjectOutput<'a, 'b, W> {
    writer: &'a mut BundleWriter<'b, W>,
    hasher: ObjectHasher,
    failure: Option<BundleError>,
}

impl<W: Write> Write for ObjectOutput<'_, '_, W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for chunk in bytes.chunks(CHUNK_BYTES) {
            if let Err(error) = self
                .hasher
                .update(chunk)
                .map_err(BundleError::from)
                .and_then(|()| self.writer.put(chunk, true))
            {
                self.failure = Some(error);
                return Err(std::io::Error::other("bundle write failed"));
            }
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct BundleReader<'a, R> {
    input: R,
    hash: Sha256,
    read: u64,
    options: &'a BundleOptions,
    cancel: &'a CancellationToken,
}

impl<'a, R: Read> BundleReader<'a, R> {
    fn new(input: R, options: &'a BundleOptions, cancel: &'a CancellationToken) -> Self {
        Self {
            input,
            hash: Sha256::new(),
            read: 0,
            options,
            cancel,
        }
    }

    fn manifest(&mut self) -> Result<BundleManifest> {
        let mut header = [0_u8; HEADER_LEN];
        self.exact(&mut header, true)?;
        if &header[..8] != BUNDLE_MAGIC
            || header[8..10] != 1_u16.to_be_bytes()
            || header[10..12] != 0_u16.to_be_bytes()
        {
            return Err(BundleError::UnsupportedVersion);
        }
        let mut length = [0_u8; 4];
        length.copy_from_slice(&header[12..16]);
        let length = u64::from(u32::from_be_bytes(length));
        if length > self.options.max_manifest_bytes {
            return Err(BundleError::Limit("manifest bytes"));
        }
        self.check_available(length)?;
        let length = self.options.model_limits.metadata_len(length)?;
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(length)
            .map_err(|_| BundleError::Allocation("manifest"))?;
        bytes.resize(length, 0);
        self.exact(&mut bytes, true)?;
        let manifest: BundleManifest = decode_metadata(&bytes, &self.options.model_limits)?;
        check_manifest(&manifest, length, self.options)?;
        Ok(manifest)
    }

    fn check_available(&self, length: u64) -> Result<()> {
        let total = self
            .read
            .checked_add(length)
            .ok_or(BundleError::Limit("bundle bytes"))?;
        if total > self.options.max_total_bytes {
            Err(BundleError::Limit("bundle bytes"))
        } else {
            Ok(())
        }
    }

    fn exact(&mut self, bytes: &mut [u8], digest: bool) -> Result<()> {
        self.check_available(
            u64::try_from(bytes.len()).map_err(|_| BundleError::Limit("bundle bytes"))?,
        )?;
        for chunk in bytes.chunks_mut(CHUNK_BYTES) {
            let mut remaining = chunk;
            while !remaining.is_empty() {
                self.cancel.check()?;
                let count = match self.input.read(remaining) {
                    Ok(0) => return Err(BundleError::Truncated),
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(io("read bundle", error)),
                };
                let read_bytes = remaining
                    .get(..count)
                    .ok_or(BundleError::Invalid("reader returned an invalid length"))?;
                if digest {
                    self.hash.update(read_bytes);
                }
                self.read = self
                    .read
                    .checked_add(
                        u64::try_from(count).map_err(|_| BundleError::Limit("bundle bytes"))?,
                    )
                    .ok_or(BundleError::Limit("bundle bytes"))?;
                remaining = remaining
                    .get_mut(count..)
                    .ok_or(BundleError::Invalid("reader returned an invalid length"))?;
            }
        }
        Ok(())
    }

    fn require_eof(&mut self) -> Result<()> {
        let mut byte = [0_u8; 1];
        loop {
            self.cancel.check()?;
            match self.input.read(&mut byte) {
                Ok(0) => return Ok(()),
                Ok(_) => return Err(BundleError::Invalid("bundle has trailing bytes")),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(io("read bundle end", error)),
            }
        }
    }
}
