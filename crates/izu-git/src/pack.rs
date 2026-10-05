//! Original bounded reader for the public Git pack v2/v3 interchange format.
//! It validates all reconstructed object sizes before installed Git indexes
//! a pack. It does not publish history or treat SHA-1 as native integrity.
use crate::{Error, GitLimits, GitObjectId, Result, objects};
use flate2::{Decompress, FlushDecompress, Status};
use izu_model::CancellationToken;
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::time::Instant;

#[derive(Clone, Copy)]
enum Base {
    Full(&'static str),
    Offset(usize),
    Reference(GitObjectId),
}
struct Encoded {
    base: Base,
    bytes: Vec<u8>,
}
struct Resolved {
    kind: &'static str,
    bytes: Vec<u8>,
    depth: usize,
}

fn invalid(reason: &str) -> Error {
    Error::InvalidSource(format!("invalid Git pack: {reason}"))
}
fn check(cancel: &CancellationToken, deadline: Instant) -> Result<()> {
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(Error::TimedOut);
    }
    Ok(())
}
fn byte(bytes: &[u8], position: &mut usize) -> Result<u8> {
    let value = bytes
        .get(*position)
        .copied()
        .ok_or_else(|| invalid("truncated integer"))?;
    *position = position.checked_add(1).ok_or(Error::Limit("pack cursor"))?;
    Ok(value)
}
fn size(bytes: &[u8], position: &mut usize) -> Result<usize> {
    let mut value = 0_usize;
    for shift in (0..usize::BITS).step_by(7) {
        let next = byte(bytes, position)?;
        let part = usize::from(next & 0x7f);
        if part > (usize::MAX >> shift) {
            return Err(Error::Limit("pack size integer"));
        }
        value |= part << shift;
        if next & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Error::Limit("pack size integer"))
}
fn object_header(bytes: &[u8], position: &mut usize) -> Result<(u8, usize)> {
    let mut next = byte(bytes, position)?;
    let kind = (next >> 4) & 7;
    let mut length = usize::from(next & 15);
    let mut shift = 4;
    while next & 0x80 != 0 {
        if shift >= usize::BITS {
            return Err(Error::Limit("pack object length"));
        }
        next = byte(bytes, position)?;
        let part = usize::from(next & 127);
        if part > (usize::MAX >> shift) {
            return Err(Error::Limit("pack object length"));
        }
        length |= part << shift;
        shift += 7;
    }
    Ok((kind, length))
}
fn offset(bytes: &[u8], position: &mut usize) -> Result<usize> {
    let mut next = byte(bytes, position)?;
    let mut value = usize::from(next & 127);
    while next & 128 != 0 {
        next = byte(bytes, position)?;
        value = value
            .checked_add(1)
            .and_then(|value| value.checked_mul(128))
            .and_then(|value| value.checked_add(usize::from(next & 127)))
            .ok_or(Error::Limit("pack base offset"))?;
    }
    Ok(value)
}
fn inflate(
    bytes: &[u8],
    length: usize,
    limit: usize,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<(Vec<u8>, usize)> {
    if length > limit {
        return Err(Error::Limit("inflated pack object"));
    }
    let mut inflater = Decompress::new(true);
    let mut output = Vec::new();
    let mut block = [0_u8; 8192];
    loop {
        check(cancel, deadline)?;
        let consumed = usize::try_from(inflater.total_in())
            .map_err(|_| Error::Limit("compressed pack object"))?;
        let input = bytes
            .get(consumed..)
            .ok_or_else(|| invalid("invalid zlib cursor"))?;
        let before_in = inflater.total_in();
        let before_out = inflater.total_out();
        let status = inflater
            .decompress(input, &mut block, FlushDecompress::None)
            .map_err(|_| invalid("invalid zlib stream"))?;
        let produced = usize::try_from(inflater.total_out() - before_out)
            .map_err(|_| Error::Limit("inflated pack object"))?;
        let new_length = output
            .len()
            .checked_add(produced)
            .ok_or(Error::Limit("inflated pack object"))?;
        if new_length > length || new_length > limit {
            return Err(Error::Limit("inflated pack object"));
        }
        output
            .try_reserve(produced)
            .map_err(|_| Error::Limit("pack allocation"))?;
        output.extend_from_slice(&block[..produced]);
        if status == Status::StreamEnd {
            if output.len() != length {
                return Err(invalid("zlib length disagrees with object header"));
            }
            let consumed = usize::try_from(inflater.total_in())
                .map_err(|_| Error::Limit("compressed pack object"))?;
            return Ok((output, consumed));
        }
        if inflater.total_in() == before_in && inflater.total_out() == before_out {
            return Err(invalid("truncated or stalled zlib stream"));
        }
    }
}

fn delta(
    base: &[u8],
    program: &[u8],
    limit: usize,
    cancel: &CancellationToken,
    deadline: Instant,
) -> Result<Vec<u8>> {
    let mut position = 0;
    let base_length = size(program, &mut position)?;
    let length = size(program, &mut position)?;
    if base_length != base.len() {
        return Err(invalid("delta base length mismatch"));
    }
    if length > limit {
        return Err(Error::Limit("reconstructed pack object"));
    }
    let mut result = Vec::new();
    result
        .try_reserve_exact(length)
        .map_err(|_| Error::Limit("delta allocation"))?;
    while position < program.len() {
        check(cancel, deadline)?;
        let instruction = byte(program, &mut position)?;
        let data = if instruction & 128 == 0 {
            let length = usize::from(instruction);
            if length == 0 {
                return Err(invalid("reserved delta instruction"));
            }
            let end = position
                .checked_add(length)
                .ok_or(Error::Limit("delta cursor"))?;
            let data = program
                .get(position..end)
                .ok_or_else(|| invalid("truncated delta insertion"))?;
            position = end;
            data
        } else {
            let mut start = 0_usize;
            let mut length = 0_usize;
            for bit in 0..4 {
                if instruction & (1 << bit) != 0 {
                    start |= usize::from(byte(program, &mut position)?) << (8 * bit);
                }
            }
            for bit in 0..3 {
                if instruction & (1 << (4 + bit)) != 0 {
                    length |= usize::from(byte(program, &mut position)?) << (8 * bit);
                }
            }
            if length == 0 {
                length = 0x10000;
            }
            let end = start
                .checked_add(length)
                .ok_or(Error::Limit("delta copy range"))?;
            base.get(start..end)
                .ok_or_else(|| invalid("delta copy escapes base object"))?
        };
        if result
            .len()
            .checked_add(data.len())
            .is_none_or(|new_length| new_length > length)
        {
            return Err(invalid("delta instructions exceed result length"));
        }
        result.extend_from_slice(data);
    }
    if result.len() != length {
        return Err(invalid("delta result is incomplete"));
    }
    Ok(result)
}

/// Admit one self-contained SHA-1 pack. All budget checks occur before an
/// authoritative Git indexing step. Ref deltas with external bases are refused.
pub(crate) fn validate(bytes: &[u8], limits: &GitLimits, cancel: &CancellationToken) -> Result<()> {
    if bytes.len() > limits.max_pack_bytes {
        return Err(Error::Limit("compressed Git pack"));
    }
    if bytes.len() < 32 || bytes.get(..4) != Some(b"PACK") {
        return Err(invalid("missing PACK header"));
    }
    let deadline = Instant::now()
        .checked_add(limits.command_timeout)
        .ok_or(Error::Limit("pack deadline"))?;
    check(cancel, deadline)?;
    let version = u32::from_be_bytes(
        bytes
            .get(4..8)
            .ok_or_else(|| invalid("header"))?
            .try_into()
            .map_err(|_| invalid("header"))?,
    );
    if version != 2 && version != 3 {
        return Err(Error::Unsupported(vec![format!(
            "Git pack version {version}"
        )]));
    }
    let count = usize::try_from(u32::from_be_bytes(
        bytes
            .get(8..12)
            .ok_or_else(|| invalid("header"))?
            .try_into()
            .map_err(|_| invalid("header"))?,
    ))
    .map_err(|_| Error::Limit("pack object count"))?;
    if count > limits.max_objects {
        return Err(Error::Limit("pack object count"));
    }
    let trailer = bytes
        .len()
        .checked_sub(20)
        .ok_or_else(|| invalid("trailer"))?;
    let mut checksum = Sha1::new();
    for block in bytes[..trailer].chunks(8192) {
        check(cancel, deadline)?;
        checksum.update(block);
    }
    if checksum.finalize().as_slice() != &bytes[trailer..] {
        return Err(invalid("pack checksum mismatch"));
    }
    let stream = &bytes[..trailer];
    let mut position = 12;
    let mut offsets = BTreeMap::new();
    let mut encoded = Vec::new();
    encoded
        .try_reserve_exact(count)
        .map_err(|_| Error::Limit("pack object metadata"))?;
    let mut inflated_total = 0_usize;
    let mut reconstructed_total = 0_usize;
    for index in 0..count {
        check(cancel, deadline)?;
        let entry_offset = position;
        let (kind, length) = object_header(stream, &mut position)?;
        if length > limits.max_object_bytes {
            return Err(Error::Limit("pack object payload"));
        }
        let base = match kind {
            1 => Base::Full("commit"),
            2 => Base::Full("tree"),
            3 => Base::Full("blob"),
            4 => Base::Full("tag"),
            6 => {
                let distance = offset(stream, &mut position)?;
                if distance == 0 {
                    return Err(invalid("delta references itself"));
                }
                let base_offset = entry_offset
                    .checked_sub(distance)
                    .ok_or_else(|| invalid("delta base precedes pack"))?;
                Base::Offset(
                    *offsets
                        .get(&base_offset)
                        .ok_or_else(|| invalid("delta base is not an earlier object"))?,
                )
            }
            7 => {
                let end = position
                    .checked_add(20)
                    .ok_or(Error::Limit("pack base ID"))?;
                let id = stream
                    .get(position..end)
                    .ok_or_else(|| invalid("truncated delta base ID"))?;
                let id =
                    GitObjectId::from_bytes(id.try_into().map_err(|_| invalid("delta base ID"))?);
                position = end;
                Base::Reference(id)
            }
            _ => return Err(invalid("reserved object type")),
        };
        let (payload, consumed) = inflate(
            stream
                .get(position..)
                .ok_or_else(|| invalid("compressed object"))?,
            length,
            limits.max_object_bytes,
            cancel,
            deadline,
        )?;
        position = position
            .checked_add(consumed)
            .ok_or(Error::Limit("pack cursor"))?;
        inflated_total = inflated_total
            .checked_add(payload.len())
            .ok_or(Error::Limit("total inflated pack payload"))?;
        let result_length = match base {
            Base::Full(_) => length,
            _ => {
                let mut cursor = 0;
                if size(&payload, &mut cursor)? > limits.max_object_bytes {
                    return Err(Error::Limit("delta base object"));
                }
                size(&payload, &mut cursor)?
            }
        };
        if result_length > limits.max_object_bytes {
            return Err(Error::Limit("reconstructed pack object"));
        }
        reconstructed_total = reconstructed_total
            .checked_add(result_length)
            .ok_or(Error::Limit("total reconstructed pack payload"))?;
        if inflated_total > limits.max_total_object_bytes
            || reconstructed_total > limits.max_total_object_bytes
        {
            return Err(Error::Limit("total pack object payload"));
        }
        offsets.insert(entry_offset, index);
        encoded.push(Encoded {
            base,
            bytes: payload,
        });
    }
    if position != trailer {
        return Err(invalid("object count or trailing pack bytes mismatch"));
    }
    let mut resolved: Vec<Option<Resolved>> = (0..count).map(|_| None).collect();
    let mut identities = BTreeMap::new();
    let mut remaining = count;
    for _pass in 0..=limits.max_delta_depth {
        let mut progress = false;
        for index in 0..count {
            check(cancel, deadline)?;
            if resolved[index].is_some() {
                continue;
            }
            let entry = &encoded[index];
            let value = match entry.base {
                Base::Full(kind) => {
                    let bytes = std::mem::take(&mut encoded[index].bytes);
                    Resolved {
                        kind,
                        bytes,
                        depth: 0,
                    }
                }
                base => {
                    let base_index = match base {
                        Base::Offset(index) => Some(index),
                        Base::Reference(id) => identities.get(&id).copied(),
                        Base::Full(_) => None,
                    };
                    let Some(base) = base_index
                        .and_then(|index| resolved.get(index))
                        .and_then(Option::as_ref)
                    else {
                        continue;
                    };
                    let depth = base
                        .depth
                        .checked_add(1)
                        .ok_or(Error::Limit("pack delta depth"))?;
                    if depth > limits.max_delta_depth {
                        return Err(Error::Limit("pack delta depth"));
                    }
                    let bytes = delta(
                        &base.bytes,
                        &entry.bytes,
                        limits.max_object_bytes,
                        cancel,
                        deadline,
                    )?;
                    let kind = base.kind;
                    encoded[index].bytes.clear();
                    Resolved { kind, bytes, depth }
                }
            };
            let id = objects::object_id(value.kind, &value.bytes);
            identities.insert(id, index);
            resolved[index] = Some(value);
            remaining -= 1;
            progress = true;
        }
        if remaining == 0 {
            return Ok(());
        }
        if !progress {
            return Err(invalid("thin pack, missing delta base or delta cycle"));
        }
    }
    Err(Error::Limit("pack delta resolution depth"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;
    fn one(kind: u8, body: &[u8], claimed: usize) -> Vec<u8> {
        let mut result = b"PACK".to_vec();
        result.extend_from_slice(&2_u32.to_be_bytes());
        result.extend_from_slice(&1_u32.to_be_bytes());
        let mut length = claimed;
        let mut head = (kind << 4) | (length as u8 & 15);
        length >>= 4;
        if length != 0 {
            head |= 128;
        }
        result.push(head);
        while length != 0 {
            let mut next = length as u8 & 127;
            length >>= 7;
            if length != 0 {
                next |= 128;
            }
            result.push(next);
        }
        let mut zip = ZlibEncoder::new(Vec::new(), Compression::default());
        zip.write_all(body).expect("fixture");
        result.extend_from_slice(&zip.finish().expect("fixture"));
        let checksum = Sha1::digest(&result);
        result.extend_from_slice(&checksum);
        result
    }
    #[test]
    fn pack_size_count_checksum_and_inflate_limits_are_admitted_before_indexing() {
        let token = CancellationToken::new();
        let limits = GitLimits::default();
        let valid = one(3, b"source", 6);
        validate(&valid, &limits, &token).expect("valid public pack");
        let mut checksum = valid.clone();
        checksum[12] ^= 1;
        assert!(validate(&checksum, &limits, &token).is_err());
        let mut small = limits.clone();
        small.max_pack_bytes = 32;
        assert!(matches!(
            validate(&valid, &small, &token),
            Err(Error::Limit(_))
        ));
        let mut small = limits.clone();
        small.max_object_bytes = 4;
        assert!(matches!(
            validate(&valid, &small, &token),
            Err(Error::Limit(_))
        ));
        let bomb = one(3, &[42; 8192], 1);
        assert!(matches!(
            validate(&bomb, &limits, &token),
            Err(Error::Limit(_))
        ));
        let mut extra = valid[..valid.len() - 20].to_vec();
        extra.push(0);
        let sum = Sha1::digest(&extra);
        extra.extend_from_slice(&sum);
        assert!(validate(&extra, &limits, &token).is_err());
    }
    #[test]
    fn delta_lengths_copy_ranges_and_reserved_instructions_are_checked() {
        let deadline = Instant::now() + std::time::Duration::from_secs(1);
        let token = CancellationToken::new();
        assert_eq!(
            delta(b"abc", &[3, 3, 0x90, 3], 32, &token, deadline).expect("copy"),
            b"abc"
        );
        assert_eq!(
            delta(b"abc", &[3, 3, 3, b'x', b'y', b'z'], 32, &token, deadline).expect("insert"),
            b"xyz"
        );
        assert!(delta(b"abc", &[3, 3, 0], 32, &token, deadline).is_err());
        assert!(delta(b"abc", &[3, 4, 0x90, 4], 32, &token, deadline).is_err());
        assert!(delta(b"abc", &[3, 127, 1, b'x'], 32, &token, deadline).is_err());
        assert!(delta(b"abc", &[2, 3, 0x90, 3], 32, &token, deadline).is_err());
    }
    fn entry(result: &mut Vec<u8>, kind: u8, base: &[u8], payload: &[u8]) {
        let mut length = payload.len();
        let mut head = (kind << 4) | (length as u8 & 15);
        length >>= 4;
        if length != 0 {
            head |= 128;
        }
        result.push(head);
        while length != 0 {
            let mut next = length as u8 & 127;
            length >>= 7;
            if length != 0 {
                next |= 128;
            }
            result.push(next);
        }
        result.extend_from_slice(base);
        let mut zip = ZlibEncoder::new(Vec::new(), Compression::default());
        zip.write_all(payload).expect("fixture");
        result.extend_from_slice(&zip.finish().expect("fixture"));
    }
    fn finish(mut result: Vec<u8>) -> Vec<u8> {
        let checksum = Sha1::digest(&result);
        result.extend_from_slice(&checksum);
        result
    }
    fn header(count: u32) -> Vec<u8> {
        let mut result = b"PACK".to_vec();
        result.extend_from_slice(&2_u32.to_be_bytes());
        result.extend_from_slice(&count.to_be_bytes());
        result
    }
    #[test]
    fn full_pack_offset_and_forward_reference_deltas_obey_depth_and_result_budgets() {
        let token = CancellationToken::new();
        let limits = GitLimits::default();
        let mut chain = header(3);
        let first = chain.len();
        entry(&mut chain, 3, &[], b"abc");
        let second = chain.len();
        let distance = u8::try_from(second - first).expect("one byte fixture");
        assert!(distance < 128);
        entry(&mut chain, 6, &[distance], &[3, 4, 0x90, 3, 1, b'd']);
        let third = chain.len();
        let distance = u8::try_from(third - second).expect("one byte fixture");
        assert!(distance < 128);
        entry(&mut chain, 6, &[distance], &[4, 5, 0x90, 4, 1, b'e']);
        let chain = finish(chain);
        validate(&chain, &limits, &token).expect("offset delta chain");
        let mut shallow = limits.clone();
        shallow.max_delta_depth = 1;
        assert!(matches!(
            validate(&chain, &shallow, &token),
            Err(Error::Limit("pack delta depth"))
        ));
        let mut count = limits.clone();
        count.max_objects = 1;
        assert!(matches!(
            validate(&chain, &count, &token),
            Err(Error::Limit("pack object count"))
        ));
        let base = objects::object_id("blob", b"abc");
        let mut forward = header(2);
        entry(&mut forward, 7, base.as_bytes(), &[3, 4, 0x90, 3, 1, b'd']);
        entry(&mut forward, 3, &[], b"abc");
        validate(&finish(forward), &limits, &token)
            .expect("self-contained forward reference delta");
        let mut thin = header(1);
        entry(&mut thin, 7, base.as_bytes(), &[3, 4, 0x90, 3, 1, b'd']);
        assert!(validate(&finish(thin), &limits, &token).is_err());
        let mut expansion = header(2);
        let first = expansion.len();
        entry(&mut expansion, 3, &[], &[42; 1000]);
        let second = expansion.len();
        let distance = u8::try_from(second - first).expect("one byte fixture");
        assert!(distance < 128);
        entry(
            &mut expansion,
            6,
            &[distance],
            &[0xe8, 7, 0xe8, 7, 0xb0, 0xe8, 3],
        );
        let mut budget = limits.clone();
        budget.max_total_object_bytes = 1500;
        assert!(matches!(
            validate(&finish(expansion), &budget, &token),
            Err(Error::Limit("total pack object payload"))
        ));
        let mut result_bomb = header(2);
        let first = result_bomb.len();
        entry(&mut result_bomb, 3, &[], b"abc");
        let second = result_bomb.len();
        let distance = u8::try_from(second - first).expect("one byte fixture");
        entry(&mut result_bomb, 6, &[distance], &[3, 0x80, 0x10, 0x90, 3]);
        budget.max_object_bytes = 1024;
        assert!(matches!(
            validate(&finish(result_bomb), &budget, &token),
            Err(Error::Limit("reconstructed pack object"))
        ));
    }
}
