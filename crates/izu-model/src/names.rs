use std::{borrow::Borrow, fmt, str::FromStr};

use caseless::Caseless;
use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Visitor};
use unicode_normalization::UnicodeNormalization;

use crate::error::check_len;
use crate::limits::{HARD_NAME_BYTES, HARD_PATH_BYTES, HARD_SYMLINK_BYTES};
use crate::{Limits, ModelError, Validate};

/// UTF-8 POSIX relative path. Construction never normalizes user input.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RepoPath(String);

impl RepoPath {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        check_len("path bytes", value.len(), HARD_PATH_BYTES)?;
        if value.is_empty() {
            return Err(ModelError::InvalidPath {
                reason: "path is empty",
            });
        }
        if value.starts_with('/') {
            return Err(ModelError::InvalidPath {
                reason: "path is absolute",
            });
        }
        if value.contains('\0') {
            return Err(ModelError::InvalidPath {
                reason: "path contains NUL",
            });
        }
        for part in value.split('/') {
            if part.is_empty() {
                return Err(ModelError::InvalidPath {
                    reason: "path contains an empty component",
                });
            }
            if part == "." || part == ".." {
                return Err(ModelError::InvalidPath {
                    reason: "path contains a dot component",
                });
            }
            if [".izu", ".izu-recovery", ".ezy", ".ezy-recovery", ".git"]
                .iter()
                .any(|reserved| part.eq_ignore_ascii_case(reserved))
            {
                return Err(ModelError::InvalidPath {
                    reason: "repository metadata path is reserved",
                });
            }
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Conservative canonical-caseless namespace key. This never replaces the
    /// stored spelling: NFD, full default case folding, then NFD again. Folding
    /// is required because lowercasing misses aliases such as sigma and ß/SS.
    pub fn namespace_key(&self) -> Result<String, ModelError> {
        collect_chars(self.0.nfd().default_case_fold().nfd())
    }
}

fn collect_chars(chars: impl Iterator<Item = char>) -> Result<String, ModelError> {
    let mut output = String::new();
    for ch in chars {
        output
            .try_reserve(ch.len_utf8())
            .map_err(|_| ModelError::Allocation {
                resource: "path namespace key",
            })?;
        output.push(ch);
    }
    Ok(output)
}

impl Validate for RepoPath {
    fn validate(&self, limits: &Limits) -> Result<(), ModelError> {
        check_len("path bytes", self.0.len(), limits.max_path_bytes)
    }
}

impl Borrow<str> for RepoPath {
    fn borrow(&self) -> &str {
        self.as_str()
    }
}

/// Portable named-reference spelling; components are ASCII letters, digits,
/// dots, underscores or hyphens, with an alphanumeric first character.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct RefName(String);

impl RefName {
    pub fn new(value: impl Into<String>) -> Result<Self, ModelError> {
        let value = value.into();
        validate_reference_name(&value)?;
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

pub(crate) fn validate_reference_name(value: &str) -> Result<(), ModelError> {
    check_len("reference name bytes", value.len(), HARD_NAME_BYTES)?;
    for part in value.split('/') {
        let first = part
            .as_bytes()
            .first()
            .copied()
            .ok_or(ModelError::InvalidName {
                kind: "reference name",
                reason: "empty component",
            })?;
        if !first.is_ascii_alphanumeric()
            || !part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return Err(ModelError::InvalidName {
                kind: "reference name",
                reason: "unsupported spelling",
            });
        }
    }
    Ok(())
}

macro_rules! string_impls {
    ($name:ident) => {
        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl FromStr for $name {
            type Err = ModelError;
            fn from_str(value: &str) -> Result<Self, Self::Err> {
                Self::new(value)
            }
        }
        impl Serialize for $name {
            fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
                serializer.serialize_str(&self.0)
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let value = String::deserialize(deserializer)?;
                Self::new(value).map_err(serde::de::Error::custom)
            }
        }
    };
}

string_impls!(RepoPath);
string_impls!(RefName);

/// Raw POSIX symlink target, retained exactly without resolving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SymlinkTarget(Vec<u8>);

impl SymlinkTarget {
    pub fn new(bytes: Vec<u8>) -> Result<Self, ModelError> {
        check_len("symlink target bytes", bytes.len(), HARD_SYMLINK_BYTES)?;
        if bytes.is_empty() || bytes.contains(&0) {
            return Err(ModelError::InvalidMetadata {
                reason: "symlink target is empty or contains NUL",
            });
        }
        Ok(Self(bytes))
    }
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

impl Serialize for SymlinkTarget {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut hex = String::new();
        hex.try_reserve(self.0.len() * 2)
            .map_err(serde::ser::Error::custom)?;
        for b in &self.0 {
            let digits = b"0123456789abcdef";
            hex.push(char::from(digits[usize::from(b >> 4)]));
            hex.push(char::from(digits[usize::from(b & 15)]));
        }
        serializer.serialize_str(&hex)
    }
}

impl<'de> Deserialize<'de> for SymlinkTarget {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct TargetVisitor;
        impl Visitor<'_> for TargetVisitor {
            type Value = SymlinkTarget;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a lowercase hexadecimal POSIX symlink target")
            }
            fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
                decode_symlink(value).map_err(E::custom)
            }
        }
        deserializer.deserialize_str(TargetVisitor)
    }
}

fn decode_symlink(value: &str) -> Result<SymlinkTarget, ModelError> {
    if value.len() > HARD_SYMLINK_BYTES * 2 || !value.len().is_multiple_of(2) {
        return Err(ModelError::InvalidMetadata {
            reason: "invalid symlink target hex length",
        });
    }
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(value.len() / 2)
        .map_err(|_| ModelError::Allocation {
            resource: "symlink target bytes",
        })?;
    let digit = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    };
    for pair in value.as_bytes().as_chunks::<2>().0 {
        let high = digit(pair[0]).ok_or(ModelError::InvalidMetadata {
            reason: "invalid symlink target hex",
        })?;
        let low = digit(pair[1]).ok_or(ModelError::InvalidMetadata {
            reason: "invalid symlink target hex",
        })?;
        bytes.push((high << 4) | low);
    }
    SymlinkTarget::new(bytes)
}
