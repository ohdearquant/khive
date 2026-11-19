#[cfg(feature = "mmap")]
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

use bytemuck::cast_slice;
#[cfg(feature = "mmap")]
use memmap2::MmapOptions;

use crate::{
    error::{Result, VamanaError},
    graph::VamanaGraph,
};

#[cfg(all(test, feature = "mmap"))]
use super::perf_tests;
#[cfg(all(doc, not(feature = "mmap")))]
use super::VamanaIndex;
#[cfg(feature = "mmap")]
use super::{
    map_checkpoint_segment, parse_v2_commit, IndexMetadata, PersistedFingerprint, VamanaIndex,
    VectorStorage, METADATA_MAGIC, V2_COMMIT_MAGIC,
};
use super::{ParsedLifecycle, GRAPH_MAGIC, LIFECYCLE_MAGIC};

/// Parse lifecycle.bin bytes into `ParsedLifecycle`.
pub(super) fn parse_lifecycle(
    data: &[u8],
    num_vectors: usize,
    _max_degree: usize,
) -> Result<ParsedLifecycle> {
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
pub(super) fn encode_metadata(index: &VamanaIndex) -> Vec<u8> {
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
pub(super) fn stage_legacy_replacement(destination: &Path, bytes: &[u8]) -> Result<PathBuf> {
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
pub(super) fn read_metadata(path: &Path) -> Result<IndexMetadata> {
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
pub(super) fn encode_graph(graph: &VamanaGraph, max_degree: usize) -> Result<Vec<u8>> {
    encode_graph_inner(graph, Some(max_degree))
}

pub(super) fn encode_graph_lossless(graph: &VamanaGraph) -> Result<Vec<u8>> {
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
pub(super) fn read_graph(
    path: &Path,
    max_degree: usize,
    num_vectors: usize,
) -> Result<VamanaGraph> {
    let data = map_checkpoint_segment(path)?;
    parse_graph(&data, max_degree, num_vectors)
}

pub(super) fn parse_graph(
    data: &[u8],
    max_degree: usize,
    num_vectors: usize,
) -> Result<VamanaGraph> {
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
pub(super) const VECTOR_HASH_CHUNK_BYTES: usize = 64 * 1024;

#[cfg(feature = "mmap")]
pub(super) fn hash_vectors_file(path: &Path) -> std::io::Result<([u8; 32], usize)> {
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
pub(super) fn hash_file_mmap(path: &Path) -> Result<[u8; 32]> {
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
pub(super) fn mmap_vectors(path: &Path, expected_len_f32: usize) -> Result<VectorStorage> {
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
