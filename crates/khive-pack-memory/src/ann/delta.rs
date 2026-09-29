//! Memory-owned, append-only checkpoint overlay for a stable v2 Vamana segment.
//!
//! A publication writes only its new immutable chunks, then atomically
//! replaces a fixed-size HEAD. Orphan chunks from interrupted publications are
//! never reachable from HEAD; compaction clears HEAD after the full segment is
//! committed and removes the old chunks.

use std::{fs, path::Path};

use uuid::Uuid;

use super::AnnBridge;
use khive_vamana::AuxiliarySidecarReader;

pub(super) const HEAD_FILE: &str = "memory_delta.head";
const HEAD_MAGIC: &[u8; 8] = b"KHMDEH01";
const CHUNK_MAGIC: &[u8; 8] = b"KHMDEC01";
const HEAD_LEN: usize = 8 + 32 + 16 + 8 + 8 + 8 + 32;
const CHUNK_LEN: usize = 8 + 32 + 16 + 16 + 8 + 8 + 8 + 32;

#[derive(Clone)]
pub(super) struct DeltaBatch {
    pub(super) applied_seq: u64,
    pub(super) raw_count: u64,
    pub(super) ops: Vec<(Uuid, Option<Vec<f32>>)>,
}

pub(super) struct DeltaOverlay {
    pub(super) batches: Vec<DeltaBatch>,
    pub(super) raw_count: u64,
    pub(super) applied_seq: u64,
    pub(super) identity: [u8; 32],
    pub(super) last_nonce: Uuid,
}

pub(super) struct DeltaPublication {
    pub(super) identity: [u8; 32],
    pub(super) last_nonce: Uuid,
}

struct Head {
    nonce: Uuid,
    applied_seq: u64,
    raw_count: u64,
    identity: [u8; 32],
}

fn identity(base_digest: &[u8; 32], head: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(base_digest);
    hash.update(head);
    *hash.finalize().as_bytes()
}

fn checksum(prefix: &[u8], payload: &[u8]) -> [u8; 32] {
    let mut hash = blake3::Hasher::new();
    hash.update(prefix);
    hash.update(payload);
    *hash.finalize().as_bytes()
}

/// Cumulative raw write-log operations permitted before a full segment rewrite.
pub(super) fn compaction_limit(base_ops: usize) -> u64 {
    u64::try_from(base_ops / 10 + usize::from(!base_ops.is_multiple_of(10)))
        .unwrap_or(u64::MAX)
        .max(5_000)
}

fn chunk_name(nonce: Uuid) -> String {
    format!("memory_delta-{nonce}.bin")
}

fn read_optional(
    reader: &AuxiliarySidecarReader,
    name: &str,
    max_bytes: usize,
) -> Result<Option<Vec<u8>>, String> {
    reader
        .read_bounded(name, max_bytes)
        .map_err(|error| format!("read memory delta {name}: {error}"))
}

fn take<'a>(bytes: &'a [u8], offset: &mut usize, len: usize) -> Result<&'a [u8], String> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| "memory delta offset overflow".to_string())?;
    let part = bytes
        .get(*offset..end)
        .ok_or_else(|| "memory delta is truncated".to_string())?;
    *offset = end;
    Ok(part)
}

fn read_u64(bytes: &[u8], offset: &mut usize) -> Result<u64, String> {
    Ok(u64::from_le_bytes(
        take(bytes, offset, 8)?.try_into().unwrap(),
    ))
}

fn read_nonce(bytes: &[u8], offset: &mut usize) -> Result<Uuid, String> {
    Uuid::from_slice(take(bytes, offset, 16)?)
        .map_err(|error| format!("memory delta nonce: {error}"))
}

