//! Vamana index: build, search, save/load, and snapshot serialization.

use std::collections::{HashMap, HashSet};
#[cfg(feature = "mmap")]
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use bytemuck::cast_slice;
#[cfg(feature = "mmap")]
use memmap2::MmapOptions;

#[cfg(test)]
use crate::graph::CodesView;
use crate::{
    config::VamanaConfig,
    error::{Result, VamanaError},
    graph::{is_tombstoned_bit, VamanaGraph, VisitedSet},
};

#[cfg(feature = "mmap")]
const METADATA_MAGIC: &[u8; 8] = b"KHVVAMM1";
const GRAPH_MAGIC: &[u8; 8] = b"KHVVAMG1";

// v2 commit-record magic written into metadata.bin by save_atomic.
const V2_COMMIT_MAGIC: &[u8; 8] = b"KHVVAMG2";

// lifecycle.bin magic for v2 persistence.
const LIFECYCLE_MAGIC: &[u8; 8] = b"KHVVLIF1";
const PORTABLE_MAGIC: &[u8; 8] = b"KHVVAMAC";
const PORTABLE_VERSION: u32 = 1;
const PORTABLE_IDS_MAGIC: &[u8; 8] = b"KHVEXTID";
const PORTABLE_IDS_VERSION: u32 = 1;

/// Default ops-since-consolidation threshold (ADR-052 §2, OQ5 resolution).
const DEFAULT_CONSOLIDATION_TAU: usize = 40_000;

mod search_visited;
use search_visited::SearchVisitedPool;

mod snapshot_types;
#[cfg(all(test, feature = "mmap", windows))]
use snapshot_types::publication_file_identity;
#[cfg(feature = "mmap")]
use snapshot_types::{
    encode_codes_bin, is_read_only_lock_create_error, parse_codes_bin, publication_file_snapshot,
};
use snapshot_types::{CodeStore, VectorStorage};
pub use snapshot_types::{
    CorpusFingerprint, PersistedFingerprint, VamanaIndexSnapshot, VamanaSnapshot,
    VAMANA_SNAPSHOT_FORMAT, VAMANA_SNAPSHOT_VERSION,
};
#[cfg(test)]
use snapshot_types::{VamanaIndexSnapshotRaw, VamanaSnapshotRaw};
#[cfg(all(test, feature = "mmap"))]
use snapshot_types::{CODES_HEADER_LEN, CODES_MAGIC};

mod index_struct;
#[cfg(feature = "parallel")]
use index_struct::build_pool;
pub use index_struct::VamanaIndex;
#[cfg(all(test, feature = "parallel"))]
use index_struct::{build_thread_count, resolve_build_threads};
use index_struct::{require_finite, train_codec_and_encode, IndexMetadata};

mod index_load_save;
mod index_mutation;

mod repair;
#[cfg(feature = "mmap")]
use repair::capped_reverse_adjacency;
use repair::{
    elect_medoid, exact_search, set_tombstone_bit, tombstone_words_for, validate_reverse_adjacency,
    wolverine_repair,
};

