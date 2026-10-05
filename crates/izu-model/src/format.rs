use std::io::{self, Write};

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{Limits, ModelError, ObjectId, Validate};

pub const FRAME_MAGIC: &[u8; 8] = b"IZUOBJ1\0";
pub const FRAME_HEADER_LEN: usize = 17;
const MAX_JSON_DEPTH: usize = 64;
const MAX_JSON_NODES: usize = 1_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[repr(u8)]
pub enum ObjectKind {
    Blob = 1,
    Tree = 2,
    Revision = 3,
    Operation = 4,
    Candidate = 5,
    Evidence = 6,
}

impl TryFrom<u8> for ObjectKind {
    type Error = ModelError;
    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            1 => Ok(Self::Blob),
            2 => Ok(Self::Tree),
            3 => Ok(Self::Revision),
            4 => Ok(Self::Operation),
            5 => Ok(Self::Candidate),
            6 => Ok(Self::Evidence),
            _ => Err(ModelError::InvalidFrame {
                reason: "unknown object kind",
            }),
        }
    }
}

pub fn frame_header(
    kind: ObjectKind,
    payload_len: u64,
    limits: &Limits,
) -> Result<[u8; FRAME_HEADER_LEN], ModelError> {
    limits.check_payload(kind, payload_len)?;
    let mut bytes = [0u8; FRAME_HEADER_LEN];
    bytes[..8].copy_from_slice(FRAME_MAGIC);
    bytes[8] = kind as u8;
    bytes[9..].copy_from_slice(&payload_len.to_be_bytes());
    Ok(bytes)
}

/// Parse a fixed header before trusting or allocating for its length.
pub fn parse_frame_header(bytes: &[u8], limits: &Limits) -> Result<(ObjectKind, u64), ModelError> {
    if bytes.len() != FRAME_HEADER_LEN {
        return Err(ModelError::InvalidFrame {
            reason: "incorrect header length",
        });
    }
    if &bytes[..8] != FRAME_MAGIC {
        return Err(ModelError::InvalidFrame {
            reason: "unknown magic or format version",
        });
    }
    let kind = ObjectKind::try_from(bytes[8])?;
    let mut length = [0u8; 8];
    length.copy_from_slice(&bytes[9..]);
    let length = u64::from_be_bytes(length);
    limits.check_payload(kind, length)?;
    Ok((kind, length))
}

/// Streaming hash with exact length accounting. No payload allocation is needed.
pub struct ObjectHasher {
    hash: Sha256,
    expected: u64,
    written: u64,
}

impl ObjectHasher {
    pub fn new(kind: ObjectKind, payload_len: u64, limits: &Limits) -> Result<Self, ModelError> {
        let header = frame_header(kind, payload_len, limits)?;
        let mut hash = Sha256::new();
        hash.update(header);
        Ok(Self {
            hash,
            expected: payload_len,
            written: 0,
        })
    }
    pub fn update(&mut self, bytes: &[u8]) -> Result<(), ModelError> {
        let length = u64::try_from(bytes.len()).map_err(|_| ModelError::InvalidFrame {
            reason: "payload chunk length overflow",
        })?;
        let written = self
            .written
            .checked_add(length)
            .ok_or(ModelError::InvalidFrame {
                reason: "payload length overflow",
            })?;
        if written > self.expected {
            return Err(ModelError::LengthMismatch {
                expected: self.expected,
                actual: written,
            });
        }
        self.hash.update(bytes);
        self.written = written;
        Ok(())
    }
    pub fn finish(self) -> Result<ObjectId, ModelError> {
        if self.written != self.expected {
            return Err(ModelError::LengthMismatch {
                expected: self.expected,
                actual: self.written,
            });
        }
        let digest: [u8; 32] = self.hash.finalize().into();
        Ok(ObjectId::from_bytes(digest))
    }
}

