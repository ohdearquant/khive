//! Memory-owned, append-only checkpoint overlay for a stable v2 Vamana segment.
//!
//! A publication coalesces the batches applied since the previous publication
//! into one new immutable chunk, then atomically replaces a fixed-size HEAD.
//! Orphan chunks from interrupted publications are never reachable from HEAD.
//! After a full segment is committed, cleanup walks the retired chain from
//! HEAD, removes HEAD and every chunk on that chain, then runs a bounded
//! best-effort sweep for crash leftovers.

use std::collections::HashSet;
use std::path::Path;

use uuid::Uuid;

use super::AnnBridge;
use khive_vamana::{AuxiliarySidecarCleaner, AuxiliarySidecarReader};

pub(super) const HEAD_FILE: &str = "memory_delta.head";
const HEAD_MAGIC: &[u8; 8] = b"KHMDEH01";
const CHUNK_MAGIC: &[u8; 8] = b"KHMDEC01";
const HEAD_LEN: usize = 8 + 32 + 16 + 8 + 8 + 8 + 32;
const CHUNK_LEN: usize = 8 + 32 + 16 + 16 + 8 + 8 + 8 + 32;
// Magic, base digest, nonce, and previous nonce: enough to follow a link.
const CHUNK_LINK_LEN: usize = 8 + 32 + 16 + 16;
const MIN_COMPACTION_OPS: usize = 5_000;
// The retired chain is removed by following its links, so this budget bounds
// only the sweep for crash leftovers (unpublished chunks and staging files).
const ORPHAN_SCAN_BUDGET: usize = MIN_COMPACTION_OPS * 2;
// Publication keeps every new chain within this cleanup walk budget, separate
// from the cumulative raw-operation policy. Legacy or malformed longer chains
// are still walked within this bound and left to the bounded orphan sweep.
pub(super) const MAX_RETIRED_CHAIN: usize = 100_000;

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
    pub(super) chunk_count: usize,
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
        .max(MIN_COMPACTION_OPS as u64)
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
    if bridge.needs_full_compaction() {
        return Err("memory delta is empty or requires full compaction".into());
    }
    let chunk_count = bridge
        .delta_chunks
        .checked_add(1)
        .ok_or_else(|| "memory delta chunk count overflow".to_string())?;
    // One chunk per publication: the last final-state operation per UUID wins,
    // raw counts add up, and the newest batch supplies the chunk watermark.
    let mut seen = HashSet::new();
    let mut ops = Vec::new();
    for (id, vector) in bridge
        .delta_batches
        .iter()
        .rev()
        .flat_map(|batch| batch.ops.iter().rev())
    {
        if seen.insert(*id) {
            ops.push((*id, vector.clone()));
        }
    }
    ops.reverse();
    let coalesced = DeltaBatch {
        applied_seq: bridge
            .delta_batches
            .last()
            .map_or(0, |batch| batch.applied_seq),
        raw_count: bridge
            .delta_batches
            .iter()
            .fold(0u64, |sum, batch| sum.saturating_add(batch.raw_count)),
        ops,
    };
    let nonce = Uuid::new_v4();
    let previous = bridge.last_delta_nonce.unwrap_or(Uuid::nil());
    let bytes = encode_chunk(&base_digest, nonce, previous, &coalesced);
    khive_vamana::write_auxiliary_sidecar_atomic(dir, &chunk_name(nonce), &bytes)
        .map_err(|error| format!("publish memory delta chunk: {error}"))?;
    let applied_seq = bridge
        .index
        .last_applied_seq()
        .unwrap_or(bridge.base_applied_seq);
    let mut head = Vec::with_capacity(HEAD_LEN);
    head.extend_from_slice(HEAD_MAGIC);
    head.extend_from_slice(&base_digest);
    head.extend_from_slice(nonce.as_bytes());
    head.extend_from_slice(&bridge.base_applied_seq.to_le_bytes());
    head.extend_from_slice(&applied_seq.to_le_bytes());
    head.extend_from_slice(&bridge.delta_raw_ops.to_le_bytes());
    let hash = checksum(&head, &[]);
    head.extend_from_slice(&hash);
    let publication = DeltaPublication {
        identity: identity(&base_digest, &head),
        last_nonce: nonce,
        chunk_count,
    };
    khive_vamana::write_auxiliary_sidecar_atomic(dir, HEAD_FILE, &head)
        .map_err(|error| format!("publish memory delta HEAD: {error}"))?;
    Ok(publication)
}