fn parse_head(bytes: &[u8], base_digest: &[u8; 32], base_seq: u64) -> Result<Option<Head>, String> {
    if bytes.len() != HEAD_LEN || &bytes[..8] != HEAD_MAGIC {
        return Err("memory delta HEAD length or magic is invalid".into());
    }
    if checksum(&bytes[..80], &[]) != bytes[80..] {
        return Err("memory delta HEAD checksum mismatch".into());
    }
    let mut offset = 40;
    let nonce = read_nonce(bytes, &mut offset)?;
    let recorded_base_seq = read_u64(bytes, &mut offset)?;
    let applied_seq = read_u64(bytes, &mut offset)?;
    let raw_count = read_u64(bytes, &mut offset)?;
    if nonce.is_nil() || applied_seq <= recorded_base_seq || raw_count == 0 {
        return Err("memory delta HEAD watermark or count is invalid".into());
    }
    if &bytes[8..40] != base_digest || recorded_base_seq != base_seq {
        // Full segment publication commits metadata before it can unlink the
        // predecessor HEAD. The new base already contains that old overlay,
        // so ignore the stale HEAD only when its watermark is covered.
        if base_seq >= applied_seq {
            return Ok(None);
        }
        return Err("memory delta HEAD base commit mismatch".into());
    }
    Ok(Some(Head {
        nonce,
        applied_seq,
        raw_count,
        identity: identity(base_digest, bytes),
    }))
}

fn read_head(
    reader: &AuxiliarySidecarReader,
    base_digest: &[u8; 32],
    base_seq: u64,
) -> Result<Option<Head>, String> {
    read_optional(reader, HEAD_FILE, HEAD_LEN)?
        .map(|bytes| parse_head(&bytes, base_digest, base_seq))
        .transpose()
        .map(Option::flatten)
}

/// Read the committed overlay watermark without restoring graph or chunks.
pub(super) fn read_info(
    dir: &Path,
    base_digest: &[u8; 32],
    base_seq: u64,
) -> Result<Option<(u64, u64)>, String> {
    let reader = AuxiliarySidecarReader::open(dir)
        .map_err(|error| format!("open memory delta directory: {error}"))?;
    Ok(read_head(&reader, base_digest, base_seq)?.map(|head| (head.applied_seq, head.raw_count)))
}

/// A fresh HEAD nonce changes the publication identity on every checkpoint.
pub(super) fn publication_digest(dir: &Path) -> Result<Option<[u8; 32]>, String> {
    let Some(base_digest) = khive_vamana::segment_commit_digest(dir)? else {
        return Ok(None);
    };
    let base_seq = khive_vamana::read_commit_info(dir)
        .map_err(|error| error.to_string())?
        .and_then(|info| info.last_applied_seq)
        .unwrap_or(0);
    let reader = AuxiliarySidecarReader::open(dir)
        .map_err(|error| format!("open memory delta directory: {error}"))?;
    Ok(Some(match read_head(&reader, &base_digest, base_seq)? {
        Some(head) => head.identity,
        None => base_digest,
    }))
}

fn parse_chunk(
    bytes: &[u8],
    expected_nonce: Uuid,
    base_digest: &[u8; 32],
    dimensions: usize,
) -> Result<(Uuid, DeltaBatch), String> {
    if bytes.len() < CHUNK_LEN || &bytes[..8] != CHUNK_MAGIC || &bytes[8..40] != base_digest {
        return Err("memory delta chunk header or base commit mismatch".into());
    }
    if checksum(&bytes[..96], &bytes[CHUNK_LEN..]) != bytes[96..CHUNK_LEN] {
        return Err("memory delta chunk checksum mismatch".into());
    }
    let mut offset = 40;
    let nonce = read_nonce(bytes, &mut offset)?;
    let previous = read_nonce(bytes, &mut offset)?;
    let applied_seq = read_u64(bytes, &mut offset)?;
    let raw_count = read_u64(bytes, &mut offset)?;
    let op_count = usize::try_from(read_u64(bytes, &mut offset)?)
        .map_err(|_| "memory delta chunk op count overflow")?;
    if nonce != expected_nonce
        || previous == nonce
        || raw_count == 0
        || op_count == 0
        || u64::try_from(op_count).unwrap_or(u64::MAX) > raw_count
    {
        return Err("memory delta chunk nonce, count, or linkage is invalid".into());
    }
    let payload = &bytes[CHUNK_LEN..];
    if op_count > payload.len() / 17 {
        return Err("memory delta chunk op count exceeds payload".into());
    }
    offset = 0;
    let mut ops = Vec::with_capacity(op_count);
    for _ in 0..op_count {
        let id = Uuid::from_slice(take(payload, &mut offset, 16)?)
            .map_err(|error| format!("memory delta UUID: {error}"))?;
        let vector = match take(payload, &mut offset, 1)?[0] {
            0 => None,
            1 => {
                let length = usize::try_from(read_u64(payload, &mut offset)?)
                    .map_err(|_| "memory delta vector length overflow")?;
                if length != dimensions {
                    return Err("memory delta vector dimensions mismatch".into());
                }
                let mut vector = Vec::with_capacity(length);
                for _ in 0..length {
                    let value =
                        f32::from_le_bytes(take(payload, &mut offset, 4)?.try_into().unwrap());
                    if !value.is_finite() {
                        return Err("memory delta vector has non-finite value".into());
                    }
                    vector.push(value);
                }
                Some(vector)
            }
            _ => return Err("memory delta operation tag is invalid".into()),
        };
        ops.push((id, vector));
    }
    if offset != payload.len() {
        return Err("memory delta chunk has trailing bytes".into());
    }
    Ok((
        previous,
        DeltaBatch {
            applied_seq,
            raw_count,
            ops,
        },
    ))
}