pub fn hash_object(
    kind: ObjectKind,
    payload: &[u8],
    limits: &Limits,
) -> Result<ObjectId, ModelError> {
    let length = u64::try_from(payload.len()).map_err(|_| ModelError::InvalidFrame {
        reason: "payload length overflow",
    })?;
    let mut hash = ObjectHasher::new(kind, length, limits)?;
    hash.update(payload)?;
    hash.finish()
}

/// Unique compact JSON encoding: sorted UTF-8 keys, integer numbers only,
/// serde_json string escapes, no whitespace, and no serializer-dependent maps.
pub fn encode_metadata<T: Serialize + Validate>(
    value: &T,
    limits: &Limits,
) -> Result<Vec<u8>, ModelError> {
    limits.validate()?;
    value.validate(limits)?;
    let mut staging = BoundedWriter::new(limits.max_metadata_bytes);
    let serialized = serde_json::to_writer(&mut staging, value);
    staging.resolve(serialized)?;
    preflight(&staging.bytes)?;
    let json: Value = serde_json::from_slice(&staging.bytes)?;
    drop(staging);
    canonical_value(&json, limits)
}

/// Native boundary decoder. Bare serde_json decoding is not the native format
/// boundary: this function performs size, cost, canonical-byte and type checks.
pub fn decode_metadata<T: DeserializeOwned + Serialize + Validate>(
    payload: &[u8],
    limits: &Limits,
) -> Result<T, ModelError> {
    limits.metadata_len(
        u64::try_from(payload.len()).map_err(|_| ModelError::Allocation {
            resource: "metadata bytes",
        })?,
    )?;
    preflight(payload)?;
    let json: Value = serde_json::from_slice(payload)?;
    if canonical_value(&json, limits)? != payload {
        return Err(ModelError::NonCanonical);
    }
    drop(json);
    let typed: T = serde_json::from_slice(payload)?;
    typed.validate(limits)?;
    // Typed containers can collapse duplicate set/map elements, and schemas can
    // have semantic canonical forms beyond JSON. They must preserve exact bytes.
    if encode_metadata(&typed, limits)? != payload {
        return Err(ModelError::NonCanonical);
    }
    Ok(typed)
}

fn canonical_value(value: &Value, limits: &Limits) -> Result<Vec<u8>, ModelError> {
    let mut output = BoundedWriter::new(limits.max_metadata_bytes);
    write_value(value, &mut output, 0)?;
    Ok(output.bytes)
}

fn write_value(value: &Value, writer: &mut BoundedWriter, depth: usize) -> Result<(), ModelError> {
    if depth > MAX_JSON_DEPTH {
        return Err(ModelError::InvalidMetadata {
            reason: "JSON nesting exceeds 64",
        });
    }
    match value {
        Value::Null => writer.put(b"null"),
        Value::Bool(true) => writer.put(b"true"),
        Value::Bool(false) => writer.put(b"false"),
        Value::Number(number) => {
            if !number.is_i64() && !number.is_u64() {
                return Err(ModelError::InvalidMetadata {
                    reason: "floating-point JSON is unsupported",
                });
            }
            let result = serde_json::to_writer(&mut *writer, number);
            writer.resolve(result)
        }
        Value::String(string) => {
            let result = serde_json::to_writer(&mut *writer, string);
            writer.resolve(result)
        }
        Value::Array(array) => {
            writer.put(b"[")?;
            for (index, child) in array.iter().enumerate() {
                if index != 0 {
                    writer.put(b",")?;
                }
                write_value(child, writer, depth + 1)?;
            }
            writer.put(b"]")
        }
        Value::Object(object) => {
            let mut keys = Vec::new();
            keys.try_reserve_exact(object.len())
                .map_err(|_| ModelError::Allocation {
                    resource: "JSON object keys",
                })?;
            keys.extend(object.keys());
            keys.sort_unstable();
            writer.put(b"{")?;
            for (index, key) in keys.into_iter().enumerate() {
                if index != 0 {
                    writer.put(b",")?;
                }
                let result = serde_json::to_writer(&mut *writer, key);
                writer.resolve(result)?;
                writer.put(b":")?;
                let child = object.get(key).ok_or(ModelError::InvalidMetadata {
                    reason: "JSON object key disappeared",
                })?;
                write_value(child, writer, depth + 1)?;
            }
            writer.put(b"}")
        }
    }
}