fn encode_portable_ids(index: &VamanaIndex, external_ids: &[(u32, String)]) -> Result<Vec<u8>> {
    if external_ids.len() != index.live_count() {
        return Err(VamanaError::invalid_format(format!(
            "portable ID count {} != live count {}",
            external_ids.len(),
            index.live_count()
        )));
    }
    let mut ids = external_ids.to_vec();
    ids.sort_unstable_by_key(|(ordinal, _)| *ordinal);
    let mut seen_ordinals = HashSet::with_capacity(ids.len());
    let mut seen_ids = HashSet::with_capacity(ids.len());
    for (ordinal, id) in &ids {
        if *ordinal as usize >= index.num_vectors
            || index.is_tombstoned(*ordinal)
            || id.is_empty()
            || !seen_ordinals.insert(*ordinal)
            || !seen_ids.insert(id.as_str())
        {
            return Err(VamanaError::invalid_format(format!(
                "invalid portable ID entry for ordinal {ordinal}"
            )));
        }
    }
    for ordinal in 0..index.num_vectors as u32 {
        if !index.is_tombstoned(ordinal) && !seen_ordinals.contains(&ordinal) {
            return Err(VamanaError::invalid_format(format!(
                "missing portable ID for live ordinal {ordinal}"
            )));
        }
    }

    let mut buf = Vec::new();
    buf.extend_from_slice(PORTABLE_IDS_MAGIC);
    buf.extend_from_slice(&PORTABLE_IDS_VERSION.to_le_bytes());
    buf.extend_from_slice(&(ids.len() as u64).to_le_bytes());
    for (ordinal, id) in ids {
        let len = u32::try_from(id.len())
            .map_err(|_| VamanaError::invalid_format("portable ID length overflows u32".into()))?;
        buf.extend_from_slice(&ordinal.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(id.as_bytes());
    }
    Ok(buf)
}

fn parse_portable_ids(data: &[u8], index: &VamanaIndex) -> Result<Vec<(u32, String)>> {
    let mut offset = 0;
    if take_bytes(data, &mut offset, 8, "portable ID magic")? != PORTABLE_IDS_MAGIC {
        return Err(VamanaError::invalid_format(
            "portable_ids.bin magic mismatch".into(),
        ));
    }
    let version = read_u32(data, &mut offset, "portable ID version")?;
    if version != PORTABLE_IDS_VERSION {
        return Err(VamanaError::invalid_format(format!(
            "unsupported portable ID version {version}"
        )));
    }
    let count = usize::try_from(read_u64(data, &mut offset, "portable ID count")?)
        .map_err(|_| VamanaError::invalid_format("portable ID count overflows usize".into()))?;
    if count != index.live_count() {
        return Err(VamanaError::invalid_format(format!(
            "portable ID count {count} != live count {}",
            index.live_count()
        )));
    }

    let mut entries = Vec::with_capacity(count);
    let mut seen_ordinals = HashSet::with_capacity(count);
    let mut seen_ids = HashSet::with_capacity(count);
    let mut previous = None;
    for _ in 0..count {
        let ordinal = read_u32(data, &mut offset, "portable ID ordinal")?;
        let len = read_u32(data, &mut offset, "portable ID length")? as usize;
        let raw = take_bytes(data, &mut offset, len, "portable ID bytes")?;
        let id = std::str::from_utf8(raw)
            .map_err(|_| VamanaError::invalid_format("portable ID is not UTF-8".into()))?
            .to_owned();
        if previous.is_some_and(|previous| ordinal <= previous)
            || ordinal as usize >= index.num_vectors
            || index.is_tombstoned(ordinal)
            || id.is_empty()
            || !seen_ordinals.insert(ordinal)
            || !seen_ids.insert(id.clone())
        {
            return Err(VamanaError::invalid_format(format!(
                "invalid portable ID entry for ordinal {ordinal}"
            )));
        }
        previous = Some(ordinal);
        entries.push((ordinal, id));
    }
    if offset != data.len() {
        return Err(VamanaError::invalid_format(format!(
            "portable_ids.bin has {} trailing bytes",
            data.len() - offset
        )));
    }
    for ordinal in 0..index.num_vectors as u32 {
        if !index.is_tombstoned(ordinal) && !seen_ordinals.contains(&ordinal) {
            return Err(VamanaError::invalid_format(format!(
                "missing portable ID for live ordinal {ordinal}"
            )));
        }
    }
    Ok(entries)
}

fn encode_portable_container(segments: &[(&str, Vec<u8>)]) -> Result<Vec<u8>> {
    let segment_count = u32::try_from(segments.len())
        .map_err(|_| VamanaError::invalid_format("portable segment count overflows u32".into()))?;
    let table_len = segments.iter().try_fold(16usize, |total, (name, _)| {
        total
            .checked_add(4 + name.len() + 8 + 8 + 32)
            .ok_or_else(|| VamanaError::invalid_format("portable table length overflow".into()))
    })?;
    let payload_len = segments.iter().try_fold(0usize, |total, (_, payload)| {
        total
            .checked_add(payload.len())
            .ok_or_else(|| VamanaError::invalid_format("portable payload length overflow".into()))
    })?;
    let mut buf = Vec::with_capacity(
        table_len
            .checked_add(payload_len)
            .ok_or_else(|| VamanaError::invalid_format("portable container overflow".into()))?,
    );
    buf.extend_from_slice(PORTABLE_MAGIC);
    buf.extend_from_slice(&PORTABLE_VERSION.to_le_bytes());
    buf.extend_from_slice(&segment_count.to_le_bytes());
    let mut payload_offset = table_len;
    for (name, payload) in segments {
        let name_len = u32::try_from(name.len())
            .map_err(|_| VamanaError::invalid_format("portable segment name too long".into()))?;
        buf.extend_from_slice(&name_len.to_le_bytes());
        buf.extend_from_slice(name.as_bytes());
        buf.extend_from_slice(&(payload_offset as u64).to_le_bytes());
        buf.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        buf.extend_from_slice(blake3::hash(payload).as_bytes());
        payload_offset += payload.len();
    }
    for (_, payload) in segments {
        buf.extend_from_slice(payload);
    }
    Ok(buf)
}

struct PortableSegment {
    offset: usize,
    len: usize,
    checksum: [u8; 32],
}

fn parse_portable_container(data: &[u8]) -> Result<HashMap<String, &[u8]>> {
    let mut offset = 0;
    if take_bytes(data, &mut offset, 8, "portable magic")? != PORTABLE_MAGIC {
        return Err(VamanaError::invalid_format(
            "portable container magic mismatch".into(),
        ));
    }
    let version = read_u32(data, &mut offset, "portable version")?;
    if version != PORTABLE_VERSION {
        return Err(VamanaError::invalid_format(format!(
            "unsupported portable container version {version}"
        )));
    }
    let segment_count = read_u32(data, &mut offset, "portable segment count")? as usize;
    if !(4..=5).contains(&segment_count) {
        return Err(VamanaError::invalid_format(format!(
            "portable segment count {segment_count} is invalid"
        )));
    }

    let allowed = [
        "metadata.bin",
        "vectors.bin",
        "graph.bin",
        "lifecycle.bin",
        "portable_ids.bin",
    ];
    let mut table = HashMap::with_capacity(segment_count);
    for _ in 0..segment_count {
        let name_len = read_u32(data, &mut offset, "portable segment name length")? as usize;
        let name = std::str::from_utf8(take_bytes(
            data,
            &mut offset,
            name_len,
            "portable segment name",
        )?)
        .map_err(|_| VamanaError::invalid_format("portable segment name is not UTF-8".into()))?
        .to_owned();
        if !allowed.contains(&name.as_str()) {
            return Err(VamanaError::invalid_format(format!(
                "unknown portable segment {name}"
            )));
        }
        let payload_offset = usize::try_from(read_u64(data, &mut offset, "payload offset")?)
            .map_err(|_| VamanaError::invalid_format("payload offset overflows usize".into()))?;
        let payload_len = usize::try_from(read_u64(data, &mut offset, "payload length")?)
            .map_err(|_| VamanaError::invalid_format("payload length overflows usize".into()))?;
        let mut checksum = [0; 32];
        checksum.copy_from_slice(take_bytes(data, &mut offset, 32, "payload checksum")?);
        if table
            .insert(
                name.clone(),
                PortableSegment {
                    offset: payload_offset,
                    len: payload_len,
                    checksum,
                },
            )
            .is_some()
        {
            return Err(VamanaError::invalid_format(format!(
                "duplicate portable segment {name}"
            )));
        }
    }

    let table_end = offset;
    let mut ranges = Vec::with_capacity(table.len());
    for (name, segment) in &table {
        let end = segment.offset.checked_add(segment.len).ok_or_else(|| {
            VamanaError::invalid_format(format!("portable segment {name} range overflows"))
        })?;
        if segment.offset < table_end || end > data.len() {
            return Err(VamanaError::invalid_format(format!(
                "portable segment {name} range is out of bounds"
            )));
        }
        ranges.push((segment.offset, end, name));
    }
    ranges.sort_unstable_by_key(|(start, _, _)| *start);
    for pair in ranges.windows(2) {
        if pair[0].1 > pair[1].0 {
            return Err(VamanaError::invalid_format(format!(
                "portable segments {} and {} overlap",
                pair[0].2, pair[1].2
            )));
        }
    }

    let mut payloads = HashMap::with_capacity(table.len());
    for (name, segment) in table {
        let payload = &data[segment.offset..segment.offset + segment.len];
        if blake3::hash(payload).as_bytes() != &segment.checksum {
            return Err(VamanaError::invalid_format(format!(
                "portable segment {name} checksum mismatch"
            )));
        }
        payloads.insert(name, payload);
    }
    Ok(payloads)
}

fn required_segment<'a>(segments: &'a HashMap<String, &'a [u8]>, name: &str) -> Result<&'a [u8]> {
    segments
        .get(name)
        .copied()
        .ok_or_else(|| VamanaError::invalid_format(format!("missing portable segment {name}")))
}

fn take_bytes<'a>(data: &'a [u8], offset: &mut usize, len: usize, field: &str) -> Result<&'a [u8]> {
    let end = offset
        .checked_add(len)
        .ok_or_else(|| VamanaError::invalid_format(format!("{field} offset overflows")))?;
    if end > data.len() {
        return Err(VamanaError::invalid_format(format!(
            "portable container truncated at {field}"
        )));
    }
    let bytes = &data[*offset..end];
    *offset = end;
    Ok(bytes)
}

fn read_u32(data: &[u8], offset: &mut usize, field: &str) -> Result<u32> {
    Ok(u32::from_le_bytes(
        take_bytes(data, offset, 4, field)?
            .try_into()
            .expect("four-byte field"),
    ))
}

fn read_u64(data: &[u8], offset: &mut usize, field: &str) -> Result<u64> {
    Ok(u64::from_le_bytes(
        take_bytes(data, offset, 8, field)?
            .try_into()
            .expect("eight-byte field"),
    ))
}