pub(super) fn read(
    dir: &Path,
    base_digest: &[u8; 32],
    base_seq: u64,
    dimensions: usize,
    base_ops: usize,
) -> Result<Option<DeltaOverlay>, String> {
    let reader = AuxiliarySidecarReader::open(dir)
        .map_err(|error| format!("open memory delta directory: {error}"))?;
    let Some(head) = read_head(&reader, base_digest, base_seq)? else {
        return Ok(None);
    };
    if head.raw_count >= compaction_limit(base_ops) {
        return Err("memory delta passed mandatory compaction bound".into());
    }
    let mut remaining = head.raw_count;
    let mut next_seq = head.applied_seq;
    let mut nonce = head.nonce;
    let mut reversed = Vec::new();
    while !nonce.is_nil() {
        let max_chunk_bytes = usize::try_from(remaining)
            .ok()
            .and_then(|remaining| {
                dimensions
                    .checked_mul(4)
                    .and_then(|vector_bytes| vector_bytes.checked_add(25))
                    .and_then(|per_op| per_op.checked_mul(remaining))
                    .and_then(|payload| payload.checked_add(CHUNK_LEN))
            })
            .ok_or_else(|| "memory delta chunk size bound overflow".to_string())?;
        let bytes = read_optional(&reader, &chunk_name(nonce), max_chunk_bytes)?
            .ok_or_else(|| format!("committed memory delta chunk {nonce} is missing"))?;
        let (previous, batch) = parse_chunk(&bytes, nonce, base_digest, dimensions)?;
        let bad_seq = if reversed.is_empty() {
            batch.applied_seq != next_seq
        } else {
            batch.applied_seq >= next_seq
        };
        if bad_seq || batch.raw_count > remaining {
            return Err("memory delta chunk sequence or raw count is invalid".into());
        }
        remaining -= batch.raw_count;
        next_seq = batch.applied_seq;
        nonce = previous;
        reversed.push(batch);
        if reversed.len() as u64 > head.raw_count {
            return Err("memory delta chain exceeds raw operation count".into());
        }
    }
    reversed.reverse();
    let mut seq = base_seq;
    for batch in &reversed {
        if batch.applied_seq <= seq {
            return Err("memory delta chunk sequence is not increasing".into());
        }
        seq = batch.applied_seq;
    }
    if remaining != 0 || seq != head.applied_seq {
        return Err("memory delta chain does not match HEAD".into());
    }
    Ok(Some(DeltaOverlay {
        batches: reversed,
        raw_count: head.raw_count,
        applied_seq: head.applied_seq,
        identity: head.identity,
        last_nonce: head.nonce,
    }))
}