/// Chunk names on the chain HEAD currently names, newest first. Following the
/// `previous` links costs one short read per chunk, independent of how many
/// other entries the directory holds. The walk is best effort: it stops at a
/// missing or malformed link, at a link that revisits a chunk already on the
/// walk, and after `MAX_RETIRED_CHAIN` chunks, and leaves the rest to the
/// orphan sweep.
fn retired_chain(dir: &Path) -> Vec<String> {
    retired_chain_within(dir, MAX_RETIRED_CHAIN)
}

fn retired_chain_within(dir: &Path, max_chunks: usize) -> Vec<String> {
    let reader = match AuxiliarySidecarReader::open(dir) {
        Ok(reader) => reader,
        Err(error) => {
            tracing::warn!(%error, "memory delta retired chain directory open failed");
            return Vec::new();
        }
    };
    let head = match read_optional(&reader, HEAD_FILE, HEAD_LEN) {
        Ok(Some(head)) => head,
        Ok(None) => return Vec::new(),
        Err(error) => {
            tracing::warn!(%error, "memory delta retired HEAD read failed");
            return Vec::new();
        }
    };
    if head.len() != HEAD_LEN
        || &head[..8] != HEAD_MAGIC
        || checksum(&head[..80], &[]) != head[80..]
    {
        tracing::warn!("memory delta retired HEAD is malformed; leaving its chain to the sweep");
        return Vec::new();
    }
    let Ok(mut nonce) = read_nonce(&head, &mut 40) else {
        return Vec::new();
    };
    let Ok(raw_count) = read_u64(&head, &mut 72) else {
        return Vec::new();
    };
    // HEAD's checksum proves its integrity, not that its raw count is sane, and
    // a chunk may link back to one already visited. Every chunk on the walk is
    // therefore distinct and the walk is capped by `max_chunks` as well as by
    // HEAD's raw count; neither a looping link nor a huge count can make it
    // run or grow without bound.
    let mut names = Vec::new();
    let mut visited = HashSet::new();
    while !nonce.is_nil() && (names.len() as u64) < raw_count {
        if names.len() >= max_chunks {
            tracing::warn!(
                max_chunks,
                "memory delta retired chain exceeds the walk bound; leaving the rest to the sweep"
            );
            break;
        }
        if !visited.insert(nonce) {
            tracing::warn!(
                "memory delta retired chain revisits a chunk; leaving the rest to the sweep"
            );
            break;
        }
        let name = chunk_name(nonce);
        let link = match reader.read_prefix(&name, CHUNK_LINK_LEN) {
            Ok(Some(link)) => link,
            Ok(None) => break,
            Err(error) => {
                tracing::warn!(%error, "memory delta retired chunk read failed");
                break;
            }
        };
        names.push(name);
        if link.len() != CHUNK_LINK_LEN
            || &link[..8] != CHUNK_MAGIC
            || link[40..56] != *nonce.as_bytes()
        {
            break;
        }
        let Ok(previous) = Uuid::from_slice(&link[56..72]) else {
            break;
        };
        nonce = previous;
    }
    names
}

/// Once the new full segment is committed, the retired chain is read from
/// HEAD and its chunks are deleted by name, oldest first, with HEAD removed
/// last. Readers already ignore that HEAD because the new base covers its
/// watermark, and a crash part-way leaves HEAD naming exactly the surviving
/// newer chunks, so the next full checkpoint resumes the walk. A bounded
/// directory sweep then catches crash leftovers. Only HEAD removal is
/// reported: chunk deletion is best effort so a failed unlink cannot
/// invalidate an otherwise complete full checkpoint.
pub(super) fn clear(dir: &Path) -> Result<(), String> {
    clear_with_scan_budget(dir, ORPHAN_SCAN_BUDGET)
}