// ---- V2 persistence helpers ----

/// Corpus identity check used by `save_atomic` / `load_or_build`.
/// Separate from `CorpusFingerprint` (which is part of the snapshot API).
struct V2CorpusFingerprint {
    vector_count: u64,
    dimensions: u64,
    content_hash: [u8; 32],
}

/// Parsed content of a KHVVAMG2 commit record (metadata.bin written by save_atomic).
struct V2Commit {
    vectors_hash: [u8; 32],
    graph_hash: [u8; 32],
    lifecycle_hash: [u8; 32],
    fingerprint: V2CorpusFingerprint,
    index_meta: IndexMetadata,
    /// Write-log watermark trailer. `None` when the record predates the field
    /// (short layout) — the record length, not a sentinel value, discriminates,
    /// so a legitimate watermark of 0 (empty log at save time) round-trips.
    last_applied_seq: Option<u64>,
    /// blake3 checksum of the `codes.bin` segment; `None` on pre-trailer
    /// records and on containers that omit the codes segment. Read only by
    /// the mmap load path; parsed unconditionally so record validation stays
    /// identical across feature sets.
    #[cfg_attr(not(feature = "mmap"), allow(dead_code))]
    codes_hash: Option<[u8; 32]>,
}

/// Parsed lifecycle.bin content.
struct ParsedLifecycle {
    tombstones: Vec<u64>,
    free_slots: Vec<u32>,
    reverse_adj: Vec<Vec<u32>>,
    ops_since_consolidation: usize,
}

fn validate_free_slots(free_slots: &[u32], tombstones: &[u64], num_vectors: usize) -> Result<()> {
    let mut seen = HashSet::with_capacity(free_slots.len());
    for &slot in free_slots {
        if slot as usize >= num_vectors
            || !is_tombstoned_bit(tombstones, slot as usize)
            || !seen.insert(slot)
        {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin invalid free slot {slot}"
            )));
        }
    }
    Ok(())
}

/// Rejects a rebuild candidate only against a structurally valid incumbent (see
/// [`validate_v2_structural`]); a corrupt incumbent is treated as no incumbent, or a
/// repair checkpoint could never publish below its sequence. See
/// crates/khive-vamana/docs/index.md#checkpoint-sequence-regression-guard.
#[cfg(feature = "mmap")]
fn reject_checkpoint_sequence_regression(path: &Path, candidate: Option<u64>) -> Result<()> {
    let metadata = match fs::read(path.join("metadata.bin")) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if metadata.len() < V2_COMMIT_MAGIC.len()
        || &metadata[..V2_COMMIT_MAGIC.len()] != V2_COMMIT_MAGIC
    {
        return Ok(());
    }
    let Ok(commit) = parse_v2_commit(&metadata) else {
        return Ok(());
    };
    let (vectors_hash, vectors_len) = match hash_vectors_file(&path.join("vectors.bin")) {
        Ok(result) => result,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if vectors_hash != commit.vectors_hash {
        return Ok(());
    }
    let segments = [
        ("graph.bin", commit.graph_hash),
        ("lifecycle.bin", commit.lifecycle_hash),
    ];
    let mut lifecycle_data: Option<MappedCheckpointSegment> = None;
    for (name, expected) in segments {
        let data = match map_checkpoint_segment(&path.join(name)) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if blake3::hash(&data).as_bytes() != &expected {
            return Ok(());
        }
        if name == "lifecycle.bin" {
            lifecycle_data = Some(data);
        }
    }
    let mut codes_data: Option<MappedCheckpointSegment> = None;
    if let Some(expected) = commit.codes_hash {
        let data = match map_checkpoint_segment(&path.join("codes.bin")) {
            Ok(data) => data,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error.into()),
        };
        if blake3::hash(&data).as_bytes() != &expected {
            return Ok(());
        }
        codes_data = Some(data);
    }
    let Some(incumbent) = commit.last_applied_seq else {
        return Ok(());
    };

    let lifecycle_data = lifecycle_data.expect("lifecycle.bin hashed above");
    let max_degree = commit.index_meta.max_degree;
    let num_vectors = commit.index_meta.num_vectors;
    let dimensions = commit.index_meta.dimensions;
    // Replicates every check `load_v2_fast` applies to a v2 checkpoint, so guard-valid
    // implies loader-loadable. See crates/khive-vamana/docs/index.md#checkpoint-sequence-regression-guard.
    let structurally_valid = (|| -> Result<()> {
        let config = VamanaConfig {
            dimensions,
            max_degree,
            search_list_size: commit.index_meta.search_list_size,
            alpha: commit.index_meta.alpha,
        };
        config.validate()?;

        let graph = read_graph(&path.join("graph.bin"), max_degree, num_vectors)?;

        let expected_len_f32 = num_vectors
            .checked_mul(dimensions)
            .ok_or_else(|| VamanaError::invalid_format("v2 metadata overflow".into()))?;
        let expected_vectors_bytes = expected_len_f32
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| {
                VamanaError::invalid_format("vectors.bin byte length overflow".into())
            })?;
        if vectors_len != expected_vectors_bytes {
            return Err(VamanaError::invalid_format(format!(
                "vectors.bin byte length {vectors_len} != expected {expected_vectors_bytes}"
            )));
        }

        let lifecycle = parse_lifecycle(&lifecycle_data, num_vectors, max_degree)?;
        validate_v2_structural(&graph, &lifecycle, num_vectors)?;
        // codes.bin gets the same structural check as load_v2_fast applies.
        if let Some(codes_data) = &codes_data {
            parse_codes_bin(codes_data, dimensions, num_vectors)?;
        }
        Ok(())
    })();
    match structurally_valid {
        Ok(()) => {}
        Err(VamanaError::InvalidFormat { .. }) => return Ok(()),
        Err(error) => return Err(error),
    }

    if candidate.is_none_or(|sequence| sequence < incumbent) {
        return Err(VamanaError::CheckpointSequenceRegression {
            candidate,
            incumbent,
        });
    }
    Ok(())
}