fn encode_chunk(
    base_digest: &[u8; 32],
    nonce: Uuid,
    previous: Uuid,
    batch: &DeltaBatch,
) -> Vec<u8> {
    let mut payload = Vec::new();
    for (id, vector) in &batch.ops {
        payload.extend_from_slice(id.as_bytes());
        match vector {
            None => payload.push(0),
            Some(vector) => {
                payload.push(1);
                payload.extend_from_slice(&(vector.len() as u64).to_le_bytes());
                for value in vector {
                    payload.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
    }
    let mut bytes = Vec::with_capacity(CHUNK_LEN + payload.len());
    bytes.extend_from_slice(CHUNK_MAGIC);
    bytes.extend_from_slice(base_digest);
    bytes.extend_from_slice(nonce.as_bytes());
    bytes.extend_from_slice(previous.as_bytes());
    bytes.extend_from_slice(&batch.applied_seq.to_le_bytes());
    bytes.extend_from_slice(&batch.raw_count.to_le_bytes());
    bytes.extend_from_slice(&(batch.ops.len() as u64).to_le_bytes());
    let hash = checksum(&bytes, &payload);
    bytes.extend_from_slice(&hash);
    bytes.extend_from_slice(&payload);
    bytes
}

pub(super) fn write(dir: &Path, bridge: &AnnBridge) -> Result<DeltaPublication, String> {
    let base_digest = bridge
        .base_commit_digest
        .ok_or_else(|| "memory delta has no base segment commit".to_string())?;
    if bridge.delta_batches.is_empty() || bridge.delta_raw_ops >= compaction_limit(bridge.base_ops)
    {
        return Err("memory delta is empty or requires full compaction".into());
    }
    let mut previous = bridge.last_delta_nonce.unwrap_or(Uuid::nil());
    for batch in &bridge.delta_batches {
        let nonce = Uuid::new_v4();
        let bytes = encode_chunk(&base_digest, nonce, previous, batch);
        khive_vamana::write_auxiliary_sidecar_atomic(dir, &chunk_name(nonce), &bytes)
            .map_err(|error| format!("publish memory delta chunk: {error}"))?;
        previous = nonce;
    }
    let applied_seq = bridge
        .index
        .last_applied_seq()
        .unwrap_or(bridge.base_applied_seq);
    let mut head = Vec::with_capacity(HEAD_LEN);
    head.extend_from_slice(HEAD_MAGIC);
    head.extend_from_slice(&base_digest);
    head.extend_from_slice(previous.as_bytes());
    head.extend_from_slice(&bridge.base_applied_seq.to_le_bytes());
    head.extend_from_slice(&applied_seq.to_le_bytes());
    head.extend_from_slice(&bridge.delta_raw_ops.to_le_bytes());
    let hash = checksum(&head, &[]);
    head.extend_from_slice(&hash);
    let publication = DeltaPublication {
        identity: identity(&base_digest, &head),
        last_nonce: previous,
    };
    khive_vamana::write_auxiliary_sidecar_atomic(dir, HEAD_FILE, &head)
        .map_err(|error| format!("publish memory delta HEAD: {error}"))?;
    Ok(publication)
}

/// Once the new full segment is committed, HEAD removal makes every old chunk
/// unreachable. Cleanup is best effort so a failed orphan deletion cannot
/// invalidate an otherwise complete full checkpoint.
pub(super) fn clear(dir: &Path) -> Result<(), String> {
    khive_vamana::remove_auxiliary_sidecar(dir, HEAD_FILE)
        .map_err(|error| format!("remove memory delta HEAD: {error}"))?;
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "memory delta orphan scan failed");
            return Ok(());
        }
    };
    let mut chunks = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                tracing::warn!(%error, "memory delta orphan directory entry failed");
                continue;
            }
        };
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let Some(raw) = name
            .strip_prefix("memory_delta-")
            .and_then(|raw| raw.strip_suffix(".bin"))
        else {
            continue;
        };
        if Uuid::parse_str(raw).is_ok() {
            chunks.push(name.to_string());
        }
    }
    if let Err(error) = khive_vamana::remove_auxiliary_sidecars(dir, &chunks) {
        tracing::warn!(%error, "memory delta orphan cleanup failed");
    }
    Ok(())
}
