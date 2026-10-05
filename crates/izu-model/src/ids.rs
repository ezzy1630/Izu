use std::{fmt, str::FromStr};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};

use crate::ModelError;

const HEX: &[u8; 16] = b"0123456789abcdef";

fn parse_hex<const N: usize>(value: &str, kind: &'static str) -> Result<[u8; N], ModelError> {
    let invalid = || ModelError::InvalidId { kind, bytes: N };
    if value.len() != N * 2 {
        return Err(invalid());
    }
    let mut output = [0u8; N];
    for (index, pair) in value.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |byte: u8| match byte {
            b'0'..=b'9' => Some(byte - b'0'),
            b'a'..=b'f' => Some(byte - b'a' + 10),
            _ => None,
        };
        let high = digit(pair[0]).ok_or_else(invalid)?;
        let low = digit(pair[1]).ok_or_else(invalid)?;
        output[index] = (high << 4) | low;
    }
    Ok(output)
}

fn fmt_hex(bytes: &[u8], f: &mut fmt::Formatter<'_>) -> fmt::Result {
    for byte in bytes {
        write!(
            f,
            "{}{}",
            char::from(HEX[usize::from(byte >> 4)]),
            char::from(HEX[usize::from(byte & 15)])
        )?;
    }
    Ok(())
}

macro_rules! id_type {
    ($name:ident, $size:expr, $kind:literal) => {
        #[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name([u8; $size]);
        impl $name {
            pub const fn from_bytes(bytes: [u8; $size]) -> Self {
                Self(bytes)
            }
            pub const fn as_bytes(&self) -> &[u8; $size] {
                &self.0
            }
        }
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                fmt_hex(&self.0, f)
            }
        }
        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self)
            }
        }
        impl FromStr for $name {
            type Err = ModelError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                parse_hex(value, $kind).map(Self)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.collect_str(self)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                struct IdVisitor;
                impl Visitor<'_> for IdVisitor {
                    type Value = $name;
                    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                        f.write_str(concat!("a lowercase hexadecimal ", $kind))
                    }
                    fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                        value.parse().map_err(E::custom)
                    }
                }
                deserializer.deserialize_str(IdVisitor)
            }
        }
    };
}

id_type!(ObjectId, 32, "object ID");
id_type!(ChangeId, 16, "change ID");
id_type!(WorkspaceId, 16, "workspace ID");
id_type!(CheckAttemptId, 16, "check attempt ID");

macro_rules! object_id_type {
    ($name:ident, $kind:literal) => {
        id_type!($name, 32, $kind);
        impl $name {
            pub const fn from_object(id: ObjectId) -> Self {
                Self(*id.as_bytes())
            }
            pub const fn object_id(self) -> ObjectId {
                ObjectId::from_bytes(self.0)
            }
        }
    };
}

object_id_type!(TreeId, "tree ID");
object_id_type!(RevisionId, "revision ID");
object_id_type!(OperationId, "operation ID");
object_id_type!(CandidateId, "candidate ID");
object_id_type!(EvidenceId, "evidence ID");

pub(crate) fn validate_git_id(value: &str) -> Result<(), ModelError> {
    match value.len() {
        40 => parse_hex::<20>(value, "Git object ID").map(|_| ()),
        64 => parse_hex::<32>(value, "Git object ID").map(|_| ()),
        _ => Err(ModelError::InvalidId {
            kind: "Git object ID",
            bytes: 20,
        }),
    }
}