/// Structural validity of a v2 commit's graph + lifecycle state beyond the blake3 checksum
/// the caller already verified. Shared by `load_v2_fast` and
/// `reject_checkpoint_sequence_regression` so both agree on what counts as a valid
/// incumbent. See crates/khive-vamana/docs/index.md#checkpoint-sequence-regression-guard.
#[cfg(feature = "mmap")]
fn validate_v2_structural(
    graph: &VamanaGraph,
    lifecycle: &ParsedLifecycle,
    num_vectors: usize,
) -> Result<usize> {
    if graph.node_count() != num_vectors {
        return Err(VamanaError::invalid_format(format!(
            "graph node count {} != commit num_vectors {}",
            graph.node_count(),
            num_vectors
        )));
    }

    // INVARIANT (graph.rs:173-182): reverse_adj[v] == { u | v ∈ adjacency[u] }. A
    // checksum-valid but writer-bugged lifecycle segment can satisfy parse_lifecycle's
    // per-list shape checks while still violating this — a false in-neighbor corrupts
    // Wolverine delete-repair.
    let adjacency = graph.adjacency();
    let mut incoming_counts = vec![0_usize; num_vectors];
    for neighbors in adjacency {
        for &v in neighbors {
            incoming_counts[v as usize] += 1;
        }
    }

    // Non-medoid forward lists have the configured out-degree bound. The
    // medoid may exceed it, so use a single sorted forward-list copy there:
    // checking a legal star must not scan its large list for every destination.
    let medoid = graph.medoid() as usize;
    let mut medoid_neighbors = adjacency[medoid].clone();
    medoid_neighbors.sort_unstable();
    for (v, got) in lifecycle.reverse_adj.iter().enumerate() {
        // parse_lifecycle already rejects duplicate parents. Equal cardinality
        // and inclusion in the true incoming set therefore prove exact equality.
        let cardinality_matches = incoming_counts[v] == got.len();
        let parents_match = cardinality_matches
            && got.iter().all(|&parent| {
                if parent as usize == medoid {
                    medoid_neighbors
                        .binary_search_by(|neighbor| {
                            #[cfg(all(test, feature = "mmap"))]
                            checkpoint_allocation_tests::record_medoid_comparison();
                            neighbor.cmp(&(v as u32))
                        })
                        .is_ok()
                } else {
                    adjacency[parent as usize].contains(&(v as u32))
                }
            });
        if !parents_match {
            // Preserve the existing diagnostic without constructing a second
            // inverse on successful loads. Sources are already in sorted order.
            let exp: Vec<u32> = adjacency
                .iter()
                .enumerate()
                .filter(|(_, neighbors)| neighbors.contains(&(v as u32)))
                .map(|(source, _)| source as u32)
                .collect();
            let mut got_sorted = got.clone();
            got_sorted.sort_unstable();
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin reverse_adj[{v}] is not the inverse of graph.bin \
                 forward adjacency: expected {exp:?}, got {got_sorted:?}"
            )));
        }
    }

    let tombstone_count: usize = lifecycle
        .tombstones
        .iter()
        .map(|w| w.count_ones() as usize)
        .sum();
    if tombstone_count > num_vectors {
        return Err(VamanaError::invalid_format(format!(
            "lifecycle.bin tombstone_count {tombstone_count} exceeds num_vectors {num_vectors}"
        )));
    }

    validate_free_slots(&lifecycle.free_slots, &lifecycle.tombstones, num_vectors)?;

    Ok(tombstone_count)
}

/// Write the KHVVAMG2 commit record including embedded v1 metadata fields.
#[allow(clippy::too_many_arguments)]
#[cfg(all(feature = "mmap", test))]
fn write_v2_commit_full(
    path: &Path,
    vectors_hash: &[u8; 32],
    graph_hash: &[u8; 32],
    lifecycle_hash: &[u8; 32],
    fp: &V2CorpusFingerprint,
    num_vectors: usize,
    dimensions: usize,
    max_degree: usize,
    search_list_size: usize,
    alpha: f64,
    last_applied_seq: Option<u64>,
    codes_hash: Option<&[u8; 32]>,
    publication_nonce: Option<&[u8; 16]>,
) -> Result<()> {
    let buf = encode_v2_commit_full(
        vectors_hash,
        graph_hash,
        lifecycle_hash,
        fp,
        num_vectors,
        dimensions,
        max_degree,
        search_list_size,
        alpha,
        last_applied_seq,
        codes_hash,
        publication_nonce,
    );
    let file = File::create(path)?;
    let mut w = std::io::BufWriter::new(file);
    w.write_all(&buf)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn encode_v2_commit_full(
    vectors_hash: &[u8; 32],
    graph_hash: &[u8; 32],
    lifecycle_hash: &[u8; 32],
    fp: &V2CorpusFingerprint,
    num_vectors: usize,
    dimensions: usize,
    max_degree: usize,
    search_list_size: usize,
    alpha: f64,
    last_applied_seq: Option<u64>,
    codes_hash: Option<&[u8; 32]>,
    publication_nonce: Option<&[u8; 16]>,
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(236);
    buf.extend_from_slice(V2_COMMIT_MAGIC);
    buf.extend_from_slice(vectors_hash);
    buf.extend_from_slice(graph_hash);
    buf.extend_from_slice(lifecycle_hash);
    buf.extend_from_slice(&fp.vector_count.to_le_bytes());
    buf.extend_from_slice(&fp.dimensions.to_le_bytes());
    buf.extend_from_slice(&fp.content_hash);
    buf.extend_from_slice(&(num_vectors as u64).to_le_bytes());
    buf.extend_from_slice(&(dimensions as u64).to_le_bytes());
    buf.extend_from_slice(&(max_degree as u64).to_le_bytes());
    buf.extend_from_slice(&(search_list_size as u64).to_le_bytes());
    buf.extend_from_slice(&alpha.to_le_bytes());
    // Fixed-size trailer: flags byte (bit0 = watermark present, bit1 =
    // codes hash present) + watermark + codes hash. Written unconditionally so
    // all new records share one length; the short pre-trailer layout parses as
    // a pre-amendment record.
    let mut flags = 0u8;
    if last_applied_seq.is_some() {
        flags |= 1;
    }
    if codes_hash.is_some() {
        flags |= 2;
    }
    buf.push(flags);
    buf.extend_from_slice(&last_applied_seq.unwrap_or(0).to_le_bytes());
    buf.extend_from_slice(codes_hash.unwrap_or(&[0u8; 32]));
    // New file-backed checkpoints append a per-publication nonce. The length,
    // rather than a flag bit, discriminates this backward-compatible layout:
    // old base and 41-byte-trailer records remain valid, while portable
    // containers can keep deterministic metadata by passing `None`.
    if let Some(nonce) = publication_nonce {
        buf.extend_from_slice(nonce);
    }
    buf
}

/// Parse a KHVVAMG2 commit record from bytes.
fn parse_v2_commit(data: &[u8]) -> Result<V2Commit> {
    // magic(8) + 3 hashes(96) + fp.vector_count(8) + fp.dimensions(8) + fp.content_hash(32)
    // + num_vectors(8) + dimensions(8) + max_degree(8) + search_list_size(8) + alpha(8)
    // + optional trailer: flags(1) + last_applied_seq(8) + codes_hash(32)
    // + optional per-publication nonce(16)
    let base_len = 8 + 32 + 32 + 32 + 8 + 8 + 32 + 8 + 8 + 8 + 8 + 8;
    let trailer_len = base_len + 41;
    let publication_trailer_len = trailer_len + 16;
    if data.len() != base_len && data.len() != trailer_len && data.len() != publication_trailer_len
    {
        return Err(VamanaError::invalid_format(format!(
            "v2 commit record length {} != {base_len}, {trailer_len}, or \
             {publication_trailer_len}",
            data.len()
        )));
    }
    if &data[..8] != V2_COMMIT_MAGIC {
        return Err(VamanaError::invalid_format(
            "v2 commit record magic mismatch".into(),
        ));
    }

    let mut offset = 8usize;

    let mut vectors_hash = [0u8; 32];
    vectors_hash.copy_from_slice(&data[offset..offset + 32]);
    offset += 32;

    let mut graph_hash = [0u8; 32];
    graph_hash.copy_from_slice(&data[offset..offset + 32]);
    offset += 32;

    let mut lifecycle_hash = [0u8; 32];
    lifecycle_hash.copy_from_slice(&data[offset..offset + 32]);
    offset += 32;

    let vector_count = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;
    let fp_dimensions = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    let mut content_hash = [0u8; 32];
    content_hash.copy_from_slice(&data[offset..offset + 32]);
    offset += 32;

    let num_vectors = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("v2 commit num_vectors overflow".into()))?;
    offset += 8;
    let dimensions = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("v2 commit dimensions overflow".into()))?;
    offset += 8;
    let max_degree = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("v2 commit max_degree overflow".into()))?;
    offset += 8;
    let search_list_size = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("v2 commit search_list_size overflow".into()))?;
    offset += 8;
    let alpha = f64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
    offset += 8;

    let (last_applied_seq, codes_hash) = if data.len() >= trailer_len {
        let flags = data[offset];
        offset += 1;
        let seq = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        offset += 8;
        let mut hash = [0u8; 32];
        hash.copy_from_slice(&data[offset..offset + 32]);
        (
            (flags & 1 != 0).then_some(seq),
            (flags & 2 != 0).then_some(hash),
        )
    } else {
        (None, None)
    };

    if num_vectors == 0 {
        return Err(VamanaError::invalid_format(
            "v2 commit num_vectors is 0".into(),
        ));
    }
    if dimensions == 0 {
        return Err(VamanaError::invalid_format(
            "v2 commit dimensions is 0".into(),
        ));
    }

    VamanaConfig {
        dimensions,
        max_degree,
        search_list_size,
        alpha,
    }
    .validate()
    .map_err(|err| match err {
        VamanaError::InvalidConfig { reason } => {
            VamanaError::invalid_format(format!("v2 commit invalid config: {reason}"))
        }
        other => other,
    })?;

    Ok(V2Commit {
        vectors_hash,
        graph_hash,
        lifecycle_hash,
        fingerprint: V2CorpusFingerprint {
            vector_count,
            dimensions: fp_dimensions,
            content_hash,
        },
        index_meta: IndexMetadata {
            num_vectors,
            dimensions,
            max_degree,
            search_list_size,
            alpha,
        },
        last_applied_seq,
        codes_hash,
    })
}