fn clear_with_scan_budget(dir: &Path, scan_budget: usize) -> Result<(), String> {
    clear_with_scan_budget_then(dir, scan_budget, || {})
}

fn clear_with_scan_budget_then(
    dir: &Path,
    scan_budget: usize,
    after_head: impl FnOnce(),
) -> Result<(), String> {
    let cleaner = AuxiliarySidecarCleaner::open(dir)
        .map_err(|error| format!("open memory delta directory for cleanup: {error}"))?;
    let mut retired = retired_chain(dir);
    retired.reverse();
    if let Err(error) = cleaner.remove_many_and_sync(&retired) {
        tracing::warn!(%error, "memory delta retired chain cleanup failed");
    }
    cleaner
        .remove_and_sync(HEAD_FILE)
        .map_err(|error| format!("remove memory delta HEAD: {error}"))?;
    after_head();
    let (entries, truncated) = match cleaner.scan_names_bounded(scan_budget) {
        Ok(entries) => entries,
        Err(error) => {
            tracing::warn!(%error, "memory delta orphan scan failed");
            return Ok(());
        }
    };
    if truncated {
        tracing::warn!(scan_budget, "memory delta orphan scan budget reached; remaining entries await a later full checkpoint");
    }
    let mut chunks = Vec::new();
    for name in entries {
        // `.bin.tmp` is the staging name of a chunk whose publication never
        // reached its rename; writers hold the checkpoint lock, so it is stale.
        let Some(raw) = name.strip_prefix("memory_delta-").and_then(|raw| {
            raw.strip_suffix(".bin.tmp")
                .or_else(|| raw.strip_suffix(".bin"))
        }) else {
            continue;
        };
        if Uuid::parse_str(raw).is_ok() {
            chunks.push(name);
        }
    }
    if let Err(error) = cleaner.remove_many_and_sync(&chunks) {
        tracing::warn!(%error, "memory delta orphan cleanup failed");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::{segment_commit_digest, write_external_ids_sidecar};
    use super::*;
    use std::fs;

    fn persisted_base(dir: &Path) -> AnnBridge {
        let mut base = AnnBridge::build(
            vec![1.0, 0.0, 0.0, 0.0],
            4,
            vec![Uuid::new_v4()],
            std::collections::HashSet::new(),
        )
        .expect("build base");
        base.set_applied_seq(1);
        base.save_atomic(dir).expect("persist base");
        AnnBridge::load(dir).expect("load base")
    }

    fn publish(dir: &Path, bridge: &mut AnnBridge) -> DeltaPublication {
        let publication = write(dir, bridge).expect("publish delta");
        bridge.mark_delta_checkpoint(&publication);
        publication
    }

    fn delta_names(dir: &Path) -> Vec<String> {
        fs::read_dir(dir)
            .expect("list segment directory")
            .filter_map(|entry| {
                entry
                    .expect("directory entry")
                    .file_name()
                    .into_string()
                    .ok()
            })
            .filter(|name| name.starts_with("memory_delta-"))
            .collect()
    }

    #[test]
    fn publication_coalesces_pending_batches_into_one_chunk() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut bridge = persisted_base(dir);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        let batches = [
            (
                vec![
                    (a, Some(vec![0.0, 1.0, 0.0, 0.0])),
                    (b, Some(vec![0.0, 0.0, 1.0, 0.0])),
                ],
                2,
                2,
            ),
            (vec![(a, Some(vec![0.0, 0.0, 0.0, 1.0]))], 3, 3),
            (vec![(b, None)], 4, 1),
        ];
        for (ops, seq, raw) in batches {
            bridge
                .apply_final_ops(ops.clone(), seq)
                .expect("apply batch");
            bridge.record_delta_batch(ops, seq, raw);
        }

        let publication = publish(dir, &mut bridge);
        assert_eq!(bridge.delta_chunks, 1, "pending batches are not chunks");

        assert_eq!(
            delta_names(dir),
            vec![chunk_name(publication.last_nonce)],
            "three pending batches publish exactly one chunk"
        );
        let base_digest = segment_commit_digest(dir).unwrap().expect("base commit");
        let overlay = read(dir, &base_digest, 1, 4, 1)
            .expect("read coalesced chain")
            .expect("HEAD exists");
        assert_eq!(overlay.batches.len(), 1);
        let chunk = &overlay.batches[0];
        assert_eq!((chunk.applied_seq, chunk.raw_count), (4, 6));
        assert_eq!(
            chunk.ops,
            vec![(a, Some(vec![0.0, 0.0, 0.0, 1.0])), (b, None)],
            "the last final-state operation per UUID wins"
        );
        let adopted = AnnBridge::load(dir).expect("replay coalesced chunk");
        assert_eq!(adopted.delta_chunks, 1);
        assert_eq!(adopted.commit_digest, Some(publication.identity));
        assert!(adopted.id_map.contains(&a));
    }

    fn add_delta_op(bridge: &mut AnnBridge, seq: u64) -> Uuid {
        let id = Uuid::new_v4();
        let ops = vec![(id, Some(vec![0.0, 1.0, seq as f32, 0.0]))];
        bridge
            .apply_final_ops(ops.clone(), seq)
            .expect("apply tail");
        bridge.record_delta_batch(ops, seq, 1);
        id
    }

    fn accept_checkpoint(bridge: &mut AnnBridge, written: super::super::WrittenCheckpoint) -> bool {
        match written {
            super::super::WrittenCheckpoint::Delta(publication) => {
                bridge.mark_delta_checkpoint(&publication);
                bridge.mark_checkpointed();
                false
            }
            super::super::WrittenCheckpoint::Full(digest) => {
                bridge.mark_full_checkpoint_base(digest);
                bridge.mark_checkpointed();
                true
            }
        }
    }

    #[test]
    fn checkpoint_publication_compacts_before_chain_exceeds_cleanup_walk() {
        const WALK_CAP: usize = 2;
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut bridge = persisted_base(dir);
        let base_metadata = fs::read(dir.join("metadata.bin")).expect("base metadata");
        let mut ids = Vec::new();
        for seq in 2..=4 {
            ids.push(add_delta_op(&mut bridge, seq));
        }
        assert_eq!(bridge.delta_batches.len(), 3);
        let first = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
            .expect("publish coalesced chunk");
        assert!(!accept_checkpoint(&mut bridge, first));
        assert_eq!(bridge.delta_chunks, 1);
        assert_eq!(bridge.delta_raw_ops, 3);
        ids.push(add_delta_op(&mut bridge, 5));
        let second = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
            .expect("publish chunk at walk boundary");
        assert!(!accept_checkpoint(&mut bridge, second));
        assert_eq!(bridge.delta_chunks, WALK_CAP);
        assert_eq!(fs::read(dir.join("metadata.bin")).unwrap(), base_metadata);
        let retired = retired_chain_within(dir, WALK_CAP);
        assert_eq!(retired.len(), WALK_CAP);
        assert_eq!(delta_names(dir).len(), WALK_CAP);

        bridge = AnnBridge::load(dir).expect("restart at chain boundary");
        ids.push(add_delta_op(&mut bridge, 6));
        assert!(bridge.delta_raw_ops < compaction_limit(bridge.base_ops));
        let third = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
            .expect("checkpoint after restored chain reaches walk bound");
        let full = accept_checkpoint(&mut bridge, third);
        let observed = retired_chain_within(dir, WALK_CAP + 1);
        assert!(
            observed.len() <= WALK_CAP,
            "PUBLISHED_CHAIN_EXCEEDS_CLEANUP_WALK: {} chunks, cap {WALK_CAP}",
            observed.len()
        );
        assert!(full, "chain bound must select a full checkpoint");
        assert_ne!(fs::read(dir.join("metadata.bin")).unwrap(), base_metadata);
        assert!(!dir.join(HEAD_FILE).exists());
        assert!(delta_names(dir).is_empty(), "full save retires every chunk");
        assert_eq!(bridge.delta_chunks, 0, "FULL_CHUNK_COUNT_RESET");
        assert!(bridge.delta_batches.is_empty());
        assert_eq!(bridge.delta_raw_ops, 0);
        let adopted = AnnBridge::load(dir).expect("adopt full checkpoint");
        assert_eq!(adopted.delta_chunks, 0);
        assert_eq!(adopted.index.last_applied_seq(), Some(6));
        for id in ids {
            assert!(adopted.id_map.contains(&id));
        }

        let next = add_delta_op(&mut bridge, 7);
        let first_new = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
            .expect("first delta over compacted base");
        assert!(!accept_checkpoint(&mut bridge, first_new));
        assert_eq!(bridge.delta_chunks, 1);
        let next_adopter = AnnBridge::load(dir).expect("adopt post-compaction delta");
        assert_eq!(next_adopter.delta_chunks, 1);
        assert!(next_adopter.id_map.contains(&next));
    }

    #[test]
    fn failed_head_publication_keeps_accepted_chunk_count_and_retry_chain() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut bridge = persisted_base(dir);
        add_delta_op(&mut bridge, 2);
        publish(dir, &mut bridge);
        assert_eq!(bridge.delta_chunks, 1);
        let head_before = fs::read(dir.join(HEAD_FILE)).expect("accepted HEAD");
        let identity_before = bridge.commit_digest;
        let nonce_before = bridge.last_delta_nonce;
        let pending = add_delta_op(&mut bridge, 3);
        let blocked_stage = dir.join(format!("{HEAD_FILE}.tmp"));
        fs::create_dir(&blocked_stage).expect("block HEAD staging, retain old HEAD");

        let refusal = write(dir, &bridge)
            .err()
            .expect("HEAD publication must fail");
        assert!(refusal.contains("publish memory delta HEAD"), "{refusal}");
        assert_eq!(bridge.delta_chunks, 1);
        assert_eq!(bridge.commit_digest, identity_before);
        assert_eq!(bridge.last_delta_nonce, nonce_before);
        assert_eq!(bridge.delta_batches.len(), 1);
        assert_eq!(fs::read(dir.join(HEAD_FILE)).unwrap(), head_before);
        assert_eq!(delta_names(dir).len(), 2, "failed HEAD leaves one orphan");
        let before_retry = AnnBridge::load(dir).expect("old accepted chain remains loadable");
        assert_eq!(before_retry.delta_chunks, 1);
        assert!(!before_retry.id_map.contains(&pending));

        fs::remove_dir(&blocked_stage).expect("unblock HEAD staging");
        publish(dir, &mut bridge);
        assert_eq!(bridge.delta_chunks, 2, "orphan must not count as accepted");
        assert_eq!(retired_chain_within(dir, 3).len(), 2);
        assert_eq!(delta_names(dir).len(), 3);
        let adopted = AnnBridge::load(dir).expect("adopt successful retry");
        assert_eq!(adopted.delta_chunks, 2);
        assert!(adopted.id_map.contains(&pending));
    }

    #[test]
    fn failed_full_publication_retains_delta_count_and_pending_batch() {
        const WALK_CAP: usize = 2;
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut bridge = persisted_base(dir);
        for seq in 2..=3 {
            add_delta_op(&mut bridge, seq);
            publish(dir, &mut bridge);
        }
        let pending = add_delta_op(&mut bridge, 4);
        let head_before = fs::read(dir.join(HEAD_FILE)).expect("accepted HEAD");
        let metadata_before = fs::read(dir.join("metadata.bin")).expect("accepted base");
        let identity_before = bridge.commit_digest;
        let invalid_dir = dir.join("not-a-checkpoint-directory");
        fs::write(&invalid_dir, b"regular file").expect("block checkpoint directory");

        assert!(super::super::write_file_checkpoint(&invalid_dir, &bridge, WALK_CAP).is_err());
        assert_eq!(bridge.delta_chunks, WALK_CAP);
        assert_eq!(bridge.delta_batches.len(), 1);
        assert_eq!(bridge.delta_raw_ops, 3);
        assert_eq!(bridge.commit_digest, identity_before);
        assert_eq!(fs::read(dir.join(HEAD_FILE)).unwrap(), head_before);
        assert_eq!(fs::read(dir.join("metadata.bin")).unwrap(), metadata_before);
        let still_published = AnnBridge::load(dir).expect("old accepted checkpoint is intact");
        assert_eq!(still_published.delta_chunks, WALK_CAP);
        assert!(!still_published.id_map.contains(&pending));

        let retry = super::super::write_file_checkpoint(dir, &bridge, WALK_CAP)
            .expect("full publication retries from retained chain");
        assert!(accept_checkpoint(&mut bridge, retry));
        assert_eq!(bridge.delta_chunks, 0);
        let adopted = AnnBridge::load(dir).expect("adopt full retry");
        assert_eq!(adopted.delta_chunks, 0);
        assert!(adopted.id_map.contains(&pending));
        assert!(delta_names(dir).is_empty());
    }

    #[test]
    fn full_checkpoint_reclaims_retired_chain_longer_than_scan_budget() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut bridge = persisted_base(dir);
        let mut chain = Vec::new();
        for seq in 2..=7u64 {
            let ops = vec![(Uuid::new_v4(), Some(vec![0.0, 1.0, seq as f32, 0.0]))];
            bridge
                .apply_final_ops(ops.clone(), seq)
                .expect("apply tail");
            bridge.record_delta_batch(ops, seq, 1);
            chain.push(chunk_name(publish(dir, &mut bridge).last_nonce));
        }
        assert_eq!(chain.len(), 6);

        // Commit the new base exactly as a full save does, then retire the
        // chain with a scan budget far smaller than the chain.
        bridge.index.save_atomic(dir).expect("commit new base");
        let digest = segment_commit_digest(dir).unwrap().expect("new commit");
        write_external_ids_sidecar(dir, &digest, &bridge.id_map).expect("commit UUID sidecar");
        clear_with_scan_budget(dir, 1).expect("retire chain");

        assert!(!dir.join(HEAD_FILE).exists());
        let left: Vec<_> = chain
            .iter()
            .filter(|name| dir.join(name).exists())
            .collect();
        assert!(
            left.is_empty(),
            "every chunk on the retired chain must be removed, left {left:?}"
        );
        assert!(AnnBridge::load(dir).is_ok(), "the new base adopts cleanly");
    }

    /// Publish a six-chunk chain, then commit a new base over it exactly as a
    /// full save does, leaving the chain retired but not yet cleaned.
    fn retired_six_chunk_chain(dir: &Path) -> Vec<String> {
        let mut bridge = persisted_base(dir);
        let mut chain = Vec::new();
        for seq in 2..=7u64 {
            let ops = vec![(Uuid::new_v4(), Some(vec![0.0, 1.0, seq as f32, 0.0]))];
            bridge
                .apply_final_ops(ops.clone(), seq)
                .expect("apply tail");
            bridge.record_delta_batch(ops, seq, 1);
            chain.push(chunk_name(publish(dir, &mut bridge).last_nonce));
        }
        bridge.index.save_atomic(dir).expect("commit new base");
        let digest = segment_commit_digest(dir).unwrap().expect("new commit");
        write_external_ids_sidecar(dir, &digest, &bridge.id_map).expect("commit UUID sidecar");
        chain
    }

    #[test]
    fn retired_chunks_are_removed_before_head() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path().to_path_buf();
        let chain = retired_six_chunk_chain(&dir);
        let observed = dir.clone();
        clear_with_scan_budget_then(&dir, 1, move || {
            let left: Vec<_> = chain
                .iter()
                .filter(|name| observed.join(name).exists())
                .collect();
            assert!(
                left.is_empty(),
                "HEAD must outlive every retired chunk, left at HEAD removal {left:?}"
            );
        })
        .expect("retire chain");
        assert!(!dir.join(HEAD_FILE).exists());
    }

    #[test]
    fn retired_chain_cleanup_resumes_after_a_crash_before_head_removal() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let chain = retired_six_chunk_chain(dir);
        // A crash part-way through cleanup leaves HEAD and the newer chunks,
        // because chunks go oldest first and HEAD goes last.
        for name in &chain[..3] {
            fs::remove_file(dir.join(name)).expect("remove oldest chunk");
        }
        assert!(
            AnnBridge::load(dir).is_ok(),
            "a retired HEAD covered by the new base is ignored by the loader"
        );

        clear_with_scan_budget(dir, 1).expect("resume cleanup");

        assert!(!dir.join(HEAD_FILE).exists());
        let left: Vec<_> = chain
            .iter()
            .filter(|name| dir.join(name).exists())
            .collect();
        assert!(left.is_empty(), "resumed cleanup must remove {left:?}");
    }

    /// A checksum-valid HEAD naming `nonce` with an arbitrary raw count.
    fn head_with_count(nonce: Uuid, raw_count: u64) -> Vec<u8> {
        let mut head = Vec::with_capacity(HEAD_LEN);
        head.extend_from_slice(HEAD_MAGIC);
        head.extend_from_slice(&[7u8; 32]);
        head.extend_from_slice(nonce.as_bytes());
        head.extend_from_slice(&1u64.to_le_bytes());
        head.extend_from_slice(&2u64.to_le_bytes());
        head.extend_from_slice(&raw_count.to_le_bytes());
        let hash = checksum(&head, &[]);
        head.extend_from_slice(&hash);
        head
    }

    fn write_chunk_linking(dir: &Path, nonce: Uuid, previous: Uuid) {
        let batch = DeltaBatch {
            applied_seq: 2,
            raw_count: 1,
            ops: vec![(Uuid::new_v4(), None)],
        };
        fs::write(
            dir.join(chunk_name(nonce)),
            encode_chunk(&[7u8; 32], nonce, previous, &batch),
        )
        .expect("write linked chunk");
    }

    #[test]
    fn retired_chain_stops_at_a_chunk_linked_to_itself() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let only = Uuid::from_u128(1);
        write_chunk_linking(dir, only, only);
        fs::write(dir.join(HEAD_FILE), head_with_count(only, u64::MAX)).expect("write HEAD");

        assert_eq!(
            retired_chain(dir),
            vec![chunk_name(only)],
            "a self-linked chunk is collected once and the walk ends"
        );
    }

    #[test]
    fn retired_chain_stops_when_two_chunks_link_to_each_other() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let (first, second) = (Uuid::from_u128(1), Uuid::from_u128(2));
        write_chunk_linking(dir, first, second);
        write_chunk_linking(dir, second, first);
        fs::write(dir.join(HEAD_FILE), head_with_count(first, u64::MAX)).expect("write HEAD");

        assert_eq!(
            retired_chain(dir),
            vec![chunk_name(first), chunk_name(second)],
            "each chunk of a cycle is collected once"
        );
    }

    #[test]
    fn retired_chain_returns_every_chunk_newest_first() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut expected = retired_six_chunk_chain(dir);
        expected.reverse();

        assert_eq!(retired_chain(dir), expected);
    }

    #[test]
    fn retired_chain_stops_at_the_walk_bound() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut newest_first = retired_six_chunk_chain(dir);
        newest_first.reverse();

        assert_eq!(
            retired_chain_within(dir, 3),
            newest_first[..3].to_vec(),
            "only the newest chunks within the bound are collected"
        );
    }

    #[test]
    fn orphan_sweep_removes_stale_chunk_staging_files() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let staged = format!("{}.tmp", chunk_name(Uuid::new_v4()));
        fs::write(dir.join(&staged), b"interrupted staging").expect("stale staging file");
        fs::write(dir.join("unrelated.tmp"), b"keep").expect("unrelated file");

        clear_with_scan_budget(dir, 10).expect("sweep");

        assert!(
            !dir.join(&staged).exists(),
            "stale chunk staging file removed"
        );
        assert!(
            dir.join("unrelated.tmp").exists(),
            "the sweep removes only delta chunk names"
        );
    }

    #[test]
    fn full_save_succeeds_when_cleanup_fails_after_commit() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        let mut bridge = persisted_base(dir);
        let before = fs::read(dir.join("metadata.bin")).expect("base metadata");
        // A directory at the HEAD name cannot be unlinked as a file, so
        // cleanup fails only after the new base has committed.
        fs::create_dir(dir.join(HEAD_FILE)).expect("unremovable HEAD entry");
        assert!(clear(dir).is_err(), "control: cleanup itself must fail");
        bridge.set_applied_seq(2);

        let digest = bridge
            .save_atomic(dir)
            .expect("a committed full save reports success");

        assert_ne!(fs::read(dir.join("metadata.bin")).unwrap(), before);
        assert_eq!(segment_commit_digest(dir).unwrap(), Some(digest));
    }

    #[test]
    fn orphan_cleanup_bounds_directory_visits_after_removing_head() {
        let temp = tempfile::tempdir().expect("segment directory");
        let dir = temp.path();
        fs::write(dir.join(HEAD_FILE), b"old HEAD").expect("write old HEAD");
        for nonce in 1..=3 {
            fs::write(dir.join(chunk_name(Uuid::from_u128(nonce))), b"orphan")
                .expect("write orphan chunk");
        }

        clear_with_scan_budget(dir, 2).expect("bounded orphan cleanup");

        assert!(!dir.join(HEAD_FILE).exists(), "HEAD must be removed first");
        let remaining = fs::read_dir(dir)
            .expect("list remaining chunks")
            .filter(|entry| {
                entry
                    .as_ref()
                    .expect("directory entry")
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with("memory_delta-") && name.ends_with(".bin"))
            })
            .count();
        assert_eq!(remaining, 1, "only two directory entries may be visited");

        clear_with_scan_budget(dir, 2).expect("resume orphan cleanup");
        assert_eq!(
            fs::read_dir(dir).expect("list drained directory").count(),
            0
        );
    }

    #[test]
    fn orphan_cleanup_keeps_one_directory_after_path_replacement() {
        let temp = tempfile::tempdir().expect("parent directory");
        let dir = temp.path().join("checkpoint");
        let moved = temp.path().join("moved-checkpoint");
        fs::create_dir(&dir).expect("create checkpoint directory");
        let old_chunk = chunk_name(Uuid::from_u128(1));
        let replacement_chunk = chunk_name(Uuid::from_u128(2));
        fs::write(dir.join(HEAD_FILE), b"old HEAD").expect("write old HEAD");
        fs::write(dir.join(&old_chunk), b"old orphan").expect("write old chunk");

        clear_with_scan_budget_then(&dir, 10, || {
            fs::rename(&dir, &moved).expect("move opened directory");
            fs::create_dir(&dir).expect("replace checkpoint directory");
            fs::write(dir.join(HEAD_FILE), b"replacement HEAD").expect("write replacement HEAD");
            fs::write(dir.join(&replacement_chunk), b"replacement chunk")
                .expect("write replacement chunk");
        })
        .expect("cleanup opened directory");

        assert!(!moved.join(HEAD_FILE).exists());
        assert!(!moved.join(&old_chunk).exists());
        assert_eq!(fs::read(dir.join(HEAD_FILE)).unwrap(), b"replacement HEAD");
        assert_eq!(
            fs::read(dir.join(replacement_chunk)).unwrap(),
            b"replacement chunk"
        );
    }
}