struct BoundedWriter {
    bytes: Vec<u8>,
    limit: u64,
    error: Option<ModelError>,
}

impl BoundedWriter {
    fn new(limit: u64) -> Self {
        Self {
            bytes: Vec::new(),
            limit,
            error: None,
        }
    }
    fn put(&mut self, bytes: &[u8]) -> Result<(), ModelError> {
        let actual =
            self.bytes
                .len()
                .checked_add(bytes.len())
                .ok_or(ModelError::InvalidMetadata {
                    reason: "serialized size overflow",
                })?;
        let actual_u64 = u64::try_from(actual).map_err(|_| ModelError::Allocation {
            resource: "metadata bytes",
        })?;
        if actual_u64 > self.limit {
            return Err(ModelError::LimitExceeded {
                resource: "metadata bytes",
                actual: actual_u64,
                limit: self.limit,
            });
        }
        self.bytes
            .try_reserve(bytes.len())
            .map_err(|_| ModelError::Allocation {
                resource: "metadata bytes",
            })?;
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }
    fn resolve(&mut self, result: Result<(), serde_json::Error>) -> Result<(), ModelError> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        result.map_err(ModelError::Json)
    }
}

impl Write for BoundedWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        match self.put(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.error = Some(error);
                Err(io::Error::other(
                    "metadata encoding bound or allocation failure",
                ))
            }
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Allocation-free cost scan runs before serde can allocate a hostile deeply
/// nested value or millions of tiny nodes. serde remains the syntax validator.
fn preflight(bytes: &[u8]) -> Result<(), ModelError> {
    let mut depth = 0usize;
    let mut nodes = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'{' | b'[' => {
                depth = depth.checked_add(1).ok_or(ModelError::InvalidMetadata {
                    reason: "JSON nesting overflow",
                })?;
                if depth > MAX_JSON_DEPTH {
                    return Err(ModelError::InvalidMetadata {
                        reason: "JSON nesting exceeds 64",
                    });
                }
                nodes += 1;
                index += 1;
            }
            b'}' | b']' => {
                depth = depth.checked_sub(1).ok_or(ModelError::InvalidMetadata {
                    reason: "unbalanced JSON nesting",
                })?;
                index += 1;
            }
            b'"' => {
                nodes += 1;
                index += 1;
                while index < bytes.len() {
                    match bytes[index] {
                        b'"' => {
                            index += 1;
                            break;
                        }
                        b'\\' => {
                            index = index.checked_add(2).ok_or(ModelError::InvalidMetadata {
                                reason: "JSON offset overflow",
                            })?;
                        }
                        _ => index += 1,
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                nodes += 1;
                let start = index;
                while index < bytes.len()
                    && !matches!(
                        bytes[index],
                        b' ' | b'\t' | b'\r' | b'\n' | b',' | b']' | b'}' | b':'
                    )
                {
                    index += 1;
                }
                if index - start > 21 {
                    return Err(ModelError::InvalidMetadata {
                        reason: "JSON number token exceeds integer range",
                    });
                }
            }
            b't' | b'f' | b'n' => {
                nodes += 1;
                while index < bytes.len() && bytes[index].is_ascii_alphabetic() {
                    index += 1;
                }
            }
            _ => index += 1,
        }
        if nodes > MAX_JSON_NODES {
            return Err(ModelError::LimitExceeded {
                resource: "JSON nodes",
                actual: nodes as u64,
                limit: MAX_JSON_NODES as u64,
            });
        }
    }
    Ok(())
}