fn encode_lifecycle(
    tombstones: &[u64],
    free_slots: &[u32],
    reverse_adj: &[Vec<u32>],
    ops_since_consolidation: usize,
) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(LIFECYCLE_MAGIC);
    buf.extend_from_slice(&(tombstones.len() as u64).to_le_bytes());
    for &word in tombstones {
        buf.extend_from_slice(&word.to_le_bytes());
    }
    buf.extend_from_slice(&(free_slots.len() as u64).to_le_bytes());
    for &slot in free_slots {
        buf.extend_from_slice(&slot.to_le_bytes());
    }
    buf.extend_from_slice(&(ops_since_consolidation as u64).to_le_bytes());
    buf.extend_from_slice(&(reverse_adj.len() as u64).to_le_bytes());
    for neighbors in reverse_adj {
        buf.extend_from_slice(&(neighbors.len() as u32).to_le_bytes());
        for &neighbor in neighbors {
            buf.extend_from_slice(&neighbor.to_le_bytes());
        }
    }
    buf
}

/// Parse lifecycle.bin bytes into `ParsedLifecycle`.
fn parse_lifecycle(data: &[u8], num_vectors: usize, _max_degree: usize) -> Result<ParsedLifecycle> {
    if data.len() < 8 {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin too short".into(),
        ));
    }
    if &data[..8] != LIFECYCLE_MAGIC {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin magic mismatch".into(),
        ));
    }

    let mut offset = 8usize;

    // Tombstones.
    if offset + 8 > data.len() {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin truncated at tombstone_words".into(),
        ));
    }
    let ts_words = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("lifecycle.bin ts_words overflows usize".into()))?;
    offset += 8;
    let ts_bytes = ts_words
        .checked_mul(8)
        .ok_or_else(|| VamanaError::invalid_format("lifecycle.bin ts_words overflows".into()))?;
    let ts_end = offset
        .checked_add(ts_bytes)
        .ok_or_else(|| VamanaError::invalid_format("lifecycle.bin ts_end overflows".into()))?;
    if ts_end > data.len() {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin truncated at tombstone data".into(),
        ));
    }
    let mut tombstones = Vec::with_capacity(ts_words);
    for _ in 0..ts_words {
        let word = u64::from_le_bytes(data[offset..offset + 8].try_into().unwrap());
        tombstones.push(word);
        offset += 8;
    }
    // Ensure tombstone bitvec covers exactly the valid node-id domain.
    let needed_words = num_vectors.div_ceil(64);
    if tombstones.len() < needed_words {
        tombstones.resize(needed_words, 0);
    } else if tombstones.len() > needed_words {
        if tombstones[needed_words..].iter().any(|&word| word != 0) {
            return Err(VamanaError::invalid_format(
                "lifecycle.bin tombstone words exceed num_vectors".into(),
            ));
        }
        tombstones.truncate(needed_words);
    }

    let valid_bits_in_last_word = num_vectors % 64;
    if valid_bits_in_last_word != 0 {
        let valid_mask = (1u64 << valid_bits_in_last_word) - 1;
        if tombstones
            .last()
            .is_some_and(|last_word| last_word & !valid_mask != 0)
        {
            return Err(VamanaError::invalid_format(
                "lifecycle.bin tombstone bits outside num_vectors".into(),
            ));
        }
    }

    // Free slots.
    if offset + 8 > data.len() {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin truncated at free_slots_count".into(),
        ));
    }
    let fs_count = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("lifecycle.bin fs_count overflows usize".into()))?;
    offset += 8;
    let fs_bytes = fs_count
        .checked_mul(4)
        .ok_or_else(|| VamanaError::invalid_format("lifecycle.bin fs_count overflows".into()))?;
    let fs_end = offset
        .checked_add(fs_bytes)
        .ok_or_else(|| VamanaError::invalid_format("lifecycle.bin fs_end overflows".into()))?;
    if fs_end > data.len() {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin truncated at free_slots data".into(),
        ));
    }
    let mut free_slots = Vec::with_capacity(fs_count);
    for _ in 0..fs_count {
        let slot = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
        free_slots.push(slot);
        offset += 4;
    }

    // ops_since_consolidation.
    if offset + 8 > data.len() {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin truncated at ops".into(),
        ));
    }
    let ops_since_consolidation = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| VamanaError::invalid_format("lifecycle.bin ops overflows usize".into()))?;
    offset += 8;

    // Reverse adjacency.
    if offset + 8 > data.len() {
        return Err(VamanaError::invalid_format(
            "lifecycle.bin truncated at rev_num_nodes".into(),
        ));
    }
    let rev_num_nodes = usize::try_from(u64::from_le_bytes(
        data[offset..offset + 8].try_into().unwrap(),
    ))
    .map_err(|_| {
        VamanaError::invalid_format("lifecycle.bin rev_num_nodes overflows usize".into())
    })?;
    offset += 8;

    if rev_num_nodes != num_vectors {
        return Err(VamanaError::invalid_format(format!(
            "lifecycle.bin rev_num_nodes {rev_num_nodes} != num_vectors {num_vectors}"
        )));
    }

    // Every declared node contributes at least its 4-byte degree field. Reject a
    // rev_num_nodes that cannot possibly fit the remaining bytes before requesting an
    // allocation sized to it — same pattern as parse_graph's num_nodes preflight, needed
    // here because a re-signed lifecycle.bin can pass the rev_num_nodes == num_vectors
    // check above while the file itself is truncated well short of that many nodes.
    let min_remaining_bytes = rev_num_nodes.checked_mul(4).ok_or_else(|| {
        VamanaError::invalid_format(
            "lifecycle.bin rev_num_nodes overflows minimum size check".into(),
        )
    })?;
    if data.len() - offset < min_remaining_bytes {
        return Err(VamanaError::invalid_format(format!(
            "lifecycle.bin too short for {rev_num_nodes} declared reverse-adjacency nodes: {} bytes remaining, need at least {min_remaining_bytes}",
            data.len() - offset
        )));
    }

    let mut reverse_adj: Vec<Vec<u32>> = Vec::with_capacity(rev_num_nodes);
    for node in 0..rev_num_nodes {
        if offset + 4 > data.len() {
            return Err(VamanaError::invalid_format(
                "lifecycle.bin truncated at rev degree".into(),
            ));
        }
        let degree = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        // Reverse-adjacency degree is bounded by num_vectors-1 (every other node pointing
        // here), not by max_degree (which caps forward out-degree only). A hub node in a
        // legitimate graph can have up to num_vectors-1 inbound edges.
        if degree > num_vectors.saturating_sub(1) {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin node {node} rev degree {degree} > num_vectors-1"
            )));
        }
        let neighbors_end = offset.checked_add(degree * 4).ok_or_else(|| {
            VamanaError::invalid_format("lifecycle.bin neighbors_end overflows".into())
        })?;
        if neighbors_end > data.len() {
            return Err(VamanaError::invalid_format(
                "lifecycle.bin truncated at rev neighbors".into(),
            ));
        }
        let mut neighbors = Vec::with_capacity(degree);
        let mut seen = std::collections::HashSet::with_capacity(degree);
        for _ in 0..degree {
            let nb = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;
            if nb as usize >= num_vectors {
                return Err(VamanaError::invalid_format(format!(
                    "lifecycle.bin rev neighbor {nb} >= num_vectors {num_vectors}"
                )));
            }
            if nb as usize == node {
                return Err(VamanaError::invalid_format(format!(
                    "lifecycle.bin node {node} has self-reference in reverse adjacency"
                )));
            }
            if !seen.insert(nb) {
                return Err(VamanaError::invalid_format(format!(
                    "lifecycle.bin node {node} has duplicate rev neighbor {nb}"
                )));
            }
            neighbors.push(nb);
        }
        reverse_adj.push(neighbors);
    }

    if offset != data.len() {
        return Err(VamanaError::invalid_format(format!(
            "lifecycle.bin has {} trailing bytes",
            data.len() - offset
        )));
    }

    Ok(ParsedLifecycle {
        tombstones,
        free_slots,
        reverse_adj,
        ops_since_consolidation,
    })
}

