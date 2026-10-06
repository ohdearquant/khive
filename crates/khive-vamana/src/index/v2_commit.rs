use std::collections::HashSet;
#[cfg(feature = "mmap")]
use std::{fs, path::Path};
#[cfg(all(test, feature = "mmap"))]
use std::{fs::File, io::Write};

#[cfg(feature = "mmap")]
use crate::graph::VamanaGraph;
use crate::{
    config::VamanaConfig,
    error::{Result, VamanaError},
    graph::is_tombstoned_bit,
};

#[cfg(all(test, feature = "mmap"))]
use super::checkpoint_allocation_tests;
#[cfg(feature = "mmap")]
use super::{
    hash_vectors_file, map_checkpoint_segment, parse_codes_bin, parse_lifecycle, read_graph,
    MappedCheckpointSegment,
};
use super::{IndexMetadata, LIFECYCLE_MAGIC, V2_COMMIT_MAGIC};

// ---- V2 persistence helpers ----

/// Corpus identity check used by `save_atomic` / `load_or_build`.
/// Separate from `CorpusFingerprint` (which is part of the snapshot API).
pub(super) struct V2CorpusFingerprint {
    pub(super) vector_count: u64,
    pub(super) dimensions: u64,
    pub(super) content_hash: [u8; 32],
}

/// Parsed content of a KHVVAMG2 commit record (metadata.bin written by save_atomic).
pub(super) struct V2Commit {
    pub(super) vectors_hash: [u8; 32],
    pub(super) graph_hash: [u8; 32],
    pub(super) lifecycle_hash: [u8; 32],
    pub(super) fingerprint: V2CorpusFingerprint,
    pub(super) index_meta: IndexMetadata,
    /// Write-log watermark trailer. `None` when the record predates the field
    /// (short layout) — the record length, not a sentinel value, discriminates,
    /// so a legitimate watermark of 0 (empty log at save time) round-trips.
    pub(super) last_applied_seq: Option<u64>,
    /// blake3 checksum of the `codes.bin` segment; `None` on pre-trailer
    /// records and on containers that omit the codes segment. Read only by
    /// the mmap load path; parsed unconditionally so record validation stays
    /// identical across feature sets.
    #[cfg_attr(not(feature = "mmap"), allow(dead_code))]
    pub(super) codes_hash: Option<[u8; 32]>,
}

/// Parsed lifecycle.bin content.
pub(super) struct ParsedLifecycle {
    pub(super) tombstones: Vec<u64>,
    pub(super) free_slots: Vec<u32>,
    pub(super) reverse_adj: Vec<Vec<u32>>,
    pub(super) ops_since_consolidation: usize,
}

pub(super) fn validate_free_slots(
    free_slots: &[u32],
    tombstones: &[u64],
    num_vectors: usize,
) -> Result<()> {
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
pub(super) fn reject_checkpoint_sequence_regression(
    path: &Path,
    candidate: Option<u64>,
) -> Result<()> {
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
pub(super) fn validate_v2_structural(
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
pub(super) fn write_v2_commit_full(
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
pub(super) fn encode_v2_commit_full(
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
pub(super) fn parse_v2_commit(data: &[u8]) -> Result<V2Commit> {
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

pub(super) fn encode_lifecycle(
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