#[cfg(feature = "mmap")]
fn encode_metadata(index: &VamanaIndex) -> Vec<u8> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(METADATA_MAGIC);
    buf.extend_from_slice(&(index.num_vectors as u64).to_le_bytes());
    buf.extend_from_slice(&(index.dimensions as u64).to_le_bytes());
    buf.extend_from_slice(&(index.config.max_degree as u64).to_le_bytes());
    buf.extend_from_slice(&(index.config.search_list_size as u64).to_le_bytes());
    buf.extend_from_slice(&index.config.alpha.to_le_bytes());
    buf
}

#[cfg(feature = "mmap")]
fn stage_legacy_replacement(destination: &Path, bytes: &[u8]) -> Result<PathBuf> {
    let existing_permissions = match fs::metadata(destination) {
        Ok(metadata) => Some(metadata.permissions()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let filename = destination
        .file_name()
        .expect("legacy segment has a fixed filename")
        .to_string_lossy();
    let temporary =
        destination.with_file_name(format!(".{filename}.legacy-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        if existing_permissions.is_some() {
            options.mode(0o600);
        }
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> std::io::Result<()> {
        if let Some(permissions) = existing_permissions {
            file.set_permissions(permissions)?;
        }
        file.write_all(bytes)?;
        file.sync_all()
    })();
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result?;
    Ok(temporary)
}

#[cfg(feature = "mmap")]
fn read_metadata(path: &Path) -> Result<IndexMetadata> {
    let data = fs::read(path)?;
    if data.len() < 8 {
        return Err(VamanaError::invalid_format("metadata.bin too short".into()));
    }
    if &data[..8] != METADATA_MAGIC {
        return Err(VamanaError::invalid_format(
            "metadata.bin magic mismatch".into(),
        ));
    }
    let expected_len = 8 + 5 * 8; // magic + 4 u64 + 1 f64
    if data.len() < expected_len {
        return Err(VamanaError::invalid_format("metadata.bin truncated".into()));
    }
    let num_vectors = usize::try_from(u64::from_le_bytes(data[8..16].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("num_vectors overflows usize".into()))?;
    let dimensions = usize::try_from(u64::from_le_bytes(data[16..24].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("dimensions overflows usize".into()))?;
    let max_degree = usize::try_from(u64::from_le_bytes(data[24..32].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("max_degree overflows usize".into()))?;
    let search_list_size = usize::try_from(u64::from_le_bytes(data[32..40].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("search_list_size overflows usize".into()))?;
    let alpha = f64::from_le_bytes(data[40..48].try_into().unwrap());

    if num_vectors == 0 {
        return Err(VamanaError::invalid_format("num_vectors is 0".into()));
    }
    if dimensions == 0 {
        return Err(VamanaError::invalid_format("dimensions is 0".into()));
    }

    Ok(IndexMetadata {
        num_vectors,
        dimensions,
        max_degree,
        search_list_size,
        alpha,
    })
}

#[cfg(feature = "mmap")]
fn encode_graph(graph: &VamanaGraph, max_degree: usize) -> Result<Vec<u8>> {
    encode_graph_inner(graph, Some(max_degree))
}

fn encode_graph_lossless(graph: &VamanaGraph) -> Result<Vec<u8>> {
    encode_graph_inner(graph, None)
}

fn encode_graph_inner(graph: &VamanaGraph, medoid_degree_limit: Option<usize>) -> Result<Vec<u8>> {
    let num_nodes = u32::try_from(graph.node_count()).map_err(|_| VamanaError::TooManyVectors {
        count: graph.node_count(),
    })?;
    let medoid = graph.medoid();

    // Cap the medoid's adjacency list at max_degree before serialization.
    // The medoid-pin in insert() may transiently allow the medoid to exceed
    // max_degree (by one edge per orphan-pinned insert; K consecutive inserts
    // can accumulate K overflow edges). We drop all overflow entries here so
    // the written graph satisfies the loader degree constraint.
    let medoid_adj_capped: Vec<u32>;
    let adjacency = graph.adjacency();
    let medoid_neighbors = &adjacency[medoid as usize];
    let medoid_capped: &[u32] =
        if medoid_degree_limit.is_some_and(|limit| medoid_neighbors.len() > limit) {
            medoid_adj_capped =
                medoid_neighbors[..medoid_degree_limit.expect("checked above")].to_vec();
            &medoid_adj_capped
        } else {
            medoid_neighbors
        };

    let total_edges: usize = adjacency
        .iter()
        .enumerate()
        .map(|(i, v)| {
            if i == medoid as usize {
                medoid_capped.len()
            } else {
                v.len()
            }
        })
        .sum();
    // magic(8) + num_nodes(4) + medoid(4) + per-node degree(4) + all edges(4 each)
    let capacity = 8 + 4 + 4 + num_nodes as usize * 4 + total_edges * 4;
    let mut buf = Vec::with_capacity(capacity);

    buf.extend_from_slice(GRAPH_MAGIC);
    buf.extend_from_slice(&num_nodes.to_le_bytes());
    buf.extend_from_slice(&medoid.to_le_bytes());

    for (i, neighbors) in adjacency.iter().enumerate() {
        let neighbors: &[u32] = if i == medoid as usize {
            medoid_capped
        } else {
            neighbors
        };
        let degree = u32::try_from(neighbors.len()).map_err(|_| {
            VamanaError::invalid_format(format!(
                "neighbor list length {} overflows u32",
                neighbors.len()
            ))
        })?;
        buf.extend_from_slice(&degree.to_le_bytes());
        for &nb in neighbors {
            buf.extend_from_slice(&nb.to_le_bytes());
        }
    }

    Ok(buf)
}

#[cfg(feature = "mmap")]
fn read_graph(path: &Path, max_degree: usize, num_vectors: usize) -> Result<VamanaGraph> {
    let data = map_checkpoint_segment(path)?;
    parse_graph(&data, max_degree, num_vectors)
}

fn parse_graph(data: &[u8], max_degree: usize, num_vectors: usize) -> Result<VamanaGraph> {
    if data.len() < 16 {
        return Err(VamanaError::invalid_format("graph.bin too short".into()));
    }
    if &data[..8] != GRAPH_MAGIC {
        return Err(VamanaError::invalid_format(
            "graph.bin magic mismatch".into(),
        ));
    }

    let num_nodes = u32::from_le_bytes(data[8..12].try_into().unwrap()) as usize;
    let medoid = u32::from_le_bytes(data[12..16].try_into().unwrap());

    if num_nodes != num_vectors {
        return Err(VamanaError::invalid_format(format!(
            "graph num_nodes {num_nodes} != num_vectors {num_vectors}"
        )));
    }
    if medoid as usize >= num_nodes {
        return Err(VamanaError::invalid_format(format!(
            "medoid {medoid} >= num_nodes {num_nodes}"
        )));
    }

    let mut offset = 16usize;

    // Every declared node contributes at least its 4-byte degree field. Reject a
    // num_nodes that cannot possibly fit the remaining bytes before requesting an
    // allocation sized to it — otherwise a corrupt or forged header (e.g. num_nodes
    // near u32::MAX paired with a tiny segment) drives `Vec::with_capacity` to try
    // reserving tens of gigabytes and aborts the process instead of returning
    // `InvalidFormat`.
    let min_remaining_bytes = num_nodes.checked_mul(4).ok_or_else(|| {
        VamanaError::invalid_format("graph.bin num_nodes overflows minimum size check".into())
    })?;
    if data.len() - offset < min_remaining_bytes {
        return Err(VamanaError::invalid_format(format!(
            "graph.bin too short for {num_nodes} declared nodes: {} bytes remaining, need at least {min_remaining_bytes}",
            data.len() - offset
        )));
    }

    let mut adjacency: Vec<Vec<u32>> = Vec::with_capacity(num_nodes);

    for _node in 0..num_nodes {
        if offset + 4 > data.len() {
            return Err(VamanaError::invalid_format(
                "graph.bin truncated at degree".into(),
            ));
        }
        let degree = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;

        if degree > num_vectors.saturating_sub(1) {
            return Err(VamanaError::invalid_format(format!(
                "node {_node} degree {degree} exceeds num_vectors-1"
            )));
        }
        if degree > max_degree && _node != medoid as usize {
            return Err(VamanaError::invalid_format(format!(
                "node {_node} degree {degree} exceeds max_degree {max_degree}"
            )));
        }
        let neighbor_bytes = degree.checked_mul(4).ok_or_else(|| {
            VamanaError::invalid_format("graph.bin neighbor byte length overflows".into())
        })?;
        let neighbors_end = offset.checked_add(neighbor_bytes).ok_or_else(|| {
            VamanaError::invalid_format("graph.bin neighbor range overflows".into())
        })?;
        if neighbors_end > data.len() {
            return Err(VamanaError::invalid_format(
                "graph.bin truncated at neighbors".into(),
            ));
        }

        let mut neighbors = Vec::with_capacity(degree);
        for _ in 0..degree {
            let nb = u32::from_le_bytes(data[offset..offset + 4].try_into().unwrap());
            offset += 4;

            if nb as usize >= num_vectors {
                return Err(VamanaError::invalid_format(format!(
                    "neighbor {nb} >= num_vectors {num_vectors}"
                )));
            }
            if nb as usize == _node {
                return Err(VamanaError::invalid_format(format!(
                    "self-loop at node {_node}"
                )));
            }
            neighbors.push(nb);
        }

        // Reject duplicate neighbors — they reduce effective degree and indicate a
        // corrupted or improperly written graph file.
        let original_len = neighbors.len();
        let mut sorted = neighbors.clone();
        sorted.sort_unstable();
        sorted.dedup();
        if sorted.len() != original_len {
            return Err(VamanaError::invalid_format(format!(
                "node {_node} has duplicate neighbors"
            )));
        }

        adjacency.push(neighbors);
    }

    if offset != data.len() {
        return Err(VamanaError::invalid_format(format!(
            "graph.bin has {} trailing bytes",
            data.len() - offset
        )));
    }

    let mut graph = VamanaGraph::new(num_nodes, medoid)?;
    for (i, neighbors) in adjacency.into_iter().enumerate() {
        *graph
            .adjacency_mut_for_load()
            .get_mut(i)
            .expect("bounds checked above") = neighbors;
    }
    Ok(graph)
}

#[cfg(feature = "mmap")]
const VECTOR_HASH_CHUNK_BYTES: usize = 64 * 1024;

#[cfg(feature = "mmap")]
fn hash_vectors_file(path: &Path) -> std::io::Result<([u8; 32], usize)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut chunk = [0_u8; VECTOR_HASH_CHUNK_BYTES];
    let mut len = 0_usize;
    loop {
        let read = match file.read(&mut chunk) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        #[cfg(test)]
        perf_tests::record_vector_hash_read(chunk.len(), read);
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
        len = len.checked_add(read).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "vectors.bin length overflow",
            )
        })?;
    }
    Ok((*hasher.finalize().as_bytes(), len))
}

#[cfg(feature = "mmap")]
fn hash_file_mmap(path: &Path) -> Result<[u8; 32]> {
    let file = File::open(path)?;
    if file.metadata()?.len() == 0 {
        return Ok(*blake3::hash(&[]).as_bytes());
    }

    // SAFETY: this is a read-only mapping under the caller's publication lock.
    // A live segment must not be mutated or truncated, as on the load path.
    let mmap = unsafe { MmapOptions::new().map(&file)? };
    Ok(*blake3::hash(mmap.as_ref()).as_bytes())
}

#[cfg(feature = "mmap")]
fn mmap_vectors(path: &Path, expected_len_f32: usize) -> Result<VectorStorage> {
    let file = File::open(path)?;
    let byte_len = usize::try_from(file.metadata()?.len())
        .map_err(|_| VamanaError::invalid_format("vectors.bin file size exceeds usize".into()))?;
    let expected_bytes = expected_len_f32
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| VamanaError::invalid_format("vectors.bin byte length overflow".into()))?;
    if byte_len != expected_bytes {
        return Err(VamanaError::invalid_format(format!(
            "vectors.bin byte length {byte_len} != expected {expected_bytes}"
        )));
    }

    // SAFETY: The index exposes this mapping as read-only via `as_slice()`.
    // Callers must not mutate or truncate the vectors.bin file while this index is alive.
    let mmap = unsafe { MmapOptions::new().len(expected_bytes).map(&file)? };

    Ok(VectorStorage::Mmap {
        mmap: std::sync::Arc::new(mmap),
        len_f32: expected_len_f32,
    })
}

/// Extended commit metadata: the corpus fingerprint plus the write-log
/// watermark trailer. `last_applied_seq` is `None` for pre-amendment (short
/// layout) records — the ADR-079 Amendment 1 classifier treats that as Cold.
#[cfg(feature = "mmap")]
pub struct PersistedCommitInfo {
    pub vector_count: u64,
    pub dimensions: u64,
    pub content_hash: [u8; 32],
    pub last_applied_seq: Option<u64>,
}

/// Read the full v2 commit record from `path/metadata.bin`.
///
/// Same absence/corruption semantics as [`read_commit_fingerprint`]:
/// `Ok(None)` for a missing file, v1 magic, or an unparseable record.
#[cfg(feature = "mmap")]
pub fn read_commit_info(path: &Path) -> Result<Option<PersistedCommitInfo>> {
    let metadata_path = path.join("metadata.bin");
    let bytes = match fs::read(&metadata_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if bytes.len() < 8 || &bytes[..8] != V2_COMMIT_MAGIC {
        return Ok(None);
    }
    match parse_v2_commit(&bytes) {
        Ok(commit) => Ok(Some(PersistedCommitInfo {
            vector_count: commit.fingerprint.vector_count,
            dimensions: commit.fingerprint.dimensions,
            content_hash: commit.fingerprint.content_hash,
            last_applied_seq: commit.last_applied_seq,
        })),
        Err(_) => Ok(None),
    }
}

/// Read the v2 commit fingerprint from a persisted segment directory without
/// loading the graph or vectors.
///
/// `path` is the segment directory; this function joins `metadata.bin`
/// internally, matching the convention used by [`VamanaIndex::load`] and
/// [`VamanaIndex::load_or_build`].
///
/// Returns `Ok(None)` when:
/// - `path/metadata.bin` is absent (clean first run)
/// - the record does not begin with the KHVVAMG2 magic (v1 format or
///   unrelated file)
/// - the record is too short or otherwise cannot be parsed (torn write)
///
/// In the `None` cases the caller should treat the segment as Cold and proceed
/// to build. Returns `Err` only for unexpected IO failures (not `NotFound`).
#[cfg(feature = "mmap")]
pub fn read_commit_fingerprint(path: &Path) -> Result<Option<PersistedFingerprint>> {
    let metadata_path = path.join("metadata.bin");
    let bytes = match fs::read(&metadata_path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    if bytes.len() < 8 || &bytes[..8] != V2_COMMIT_MAGIC {
        return Ok(None);
    }
    match parse_v2_commit(&bytes) {
        Ok(commit) => Ok(Some(PersistedFingerprint {
            vector_count: commit.fingerprint.vector_count,
            dimensions: commit.fingerprint.dimensions,
            content_hash: commit.fingerprint.content_hash,
        })),
        Err(_) => Ok(None),
    }
}

/// Canonical content hash over a flat corpus slice.
///
/// This is identical to the hash stored by [`VamanaIndex::save_atomic`] in
/// the v2 commit fingerprint and compared by [`VamanaIndex::load_or_build`]
/// when deciding whether a persisted index matches the live corpus.
///
/// The hash is computed over the raw little-endian f32 bytes of `vectors` with
/// no header or padding (matching `write_vectors` which stores exactly
/// `cast_slice(vectors)`). It is order-sensitive: reordering vectors produces
/// a different digest.
///
/// Callers must pass the same normalized, row-major flat vectors they pass (or
/// would pass) to [`VamanaIndex::build`]. This function does not normalize.
pub fn corpus_content_hash(vectors: &[f32]) -> [u8; 32] {
    *blake3::hash(cast_slice(vectors)).as_bytes()
}

mod search;

#[cfg(test)]
use crate::graph::{greedy_search_inner, greedy_search_inner_sq8};

#[cfg(all(test, feature = "mmap"))]
#[path = "checkpoint_allocation_tests.rs"]
mod checkpoint_allocation_tests;

#[cfg(feature = "mmap")]
#[path = "checkpoint_segment.rs"]
mod checkpoint_segment;
#[cfg(feature = "mmap")]
use checkpoint_segment::{map_checkpoint_segment, MappedCheckpointSegment};

#[cfg(test)]
#[path = "index_perf_compat_tests.rs"]
mod perf_compat_tests;

#[cfg(test)]
#[path = "index_perf_tests.rs"]
mod perf_tests;

#[cfg(test)]
#[path = "index_search_visited_tests.rs"]
mod search_visited_tests;

// These unit tests remain a child module so they can exercise private helpers
// and the internal `VectorStorage` enum without public re-exports.
#[cfg(test)]
#[path = "index_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "index_maintenance_tests.rs"]
mod maintenance_tests;
