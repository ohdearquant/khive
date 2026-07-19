//! Vamana index: build, search, save/load, and snapshot serialization.

use std::collections::{HashMap, HashSet};

#[cfg(feature = "mmap")]
use std::{
    fs::{self, File},
    io::Write,
    path::Path,
};

use bytemuck::cast_slice;
#[cfg(feature = "mmap")]
use memmap2::MmapOptions;
#[cfg(feature = "parallel")]
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use khive_quant::{GsEncodedVector, GsSq8Codec};

use crate::{
    config::VamanaConfig,
    distance::l2_squared,
    error::{Result, VamanaError},
    graph::{
        greedy_search_inner, greedy_search_inner_sq8, is_tombstoned_bit, robust_prune_inner,
        sort_dedup_u32, CodesView, VamanaGraph, VisitedSet,
    },
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

/// Format identifier string stored in every `VamanaSnapshot`.
pub const VAMANA_SNAPSHOT_FORMAT: &str = "khive-vamana-index";
/// Snapshot format version; a mismatch causes `from_snapshot` to return an error.
pub const VAMANA_SNAPSHOT_VERSION: u32 = 1;

/// Corpus identity check stored inside a `VamanaSnapshot`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CorpusFingerprint {
    pub vector_count: u64,
    pub dimensions: u32,
}

/// Persisted commit fingerprint readable without loading the graph or vectors.
///
/// Returned by [`read_commit_fingerprint`] for warm-path classification by
/// callers that need to decide Hot/Stale/Cold without triggering a full graph
/// restore or rebuild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PersistedFingerprint {
    pub vector_count: u64,
    pub dimensions: u64,
    pub content_hash: [u8; 32],
}

/// Raw deserialization target for [`VamanaIndexSnapshot`].
#[derive(Deserialize)]
struct VamanaIndexSnapshotRaw {
    num_vectors: u64,
    dimensions: u32,
    max_degree: u32,
    search_list_size: u32,
    alpha: f64,
    medoid: u32,
    adjacency: Vec<Vec<u32>>,
    vectors: Vec<f32>,
}

impl TryFrom<VamanaIndexSnapshotRaw> for VamanaIndexSnapshot {
    type Error = VamanaError;

    fn try_from(raw: VamanaIndexSnapshotRaw) -> std::result::Result<Self, VamanaError> {
        if raw.num_vectors == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: num_vectors must be > 0".into(),
            ));
        }
        if !raw.alpha.is_finite() || raw.alpha < 1.0 {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: alpha must be finite and >= 1.0, got {}",
                raw.alpha
            )));
        }
        if raw.dimensions == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: dimensions must be > 0".into(),
            ));
        }
        if raw.max_degree == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: max_degree must be > 0".into(),
            ));
        }
        if raw.search_list_size == 0 {
            return Err(VamanaError::invalid_format(
                "VamanaIndexSnapshot: search_list_size must be > 0".into(),
            ));
        }
        if raw.search_list_size < raw.max_degree {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: search_list_size ({}) must be >= max_degree ({})",
                raw.search_list_size, raw.max_degree
            )));
        }
        let num_vectors = usize::try_from(raw.num_vectors).map_err(|_| {
            VamanaError::invalid_format("VamanaIndexSnapshot: num_vectors overflow".into())
        })?;
        let dimensions = usize::try_from(raw.dimensions).map_err(|_| {
            VamanaError::invalid_format("VamanaIndexSnapshot: dimensions overflow".into())
        })?;
        let expected_floats = num_vectors.checked_mul(dimensions).ok_or_else(|| {
            VamanaError::invalid_format(
                "VamanaIndexSnapshot: num_vectors * dimensions overflow".into(),
            )
        })?;
        if raw.vectors.len() != expected_floats {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: vectors.len() ({}) != num_vectors * dimensions ({num_vectors} * {dimensions} = {expected_floats})",
                raw.vectors.len(),
            )));
        }
        if raw.adjacency.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: adjacency.len() ({}) != num_vectors ({num_vectors})",
                raw.adjacency.len(),
            )));
        }
        for (i, &v) in raw.vectors.iter().enumerate() {
            if !v.is_finite() {
                return Err(VamanaError::non_finite(
                    "VamanaIndexSnapshot.vectors",
                    format!("index {i}: {v}"),
                ));
            }
        }
        if raw.medoid as usize >= num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "VamanaIndexSnapshot: medoid ({}) >= num_vectors ({num_vectors})",
                raw.medoid
            )));
        }
        let max_degree = usize::try_from(raw.max_degree).map_err(|_| {
            VamanaError::invalid_format("VamanaIndexSnapshot: max_degree overflow".into())
        })?;
        for (node, neighbors) in raw.adjacency.iter().enumerate() {
            if neighbors.len() > max_degree {
                return Err(VamanaError::invalid_format(format!(
                    "VamanaIndexSnapshot: node {node} degree {} exceeds max_degree {max_degree}",
                    neighbors.len()
                )));
            }
            for &nb in neighbors {
                if nb as usize >= num_vectors {
                    return Err(VamanaError::invalid_format(format!(
                        "VamanaIndexSnapshot: neighbor {nb} >= num_vectors {num_vectors}"
                    )));
                }
                if nb as usize == node {
                    return Err(VamanaError::invalid_format(format!(
                        "VamanaIndexSnapshot: self-loop at node {node}"
                    )));
                }
            }
            let mut sorted = neighbors.clone();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            if sorted.len() != before {
                return Err(VamanaError::invalid_format(format!(
                    "VamanaIndexSnapshot: node {node} has duplicate neighbors"
                )));
            }
        }
        Ok(Self {
            num_vectors: raw.num_vectors,
            dimensions: raw.dimensions,
            max_degree: raw.max_degree,
            search_list_size: raw.search_list_size,
            alpha: raw.alpha,
            medoid: raw.medoid,
            adjacency: raw.adjacency,
            vectors: raw.vectors,
        })
    }
}

/// Serialisable graph payload stored inside `VamanaSnapshot`.
/// Deserialization validates that `alpha` is finite and >= 1.0, and that all
/// vector values are finite. Use `from_snapshot` to reconstruct a live index.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "VamanaIndexSnapshotRaw")]
pub struct VamanaIndexSnapshot {
    /// Number of indexed vectors.
    pub num_vectors: u64,
    /// Vector dimensionality.
    pub dimensions: u32,
    /// Maximum out-degree used during build.
    pub max_degree: u32,
    /// Greedy-search candidate list size used during build.
    pub search_list_size: u32,
    /// Robust-prune alpha used during build.
    pub alpha: f64,
    /// Medoid node ID (start node for greedy search).
    pub medoid: u32,
    /// Adjacency lists; one `Vec<u32>` per node.
    pub adjacency: Vec<Vec<u32>>,
    /// Row-major flat vector data; `num_vectors × dimensions` `f32` values.
    pub vectors: Vec<f32>,
}

/// Raw deserialization target for [`VamanaSnapshot`].
#[derive(Deserialize)]
struct VamanaSnapshotRaw {
    format: String,
    version: u32,
    namespace: String,
    model: String,
    fingerprint: CorpusFingerprint,
    index: VamanaIndexSnapshot,
    external_ids: Vec<String>,
}

impl TryFrom<VamanaSnapshotRaw> for VamanaSnapshot {
    type Error = VamanaError;

    fn try_from(raw: VamanaSnapshotRaw) -> std::result::Result<Self, VamanaError> {
        let num_vectors = usize::try_from(raw.index.num_vectors).map_err(|_| {
            VamanaError::invalid_format("VamanaSnapshot: index.num_vectors overflow".into())
        })?;
        if raw.external_ids.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "VamanaSnapshot: external_ids.len() ({}) != num_vectors ({num_vectors})",
                raw.external_ids.len(),
            )));
        }
        Ok(Self {
            format: raw.format,
            version: raw.version,
            namespace: raw.namespace,
            model: raw.model,
            fingerprint: raw.fingerprint,
            index: raw.index,
            external_ids: raw.external_ids,
        })
    }
}

/// Self-validating snapshot of a `VamanaIndex`. Deserialization validates
/// vector finiteness and alpha range at the serde boundary.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "VamanaSnapshotRaw")]
pub struct VamanaSnapshot {
    pub format: String,
    pub version: u32,
    pub namespace: String,
    pub model: String,
    pub fingerprint: CorpusFingerprint,
    pub index: VamanaIndexSnapshot,
    /// u32 node-id → external UUID string mapping preserved for `AnnBridge`.
    pub external_ids: Vec<String>,
}

enum VectorStorage {
    Owned(Vec<f32>),
    #[cfg(feature = "mmap")]
    Mmap {
        mmap: memmap2::Mmap,
        len_f32: usize,
    },
}

impl std::fmt::Debug for VectorStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(v) => write!(f, "Owned(len={})", v.len()),
            #[cfg(feature = "mmap")]
            Self::Mmap { len_f32, .. } => write!(f, "Mmap(len_f32={len_f32})"),
        }
    }
}

impl VectorStorage {
    fn as_slice(&self) -> Result<&[f32]> {
        match self {
            Self::Owned(v) => Ok(v.as_slice()),
            #[cfg(feature = "mmap")]
            Self::Mmap { mmap, len_f32 } => {
                let floats: &[f32] = bytemuck::try_cast_slice(mmap.as_ref())
                    .map_err(|_| VamanaError::invalid_format("vector mmap cast failed".into()))?;
                if floats.len() != *len_f32 {
                    return Err(VamanaError::invalid_format(format!(
                        "mmap f32 length {} != expected {}",
                        floats.len(),
                        len_f32
                    )));
                }
                Ok(floats)
            }
        }
    }
}

const CODES_MAGIC: &[u8; 8] = b"KHVCODE1";
const CODES_HEADER_LEN: usize = 8 + 8 + 8 + 4 + 4;

/// Storage for the per-node SQ8 code table: owned per-vector allocations
/// (build and mutation paths) or the flat, memory-mapped `codes.bin` segment
/// (v2 load path). Mirrors `VectorStorage`'s Owned/Mmap split.
enum CodeStore {
    Owned(Vec<GsEncodedVector>),
    #[cfg(feature = "mmap")]
    Mmap {
        mmap: memmap2::Mmap,
        dims: usize,
        len: usize,
    },
}

impl std::fmt::Debug for CodeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(v) => write!(f, "Owned(len={})", v.len()),
            #[cfg(feature = "mmap")]
            Self::Mmap { len, .. } => write!(f, "Mmap(len={len})"),
        }
    }
}

impl CodeStore {
    fn view(&self) -> CodesView<'_> {
        match self {
            Self::Owned(v) => CodesView::Owned(v),
            #[cfg(feature = "mmap")]
            Self::Mmap { mmap, dims, len } => CodesView::Flat {
                bytes: &mmap.as_ref()[CODES_HEADER_LEN + dims * 4..][..len * dims],
                dims: *dims,
            },
        }
    }

    fn owned_mut(&mut self) -> Result<&mut Vec<GsEncodedVector>> {
        match self {
            Self::Owned(v) => Ok(v),
            #[cfg(feature = "mmap")]
            Self::Mmap { .. } => Err(VamanaError::invalid_format(
                "codes: unexpected Mmap after ensure_owned".into(),
            )),
        }
    }

    fn ensure_owned(&mut self) {
        #[cfg(feature = "mmap")]
        if let Self::Mmap { len, .. } = self {
            let len = *len;
            let view = self.view();
            let owned: Vec<GsEncodedVector> = (0..len)
                .map(|i| GsEncodedVector {
                    codes: view.code(i).to_vec(),
                })
                .collect();
            *self = Self::Owned(owned);
        }
    }
}

/// Serialize the SQ8 codec parameters and per-node codes into the `codes.bin`
/// segment layout: magic, dims, count, gs, anisotropy_ratio, per-dimension
/// minima, then `count * dims` code bytes in ordinal order.
fn encode_codes_bin(codec: &GsSq8Codec, codes: CodesView<'_>) -> Vec<u8> {
    let dims = codec.dims();
    let count = codes.len();
    let mut buf = Vec::with_capacity(CODES_HEADER_LEN + dims * 4 + count * dims);
    buf.extend_from_slice(CODES_MAGIC);
    buf.extend_from_slice(&(dims as u64).to_le_bytes());
    buf.extend_from_slice(&(count as u64).to_le_bytes());
    buf.extend_from_slice(&codec.gs.to_le_bytes());
    buf.extend_from_slice(&codec.anisotropy_ratio.to_le_bytes());
    for m in &codec.min {
        buf.extend_from_slice(&m.to_le_bytes());
    }
    for i in 0..count {
        buf.extend_from_slice(codes.code(i));
    }
    buf
}

/// Parse and validate a `codes.bin` header, returning the reconstructed codec.
/// The code bytes themselves stay in the caller's buffer/mapping at offset
/// `CODES_HEADER_LEN + dims * 4`.
fn parse_codes_bin(data: &[u8], expected_dims: usize, expected_count: usize) -> Result<GsSq8Codec> {
    if data.len() < CODES_HEADER_LEN || &data[..8] != CODES_MAGIC {
        return Err(VamanaError::invalid_format(
            "codes.bin missing or bad magic".into(),
        ));
    }
    let dims = usize::try_from(u64::from_le_bytes(data[8..16].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("codes.bin dims overflow".into()))?;
    let count = usize::try_from(u64::from_le_bytes(data[16..24].try_into().unwrap()))
        .map_err(|_| VamanaError::invalid_format("codes.bin count overflow".into()))?;
    let gs = f32::from_le_bytes(data[24..28].try_into().unwrap());
    let anisotropy_ratio = f32::from_le_bytes(data[28..32].try_into().unwrap());
    if dims != expected_dims || count != expected_count {
        return Err(VamanaError::invalid_format(format!(
            "codes.bin shape {count}x{dims} != expected {expected_count}x{expected_dims}"
        )));
    }
    let expected_len = CODES_HEADER_LEN + dims * 4 + count * dims;
    if data.len() != expected_len {
        return Err(VamanaError::invalid_format(format!(
            "codes.bin length {} != expected {expected_len}",
            data.len()
        )));
    }
    if !gs.is_finite() || gs <= 0.0 {
        return Err(VamanaError::invalid_format(
            "codes.bin non-positive gs".into(),
        ));
    }
    let mut min = Vec::with_capacity(dims);
    for d in 0..dims {
        let off = CODES_HEADER_LEN + d * 4;
        let v = f32::from_le_bytes(data[off..off + 4].try_into().unwrap());
        if !v.is_finite() {
            return Err(VamanaError::invalid_format(
                "codes.bin non-finite min".into(),
            ));
        }
        min.push(v);
    }
    Ok(GsSq8Codec {
        min,
        gs,
        gs_sq: gs * gs,
        anisotropy_ratio,
    })
}

/// An in-memory Vamana ANN index over pre-normalized vectors.
#[derive(Debug)]
pub struct VamanaIndex {
    vectors: VectorStorage,
    graph: VamanaGraph,
    config: VamanaConfig,
    num_vectors: usize,
    dimensions: usize,
    // ---- PR2: lifecycle fields (ADR-052 §2; see docs/design.md#lifecycle-fields) ----
    /// Bit-packed tombstone marks. Bit `i` set ⇒ node `i` is soft-deleted.
    tombstones: Vec<u64>,
    /// Count of currently tombstoned nodes.
    tombstone_count: usize,
    /// Cumulative delete+insert churn since the last consolidation.
    ops_since_consolidation: usize,
    /// Recycled ordinal slots from previous tombstone calls; consumed by insert (PR3).
    free_slots: Vec<u32>,
    /// Trigger tau: consolidation fires when `ops_since_consolidation >= consolidation_tau`.
    consolidation_tau: usize,
    // ---- SQ8 acquisition tier (ADR-052 §1, Step 2) ----
    /// Global-scale SQ8 codec trained over the build corpus; used for acquisition-tier distances.
    gs_codec: GsSq8Codec,
    /// Pre-encoded corpus vectors, ordinal-stable (owned or mmap `codes.bin`).
    gs_codes: CodeStore,
    /// External write-log watermark carried in the v2 commit record. `None` on
    /// indexes built or loaded from segments that predate the field; the
    /// storage layer that owns the log sets it before `save_atomic` and reads
    /// it back after load to classify restart state.
    last_applied_seq: Option<u64>,
}

struct IndexMetadata {
    num_vectors: usize,
    dimensions: usize,
    max_degree: usize,
    search_list_size: usize,
    alpha: f64,
}

/// Train + encode the SQ8 acquisition-tier codec; called by every index constructor.
fn train_codec_and_encode(vectors: &[f32], dims: usize) -> (GsSq8Codec, Vec<GsEncodedVector>) {
    let codec = GsSq8Codec::train_flat(vectors, dims);
    let codes = codec.encode_flat_par(vectors, dims);
    (codec, codes)
}

/// Scan a flat f32 slice for any non-finite value, returning an error on the first hit.
fn require_finite(values: &[f32], location: &str) -> Result<()> {
    for (i, v) in values.iter().enumerate() {
        if !v.is_finite() {
            return Err(VamanaError::non_finite(location, format!("index {i}: {v}")));
        }
    }
    Ok(())
}

impl VamanaIndex {
    /// Build from row-major flat slice. Errors if config invalid, empty, wrong length, non-finite, or N > u32::MAX.
    ///
    /// Uses `GsSq8Codec` for the acquisition-tier distance during graph construction
    /// (ADR-052 §1, Step 2: default-on for Vamana, algebraically exact in code space).
    pub fn build(vectors: &[f32], config: VamanaConfig) -> Result<Self> {
        config.validate()?;
        if vectors.is_empty() {
            return Err(VamanaError::EmptyInput);
        }
        if !vectors.len().is_multiple_of(config.dimensions) {
            return Err(VamanaError::DimensionMismatch {
                expected: config.dimensions,
                actual: vectors.len() % config.dimensions,
            });
        }
        require_finite(vectors, "build vectors")?;
        let num_vectors = vectors.len() / config.dimensions;
        if num_vectors > u32::MAX as usize {
            return Err(VamanaError::TooManyVectors { count: num_vectors });
        }

        let (gs_codec, gs_codes) = train_codec_and_encode(vectors, config.dimensions);

        let graph =
            VamanaGraph::build_sq8(vectors, CodesView::Owned(&gs_codes), &gs_codec, &config)?;
        let dimensions = config.dimensions;

        Ok(Self {
            vectors: VectorStorage::Owned(vectors.to_vec()),
            graph,
            config,
            num_vectors,
            dimensions,
            tombstones: tombstone_words_for(num_vectors),
            tombstone_count: 0,
            ops_since_consolidation: 0,
            free_slots: Vec::new(),
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: None,
        })
    }

    /// Search for `k` nearest neighbors. Errors if dimension mismatch or non-finite query values.
    ///
    /// Uses `GsSq8Codec` for acquisition-tier traversal; returned distances are exact f32 L2²
    /// (ADR-052 §1 two-tier: SQ8 for candidate selection, exact f32 for final results).
    pub fn search(&self, query: &[f32], k: usize) -> Result<Vec<(u32, f32)>> {
        if query.len() != self.dimensions {
            return Err(VamanaError::DimensionMismatch {
                expected: self.dimensions,
                actual: query.len(),
            });
        }
        if k == 0 {
            return Ok(Vec::new());
        }
        require_finite(query, "search query")?;

        let tombstones = if self.tombstone_count > 0 {
            Some(self.tombstones.as_slice())
        } else {
            None
        };
        let mut visited = VisitedSet::new(self.num_vectors);

        // OOD fallback (ADR-052 §2): if any query component lies outside the codec's
        // trained range [min_d, min_d + 255·gs], encoding clamps that dimension and
        // SQ8 distances cannot correctly order the frontier. Fall back to exact f32
        // greedy search for this query; in-distribution queries keep the SQ8 path.
        let result = if self.gs_codec.is_in_distribution(query) {
            let query_enc = self.gs_codec.encode(query);
            greedy_search_inner_sq8(
                self.vectors()?,
                self.dimensions,
                self.gs_codes.view(),
                &self.gs_codec,
                self.graph.adjacency(),
                query,
                &query_enc.codes,
                self.graph.medoid(),
                k,
                self.config.search_list_size,
                &mut visited,
                tombstones,
            )
        } else {
            greedy_search_inner(
                self.vectors()?,
                self.dimensions,
                self.graph.adjacency(),
                query,
                self.graph.medoid(),
                k,
                self.config.search_list_size,
                &mut visited,
                tombstones,
            )
        };

        let mut output = result.results;
        output.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
            a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
        });
        Ok(output)
    }

    /// Encode this index into the ADR-110 portable container.
    pub fn to_bytes(&self, external_ids: &[(u32, String)]) -> Result<Vec<u8>> {
        let vectors = cast_slice(self.vectors()?).to_vec();
        let graph = encode_graph_lossless(&self.graph)?;
        let lifecycle = encode_lifecycle(
            &self.tombstones,
            &self.free_slots,
            self.graph.reverse_adjacency(),
            self.ops_since_consolidation,
        );

        let vectors_hash = *blake3::hash(&vectors).as_bytes();
        let graph_hash = *blake3::hash(&graph).as_bytes();
        let lifecycle_hash = *blake3::hash(&lifecycle).as_bytes();
        let fingerprint = V2CorpusFingerprint {
            vector_count: self.num_vectors as u64,
            dimensions: self.dimensions as u64,
            content_hash: vectors_hash,
        };
        let metadata = encode_v2_commit_full(
            &vectors_hash,
            &graph_hash,
            &lifecycle_hash,
            &fingerprint,
            self.num_vectors,
            self.dimensions,
            self.config.max_degree,
            self.config.search_list_size,
            self.config.alpha,
            self.last_applied_seq,
            None,
        );

        let mut segments = vec![
            ("metadata.bin", metadata),
            ("vectors.bin", vectors),
            ("graph.bin", graph),
            ("lifecycle.bin", lifecycle),
        ];
        if !external_ids.is_empty() {
            segments.push(("portable_ids.bin", encode_portable_ids(self, external_ids)?));
        }
        encode_portable_container(&segments)
    }

    /// Decode an ADR-110 portable container into owned storage and its live ID mapping.
    pub fn from_bytes(bytes: &[u8]) -> Result<(Self, Vec<(u32, String)>)> {
        let segments = parse_portable_container(bytes)?;
        let metadata = required_segment(&segments, "metadata.bin")?;
        let vectors = required_segment(&segments, "vectors.bin")?;
        let graph = required_segment(&segments, "graph.bin")?;
        let lifecycle = required_segment(&segments, "lifecycle.bin")?;

        let index = Self::from_v2_bytes(metadata, vectors, graph, lifecycle)?;
        let external_ids = match segments.get("portable_ids.bin") {
            Some(ids) => parse_portable_ids(ids, &index)?,
            None => Vec::new(),
        };
        Ok((index, external_ids))
    }

    fn from_v2_bytes(
        metadata: &[u8],
        vector_bytes: &[u8],
        graph_bytes: &[u8],
        lifecycle_bytes: &[u8],
    ) -> Result<Self> {
        let commit = parse_v2_commit(metadata)?;
        let vectors_hash = *blake3::hash(vector_bytes).as_bytes();
        if vectors_hash != commit.vectors_hash
            || *blake3::hash(graph_bytes).as_bytes() != commit.graph_hash
            || *blake3::hash(lifecycle_bytes).as_bytes() != commit.lifecycle_hash
        {
            return Err(VamanaError::invalid_format(
                "v2 segment checksum mismatch".into(),
            ));
        }
        if commit.fingerprint.vector_count != commit.index_meta.num_vectors as u64
            || commit.fingerprint.dimensions != commit.index_meta.dimensions as u64
            || commit.fingerprint.content_hash != vectors_hash
        {
            return Err(VamanaError::invalid_format(
                "v2 corpus fingerprint mismatch".into(),
            ));
        }

        let config = VamanaConfig {
            dimensions: commit.index_meta.dimensions,
            max_degree: commit.index_meta.max_degree,
            search_list_size: commit.index_meta.search_list_size,
            alpha: commit.index_meta.alpha,
        };
        config.validate()?;
        let num_vectors = commit.index_meta.num_vectors;
        let expected_floats = num_vectors
            .checked_mul(config.dimensions)
            .ok_or_else(|| VamanaError::invalid_format("v2 metadata overflow".into()))?;
        let expected_bytes = expected_floats
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| {
                VamanaError::invalid_format("vectors.bin byte length overflow".into())
            })?;
        if vector_bytes.len() != expected_bytes {
            return Err(VamanaError::invalid_format(format!(
                "vectors.bin byte length {} != expected {expected_bytes}",
                vector_bytes.len()
            )));
        }
        let vectors: Vec<f32> = vector_bytes
            .chunks_exact(4)
            .map(|bytes| f32::from_le_bytes(bytes.try_into().expect("four-byte chunk")))
            .collect();
        require_finite(&vectors, "portable vectors")?;

        let mut graph = parse_graph(graph_bytes, config.max_degree, num_vectors)?;
        let parsed = parse_lifecycle(lifecycle_bytes, num_vectors, config.max_degree)?;
        validate_reverse_adjacency(&graph, &parsed.reverse_adj)?;
        graph.restore_reverse_adj(parsed.reverse_adj);

        let tombstone_count = parsed
            .tombstones
            .iter()
            .map(|word| word.count_ones() as usize)
            .sum();
        if tombstone_count > num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin tombstone_count {tombstone_count} exceeds num_vectors {num_vectors}"
            )));
        }
        let mut free_slots = HashSet::with_capacity(parsed.free_slots.len());
        for &slot in &parsed.free_slots {
            if slot as usize >= num_vectors
                || !is_tombstoned_bit(&parsed.tombstones, slot as usize)
                || !free_slots.insert(slot)
            {
                return Err(VamanaError::invalid_format(format!(
                    "lifecycle.bin invalid free slot {slot}"
                )));
            }
        }

        let (gs_codec, gs_codes) = train_codec_and_encode(&vectors, config.dimensions);
        Ok(Self {
            vectors: VectorStorage::Owned(vectors),
            graph,
            dimensions: config.dimensions,
            config,
            num_vectors,
            tombstones: parsed.tombstones,
            tombstone_count,
            ops_since_consolidation: parsed.ops_since_consolidation,
            free_slots: parsed.free_slots,
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: commit.last_applied_seq,
        })
    }

    /// Persist the index to `path` (a directory); writes `metadata.bin`, `graph.bin`, `vectors.bin`.
    #[cfg(feature = "mmap")]
    pub fn save(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        write_metadata(&path.join("metadata.bin"), self)?;
        write_graph(&path.join("graph.bin"), &self.graph, self.config.max_degree)?;
        write_vectors(&path.join("vectors.bin"), self.vectors()?)?;
        Ok(())
    }

    /// Load an index from a directory previously written by [`VamanaIndex::save`]
    /// (v1 format) or [`VamanaIndex::save_atomic`] (v2 segmented format); the format is
    /// auto-detected from `metadata.bin`'s magic. Never rebuilds — a corrupt, torn, or
    /// absent index returns an error, leaving the recovery decision to the caller (see
    /// [`Self::load_or_build`] and crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load).
    #[cfg(feature = "mmap")]
    pub fn load(path: &Path) -> Result<Self> {
        let metadata_path = path.join("metadata.bin");
        let head = fs::read(&metadata_path)?;
        if head.len() >= 8 && &head[..8] == V2_COMMIT_MAGIC {
            return Self::load_v2_raw(path);
        }

        let meta = read_metadata(&metadata_path)?;
        let config = VamanaConfig {
            dimensions: meta.dimensions,
            max_degree: meta.max_degree,
            search_list_size: meta.search_list_size,
            alpha: meta.alpha,
        };
        config.validate()?;

        let mut graph = read_graph(&path.join("graph.bin"), meta.max_degree, meta.num_vectors)?;

        if graph.node_count() != meta.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "graph node count {} != metadata num_vectors {}",
                graph.node_count(),
                meta.num_vectors
            )));
        }
        if graph.medoid() as usize >= meta.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "medoid {} >= num_vectors {}",
                graph.medoid(),
                meta.num_vectors
            )));
        }

        let expected_len_f32 = meta
            .num_vectors
            .checked_mul(meta.dimensions)
            .ok_or_else(|| VamanaError::invalid_format("metadata overflow".into()))?;
        let storage = mmap_vectors(&path.join("vectors.bin"), expected_len_f32)?;

        // v1 format does not persist reverse_adj; reconstruct O(N*R) from adjacency.
        // This must run before any tombstone call — lazy init would silently skip repair.
        graph.rebuild_reverse_adj_from_adjacency();

        let (gs_codec, gs_codes) = train_codec_and_encode(storage.as_slice()?, meta.dimensions);

        Ok(Self {
            vectors: storage,
            graph,
            config,
            num_vectors: meta.num_vectors,
            dimensions: meta.dimensions,
            tombstones: tombstone_words_for(meta.num_vectors),
            tombstone_count: 0,
            ops_since_consolidation: 0,
            free_slots: Vec::new(),
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: None,
        })
    }

    /// Crash-safe v2 save: writes `vectors.bin`, `graph.bin`, and `lifecycle.bin`, then
    /// atomically renames `metadata.bin.tmp` → `metadata.bin` as the commit record. A
    /// crash at any point before that rename leaves the previous `metadata.bin` (v1 or
    /// v2) valid and untouched — [`Self::load_or_build`] never observes a torn v2
    /// commit. See crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load for
    /// the staging/fsync sequence.
    #[cfg(feature = "mmap")]
    pub fn save_atomic(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;

        // Stage under .v2new so a crash before the metadata rename leaves the previous
        // segments (v1 or v2) intact and readable.
        let vectors_new = path.join("vectors.bin.v2new");
        let graph_new = path.join("graph.bin.v2new");
        let lifecycle_new = path.join("lifecycle.bin.v2new");
        let codes_new = path.join("codes.bin.v2new");
        let vectors_path = path.join("vectors.bin");
        let graph_path = path.join("graph.bin");
        let lifecycle_path = path.join("lifecycle.bin");
        let codes_path = path.join("codes.bin");
        let metadata_path = path.join("metadata.bin");
        let metadata_tmp = path.join("metadata.bin.tmp");

        // 1. Write vectors.bin.v2new (identical format to v1).
        write_vectors(&vectors_new, self.vectors()?)?;

        // 2. Write graph.bin.v2new (identical format to v1, medoid-overflow capped).
        write_graph(&graph_new, &self.graph, self.config.max_degree)?;

        // 3. Compute the capped reverse adjacency that matches what write_graph wrote.
        //    The medoid's forward list may exceed max_degree in-memory; write_graph caps it
        //    before serialization. Build reverse adj from the same capped view so that
        //    lifecycle.bin stays consistent with graph.bin after restore.
        let capped_reverse_adj = capped_reverse_adjacency(self);

        // 4. Write lifecycle.bin.v2new with the capped reverse adjacency.
        write_lifecycle(
            &lifecycle_new,
            &self.tombstones,
            &self.free_slots,
            &capped_reverse_adj,
            self.ops_since_consolidation,
        )?;

        // 4b. Write codes.bin.v2new (SQ8 codec + per-node codes, ADR-052
        // acquisition tier) so the load path never retrains over the corpus.
        {
            let buf = encode_codes_bin(&self.gs_codec, self.gs_codes.view());
            let file = File::create(&codes_new)?;
            let mut w = std::io::BufWriter::new(file);
            w.write_all(&buf)?;
            let file = w.into_inner().map_err(|e| e.into_error())?;
            file.sync_all()?;
        }

        // 5. Compute blake3 checksums of the four staged segments.
        let vectors_data = fs::read(&vectors_new)?;
        let graph_data = fs::read(&graph_new)?;
        let lifecycle_data = fs::read(&lifecycle_new)?;
        let codes_data = fs::read(&codes_new)?;

        let vectors_hash = blake3::hash(&vectors_data);
        let graph_hash = blake3::hash(&graph_data);
        let lifecycle_hash = blake3::hash(&lifecycle_data);
        let codes_hash = blake3::hash(&codes_data);

        // 6. Build the corpus fingerprint (content_hash = blake3 over raw vector bytes).
        let content_hash = *vectors_hash.as_bytes();
        let fp = V2CorpusFingerprint {
            vector_count: self.num_vectors as u64,
            dimensions: self.dimensions as u64,
            content_hash,
        };

        // 7. Write metadata.bin.tmp then rename atomically (the commit gate).
        write_v2_commit_full(
            &metadata_tmp,
            vectors_hash.as_bytes(),
            graph_hash.as_bytes(),
            lifecycle_hash.as_bytes(),
            &fp,
            self.num_vectors,
            self.dimensions,
            self.config.max_degree,
            self.config.search_list_size,
            self.config.alpha,
            self.last_applied_seq,
            Some(codes_hash.as_bytes()),
        )?;
        fs::rename(&metadata_tmp, &metadata_path)?;

        // Barrier: make metadata.bin durable before promoting canonical segments.
        // Commit gate is metadata.bin: after this fsync any post-crash state is either
        // (old metadata + old segments) or (new KHVVAMG2 metadata + maybe-stale segments).
        // The new-metadata path is checksum-guarded, so stale/partial segments safe-degrade
        // to rebuild — never a torn no-checksum read.
        {
            let dir_file = File::open(path)?;
            dir_file.sync_all()?;
        }

        // 8. Promote staged segments to their final names.
        fs::rename(&vectors_new, &vectors_path)?;
        fs::rename(&graph_new, &graph_path)?;
        fs::rename(&lifecycle_new, &lifecycle_path)?;
        fs::rename(&codes_new, &codes_path)?;

        // 9. Sync the directory entry so all renames are durable.
        let dir_file = File::open(path)?;
        dir_file.sync_all()?;

        Ok(())
    }

    /// Fingerprint-gated restore. On a corpus fingerprint match, loads all segments
    /// (including lifecycle state) in O(N) without rebuilding `reverse_adj`. On mismatch
    /// or a missing/corrupt v2 commit, rebuilds from `corpus_vectors` (the caller's raw
    /// flat f32 slice) using `fallback_config` or the commit's saved config, then
    /// persists via `save_atomic`. Full decision tree:
    /// crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load.
    #[cfg(feature = "mmap")]
    pub fn load_or_build(
        path: &Path,
        corpus_vectors: &[f32],
        fallback_config: VamanaConfig,
    ) -> Result<Self> {
        let metadata_path = path.join("metadata.bin");

        let metadata_bytes = match fs::read(&metadata_path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Clean first run: no saved state at all. Remove any stale staged segments.
                for suffix in &[
                    "vectors.bin.v2new",
                    "graph.bin.v2new",
                    "lifecycle.bin.v2new",
                ] {
                    let _ = fs::remove_file(path.join(suffix));
                }
                let index = Self::rebuild_from_corpus(corpus_vectors, fallback_config)?;
                index.save_atomic(path)?;
                return Ok(index);
            }
            Err(e) => return Err(e.into()),
        };

        if metadata_bytes.len() < 8 {
            let index = Self::rebuild_from_corpus(corpus_vectors, fallback_config)?;
            index.save_atomic(path)?;
            return Ok(index);
        }

        if &metadata_bytes[..8] == V2_COMMIT_MAGIC {
            let commit = match parse_v2_commit(&metadata_bytes) {
                Ok(c) => c,
                Err(_) => {
                    let index = Self::rebuild_from_corpus(corpus_vectors, fallback_config)?;
                    index.save_atomic(path)?;
                    return Ok(index);
                }
            };

            // Verify checksums of all three segments.
            let vectors_data = match fs::read(path.join("vectors.bin")) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let config = VamanaConfig {
                        dimensions: commit.index_meta.dimensions,
                        max_degree: commit.index_meta.max_degree,
                        search_list_size: commit.index_meta.search_list_size,
                        alpha: commit.index_meta.alpha,
                    };
                    let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                    index.save_atomic(path)?;
                    return Ok(index);
                }
                Err(e) => return Err(e.into()),
            };
            let graph_data = match fs::read(path.join("graph.bin")) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let config = VamanaConfig {
                        dimensions: commit.index_meta.dimensions,
                        max_degree: commit.index_meta.max_degree,
                        search_list_size: commit.index_meta.search_list_size,
                        alpha: commit.index_meta.alpha,
                    };
                    let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                    index.save_atomic(path)?;
                    return Ok(index);
                }
                Err(e) => return Err(e.into()),
            };
            let lifecycle_data = match fs::read(path.join("lifecycle.bin")) {
                Ok(d) => d,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    let config = VamanaConfig {
                        dimensions: commit.index_meta.dimensions,
                        max_degree: commit.index_meta.max_degree,
                        search_list_size: commit.index_meta.search_list_size,
                        alpha: commit.index_meta.alpha,
                    };
                    let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                    index.save_atomic(path)?;
                    return Ok(index);
                }
                Err(e) => return Err(e.into()),
            };

            let vhash = *blake3::hash(&vectors_data).as_bytes();
            let ghash = *blake3::hash(&graph_data).as_bytes();
            let lhash = *blake3::hash(&lifecycle_data).as_bytes();

            if vhash != commit.vectors_hash
                || ghash != commit.graph_hash
                || lhash != commit.lifecycle_hash
            {
                let config = VamanaConfig {
                    dimensions: commit.index_meta.dimensions,
                    max_degree: commit.index_meta.max_degree,
                    search_list_size: commit.index_meta.search_list_size,
                    alpha: commit.index_meta.alpha,
                };
                let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                index.save_atomic(path)?;
                return Ok(index);
            }

            // Verify corpus fingerprint: dimensions, count, and content hash.
            let dim = commit.index_meta.dimensions;
            if dim == 0 || !corpus_vectors.len().is_multiple_of(dim) {
                let config = VamanaConfig {
                    dimensions: commit.index_meta.dimensions,
                    max_degree: commit.index_meta.max_degree,
                    search_list_size: commit.index_meta.search_list_size,
                    alpha: commit.index_meta.alpha,
                };
                let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                index.save_atomic(path)?;
                return Ok(index);
            }
            let live_count = corpus_vectors.len() / dim;
            let live_content_hash = *blake3::hash(cast_slice(corpus_vectors)).as_bytes();

            let fp_matches = commit.fingerprint.vector_count == live_count as u64
                && commit.fingerprint.dimensions == dim as u64
                && commit.fingerprint.content_hash == live_content_hash;

            if !fp_matches {
                let config = VamanaConfig {
                    dimensions: commit.index_meta.dimensions,
                    max_degree: commit.index_meta.max_degree,
                    search_list_size: commit.index_meta.search_list_size,
                    alpha: commit.index_meta.alpha,
                };
                let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                index.save_atomic(path)?;
                return Ok(index);
            }

            // Fast path: load all segments, restore lifecycle state.
            // A corrupt-but-checksum-valid lifecycle segment (e.g. reverse_adj not the inverse
            // of graph.bin) passes the blake3 gate but fails the new bidirectional check inside
            // load_v2_fast.  Route that InvalidFormat error through the same corrupt-snapshot →
            // rebuild path used by the unknown-magic and checksum-mismatch cases above.
            match Self::load_v2_fast(path, &lifecycle_data) {
                Ok(index) => Ok(index),
                Err(VamanaError::InvalidFormat { .. }) => {
                    let config = VamanaConfig {
                        dimensions: commit.index_meta.dimensions,
                        max_degree: commit.index_meta.max_degree,
                        search_list_size: commit.index_meta.search_list_size,
                        alpha: commit.index_meta.alpha,
                    };
                    let index = Self::rebuild_from_corpus(corpus_vectors, config)?;
                    index.save_atomic(path)?;
                    Ok(index)
                }
                Err(e) => Err(e),
            }
        } else if &metadata_bytes[..8] == METADATA_MAGIC {
            // V1 format: upgrade to v2. Remove any stale staged segments first.
            for suffix in &[
                "vectors.bin.v2new",
                "graph.bin.v2new",
                "lifecycle.bin.v2new",
            ] {
                let _ = fs::remove_file(path.join(suffix));
            }
            let mut index = Self::load(path)?;
            // Release the mmap before save_atomic overwrites the same files.
            index.ensure_owned()?;
            index.save_atomic(path)?;
            Ok(index)
        } else {
            // Unknown or garbage magic: treat as corrupt snapshot and rebuild.
            // VamanaIndex::load (direct v1 callers) remains strict; load_or_build always
            // recovers because the caller supplies a corpus and fallback config.
            let index = Self::rebuild_from_corpus(corpus_vectors, fallback_config)?;
            index.save_atomic(path)?;
            Ok(index)
        }
    }

    /// Load a committed v2 index from `path` without a corpus and without rebuilding;
    /// errors (never rebuilds) if no valid v2 commit is present. See
    /// crates/khive-vamana/docs/api/persistence.md#v2-crash-safe-save-load for the full
    /// decision tree shared with `load_or_build`.
    #[cfg(feature = "mmap")]
    fn load_v2_raw(path: &Path) -> Result<Self> {
        let metadata_bytes = fs::read(path.join("metadata.bin"))?;
        if metadata_bytes.len() < 8 || &metadata_bytes[..8] != V2_COMMIT_MAGIC {
            return Err(VamanaError::invalid_format(
                "metadata.bin is not a v2 commit".into(),
            ));
        }
        let commit = parse_v2_commit(&metadata_bytes)?;

        let vectors_data = fs::read(path.join("vectors.bin"))?;
        let graph_data = fs::read(path.join("graph.bin"))?;
        let lifecycle_data = fs::read(path.join("lifecycle.bin"))?;

        if *blake3::hash(&vectors_data).as_bytes() != commit.vectors_hash
            || *blake3::hash(&graph_data).as_bytes() != commit.graph_hash
            || *blake3::hash(&lifecycle_data).as_bytes() != commit.lifecycle_hash
        {
            return Err(VamanaError::invalid_format(
                "v2 segment checksum mismatch".into(),
            ));
        }
        if let Some(expected) = commit.codes_hash {
            let codes_data = fs::read(path.join("codes.bin"))?;
            if *blake3::hash(&codes_data).as_bytes() != expected {
                return Err(VamanaError::invalid_format(
                    "v2 codes segment checksum mismatch".into(),
                ));
            }
        }

        Self::load_v2_fast(path, &lifecycle_data)
    }

    /// Load all v2 segments from `path` and restore lifecycle state from `lifecycle_data`.
    #[cfg(feature = "mmap")]
    fn load_v2_fast(path: &Path, lifecycle_data: &[u8]) -> Result<Self> {
        let meta_bytes = fs::read(path.join("metadata.bin"))?;
        let commit = parse_v2_commit(&meta_bytes)?;

        let config = VamanaConfig {
            dimensions: commit.index_meta.dimensions,
            max_degree: commit.index_meta.max_degree,
            search_list_size: commit.index_meta.search_list_size,
            alpha: commit.index_meta.alpha,
        };
        config.validate()?;

        let num_vectors = commit.index_meta.num_vectors;

        let dimensions = config.dimensions;
        let max_degree = config.max_degree;
        let mut graph = read_graph(&path.join("graph.bin"), max_degree, num_vectors)?;

        if graph.node_count() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "graph node count {} != commit num_vectors {}",
                graph.node_count(),
                num_vectors
            )));
        }

        let expected_len_f32 = num_vectors
            .checked_mul(dimensions)
            .ok_or_else(|| VamanaError::invalid_format("v2 metadata overflow".into()))?;
        let storage = mmap_vectors(&path.join("vectors.bin"), expected_len_f32)?;

        // Parse lifecycle.bin and restore state directly (no O(N*R) rebuild).
        let lifecycle = parse_lifecycle(lifecycle_data, num_vectors, max_degree)?;

        // Validate bidirectional consistency: the persisted reverse_adj must be the exact
        // inverse of the loaded forward graph. A checksum-valid but writer-bugged lifecycle
        // segment can pass parse_lifecycle's per-list shape checks (in-range, no dup, no
        // self-ref) while still being semantically wrong (phantom sources, missing entries).
        // Wolverine delete-repair relies on the invariant at graph.rs:96-98 that
        // reverse_adj[v] == { u | v ∈ adjacency[u] }; a false in-neighbor corrupts repair.
        {
            let adjacency = graph.adjacency();
            let rev = &lifecycle.reverse_adj;
            // Rebuild expected reverse_adj from the forward graph in O(N*R).
            let mut expected: Vec<Vec<u32>> = vec![Vec::new(); num_vectors];
            for (u, neighbors) in adjacency.iter().enumerate() {
                for &v in neighbors {
                    expected[v as usize].push(u as u32);
                }
            }
            // Sort both sides before comparing (persisted lists may not be sorted).
            for e in expected.iter_mut() {
                e.sort_unstable();
            }
            for (v, (exp, got)) in expected.iter().zip(rev.iter()).enumerate() {
                let mut got_sorted = got.clone();
                got_sorted.sort_unstable();
                if *exp != got_sorted {
                    return Err(VamanaError::invalid_format(format!(
                        "lifecycle.bin reverse_adj[{v}] is not the inverse of graph.bin \
                         forward adjacency: expected {exp:?}, got {got_sorted:?}"
                    )));
                }
            }
        }

        graph.restore_reverse_adj(lifecycle.reverse_adj);

        let tombstone_count = lifecycle
            .tombstones
            .iter()
            .map(|w| w.count_ones() as usize)
            .sum();

        if tombstone_count > num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin tombstone_count {tombstone_count} exceeds num_vectors {num_vectors}"
            )));
        }

        // Codes segment: mmap `codes.bin` when the commit record carries its
        // checksum (extended format); otherwise retrain from the corpus — the
        // compatibility path for segments written before the codes segment
        // existed. Retraining touches every vector page; the extended format
        // exists precisely to avoid that on the steady-state load.
        let (gs_codec, gs_codes) = match commit.codes_hash {
            Some(_) => {
                let codes_path = path.join("codes.bin");
                let file = File::open(&codes_path)?;
                let byte_len = usize::try_from(file.metadata()?.len()).map_err(|_| {
                    VamanaError::invalid_format("codes.bin file size exceeds usize".into())
                })?;
                // SAFETY: read-only mapping; callers must not mutate or
                // truncate codes.bin while this index is alive (same contract
                // as the vectors.bin mapping).
                let mmap = unsafe { MmapOptions::new().len(byte_len).map(&file)? };
                let codec = parse_codes_bin(mmap.as_ref(), dimensions, num_vectors)?;
                (
                    codec,
                    CodeStore::Mmap {
                        mmap,
                        dims: dimensions,
                        len: num_vectors,
                    },
                )
            }
            None => {
                let (codec, codes) = train_codec_and_encode(storage.as_slice()?, dimensions);
                (codec, CodeStore::Owned(codes))
            }
        };

        Ok(Self {
            vectors: storage,
            graph,
            config,
            num_vectors,
            dimensions,
            tombstones: lifecycle.tombstones,
            tombstone_count,
            ops_since_consolidation: lifecycle.ops_since_consolidation,
            free_slots: lifecycle.free_slots,
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            gs_codec,
            gs_codes,
            last_applied_seq: commit.last_applied_seq,
        })
    }

    /// Build a fresh VamanaIndex from `corpus_vectors` using the supplied `config`.
    /// Used when fingerprint mismatches, metadata is corrupt/missing, or on a clean first run.
    #[cfg(feature = "mmap")]
    fn rebuild_from_corpus(corpus_vectors: &[f32], config: VamanaConfig) -> Result<Self> {
        VamanaIndex::build(corpus_vectors, config)
    }

    /// Mean recall@k across `queries` vs. exact brute-force. Errors on empty, bad dim, or non-finite.
    pub fn recall_at_k(&self, queries: &[f32], k: usize) -> Result<f64> {
        if queries.is_empty() {
            return Err(VamanaError::EmptyInput);
        }
        if k == 0 {
            return Err(VamanaError::invalid_config(
                "k must be > 0 for recall_at_k".into(),
            ));
        }
        if !queries.len().is_multiple_of(self.dimensions) {
            return Err(VamanaError::DimensionMismatch {
                expected: self.dimensions,
                actual: queries.len() % self.dimensions,
            });
        }

        let vecs = self.vectors()?;
        let num_queries = queries.len() / self.dimensions;
        let live_count = self.num_vectors - self.tombstone_count;
        let denom = k.min(live_count) as f64;

        let tombstones = if self.tombstone_count > 0 {
            Some(self.tombstones.as_slice())
        } else {
            None
        };

        let total_recall: f64 = (0..num_queries).try_fold(0.0f64, |acc, qi| {
            let query = &queries[qi * self.dimensions..(qi + 1) * self.dimensions];
            let exact = exact_search(vecs, self.dimensions, query, k, tombstones);
            let ann = self.search(query, k)?;

            let exact_ids: std::collections::HashSet<u32> =
                exact.iter().map(|(id, _)| *id).collect();
            let ann_ids: std::collections::HashSet<u32> = ann.iter().map(|(id, _)| *id).collect();

            let overlap = exact_ids.intersection(&ann_ids).count() as f64;
            Ok::<f64, VamanaError>(acc + overlap / denom)
        })?;

        Ok(total_recall / num_queries as f64)
    }

    /// Serialise this index into a self-validating `VamanaSnapshot`.
    pub fn to_snapshot(
        &self,
        namespace: impl Into<String>,
        model: impl Into<String>,
        fingerprint: CorpusFingerprint,
        external_ids: Vec<String>,
    ) -> Result<VamanaSnapshot> {
        if external_ids.len() != self.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "external_ids length {} != num_vectors {}",
                external_ids.len(),
                self.num_vectors
            )));
        }
        let num_vectors_u64 = u64::try_from(self.num_vectors)
            .map_err(|_| VamanaError::invalid_format("num_vectors overflows u64".into()))?;
        let dimensions_u32 = u32::try_from(self.dimensions)
            .map_err(|_| VamanaError::invalid_format("dimensions overflows u32".into()))?;
        let max_degree_u32 = u32::try_from(self.config.max_degree)
            .map_err(|_| VamanaError::invalid_format("max_degree overflows u32".into()))?;
        let search_list_size_u32 = u32::try_from(self.config.search_list_size)
            .map_err(|_| VamanaError::invalid_format("search_list_size overflows u32".into()))?;
        // Cap the medoid's adjacency at max_degree before serializing.
        // The medoid-pin in insert() may transiently allow the medoid to exceed
        // max_degree (by one edge per orphan-pinned insert; K consecutive inserts
        // can accumulate K overflow edges). Capping here ensures the snapshot
        // satisfies from_snapshot()'s degree constraint.
        let medoid = self.graph.medoid();
        let max_degree_usize = self.config.max_degree;
        let adjacency: Vec<Vec<u32>> = self
            .graph
            .adjacency()
            .iter()
            .enumerate()
            .map(|(i, neighbors)| {
                if i == medoid as usize && neighbors.len() > max_degree_usize {
                    neighbors[..max_degree_usize].to_vec()
                } else {
                    neighbors.clone()
                }
            })
            .collect();
        Ok(VamanaSnapshot {
            format: VAMANA_SNAPSHOT_FORMAT.to_string(),
            version: VAMANA_SNAPSHOT_VERSION,
            namespace: namespace.into(),
            model: model.into(),
            fingerprint,
            index: VamanaIndexSnapshot {
                num_vectors: num_vectors_u64,
                dimensions: dimensions_u32,
                max_degree: max_degree_u32,
                search_list_size: search_list_size_u32,
                alpha: self.config.alpha,
                medoid,
                adjacency,
                vectors: self.vectors()?.to_vec(),
            },
            external_ids,
        })
    }

    /// Reconstruct a `VamanaIndex` from a `VamanaSnapshot`.
    pub fn from_snapshot(snapshot: &VamanaSnapshot) -> Result<Self> {
        if snapshot.format != VAMANA_SNAPSHOT_FORMAT {
            return Err(VamanaError::invalid_format(format!(
                "unsupported Vamana snapshot format: {}",
                snapshot.format
            )));
        }
        if snapshot.version != VAMANA_SNAPSHOT_VERSION {
            return Err(VamanaError::invalid_format(format!(
                "unsupported Vamana snapshot version: {}",
                snapshot.version
            )));
        }
        let ix = &snapshot.index;
        let num_vectors = usize::try_from(ix.num_vectors)
            .map_err(|_| VamanaError::invalid_format("num_vectors overflow".into()))?;
        let dimensions = usize::try_from(ix.dimensions)
            .map_err(|_| VamanaError::invalid_format("dimensions overflow".into()))?;
        let max_degree = usize::try_from(ix.max_degree)
            .map_err(|_| VamanaError::invalid_format("max_degree overflow".into()))?;
        let search_list_size = usize::try_from(ix.search_list_size)
            .map_err(|_| VamanaError::invalid_format("search_list_size overflow".into()))?;

        if snapshot.external_ids.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "external_ids length {} != num_vectors {}",
                snapshot.external_ids.len(),
                num_vectors
            )));
        }
        if ix.adjacency.len() != num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "adjacency length {} != num_vectors {}",
                ix.adjacency.len(),
                num_vectors
            )));
        }
        let expected_floats = num_vectors
            .checked_mul(dimensions)
            .ok_or_else(|| VamanaError::invalid_format("snapshot vector length overflow".into()))?;
        if ix.vectors.len() != expected_floats {
            return Err(VamanaError::invalid_format(
                "snapshot vector data length mismatch".into(),
            ));
        }
        require_finite(&ix.vectors, "snapshot vectors")?;

        let config = VamanaConfig {
            dimensions,
            max_degree,
            search_list_size,
            alpha: ix.alpha,
        };
        config.validate()?;

        let mut graph = VamanaGraph::new(num_vectors, ix.medoid)?;
        for (node, neighbors) in ix.adjacency.iter().enumerate() {
            if neighbors.len() > max_degree {
                return Err(VamanaError::invalid_format(format!(
                    "node {node} degree {} exceeds max_degree {max_degree}",
                    neighbors.len()
                )));
            }
            for &nb in neighbors {
                if nb as usize >= num_vectors {
                    return Err(VamanaError::invalid_format(format!(
                        "neighbor {nb} >= num_vectors {num_vectors}"
                    )));
                }
                if nb as usize == node {
                    return Err(VamanaError::invalid_format(format!(
                        "self-loop at node {node}"
                    )));
                }
            }
            // Reject duplicate neighbors.
            let mut sorted = neighbors.clone();
            sorted.sort_unstable();
            let before = sorted.len();
            sorted.dedup();
            if sorted.len() != before {
                return Err(VamanaError::invalid_format(format!(
                    "snapshot node {node} has duplicate neighbors"
                )));
            }
            graph.adjacency_mut_for_load()[node] = neighbors.clone();
        }

        // v1 snapshot format does not persist reverse_adj; reconstruct O(N*R) from adjacency.
        // This must run before any tombstone call — lazy init would silently skip repair.
        graph.rebuild_reverse_adj_from_adjacency();

        let (gs_codec, gs_codes) = train_codec_and_encode(&ix.vectors, dimensions);

        Ok(Self {
            vectors: VectorStorage::Owned(ix.vectors.clone()),
            graph,
            config,
            num_vectors,
            dimensions,
            tombstones: tombstone_words_for(num_vectors),
            tombstone_count: 0,
            ops_since_consolidation: 0,
            free_slots: Vec::new(),
            consolidation_tau: DEFAULT_CONSOLIDATION_TAU,
            gs_codec,
            gs_codes: CodeStore::Owned(gs_codes),
            last_applied_seq: None,
        })
    }

    /// Return a reference to the underlying Vamana graph.
    pub fn graph(&self) -> &VamanaGraph {
        &self.graph
    }

    /// Return a reference to the build configuration.
    pub fn config(&self) -> &VamanaConfig {
        &self.config
    }

    /// Return the number of indexed vectors.
    pub fn num_vectors(&self) -> usize {
        self.num_vectors
    }

    /// Return the vector dimensionality.
    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

    /// Return the flat row-major vector data as a slice.
    pub fn vectors(&self) -> Result<&[f32]> {
        self.vectors.as_slice()
    }

    /// Write-log watermark carried by the v2 commit record. `None` on indexes
    /// built in-memory or loaded from segments predating the field.
    pub fn last_applied_seq(&self) -> Option<u64> {
        self.last_applied_seq
    }

    /// Set the write-log watermark to persist with the next [`Self::save_atomic`].
    /// The caller owns the log and must pass the highest sequence whose write is
    /// reflected in this index's current state.
    pub fn set_last_applied_seq(&mut self, seq: Option<u64>) {
        self.last_applied_seq = seq;
    }

    // ---- PR2: lifecycle API (ADR-052 §2) ----

    /// True if `node_id` has been soft-deleted.
    pub fn is_tombstoned(&self, node_id: u32) -> bool {
        is_tombstoned_bit(&self.tombstones, node_id as usize)
    }

    /// Count of currently tombstoned (soft-deleted) nodes.
    pub fn tombstone_count(&self) -> usize {
        self.tombstone_count
    }

    /// Count of live (non-tombstoned) nodes.
    pub fn live_count(&self) -> usize {
        self.num_vectors - self.tombstone_count
    }

    /// Cumulative delete+insert churn since the last consolidation.
    pub fn ops_since_consolidation(&self) -> usize {
        self.ops_since_consolidation
    }

    /// True when `ops_since_consolidation >= consolidation_tau`.
    pub fn needs_consolidation(&self) -> bool {
        self.ops_since_consolidation >= self.consolidation_tau
    }

    // ---- PR3: Mmap-to-Owned promotion helper ----

    /// Promote `VectorStorage::Mmap` to `Owned` (no-op if already `Owned`); called first
    /// by both `insert` and `consolidate` so subsequent writes hit a mutable buffer.
    fn ensure_owned(&mut self) -> Result<()> {
        #[cfg(feature = "mmap")]
        if let VectorStorage::Mmap { .. } = &self.vectors {
            let owned: Vec<f32> = self.vectors.as_slice()?.to_vec();
            self.vectors = VectorStorage::Owned(owned);
        }
        self.gs_codes.ensure_owned();
        Ok(())
    }

    // ---- PR3: insert and consolidate (ADR-052 §2) ----

    /// Insert a new vector into the index. Returns the ordinal assigned to the new node.
    ///
    /// If `free_slots` is non-empty a recycled ordinal is reused; otherwise a new slot
    /// is appended. Either way, the call runs greedy search from the current medoid,
    /// selects out-edges via RobustPrune, wires back-edges with reverse_adj update, and
    /// increments `ops_since_consolidation`.
    ///
    /// Mmap-backed indexes are promoted to Owned on the first insert call. Callers that
    /// held ordinals across a previous consolidate must treat those ordinals as invalid;
    /// ordinals are NOT stable across consolidate().
    ///
    /// Returns `Err` without mutating state if the vector is non-finite, wrong dimension,
    /// or would push `num_vectors` past `u32::MAX`.
    pub fn insert(&mut self, vector: &[f32]) -> Result<u32> {
        // Preflight — validate before ANY state change (including Mmap promotion).
        if vector.len() != self.dimensions {
            return Err(VamanaError::DimensionMismatch {
                expected: self.dimensions,
                actual: vector.len(),
            });
        }
        require_finite(vector, "insert vector")?;
        if self.num_vectors >= u32::MAX as usize {
            return Err(VamanaError::TooManyVectors {
                count: self.num_vectors,
            });
        }

        // GAP-1 resolution: promote Mmap to Owned before any vector mutation.
        // Only reached when preflight passed — a rejected insert never touches Mmap.
        self.ensure_owned()?;

        // Slot assignment: recycle or append.
        let ordinal: u32;
        if !self.free_slots.is_empty() {
            // Recycle path: LIFO pop. Guard against corrupted free_slots entry.
            let candidate = *self.free_slots.last().unwrap();
            if !is_tombstoned_bit(&self.tombstones, candidate as usize) {
                return Err(VamanaError::invalid_format(format!(
                    "insert: free slot {candidate} is not tombstoned"
                )));
            }
            self.free_slots.pop();
            ordinal = candidate;

            // Clear tombstone bit and decrement count before graph wiring.
            let word = ordinal as usize / 64;
            self.tombstones[word] &= !(1u64 << (ordinal as usize % 64));
            self.tombstone_count -= 1;

            // Write vector into recycled slot in-place.
            let start = ordinal as usize * self.dimensions;
            let end = start + self.dimensions;
            match &mut self.vectors {
                VectorStorage::Owned(v) => v[start..end].copy_from_slice(vector),
                #[cfg(feature = "mmap")]
                VectorStorage::Mmap { .. } => {
                    return Err(VamanaError::invalid_format(
                        "insert: unexpected Mmap after ensure_owned".into(),
                    ))
                }
            }
            // Update SQ8 code for the recycled slot.
            let code = self.gs_codec.encode(vector);
            self.gs_codes.owned_mut()?[ordinal as usize] = code;
        } else {
            // Append path: assign next ordinal and extend storage.
            ordinal = self.num_vectors as u32;
            self.num_vectors += 1;

            // Extend graph: adjacency and reverse_adj grow atomically.
            self.graph.add_node()?;

            // Extend tombstone bitvec if the new ordinal falls in a new word.
            let word = ordinal as usize / 64;
            if word >= self.tombstones.len() {
                self.tombstones.resize(word + 1, 0);
            }

            // Append vector to Owned storage.
            match &mut self.vectors {
                VectorStorage::Owned(v) => v.extend_from_slice(vector),
                #[cfg(feature = "mmap")]
                VectorStorage::Mmap { .. } => {
                    return Err(VamanaError::invalid_format(
                        "insert: unexpected Mmap after ensure_owned".into(),
                    ))
                }
            }
            // Append SQ8 code for the new slot.
            let code = self.gs_codec.encode(vector);
            self.gs_codes.owned_mut()?.push(code);
        }

        // Graph wiring.
        let live_before = self.num_vectors - self.tombstone_count - 1; // before this insert contributed
        if live_before == 0 {
            // Only one live node (the one just inserted); skip greedy search and set medoid.
            self.graph.set_medoid(ordinal);
        } else {
            let vecs = self.vectors.as_slice()?;

            let tombstones_opt = if self.tombstone_count > 0 {
                Some(self.tombstones.as_slice())
            } else {
                None
            };

            // Insert uses exact f32 distances for graph wiring. The SQ8 codec is
            // trained on the build corpus; inserted vectors may be out of that range,
            // causing u8 clamping and wrong orderings. Exact f32 is correct here —
            // insert is not a hot path. The gs_codes entry for ordinal is already
            // written above (recycle or push) so search() uses SQ8 correctly.
            let mut visited = VisitedSet::new(self.num_vectors);
            let search_result = self.graph.greedy_search(
                vecs,
                self.dimensions,
                vector,
                self.config.search_list_size,
                self.config.search_list_size,
                &mut visited,
                tombstones_opt,
            )?;

            // Candidate pool: expanded ∪ results, excluding ordinal itself, deduped.
            let mut candidates: Vec<u32> = search_result
                .expanded
                .iter()
                .map(|(id, _)| *id)
                .chain(search_result.results.iter().map(|(id, _)| *id))
                .filter(|&id| id != ordinal)
                .collect();
            sort_dedup_u32(&mut candidates);

            let vecs = self.vectors.as_slice()?;
            let new_neighbors = robust_prune_inner(
                vecs,
                self.dimensions,
                ordinal,
                candidates,
                self.config.alpha,
                self.config.max_degree,
            );

            // Wire new node's forward adjacency and reverse_adj in lockstep.
            self.graph
                .replace_adjacency_and_update_reverse(ordinal, new_neighbors.clone());

            // INVARIANT (never-drop insert):
            // insert() never removes any existing node's inbound edge. Every node
            // reachable before this insert therefore remains reachable after it.
            // The inserted node receives at least one inbound edge from an always-
            // reachable node (a free-slot out-neighbor, or the medoid), so it is
            // reachable too. No successful insert can make a previously-findable
            // vector unfindable.
            //
            // Back-edge rule (Option E): for each selected out-neighbor j, add the
            // back-edge j→ordinal ONLY IF j has a free slot (|adj(j)| < max_degree).
            // If j is already full, SKIP the back-edge entirely — do NOT call
            // robust_prune_inner(j) and do NOT drop any of j's existing edges.
            // Pruning j's adjacency to make room is what caused earlier
            // orphan/disconnect defects.
            //
            // Trade-off: skipping back-edges on saturated neighbors lowers incremental
            // graph quality on heavily-saturated graphs (ordinal becomes less
            // well-connected via back-edges, increasing reliance on the medoid hub for
            // routing). This is a quality trade-off, not a correctness issue — recall
            // is bounded and ADR-052-acceptable. A future consolidate-side redistri-
            // bution pass (separate issue + ADR-052 amendment) can repair it.
            for &j in &new_neighbors {
                if self.graph.adjacency()[j as usize].len() < self.config.max_degree {
                    // j has a free slot: add the back-edge without dropping anything.
                    let mut j_adj: Vec<u32> = self.graph.adjacency()[j as usize]
                        .iter()
                        .copied()
                        .chain(std::iter::once(ordinal))
                        .filter(|&x| x != j)
                        .collect();
                    sort_dedup_u32(&mut j_adj);
                    self.graph.replace_adjacency_and_update_reverse(j, j_adj);
                }
                // j is full: skip the back-edge to preserve all of j's existing edges.
            }

            // Medoid-pin eager repair: if no selected out-neighbor had a free slot,
            // the inserted node has zero inbound edges and is unreachable. Pin it by
            // adding the edge medoid→ordinal. The medoid is the search entry point and
            // is always reachable; it is the designated overflow node for this edge.
            //
            // Native directory and snapshot writers cap this overflow for their
            // existing degree contract. The ADR-110 portable writer preserves it
            // losslessly so byte round trips retain reachability.
            //
            // Edge case: if the graph was empty before this insert and ordinal became
            // the medoid (live_before == 0 branch), no pin is needed — handled by the
            // `if live_before == 0` branch above this block.
            debug_assert!(
                !new_neighbors.is_empty(),
                "insert: new_neighbors must be non-empty when live_before > 0"
            );
            if self.graph.reverse_adjacency()[ordinal as usize].is_empty() {
                let medoid = self.graph.medoid();
                debug_assert_ne!(
                    medoid, ordinal,
                    "insert: medoid == ordinal in live_before>0 branch — impossible"
                );
                let mut medoid_adj: Vec<u32> = self.graph.adjacency()[medoid as usize]
                    .iter()
                    .copied()
                    .chain(std::iter::once(ordinal))
                    .filter(|&x| x != medoid)
                    .collect();
                sort_dedup_u32(&mut medoid_adj);
                // Medoid may now exceed max_degree — resolved at serialization time.
                self.graph
                    .replace_adjacency_and_update_reverse(medoid, medoid_adj);
            }
        }

        self.ops_since_consolidation += 1;
        Ok(ordinal)
    }

    /// Compact tombstoned slots: renumber live nodes to contiguous ordinals `0..M`,
    /// rebuild adjacency and `reverse_adj` over the new ordinals, and reset
    /// tombstone/free-slot state. Does NOT re-run graph construction.
    ///
    /// Returns `new_to_old` where `new_to_old[new_ordinal] == old_ordinal`, allowing
    /// callers that hold external-id maps (e.g., `AnnBridge`'s ordinal→UUID table) to
    /// remap their data. An empty `Vec` is returned on the no-op fast path (zero
    /// tombstones), signaling that ordinals are unchanged and no remap is needed.
    ///
    /// **Ordinals are NOT stable across consolidate().** Any external holder of a `u32`
    /// ordinal must rebuild its mapping using the returned `new_to_old` vector after
    /// each non-no-op consolidation. This is an invariant break visible to callers.
    ///
    /// After return: `tombstone_count == 0`, `free_slots` is empty,
    /// `ops_since_consolidation == 0`, `num_vectors == prior live_count`.
    pub fn consolidate(&mut self) -> Result<Vec<u32>> {
        // No-op fast path: no tombstones — skip Mmap promotion entirely.
        // A clean index on a Mmap-backed store stays Mmap after a no-op consolidate.
        if self.tombstone_count == 0 {
            self.ops_since_consolidation = 0;
            return Ok(Vec::new());
        }

        // GAP-1 resolution: promote Mmap to Owned before the compaction rebuild.
        // Only reached when there are tombstones to compact.
        self.ensure_owned()?;

        let m = self.num_vectors - self.tombstone_count;

        // Build old→new and new→old remap tables.
        let mut old_to_new: Vec<u32> = vec![u32::MAX; self.num_vectors];
        let mut new_to_old: Vec<u32> = Vec::with_capacity(m);
        let mut new_ord: u32 = 0;
        for (old, slot) in old_to_new.iter_mut().enumerate() {
            if !is_tombstoned_bit(&self.tombstones, old) {
                *slot = new_ord;
                new_to_old.push(old as u32);
                new_ord += 1;
            }
        }
        debug_assert_eq!(new_ord as usize, m);

        // Build compacted vector store (always Owned after consolidation).
        let old_vecs = self.vectors.as_slice()?;
        let mut new_vecs: Vec<f32> = Vec::with_capacity(m * self.dimensions);
        for &old in &new_to_old {
            let src =
                &old_vecs[old as usize * self.dimensions..(old as usize + 1) * self.dimensions];
            new_vecs.extend_from_slice(src);
        }

        // Build compacted adjacency lists with remapped ordinals.
        let mut new_adj: Vec<Vec<u32>> = vec![Vec::new(); m];
        for new_u in 0..m {
            let old_u = new_to_old[new_u] as usize;
            let remapped: Vec<u32> = self.graph.adjacency()[old_u]
                .iter()
                .filter_map(|&old_v| {
                    let nv = old_to_new[old_v as usize];
                    if nv == u32::MAX {
                        None // tombstoned target — drop
                    } else {
                        Some(nv)
                    }
                })
                .collect();
            new_adj[new_u] = remapped;
        }

        // Remap medoid.
        let old_medoid = self.graph.medoid() as usize;
        let new_medoid = old_to_new[old_medoid];
        debug_assert!(
            new_medoid != u32::MAX,
            "consolidate: medoid {old_medoid} is tombstoned — invariant violated"
        );

        // Swap in new graph state.
        let mut new_graph = VamanaGraph::new(m, new_medoid)?;
        for (i, neighbors) in new_adj.into_iter().enumerate() {
            new_graph.adjacency_mut_for_load()[i] = neighbors;
        }
        new_graph.rebuild_reverse_adj_from_adjacency();

        // Compact the SQ8 code table to match the new ordinal space.
        let codes_view = self.gs_codes.view();
        let new_gs_codes: Vec<GsEncodedVector> = new_to_old
            .iter()
            .map(|&old| GsEncodedVector {
                codes: codes_view.code(old as usize).to_vec(),
            })
            .collect();

        self.graph = new_graph;
        self.vectors = VectorStorage::Owned(new_vecs);
        self.num_vectors = m;
        self.tombstones = tombstone_words_for(m);
        self.tombstone_count = 0;
        self.free_slots.clear();
        self.ops_since_consolidation = 0;
        self.gs_codes = CodeStore::Owned(new_gs_codes);

        Ok(new_to_old)
    }

    /// Soft-delete the node at `node_id` with eager Wolverine 2-hop repair (ADR-052 §2;
    /// see crates/khive-vamana/docs/api/algorithm.md#wolverine-2-hop-repair for the rewire
    /// mechanism). If `node_id` was the medoid, a new medoid is elected (centroid-nearest
    /// live node). Returns an error without mutating any state if the op would leave zero
    /// live nodes.
    pub fn tombstone(&mut self, node_id: u32) -> Result<()> {
        let idx = node_id as usize;
        if idx >= self.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "tombstone: node_id {node_id} out of range ({} nodes)",
                self.num_vectors
            )));
        }
        if is_tombstoned_bit(&self.tombstones, idx) {
            return Err(VamanaError::invalid_format(format!(
                "tombstone: node_id {node_id} is already tombstoned"
            )));
        }
        // Preflight: reject if this would leave zero live nodes (elect_medoid would fail
        // with EmptyInput; guard here so no state is mutated on the error path).
        if self.tombstone_count + 1 >= self.num_vectors {
            return Err(VamanaError::invalid_format(format!(
                "tombstone: deleting node {node_id} would leave zero live nodes"
            )));
        }

        // Step 1: mark tombstoned, update counters.
        set_tombstone_bit(&mut self.tombstones, idx);
        self.tombstone_count += 1;
        self.ops_since_consolidation += 1;

        let vecs = self.vectors.as_slice()?;
        wolverine_repair(
            vecs,
            self.dimensions,
            &mut self.graph,
            node_id,
            &self.tombstones,
            self.config.alpha,
            self.config.max_degree,
        );

        // Step 9: if the deleted node was the medoid, re-elect.
        if self.graph.medoid() == node_id {
            let new_medoid =
                elect_medoid(vecs, self.dimensions, self.num_vectors, &self.tombstones)?;
            self.graph.set_medoid(new_medoid);
        }

        // Step 10: push to free_slots for future insert recycling (PR3).
        self.free_slots.push(node_id);

        // OQ4: after repair, the deleted node's in-neighbor set must be empty.
        debug_assert!(
            self.graph.reverse_adjacency()[idx].is_empty(),
            "tombstone: node {node_id} still has live in-neighbors post-repair"
        );

        Ok(())
    }

    /// Tombstone a batch of node ordinals, deferring medoid re-election to once per batch.
    ///
    /// Performs all structural rewires (Wolverine 2-hop repair, reverse_adj updates) for
    /// every node in `ordinals` first. Re-elects the medoid exactly once at the end, only
    /// if the current medoid is in the batch. Single-delete callers should use `tombstone()`.
    ///
    /// Returns an error without mutating any state if the batch would leave zero live nodes.
    pub fn tombstone_batch(&mut self, ordinals: &[u32]) -> Result<()> {
        if ordinals.is_empty() {
            return Ok(());
        }

        // Preflight: validate all ordinals and check the all-tombstoned case before any
        // mutation. This keeps the error path clean — no partial state on Err.
        let mut unique_live: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for &node_id in ordinals {
            let idx = node_id as usize;
            if idx >= self.num_vectors {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch: node_id {node_id} out of range ({} nodes)",
                    self.num_vectors
                )));
            }
            if is_tombstoned_bit(&self.tombstones, idx) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch: node_id {node_id} is already tombstoned"
                )));
            }
            if !unique_live.insert(node_id) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch: duplicate ordinal {node_id} in batch"
                )));
            }
        }
        let new_live = self.num_vectors - self.tombstone_count - unique_live.len();
        if new_live == 0 {
            return Err(VamanaError::invalid_format(
                "tombstone_batch: batch would leave zero live nodes".into(),
            ));
        }

        // Obtain a read-only slice over the vector store without cloning.
        // Inline the VectorStorage match rather than calling self.vectors.as_slice()
        // (a method call) so the borrow checker sees self.vectors and self.graph /
        // self.tombstones as separate fields and allows the simultaneous &mut borrows
        // inside the loop.
        let vecs: &[f32] = match &self.vectors {
            VectorStorage::Owned(v) => v.as_slice(),
            #[cfg(feature = "mmap")]
            VectorStorage::Mmap { mmap, len_f32 } => {
                let floats: &[f32] = bytemuck::try_cast_slice(mmap.as_ref())
                    .map_err(|_| VamanaError::invalid_format("vector mmap cast failed".into()))?;
                if floats.len() != *len_f32 {
                    return Err(VamanaError::invalid_format(format!(
                        "mmap f32 length {} != expected {}",
                        floats.len(),
                        len_f32
                    )));
                }
                floats
            }
        };

        let mut medoid_affected = false;

        for &node_id in ordinals {
            let idx = node_id as usize;

            // Track whether the current medoid is in this batch.
            if self.graph.medoid() == node_id {
                medoid_affected = true;
            }

            set_tombstone_bit(&mut self.tombstones, idx);
            self.tombstone_count += 1;
            self.ops_since_consolidation += 1;

            wolverine_repair(
                vecs,
                self.dimensions,
                &mut self.graph,
                node_id,
                &self.tombstones,
                self.config.alpha,
                self.config.max_degree,
            );

            self.free_slots.push(node_id);
        }

        // Single medoid re-election after all rewires — O(N*dims) once, not K times.
        if medoid_affected {
            let new_medoid =
                elect_medoid(vecs, self.dimensions, self.num_vectors, &self.tombstones)?;
            self.graph.set_medoid(new_medoid);
        }

        Ok(())
    }

    /// Tombstone a batch without Wolverine rewiring — test support only, builds the OQ1
    /// no-repair control. See crates/khive-vamana/docs/testing.md#oq1-no-repair-control.
    #[doc(hidden)]
    pub fn tombstone_batch_no_repair(&mut self, ordinals: &[u32]) -> Result<()> {
        if ordinals.is_empty() {
            return Ok(());
        }

        // Same preflight as tombstone_batch.
        let mut unique_live: std::collections::HashSet<u32> = std::collections::HashSet::new();
        for &node_id in ordinals {
            let idx = node_id as usize;
            if idx >= self.num_vectors {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch_no_repair: node_id {node_id} out of range ({} nodes)",
                    self.num_vectors
                )));
            }
            if is_tombstoned_bit(&self.tombstones, idx) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch_no_repair: node_id {node_id} is already tombstoned"
                )));
            }
            if !unique_live.insert(node_id) {
                return Err(VamanaError::invalid_format(format!(
                    "tombstone_batch_no_repair: duplicate ordinal {node_id} in batch"
                )));
            }
        }
        let new_live = self.num_vectors - self.tombstone_count - unique_live.len();
        if new_live == 0 {
            return Err(VamanaError::invalid_format(
                "tombstone_batch_no_repair: batch would leave zero live nodes".into(),
            ));
        }

        let mut medoid_affected = false;

        for &node_id in ordinals {
            let idx = node_id as usize;

            if self.graph.medoid() == node_id {
                medoid_affected = true;
            }

            set_tombstone_bit(&mut self.tombstones, idx);
            self.tombstone_count += 1;
            self.ops_since_consolidation += 1;

            // No Wolverine rewire. Just clear the deleted node's own forward adjacency so
            // there are no outgoing edges from a dead node (reverse_adj updated in lockstep).
            self.graph
                .replace_adjacency_and_update_reverse(node_id, Vec::new());

            self.free_slots.push(node_id);
        }

        if medoid_affected {
            let vecs = self.vectors.as_slice()?.to_vec();
            let new_medoid =
                elect_medoid(&vecs, self.dimensions, self.num_vectors, &self.tombstones)?;
            self.graph.set_medoid(new_medoid);
        }

        Ok(())
    }
}

// ---- PR2: tombstone bit helpers (Vec<u64> bitvec, no external crate) ----

/// Number of `u64` words needed for `n` bits.
fn tombstone_words_for(n: usize) -> Vec<u64> {
    let words = n.div_ceil(64);
    vec![0u64; words]
}

#[inline]
fn set_tombstone_bit(tombstones: &mut Vec<u64>, idx: usize) {
    let word = idx / 64;
    if word >= tombstones.len() {
        tombstones.resize(word + 1, 0);
    }
    tombstones[word] |= 1u64 << (idx % 64);
}

// ---- PR2: Wolverine 2-hop repair (ADR-052 §2 steps 3-8) ----

/// Core Wolverine repair: rewire each live in-neighbor of `deleted` to bypass it,
/// updating `reverse_adj` in lockstep. See
/// crates/khive-vamana/docs/api/algorithm.md#wolverine-2-hop-repair for the RobustPrune
/// derivation and paper references.
fn wolverine_repair(
    vectors: &[f32],
    dimensions: usize,
    graph: &mut VamanaGraph,
    deleted: u32,
    tombstones: &[u64],
    alpha: f64,
    max_degree: usize,
) {
    // Collect in-neighbors and out-neighbors before any mutation.
    let in_neighbors: Vec<u32> = graph.reverse_adjacency()[deleted as usize]
        .iter()
        .copied()
        .filter(|&p| !is_tombstoned_bit(tombstones, p as usize))
        .collect();

    let out_neighbors: Vec<u32> = graph.adjacency()[deleted as usize]
        .iter()
        .copied()
        .filter(|&v| !is_tombstoned_bit(tombstones, v as usize))
        .collect();

    for p in in_neighbors {
        // Build candidate pool: out(deleted) ∪ (adj(p) \ {deleted}), drop tombstoned.
        let mut pool: Vec<u32> = out_neighbors
            .iter()
            .copied()
            .chain(
                graph.adjacency()[p as usize]
                    .iter()
                    .copied()
                    .filter(|&v| v != deleted),
            )
            .filter(|&v| !is_tombstoned_bit(tombstones, v as usize) && v != p)
            .collect();
        sort_dedup_u32(&mut pool);

        // Use exact f32 distances for repair: the SQ8 codec is trained on the
        // build corpus and may be stale for vectors inserted after training.
        let new_neighbors = robust_prune_inner(vectors, dimensions, p, pool, alpha, max_degree);

        // Replace adjacency[p] and update reverse_adj in lockstep (PR1 invariant).
        graph.replace_adjacency_and_update_reverse(p, new_neighbors);
    }

    // Remove `deleted` from its own reverse_adj entry of every out-neighbor
    // (the deleted node's forward edges are now dead; reverse_adj must reflect this).
    for v in graph.adjacency()[deleted as usize].clone() {
        let rev = graph.adjacency_and_reverse_mut().1;
        if let Some(pos) = rev[v as usize].iter().position(|&x| x == deleted) {
            rev[v as usize].swap_remove(pos);
        }
    }

    // Clear the deleted node's own adjacency list so it has no live forward edges.
    graph.replace_adjacency_and_update_reverse(deleted, Vec::new());
}

/// Elect a new medoid: centroid of all live (non-tombstoned) vectors, nearest live node.
fn elect_medoid(
    vectors: &[f32],
    dimensions: usize,
    num_vectors: usize,
    tombstones: &[u64],
) -> Result<u32> {
    // Compute mean of live vectors.
    let mut centroid = vec![0.0f32; dimensions];
    let mut live_count = 0usize;
    for i in 0..num_vectors {
        if !is_tombstoned_bit(tombstones, i) {
            let v = &vectors[i * dimensions..(i + 1) * dimensions];
            for (c, x) in centroid.iter_mut().zip(v.iter()) {
                *c += x;
            }
            live_count += 1;
        }
    }
    if live_count == 0 {
        return Err(VamanaError::EmptyInput);
    }
    let scale = 1.0 / live_count as f32;
    for c in &mut centroid {
        *c *= scale;
    }

    // Find live node nearest the centroid.
    let mut best_id = u32::MAX;
    let mut best_dist = f32::INFINITY;
    for i in 0..num_vectors {
        if is_tombstoned_bit(tombstones, i) {
            continue;
        }
        let v = &vectors[i * dimensions..(i + 1) * dimensions];
        let d = l2_squared(&centroid, v);
        if d < best_dist || (d == best_dist && (i as u32) < best_id) {
            best_dist = d;
            best_id = i as u32;
        }
    }
    Ok(best_id)
}

fn exact_search(
    vectors: &[f32],
    dimensions: usize,
    query: &[f32],
    k: usize,
    tombstones: Option<&[u64]>,
) -> Vec<(u32, f32)> {
    let n = vectors.len() / dimensions;
    #[cfg(feature = "parallel")]
    let ids = (0..n as u32).into_par_iter();
    #[cfg(not(feature = "parallel"))]
    let ids = 0..n as u32;
    let mut dists: Vec<(u32, f32)> = ids
        .filter(|&id| {
            tombstones
                .map(|ts| !is_tombstoned_bit(ts, id as usize))
                .unwrap_or(true)
        })
        .map(|id| {
            let v = &vectors[id as usize * dimensions..(id as usize + 1) * dimensions];
            (id, l2_squared(query, v))
        })
        .collect();

    // Use select_nth_unstable_by to find the k-th element in O(N) rather than
    // full-sorting in O(N log N). Only the top-k prefix needs to be sorted.
    let effective_k = k.min(dists.len());
    if effective_k == 0 {
        return Vec::new();
    }
    if effective_k < dists.len() {
        dists.select_nth_unstable_by(effective_k - 1, |(a_id, a_d), (b_id, b_d)| {
            a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
        });
    }
    dists.truncate(effective_k);
    dists.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
        a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
    });
    dists
}

#[cfg(feature = "mmap")]
fn capped_reverse_adjacency(index: &VamanaIndex) -> Vec<Vec<u32>> {
    let adjacency = index.graph.adjacency();
    let medoid = index.graph.medoid() as usize;
    let mut reverse_adj = vec![Vec::new(); adjacency.len()];
    for (source, neighbors) in adjacency.iter().enumerate() {
        let neighbors = if source == medoid {
            &neighbors[..index.config.max_degree.min(neighbors.len())]
        } else {
            neighbors
        };
        for &target in neighbors {
            reverse_adj[target as usize].push(source as u32);
        }
    }
    reverse_adj
}

fn validate_reverse_adjacency(graph: &VamanaGraph, reverse_adj: &[Vec<u32>]) -> Result<()> {
    let mut expected = vec![Vec::new(); graph.node_count()];
    for (source, neighbors) in graph.adjacency().iter().enumerate() {
        for &target in neighbors {
            expected[target as usize].push(source as u32);
        }
    }
    for neighbors in &mut expected {
        neighbors.sort_unstable();
    }
    for (node, (expected, actual)) in expected.iter().zip(reverse_adj).enumerate() {
        let mut actual = actual.clone();
        actual.sort_unstable();
        if *expected != actual {
            return Err(VamanaError::invalid_format(format!(
                "lifecycle.bin reverse_adj[{node}] is not the inverse of graph.bin forward adjacency"
            )));
        }
    }
    Ok(())
}

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
    /// records and on containers that omit the codes segment.
    codes_hash: Option<[u8; 32]>,
}

/// Parsed lifecycle.bin content.
struct ParsedLifecycle {
    tombstones: Vec<u64>,
    free_slots: Vec<u32>,
    reverse_adj: Vec<Vec<u32>>,
    ops_since_consolidation: usize,
}

/// Write the KHVVAMG2 commit record including embedded v1 metadata fields.
#[allow(clippy::too_many_arguments)]
#[cfg(feature = "mmap")]
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
) -> Vec<u8> {
    let mut buf = Vec::with_capacity(220);
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
    buf
}

/// Parse a KHVVAMG2 commit record from bytes.
fn parse_v2_commit(data: &[u8]) -> Result<V2Commit> {
    // magic(8) + 3 hashes(96) + fp.vector_count(8) + fp.dimensions(8) + fp.content_hash(32)
    // + num_vectors(8) + dimensions(8) + max_degree(8) + search_list_size(8) + alpha(8)
    // + optional trailer: flags(1) + last_applied_seq(8) + codes_hash(32)
    let base_len = 8 + 32 + 32 + 32 + 8 + 8 + 32 + 8 + 8 + 8 + 8 + 8;
    let trailer_len = base_len + 41;
    if data.len() != base_len && data.len() != trailer_len {
        return Err(VamanaError::invalid_format(format!(
            "v2 commit record length {} != {base_len} or {trailer_len}",
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

    let (last_applied_seq, codes_hash) = if data.len() == trailer_len {
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

/// Write lifecycle.bin. See crates/khive-vamana/docs/api/persistence.md#lifecyclebin-format
/// for the full byte layout.
#[cfg(feature = "mmap")]
fn write_lifecycle(
    path: &Path,
    tombstones: &[u64],
    free_slots: &[u32],
    reverse_adj: &[Vec<u32>],
    ops_since_consolidation: usize,
) -> Result<()> {
    let buf = encode_lifecycle(tombstones, free_slots, reverse_adj, ops_since_consolidation);
    let file = File::create(path)?;
    let mut w = std::io::BufWriter::new(file);
    w.write_all(&buf)?;
    let file = w.into_inner().map_err(|e| e.into_error())?;
    file.sync_all()?;
    Ok(())
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
fn write_metadata(path: &Path, index: &VamanaIndex) -> Result<()> {
    let mut buf = Vec::with_capacity(64);
    buf.extend_from_slice(METADATA_MAGIC);
    buf.extend_from_slice(&(index.num_vectors as u64).to_le_bytes());
    buf.extend_from_slice(&(index.dimensions as u64).to_le_bytes());
    buf.extend_from_slice(&(index.config.max_degree as u64).to_le_bytes());
    buf.extend_from_slice(&(index.config.search_list_size as u64).to_le_bytes());
    buf.extend_from_slice(&index.config.alpha.to_le_bytes());
    fs::write(path, &buf)?;
    Ok(())
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
fn write_graph(path: &Path, graph: &VamanaGraph, max_degree: usize) -> Result<()> {
    let buf = encode_graph(graph, max_degree)?;
    let mut f = File::create(path)?;
    f.write_all(&buf)?;
    f.sync_all()?;
    Ok(())
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
    let data = fs::read(path)?;
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
fn write_vectors(path: &Path, vectors: &[f32]) -> Result<()> {
    let bytes: &[u8] = cast_slice(vectors);
    let mut f = File::create(path)?;
    f.write_all(bytes)?;
    f.sync_all()?;
    Ok(())
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
        mmap,
        len_f32: expected_len_f32,
    })
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

// INLINE TEST JUSTIFICATION: Tests in this module exercise private helpers
// (`exact_search`, `write_metadata`, `read_metadata`, `write_graph`, `read_graph`,
// `mmap_vectors`) and the internal `VectorStorage` enum that cannot be accessed
// from `tests/`. Moving snapshot-corruption and save/load tests here avoids
// publishing test-only re-exports. The section is larger than 300 lines because
// each persistence and snapshot variant requires independent fixture setup.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::greedy_search_inner;
    use rand::{prelude::*, SeedableRng};

    fn rand_unit_vectors(n: usize, dim: usize, seed: u64) -> Vec<f32> {
        let mut rng = rand::rngs::StdRng::seed_from_u64(seed);
        let mut raw: Vec<f32> = (0..n * dim).map(|_| rng.gen_range(-1.0f32..1.0)).collect();
        for row in raw.chunks_mut(dim) {
            let norm: f32 = row.iter().map(|x| x * x).sum::<f32>().sqrt();
            if norm > 0.0 {
                for x in row.iter_mut() {
                    *x /= norm;
                }
            }
        }
        raw
    }

    #[test]
    fn build_copies_owned_vectors() {
        let vectors = rand_unit_vectors(20, 8, 1);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(6)
            .with_search_list_size(12);
        let idx = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
        assert_eq!(idx.num_vectors(), 20);
        assert_eq!(idx.dimensions(), 8);
        assert_eq!(idx.config(), &cfg);
        assert_eq!(idx.vectors().unwrap().len(), 20 * 8);
    }

    #[test]
    fn build_rejects_dimension_mismatch() {
        let cfg = VamanaConfig::with_dimensions(4);
        let vectors = vec![0.1f32; 7]; // 7 not divisible by 4
        assert!(matches!(
            VamanaIndex::build(&vectors, cfg),
            Err(VamanaError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn search_returns_sorted_distance_pairs() {
        let vectors = rand_unit_vectors(50, 8, 2);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let query = rand_unit_vectors(1, 8, 99);
        let results = idx.search(&query, 5).unwrap();
        assert!(!results.is_empty());
        for w in results.windows(2) {
            assert!(w[0].1 <= w[1].1, "results not sorted: {:?}", results);
        }
    }

    #[test]
    fn search_rejects_query_dimension_mismatch() {
        let vectors = rand_unit_vectors(10, 8, 3);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let short_query = vec![0.5f32; 4];
        assert!(matches!(
            idx.search(&short_query, 3),
            Err(VamanaError::DimensionMismatch { .. })
        ));
    }

    #[test]
    fn search_returns_at_most_k_results() {
        let vectors = rand_unit_vectors(5, 8, 4);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let query = rand_unit_vectors(1, 8, 55);
        // Request more than corpus size
        let results = idx.search(&query, 100).unwrap();
        assert!(results.len() <= 5);
    }

    #[test]
    fn recall_at_k_rejects_empty_queries() {
        let vectors = rand_unit_vectors(10, 8, 5);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        assert!(matches!(
            idx.recall_at_k(&[], 3),
            Err(VamanaError::EmptyInput)
        ));
    }

    #[test]
    fn recall_at_k_is_one_for_exact_self_query_small_graph() {
        let vectors = rand_unit_vectors(20, 8, 6);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        // Query with the first vector itself — should find itself as nearest
        let query = vectors[..8].to_vec();
        let recall = idx.recall_at_k(&query, 1).unwrap();
        assert_eq!(recall, 1.0, "exact self-query must recall 1.0");
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn save_load_roundtrip_preserves_search_results() {
        let vectors = rand_unit_vectors(40, 8, 7);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let original = VamanaIndex::build(&vectors, cfg).unwrap();

        let dir = tempfile::tempdir().unwrap();
        original.save(dir.path()).unwrap();
        let loaded = VamanaIndex::load(dir.path()).unwrap();

        let query = rand_unit_vectors(1, 8, 123);
        let r1 = original.search(&query, 5).unwrap();
        let r2 = loaded.search(&query, 5).unwrap();
        assert_eq!(r1, r2, "save/load must preserve search results");
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_rejects_bad_metadata_magic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("metadata.bin"), b"BADMAGIC12345678").unwrap();
        assert!(matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_rejects_bad_graph_magic() {
        let vectors = rand_unit_vectors(5, 4, 8);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save(dir.path()).unwrap();

        // Overwrite graph magic
        let mut gdata = fs::read(dir.path().join("graph.bin")).unwrap();
        gdata[..8].copy_from_slice(b"BADBADBA");
        fs::write(dir.path().join("graph.bin"), &gdata).unwrap();

        assert!(matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_rejects_vector_file_wrong_length() {
        let vectors = rand_unit_vectors(5, 4, 9);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save(dir.path()).unwrap();

        // Truncate vectors.bin
        let vdata = fs::read(dir.path().join("vectors.bin")).unwrap();
        fs::write(dir.path().join("vectors.bin"), &vdata[..vdata.len() - 4]).unwrap();

        assert!(matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_rejects_neighbor_out_of_range() {
        let vectors = rand_unit_vectors(4, 4, 10);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(6);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save(dir.path()).unwrap();

        // Parse graph.bin and inject an out-of-range neighbor
        let mut gdata = fs::read(dir.path().join("graph.bin")).unwrap();
        // Find first non-zero degree node and corrupt its first neighbor
        let mut offset = 16usize;
        'outer: for _node in 0..4usize {
            let degree = u32::from_le_bytes(gdata[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            if degree > 0 {
                // Write 99 (out of range for 4 vectors) as first neighbor
                gdata[offset..offset + 4].copy_from_slice(&99u32.to_le_bytes());
                break 'outer;
            }
            offset += degree * 4;
        }
        fs::write(dir.path().join("graph.bin"), &gdata).unwrap();

        assert!(matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn loaded_vectors_are_mmap_backed_and_searchable() {
        let vectors = rand_unit_vectors(20, 8, 11);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(6)
            .with_search_list_size(12);

        let dir = tempfile::tempdir().unwrap();
        {
            let original = VamanaIndex::build(&vectors, cfg).unwrap();
            original.save(dir.path()).unwrap();
        }
        // Original index dropped; load from disk
        let loaded = VamanaIndex::load(dir.path()).unwrap();
        let query = rand_unit_vectors(1, 8, 77);
        let results = loaded.search(&query, 3).unwrap();
        assert!(!results.is_empty());
    }

    #[test]
    fn test_vamana_snapshot_roundtrip() {
        let vectors = rand_unit_vectors(8, 4, 42);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(6);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();

        let fp = CorpusFingerprint {
            vector_count: 8,
            dimensions: 4,
        };
        let ext_ids: Vec<String> = (0..8).map(|i| format!("id-{i}")).collect();
        let snapshot = idx.to_snapshot("ns", "model", fp, ext_ids.clone()).unwrap();

        assert_eq!(snapshot.format, VAMANA_SNAPSHOT_FORMAT);
        assert_eq!(snapshot.version, VAMANA_SNAPSHOT_VERSION);
        assert_eq!(snapshot.external_ids, ext_ids);
        assert_eq!(snapshot.fingerprint, fp);

        let restored = VamanaIndex::from_snapshot(&snapshot).unwrap();

        let query = rand_unit_vectors(1, 4, 99);
        let r1 = idx.search(&query, 3).unwrap();
        let r2 = restored.search(&query, 3).unwrap();
        assert_eq!(r1, r2, "snapshot roundtrip must preserve search results");
    }

    #[test]
    fn test_vamana_snapshot_rejects_bad_format() {
        let vectors = rand_unit_vectors(4, 4, 1);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(6);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let fp = CorpusFingerprint {
            vector_count: 4,
            dimensions: 4,
        };
        let ext_ids: Vec<String> = (0..4).map(|i| format!("id-{i}")).collect();
        let mut snapshot = idx.to_snapshot("ns", "model", fp, ext_ids).unwrap();

        snapshot.format = "bad-format".to_string();
        assert!(matches!(
            VamanaIndex::from_snapshot(&snapshot),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[test]
    fn test_vamana_snapshot_rejects_id_count_mismatch() {
        let vectors = rand_unit_vectors(4, 4, 2);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(6);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let fp = CorpusFingerprint {
            vector_count: 4,
            dimensions: 4,
        };
        let result = idx.to_snapshot("ns", "model", fp, vec!["only-one".into()]);
        assert!(matches!(result, Err(VamanaError::InvalidFormat { .. })));
    }

    #[test]
    fn test_vamana_stale_snapshot_rejected_by_fingerprint() {
        let vectors = rand_unit_vectors(8, 4, 42);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(6);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();

        let fp_at_build = CorpusFingerprint {
            vector_count: 8,
            dimensions: 4,
        };
        let ext_ids: Vec<String> = (0..8).map(|i| format!("id-{i}")).collect();
        let snapshot = idx
            .to_snapshot("ns", "model", fp_at_build, ext_ids)
            .unwrap();

        // Corpus change: two vectors added after the snapshot was written.
        let fp_after_change = CorpusFingerprint {
            vector_count: 10,
            dimensions: 4,
        };

        // Stale detection: fingerprints must not match.
        assert_ne!(
            snapshot.fingerprint, fp_after_change,
            "stale snapshot must be detected by fingerprint mismatch"
        );
        assert_eq!(
            snapshot.fingerprint, fp_at_build,
            "snapshot fingerprint must equal the build-time fingerprint"
        );
    }
    // ---- PR1: reverse_adj consistency via VamanaIndex ----

    /// `VamanaIndex::build` must produce a graph where every forward edge u→v is reflected
    /// in `reverse_adj[v]` and vice versa.
    #[test]
    fn index_build_reverse_adj_consistent_with_forward() {
        let vectors = rand_unit_vectors(40, 8, 0x00AD_C052);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let g = idx.graph();
        let adj = g.adjacency();
        let rev = g.reverse_adjacency();

        // Every forward edge u→v must appear in rev[v].
        for (u, neighbors) in adj.iter().enumerate() {
            for &v in neighbors {
                assert!(
                    rev[v as usize].contains(&(u as u32)),
                    "index build: forward edge {u}→{v} not in reverse_adj[{v}]"
                );
            }
        }
        // Every entry in rev[v] must be backed by a forward edge.
        for (v, in_neighbors) in rev.iter().enumerate() {
            for &u in in_neighbors {
                assert!(
                    adj[u as usize].contains(&(v as u32)),
                    "index build: reverse_adj[{v}] contains {u} \
                     but adjacency[{u}] does not contain {v}"
                );
            }
        }
    }

    /// After `VamanaIndex::load`, reverse_adj must be consistent with forward adjacency
    /// (v1 format does not persist reverse_adj; it is rebuilt at load time).
    #[cfg(feature = "mmap")]
    #[test]
    fn index_load_reverse_adj_consistent_with_forward() {
        let vectors = rand_unit_vectors(20, 4, 0x0010_AD52);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let original = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        original.save(dir.path()).unwrap();
        let loaded = VamanaIndex::load(dir.path()).unwrap();

        let g = loaded.graph();
        let adj = g.adjacency();
        let rev = g.reverse_adjacency();

        for (u, neighbors) in adj.iter().enumerate() {
            for &v in neighbors {
                assert!(
                    rev[v as usize].contains(&(u as u32)),
                    "load: forward edge {u}→{v} not in reverse_adj[{v}]"
                );
            }
        }
        for (v, in_neighbors) in rev.iter().enumerate() {
            for &u in in_neighbors {
                assert!(
                    adj[u as usize].contains(&(v as u32)),
                    "load: reverse_adj[{v}] contains {u} but adjacency[{u}] lacks {v}"
                );
            }
        }
    }

    /// After `VamanaIndex::from_snapshot`, reverse_adj must be consistent with forward adjacency.
    #[test]
    fn index_from_snapshot_reverse_adj_consistent_with_forward() {
        let vectors = rand_unit_vectors(16, 4, 0x0050_A152);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let fp = CorpusFingerprint {
            vector_count: 16,
            dimensions: 4,
        };
        let ext_ids: Vec<String> = (0..16).map(|i| format!("id-{i}")).collect();
        let snapshot = idx.to_snapshot("ns", "model", fp, ext_ids).unwrap();
        let restored = VamanaIndex::from_snapshot(&snapshot).unwrap();

        let g = restored.graph();
        let adj = g.adjacency();
        let rev = g.reverse_adjacency();

        for (u, neighbors) in adj.iter().enumerate() {
            for &v in neighbors {
                assert!(
                    rev[v as usize].contains(&(u as u32)),
                    "from_snapshot: forward edge {u}→{v} not in reverse_adj[{v}]"
                );
            }
        }
        for (v, in_neighbors) in rev.iter().enumerate() {
            for &u in in_neighbors {
                assert!(
                    adj[u as usize].contains(&(v as u32)),
                    "from_snapshot: reverse_adj[{v}] contains {u} but adjacency[{u}] lacks {v}"
                );
            }
        }
    }

    // ---- Regression tests for P0/P1 fixes ----

    /// P1: recall_at_k must reject k=0 to avoid 0/0 = NaN.
    #[test]
    fn recall_at_k_rejects_zero_k() {
        let vectors = rand_unit_vectors(10, 8, 5);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let query = rand_unit_vectors(1, 8, 77);
        assert!(
            matches!(
                idx.recall_at_k(&query, 0),
                Err(VamanaError::InvalidConfig { .. })
            ),
            "k=0 must return InvalidConfig"
        );
    }

    /// P1: load must reject duplicate neighbors in graph.bin.
    #[cfg(feature = "mmap")]
    #[test]
    fn load_rejects_duplicate_neighbors() {
        let vectors = rand_unit_vectors(5, 4, 12);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save(dir.path()).unwrap();

        // Parse graph.bin and inject a duplicate neighbor for the first node
        // that has at least 2 neighbors.
        let mut gdata = fs::read(dir.path().join("graph.bin")).unwrap();
        let mut offset = 16usize;
        'inject: for _node in 0..5usize {
            let degree = u32::from_le_bytes(gdata[offset..offset + 4].try_into().unwrap()) as usize;
            offset += 4;
            if degree >= 2 {
                // Copy first neighbor over second neighbor → duplicate.
                let first_nb = gdata[offset..offset + 4].to_vec();
                gdata[offset + 4..offset + 8].copy_from_slice(&first_nb);
                break 'inject;
            }
            offset += degree * 4;
        }
        fs::write(dir.path().join("graph.bin"), &gdata).unwrap();

        assert!(
            matches!(
                VamanaIndex::load(dir.path()),
                Err(VamanaError::InvalidFormat { .. })
            ),
            "load must reject duplicate neighbors"
        );
    }

    /// P1: load must reject graph.bin with trailing bytes.
    #[cfg(feature = "mmap")]
    #[test]
    fn load_rejects_trailing_graph_bytes() {
        let vectors = rand_unit_vectors(5, 4, 13);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save(dir.path()).unwrap();

        // Append 4 extra bytes to graph.bin.
        let mut gdata = fs::read(dir.path().join("graph.bin")).unwrap();
        gdata.extend_from_slice(&[0u8; 4]);
        fs::write(dir.path().join("graph.bin"), &gdata).unwrap();

        assert!(
            matches!(
                VamanaIndex::load(dir.path()),
                Err(VamanaError::InvalidFormat { .. })
            ),
            "load must reject trailing bytes in graph.bin"
        );
    }

    // ---- Serde-boundary NaN/Inf tests for snapshot types ----

    /// VamanaIndexSnapshot deserialization must reject non-finite vectors via TryFrom.
    /// JSON cannot encode NaN natively; the TryFrom<VamanaIndexSnapshotRaw> path is
    /// the serde boundary invoked by #[serde(try_from = "...")].
    #[test]
    fn vamana_index_snapshot_try_from_rejects_nan_vector() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 2,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1], vec![0]],
            vectors: vec![1.0, f32::NAN, 0.5, 0.5],
        };
        let result = VamanaIndexSnapshot::try_from(raw);
        assert!(
            matches!(result, Err(VamanaError::NonFiniteFloat { .. })),
            "VamanaIndexSnapshot::try_from must reject NaN in vectors"
        );
    }

    /// VamanaIndexSnapshot deserialization must reject non-finite alpha via TryFrom.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_nan_alpha() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 1,
            dimensions: 2,
            max_degree: 1,
            search_list_size: 2,
            alpha: f64::NAN,
            medoid: 0,
            adjacency: vec![vec![]],
            vectors: vec![0.5, 0.5],
        };
        let result = VamanaIndexSnapshot::try_from(raw);
        assert!(
            result.is_err(),
            "VamanaIndexSnapshot::try_from must reject NaN alpha"
        );
    }

    /// VamanaIndexSnapshot must reject alpha below 1.0 at the serde boundary.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_alpha_below_one() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 1,
            dimensions: 2,
            max_degree: 1,
            search_list_size: 2,
            alpha: 0.5,
            medoid: 0,
            adjacency: vec![vec![]],
            vectors: vec![0.5, 0.5],
        };
        let result = VamanaIndexSnapshot::try_from(raw);
        assert!(
            result.is_err(),
            "VamanaIndexSnapshot::try_from must reject alpha < 1.0"
        );
    }

    /// VamanaIndexSnapshot with valid inputs must succeed TryFrom.
    #[test]
    fn vamana_index_snapshot_try_from_accepts_valid() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 1,
            dimensions: 2,
            max_degree: 1,
            search_list_size: 2,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![]],
            vectors: vec![0.5_f32, 0.5_f32],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_ok(),
            "valid VamanaIndexSnapshot raw must be accepted"
        );
    }

    /// TryFrom must reject dimensions = 0 at the serde boundary.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_zero_dimensions() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 0,
            dimensions: 0,
            max_degree: 1,
            search_list_size: 2,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![],
            vectors: vec![],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "dimensions = 0 must be rejected"
        );
    }

    /// TryFrom must reject max_degree = 0 at the serde boundary.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_zero_max_degree() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 0,
            dimensions: 2,
            max_degree: 0,
            search_list_size: 2,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![],
            vectors: vec![],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "max_degree = 0 must be rejected"
        );
    }

    /// TryFrom must reject search_list_size < max_degree at the serde boundary.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_search_list_smaller_than_max_degree() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 0,
            dimensions: 2,
            max_degree: 8,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![],
            vectors: vec![],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "search_list_size < max_degree must be rejected"
        );
    }

    /// TryFrom must reject mismatched vectors length at the serde boundary.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_vector_count_mismatch() {
        // num_vectors=2, dimensions=2 → expect 4 floats; supply 3
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 2,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1], vec![0]],
            vectors: vec![0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "vectors.len() != num_vectors * dimensions must be rejected"
        );
    }

    /// TryFrom must reject mismatched adjacency length at the serde boundary.
    #[test]
    fn vamana_index_snapshot_try_from_rejects_adjacency_count_mismatch() {
        // num_vectors=2 → adjacency must have 2 entries; supply 1
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 2,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1]],
            vectors: vec![0.5, 0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "adjacency.len() != num_vectors must be rejected"
        );
    }

    /// VamanaSnapshot TryFrom must reject external_ids count mismatch.
    #[test]
    fn vamana_snapshot_try_from_rejects_external_ids_count_mismatch() {
        let index_raw = VamanaIndexSnapshotRaw {
            num_vectors: 2,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1], vec![0]],
            vectors: vec![0.5, 0.5, 0.5, 0.5],
        };
        let index = VamanaIndexSnapshot::try_from(index_raw).expect("valid index");
        let raw = VamanaSnapshotRaw {
            format: VAMANA_SNAPSHOT_FORMAT.into(),
            version: VAMANA_SNAPSHOT_VERSION,
            namespace: "ns".into(),
            model: "m".into(),
            fingerprint: CorpusFingerprint {
                vector_count: 2,
                dimensions: 2,
            },
            index,
            external_ids: vec!["id-0".into()], // only 1 but num_vectors = 2
        };
        assert!(
            VamanaSnapshot::try_from(raw).is_err(),
            "external_ids.len() != num_vectors must be rejected at serde boundary"
        );
    }

    /// P1: from_snapshot must reject duplicate neighbors.
    #[test]
    fn snapshot_rejects_duplicate_neighbors() {
        let vectors = rand_unit_vectors(5, 4, 14);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let fp = CorpusFingerprint {
            vector_count: 5,
            dimensions: 4,
        };
        let ext_ids: Vec<String> = (0..5).map(|i| format!("id-{i}")).collect();
        let mut snapshot = idx.to_snapshot("ns", "model", fp, ext_ids).unwrap();

        // Inject a duplicate into the first node that has at least 2 neighbors.
        for neighbors in snapshot.index.adjacency.iter_mut() {
            if neighbors.len() >= 2 {
                let dup = neighbors[0];
                neighbors[1] = dup;
                break;
            }
        }

        assert!(
            matches!(
                VamanaIndex::from_snapshot(&snapshot),
                Err(VamanaError::InvalidFormat { .. })
            ),
            "from_snapshot must reject duplicate neighbors"
        );
    }
    #[test]
    fn try_from_rejects_empty_snapshot() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 0,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![],
            vectors: vec![],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "TryFrom must reject num_vectors = 0"
        );
    }

    #[test]
    fn try_from_rejects_medoid_out_of_range() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 3,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 5, // >= num_vectors
            adjacency: vec![vec![1], vec![0], vec![]],
            vectors: vec![0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "TryFrom must reject medoid >= num_vectors"
        );
    }

    #[test]
    fn try_from_rejects_degree_exceeding_max() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 3,
            dimensions: 2,
            max_degree: 1, // max 1 neighbor
            search_list_size: 2,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1, 2], vec![0], vec![0]], // node 0 has 2 > max_degree
            vectors: vec![0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "TryFrom must reject degree > max_degree"
        );
    }

    #[test]
    fn try_from_rejects_neighbor_out_of_range() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 3,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1], vec![99], vec![0]], // neighbor 99 >= num_vectors
            vectors: vec![0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "TryFrom must reject neighbor >= num_vectors"
        );
    }

    #[test]
    fn try_from_rejects_self_loop() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 3,
            dimensions: 2,
            max_degree: 2,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![0], vec![0], vec![1]], // node 0 points to itself
            vectors: vec![0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "TryFrom must reject self-loops"
        );
    }

    #[test]
    fn try_from_rejects_duplicate_neighbors_at_serde_boundary() {
        let raw = VamanaIndexSnapshotRaw {
            num_vectors: 3,
            dimensions: 2,
            max_degree: 3,
            search_list_size: 4,
            alpha: 1.2,
            medoid: 0,
            adjacency: vec![vec![1, 1], vec![0], vec![0]], // node 0 has duplicate neighbor 1
            vectors: vec![0.5, 0.5, 0.5, 0.5, 0.5, 0.5],
        };
        assert!(
            VamanaIndexSnapshot::try_from(raw).is_err(),
            "TryFrom must reject duplicate neighbors at serde boundary"
        );
    }

    // ---- Non-finite float boundary tests (VAMANA-AUD-001) ----

    #[test]
    fn build_rejects_nan_in_vectors() {
        let mut vectors = rand_unit_vectors(10, 4, 20);
        vectors[3] = f32::NAN;
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        assert!(
            matches!(
                VamanaIndex::build(&vectors, cfg),
                Err(VamanaError::NonFiniteFloat { .. })
            ),
            "build must reject NaN in vectors"
        );
    }

    #[test]
    fn build_rejects_infinity_in_vectors() {
        let mut vectors = rand_unit_vectors(10, 4, 21);
        vectors[7] = f32::INFINITY;
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        assert!(
            matches!(
                VamanaIndex::build(&vectors, cfg),
                Err(VamanaError::NonFiniteFloat { .. })
            ),
            "build must reject Infinity in vectors"
        );
    }

    #[test]
    fn build_rejects_neg_infinity_in_vectors() {
        let mut vectors = rand_unit_vectors(10, 4, 22);
        vectors[5] = f32::NEG_INFINITY;
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        assert!(
            matches!(
                VamanaIndex::build(&vectors, cfg),
                Err(VamanaError::NonFiniteFloat { .. })
            ),
            "build must reject -Infinity in vectors"
        );
    }

    #[test]
    fn search_rejects_nan_in_query() {
        let vectors = rand_unit_vectors(10, 4, 23);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let mut query = rand_unit_vectors(1, 4, 24);
        query[1] = f32::NAN;
        assert!(
            matches!(
                idx.search(&query, 3),
                Err(VamanaError::NonFiniteFloat { .. })
            ),
            "search must reject NaN in query"
        );
    }

    #[test]
    fn search_rejects_infinity_in_query() {
        let vectors = rand_unit_vectors(10, 4, 25);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let mut query = rand_unit_vectors(1, 4, 26);
        query[0] = f32::INFINITY;
        assert!(
            matches!(
                idx.search(&query, 3),
                Err(VamanaError::NonFiniteFloat { .. })
            ),
            "search must reject Infinity in query"
        );
    }

    #[test]
    fn from_snapshot_rejects_nan_in_vectors() {
        let vectors = rand_unit_vectors(4, 4, 27);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(6);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let fp = CorpusFingerprint {
            vector_count: 4,
            dimensions: 4,
        };
        let ext_ids: Vec<String> = (0..4).map(|i| format!("id-{i}")).collect();
        let mut snapshot = idx.to_snapshot("ns", "model", fp, ext_ids).unwrap();
        snapshot.index.vectors[2] = f32::NAN;
        assert!(
            matches!(
                VamanaIndex::from_snapshot(&snapshot),
                Err(VamanaError::NonFiniteFloat { .. })
            ),
            "from_snapshot must reject NaN in vectors"
        );
    }

    /// Regression test for MEDIUM: rejected insert and no-op consolidate must NOT
    /// promote a Mmap-backed index to Owned. Verified via the private `VectorStorage`
    /// variant (only accessible inside `mod tests` due to `use super::*`).
    #[cfg(feature = "mmap")]
    #[test]
    fn mmap_atomicity_rejected_insert_and_noop_consolidate_stay_mmap() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path();

        let dim = 4usize;
        let vectors = rand_unit_vectors(8, dim, 0xABC1);
        let cfg = VamanaConfig::with_dimensions(dim)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        idx.save(path).unwrap();

        // Load: vectors are Mmap-backed.
        let mut loaded = VamanaIndex::load(path).unwrap();
        assert!(
            matches!(loaded.vectors, VectorStorage::Mmap { .. }),
            "loaded index must use Mmap-backed storage"
        );

        // Rejected insert (wrong dimension) must not promote to Owned.
        let bad = vec![0.5f32; dim + 1];
        assert!(
            loaded.insert(&bad).is_err(),
            "wrong-dim insert must return Err"
        );
        assert!(
            matches!(loaded.vectors, VectorStorage::Mmap { .. }),
            "Mmap must stay Mmap after rejected insert"
        );

        // No-op consolidate (tombstone_count == 0) must not promote to Owned.
        assert_eq!(loaded.tombstone_count(), 0);
        let remap = loaded.consolidate().unwrap();
        assert!(
            remap.is_empty(),
            "no-op consolidate must return empty remap"
        );
        assert!(
            matches!(loaded.vectors, VectorStorage::Mmap { .. }),
            "Mmap must stay Mmap after no-op consolidate"
        );
    }

    /// SQ8 recall parity gate (ADR-052 §1, Step 2).
    ///
    /// Builds one SQ8-wired index, then measures recall@10 for two search oracles
    /// on the same graph topology:
    ///   - f32 oracle  : `greedy_search_inner` (exact f32 distances throughout)
    ///   - SQ8 oracle  : `greedy_search_inner_sq8` (SQ8 acquisition + f32 re-score)
    ///
    /// Asserts: SQ8 recall >= f32 recall - 0.02 (tolerance for integer rounding).
    /// Prints actual values so the PR body can quote measured numbers.
    #[test]
    fn sq8_recall_parity_vs_f32_oracle() {
        const N: usize = 1000;
        const DIM: usize = 384;
        const K: usize = 10;
        const NUM_QUERIES: usize = 30;

        let vectors = rand_unit_vectors(N, DIM, 0xA052_0000);
        let queries = rand_unit_vectors(NUM_QUERIES, DIM, 0xA052_0001);

        let cfg = VamanaConfig::with_dimensions(DIM)
            .with_max_degree(32)
            .with_search_list_size(64);

        // Build the SQ8-wired index (ADR-052 §1 Step 2 default-on path).
        let index = VamanaIndex::build(&vectors, cfg).expect("build failed");
        let vecs = index.vectors().expect("vectors");
        let adj = index.graph.adjacency();
        let medoid = index.graph.medoid();

        let mut f32_total = 0.0f64;
        let mut sq8_total = 0.0f64;
        let live_count = N;
        let denom = K.min(live_count) as f64;

        for qi in 0..NUM_QUERIES {
            let q = &queries[qi * DIM..(qi + 1) * DIM];

            // Ground truth: exact f32 brute-force.
            let gt = exact_search(vecs, DIM, q, K, None);
            let gt_ids: std::collections::HashSet<u32> = gt.iter().map(|(id, _)| *id).collect();

            // f32 oracle: greedy search with exact f32 distances.
            let mut visited_f32 = VisitedSet::new(N);
            let f32_result = greedy_search_inner(
                vecs,
                DIM,
                adj,
                q,
                medoid,
                K,
                index.config.search_list_size,
                &mut visited_f32,
                None,
            );
            let f32_ids: std::collections::HashSet<u32> =
                f32_result.results.iter().map(|(id, _)| *id).collect();
            f32_total += f32_ids.intersection(&gt_ids).count() as f64 / denom;

            // SQ8 oracle: greedy search with SQ8 acquisition distances + f32 re-score.
            let sq8_result = index.search(q, K).expect("sq8 search failed");
            let sq8_ids: std::collections::HashSet<u32> =
                sq8_result.iter().map(|(id, _)| *id).collect();
            sq8_total += sq8_ids.intersection(&gt_ids).count() as f64 / denom;
        }

        let f32_recall = f32_total / NUM_QUERIES as f64;
        let sq8_recall = sq8_total / NUM_QUERIES as f64;
        let delta = f32_recall - sq8_recall;

        println!("sq8_recall_parity | f32_recall@10={f32_recall:.4}  sq8_recall@10={sq8_recall:.4}  delta={delta:.4}");

        assert!(
            sq8_recall >= f32_recall - 0.02,
            "SQ8 recall@10 {sq8_recall:.4} is more than 0.02 below f32 recall@10 {f32_recall:.4} (delta={delta:.4})"
        );
        assert!(
            sq8_recall >= 0.80,
            "SQ8 recall@10 {sq8_recall:.4} < 0.80 — absolute floor violated"
        );
    }

    /// OOD fallback — deterministic ranking-flip fixture (ADR-052 §2).
    ///
    /// Corpus: 10 fixed 2-D vectors in [0,1]×[0,1]. Global-scale codec: gs ≈ 0.00287
    /// (range anchored by the widest observed dim spread ~0.733). OOD query has
    /// dim 0 = -7.36 (far below min ≈ 0.28), which clamps to code 0 in dim 0.
    ///
    /// With search_list_size=1 (single-candidate frontier), SQ8-only traversal
    /// picks n1 as the nearest-1 (SQ8 codes make n1 look close via dim-1 score
    /// since the clamped dim-0 code masks the true dim-0 distances). Exact f32
    /// greedy traversal picks n6 (which is genuinely nearest at f32 dist≈58.8).
    ///
    /// index.search() gates on is_in_distribution → false → f32 fallback → n6.
    /// Removing the fallback branch makes index.search() use SQ8 → n1 → RED.
    ///
    /// Fixture verified empirically: both assertions (SQ8-only=n1, fallback=n6)
    /// hold for the built Vamana graph at the fixed random seed.
    #[test]
    fn sq8_ood_fallback_deterministic_ranking_flip() {
        use crate::graph::greedy_search_inner_sq8;

        const DIM: usize = 2;
        const N: usize = 10;

        // Fixed corpus from Python random.Random(seed=0).uniform(0,1) x (N*DIM).
        // These are NOT random in the test — they are fixed values verified to
        // produce a ranking flip between SQ8 (search_list_size=1) and exact f32.
        #[rustfmt::skip]
        let corpus: Vec<f32> = vec![
            0.844_421_85, 0.757_954_4,   // n0
            0.420_571_58, 0.258_916_75,  // n1
            0.511_274_7,  0.404_934_14,  // n2
            0.783_798_6,  0.303_312_73,  // n3
            0.476_596_95, 0.583_382,     // n4
            0.908_112_9,  0.504_686_86,  // n5
            0.281_837_84, 0.755_804_2,   // n6 — true nearest to OOD query
            0.618_369,    0.250_506_34,  // n7
            0.909_746_3,  0.982_785_48,  // n8
            0.810_217_24, 0.902_165_95,  // n9
        ];

        // OOD query: dim 0 = -7.36 (far below corpus min ≈ 0.28), dim 1 = 0.10.
        // After clamping: q_enc = [0, 0]. Corpus vector n6 encodes to [0, 176];
        // n1 encodes to [48, 3]. SQ8 dist from [0,0]: n1=(48²+9)=2313; n6=(176²)=30976.
        // SQ8 thinks n1 is much closer. Exact f32: n6 is at dist²≈58.8, n1 at ≈60.5.
        let query = vec![-7.360_714_f32, 0.100_701_2];

        // Build index with tight search_list_size to force traversal to commit early.
        // sls must be >= max_degree (VamanaConfig invariant). Use sls=4, max_degree=4.
        let cfg = VamanaConfig::with_dimensions(DIM)
            .with_max_degree(4)
            .with_search_list_size(4);
        let index = VamanaIndex::build(&corpus, cfg).expect("build failed");
        let vecs = index.vectors().expect("vectors");

        // Verify OOD gate triggers for this query.
        assert!(
            !index.gs_codec.is_in_distribution(&query),
            "query dim0={} must be below codec min≈{}; is_in_distribution must be false",
            query[0],
            index.gs_codec.min[0]
        );

        // SQ8-only path: call greedy_search_inner_sq8 directly (bypasses fallback).
        let mut visited = VisitedSet::new(N);
        let query_enc = index.gs_codec.encode(&query);
        let sq8_only = greedy_search_inner_sq8(
            vecs,
            DIM,
            index.gs_codes.view(),
            &index.gs_codec,
            index.graph.adjacency(),
            &query,
            &query_enc.codes,
            index.graph.medoid(),
            1,
            index.config.search_list_size,
            &mut visited,
            None,
        );

        // Exact brute-force ground truth.
        let gt = exact_search(vecs, DIM, &query, 1, None);
        let gt_top1 = gt[0].0;

        // index.search() with OOD fallback.
        let fallback_result = index.search(&query, 1).expect("search failed");
        let fallback_top1 = fallback_result[0].0;

        let sq8_top1 = sq8_only
            .results
            .first()
            .map(|(id, _)| *id)
            .unwrap_or(u32::MAX);

        println!(
            "sq8_ood_flip | sq8_only_top1=n{}  fallback_top1=n{}  gt_top1=n{}  \
             (expect sq8≠gt, fallback=gt)",
            sq8_top1, fallback_top1, gt_top1
        );

        // The SQ8-only path must NOT match ground truth (that's the flip this fixture proves).
        // If this fails, the corpus no longer exhibits the flip — the fixture needs updating.
        assert_ne!(
            sq8_top1, gt_top1,
            "SQ8-only path (sls=1) must miss the true nearest n{gt_top1} for this fixture \
             to be non-vacuous; got sq8=n{sq8_top1}. Fixture may need updating for this graph.",
        );

        // The fallback (f32) path must match ground truth.
        assert_eq!(
            fallback_top1, gt_top1,
            "index.search() OOD fallback must return gt_top1=n{gt_top1}, got n{fallback_top1}; \
             removing the is_in_distribution→f32 branch at search() makes this test RED"
        );
    }

    /// Equal-code collision test (ADR-052 §2).
    ///
    /// Forces two distinct f32 vectors to collide in u8 code space (low-dim corpus
    /// trained on [0, 1] quantizes differently from a corpus with range >> 1/255).
    /// Asserts that both greedy search and RobustPrune return the same neighbor
    /// ranking as the exact f32 path when SQ8 codes collide.
    #[test]
    fn sq8_equal_code_collision_correctness() {
        use crate::graph::{greedy_search_inner_sq8, robust_prune_inner, robust_prune_inner_sq8};
        use khive_quant::GsSq8Codec;

        // 1-D corpus: two vectors very close together so they quantize to the same code.
        // Codec trained on [0.0, 1.0] in 1-D: gs = 1.0/255 ≈ 0.00392.
        // v0=0.0 → code 0; v1=0.001 → code round(0.001/0.00392) = round(0.255) = 0.
        // Both map to code 0. Only exact f32 can distinguish them.
        const DIM: usize = 1;
        let vectors: Vec<f32> = vec![0.0, 0.001, 0.9];
        let codec = GsSq8Codec::train_flat(&vectors, DIM);
        let encoded: Vec<_> = (0..3)
            .map(|i| codec.encode(&vectors[i * DIM..(i + 1) * DIM]))
            .collect();

        // Verify collision: v0 and v1 should have identical codes.
        assert_eq!(
            encoded[0].codes, encoded[1].codes,
            "vectors 0 and 1 must collide in u8 code space for this test to be meaningful"
        );

        // Build a simple graph: node 0 → [1, 2], node 1 → [0, 2], node 2 → [0, 1].
        let n = 3usize;
        let adjacency: Vec<Vec<u32>> = vec![vec![1, 2], vec![0, 2], vec![0, 1]];
        let mut visited = crate::graph::VisitedSet::new(n);

        // Query = v0 (0.0). Exact nearest: v1 (d=0.001²=0.000001), then v2 (d=0.81).
        let query = vec![0.0f32; DIM];
        let query_enc = codec.encode(&query);

        // SQ8 greedy search — must tiebreak v0/v1 collision by f32 → return v1 first.
        let sq8_result = greedy_search_inner_sq8(
            &vectors,
            DIM,
            CodesView::Owned(&encoded),
            &codec,
            &adjacency,
            &query,
            &query_enc.codes,
            0, // start at node 0
            2,
            4,
            &mut visited,
            None,
        );
        let sq8_ids: Vec<u32> = sq8_result.results.iter().map(|(id, _)| *id).collect();

        // f32 greedy search — oracle.
        let mut visited_f32 = crate::graph::VisitedSet::new(n);
        let f32_result = greedy_search_inner(
            &vectors,
            DIM,
            &adjacency,
            &query,
            0,
            2,
            4,
            &mut visited_f32,
            None,
        );
        let f32_ids: Vec<u32> = f32_result.results.iter().map(|(id, _)| *id).collect();

        println!("sq8_collision | sq8_top2={sq8_ids:?}  f32_top2={f32_ids:?}");
        assert_eq!(
            sq8_ids, f32_ids,
            "SQ8 greedy search must return same top-2 as f32 oracle when codes collide"
        );

        // RobustPrune collision test: candidates [0, 1, 2] from node perspective of node 2.
        // Node 2 is at 0.9. Nearest in f32: v1 (d=(0.9-0.001)²=0.808), v0 (d=(0.9)²=0.81).
        // With alpha=1.0, all candidates should be selected (diversity check won't prune).
        let sq8_prune = robust_prune_inner_sq8(
            &vectors,
            DIM,
            CodesView::Owned(&encoded),
            &codec,
            2, // node
            vec![0, 1],
            1.0,
            2,
        );
        let f32_prune = robust_prune_inner(
            &vectors,
            DIM,
            2, // node
            vec![0, 1],
            1.0,
            2,
        );

        println!("sq8_collision prune | sq8={sq8_prune:?}  f32={f32_prune:?}");
        assert_eq!(
            sq8_prune, f32_prune,
            "RobustPrune must return same neighbors as f32 variant when codes collide"
        );
    }

    /// RobustPrune alpha-predicate regression (ADR-052 §2).
    ///
    /// Regression reproduction: when node AND multiple candidates all collapse to the same u8 code,
    /// d2_node_candidate from the SQ8 pool is 0. The strict-≤ check then reads
    /// `alpha² * dist(selected, candidate) <= 0`, which is false for any non-zero
    /// inter-selected distance — so the candidate is NOT pruned even though exact f32
    /// WOULD prune it. This test verifies the fix: use exact f32 as the predicate RHS.
    ///
    /// Fixture (exact reproduction): vectors=[0.0, 0.001, 0.0018, 1.0], DIM=1, node=0,
    /// candidates=[1,2], alpha=1.2.
    ///   - All of v0..v2 collapse to code 0 (gs=1/255, 0.001*255=0.255→0, 0.0018*255=0.459→0).
    ///   - f32 prune: selects v1, PRUNES v2 (alpha²*d(v1,v2)=0.000000922 ≤ d(v0,v2)=3.24e-6).
    ///   - SQ8 prune (broken): d2_node_candidate=0 → never prune → selects [v1, v2].
    ///   - SQ8 prune (fixed): uses exact f32 RHS → [v1] only. Matches f32 variant.
    ///
    /// VERIFIED RED when `d2_node_candidate_exact` is replaced with the old `_sq8_d2`.
    #[test]
    fn sq8_robust_prune_alpha_predicate_collision_regression() {
        use crate::graph::{robust_prune_inner, robust_prune_inner_sq8};
        use khive_quant::GsSq8Codec;

        const DIM: usize = 1;
        // v3=1.0 anchors the global scale so gs = 1.0/255.
        // v0=0.0, v1=0.001, v2=0.0018 all encode to code 0.
        let vectors: Vec<f32> = vec![0.0, 0.001, 0.0018, 1.0];
        let codec = GsSq8Codec::train_flat(&vectors, DIM);
        let encoded: Vec<_> = (0..4)
            .map(|i| codec.encode(&vectors[i * DIM..(i + 1) * DIM]))
            .collect();

        // Verify the three-way collision in code space.
        assert_eq!(
            encoded[0].codes[0], encoded[1].codes[0],
            "v0 and v1 must collide (code={}); gs={:.6}",
            encoded[0].codes[0], codec.gs
        );
        assert_eq!(
            encoded[0].codes[0], encoded[2].codes[0],
            "v0 and v2 must collide (code={}); gs={:.6}",
            encoded[0].codes[0], codec.gs
        );

        // f32 RobustPrune from node=0, candidates=[1, 2], alpha=1.2.
        let f32_result = robust_prune_inner(&vectors, DIM, 0, vec![1, 2], 1.2, 4);

        // SQ8 RobustPrune — after the predicate fix, must match f32.
        let sq8_result = robust_prune_inner_sq8(
            &vectors,
            DIM,
            CodesView::Owned(&encoded),
            &codec,
            0,
            vec![1, 2],
            1.2,
            4,
        );

        println!(
            "sq8_prune_predicate | f32={f32_result:?}  sq8={sq8_result:?}  \
             (expect both=[1], broken SQ8 would give [1,2])"
        );

        // Verify f32 selects only v1 (v2 is pruned by the alpha diversity check).
        assert_eq!(
            f32_result,
            vec![1],
            "f32 RobustPrune must prune v2 from [v1,v2]; got {f32_result:?}"
        );

        // Verify SQ8 matches (the predicate fix makes this pass; reverting breaks it).
        assert_eq!(
            sq8_result, f32_result,
            "SQ8 RobustPrune must match f32 variant; got sq8={sq8_result:?} vs f32={f32_result:?} \
             — restoring `_sq8_d2` as predicate RHS makes this test RED"
        );
    }

    // ---- read_commit_fingerprint + corpus_content_hash tests ----

    #[cfg(feature = "mmap")]
    #[test]
    fn read_commit_fingerprint_matches_save_atomic() {
        // Build a small index from normalized vectors, save it, then verify
        // that read_commit_fingerprint returns a fingerprint whose content_hash
        // matches corpus_content_hash over the same input vectors.
        let vectors = rand_unit_vectors(10, 4, 42);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save_atomic(dir.path()).unwrap();

        let fp = read_commit_fingerprint(dir.path())
            .expect("IO error")
            .expect("expected Some fingerprint after save_atomic");

        assert_eq!(fp.vector_count, 10u64, "vector_count mismatch");
        assert_eq!(fp.dimensions, 4u64, "dimensions mismatch");
        assert_eq!(
            fp.content_hash,
            corpus_content_hash(&vectors),
            "content_hash must equal corpus_content_hash over the same normalized vectors"
        );
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn read_commit_fingerprint_absent_dir_returns_none() {
        // A directory with no metadata.bin must return Ok(None), not an error.
        let dir = tempfile::tempdir().unwrap();
        let result = read_commit_fingerprint(dir.path()).expect("unexpected IO error");
        assert!(result.is_none(), "expected None for empty directory");
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn read_commit_fingerprint_v1_magic_returns_none() {
        // A metadata.bin with the v1 KHVVAMM1 magic (no v2 commit record) must
        // return Ok(None) rather than an error.
        let dir = tempfile::tempdir().unwrap();
        // Write a metadata.bin whose first 8 bytes are the v1 magic (not KHVVAMG2).
        let mut fake_meta = b"KHVVAMM1".to_vec();
        // Pad to a plausible length so any length check doesn't short-circuit first.
        fake_meta.extend_from_slice(&[0u8; 48]);
        fs::write(dir.path().join("metadata.bin"), &fake_meta).unwrap();

        let result = read_commit_fingerprint(dir.path()).expect("unexpected IO error");
        assert!(result.is_none(), "expected None for v1 magic metadata.bin");
    }

    #[test]
    fn corpus_content_hash_deterministic_and_order_sensitive() {
        let vectors = rand_unit_vectors(8, 4, 77);

        // Same input always produces the same digest.
        let h1 = corpus_content_hash(&vectors);
        let h2 = corpus_content_hash(&vectors);
        assert_eq!(h1, h2, "corpus_content_hash must be deterministic");

        // Reordering vectors (swap first and last row) must produce a different digest.
        let dim = 4;
        let mut reordered = vectors.clone();
        let n = reordered.len() / dim;
        // Swap row 0 and row n-1.
        for d in 0..dim {
            reordered.swap(d, (n - 1) * dim + d);
        }
        let h3 = corpus_content_hash(&reordered);
        assert_ne!(h1, h3, "corpus_content_hash must be order-sensitive");
    }

    #[test]
    fn portable_container_rejects_missing_duplicate_and_overlapping_segments() {
        let missing = encode_portable_container(&[
            ("metadata.bin", vec![1]),
            ("vectors.bin", vec![2]),
            ("lifecycle.bin", vec![3]),
            ("portable_ids.bin", vec![4]),
        ])
        .unwrap();
        assert!(matches!(
            VamanaIndex::from_bytes(&missing),
            Err(VamanaError::InvalidFormat { .. })
        ));

        let duplicate = encode_portable_container(&[
            ("metadata.bin", vec![1]),
            ("metadata.bin", vec![2]),
            ("graph.bin", vec![3]),
            ("lifecycle.bin", vec![4]),
        ])
        .unwrap();
        assert!(matches!(
            parse_portable_container(&duplicate),
            Err(VamanaError::InvalidFormat { .. })
        ));

        let mut overlap = encode_portable_container(&[
            ("metadata.bin", vec![1]),
            ("vectors.bin", vec![2]),
            ("graph.bin", vec![3]),
            ("lifecycle.bin", vec![4]),
        ])
        .unwrap();
        let first_offset_field = 16 + 4 + "metadata.bin".len();
        let first_payload_offset = overlap[first_offset_field..first_offset_field + 8].to_vec();
        let second_entry = first_offset_field + 8 + 8 + 32;
        let second_offset_field = second_entry + 4 + "vectors.bin".len();
        overlap[second_offset_field..second_offset_field + 8]
            .copy_from_slice(&first_payload_offset);
        assert!(matches!(
            parse_portable_container(&overlap),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[test]
    fn portable_container_rejects_overlarge_medoid_degree() {
        let config = VamanaConfig::with_dimensions(1)
            .with_max_degree(1)
            .with_search_list_size(1);
        let index = VamanaIndex::build(&[1.0], config).unwrap();
        let bytes = index.to_bytes(&[]).unwrap();
        let segments = parse_portable_container(&bytes).unwrap();
        let mut metadata = segments["metadata.bin"].to_vec();
        let mut graph = segments["graph.bin"].to_vec();

        graph[16..20].copy_from_slice(&u32::MAX.to_le_bytes());
        metadata[40..72].copy_from_slice(blake3::hash(&graph).as_bytes());
        let malformed = encode_portable_container(&[
            ("metadata.bin", metadata),
            ("vectors.bin", segments["vectors.bin"].to_vec()),
            ("graph.bin", graph),
            ("lifecycle.bin", segments["lifecycle.bin"].to_vec()),
        ])
        .unwrap();

        assert!(matches!(
            VamanaIndex::from_bytes(&malformed),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[test]
    fn portable_ids_reject_invalid_utf8() {
        let vectors = rand_unit_vectors(4, 4, 0x110);
        let config = VamanaConfig::with_dimensions(4)
            .with_max_degree(3)
            .with_search_list_size(4);
        let index = VamanaIndex::build(&vectors, config).unwrap();
        let ids: Vec<(u32, String)> = (0..4)
            .map(|ordinal| (ordinal, format!("id{ordinal}")))
            .collect();
        let mut encoded = encode_portable_ids(&index, &ids).unwrap();
        encoded[28] = 0xff;
        assert!(matches!(
            parse_portable_ids(&encoded, &index),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    // ---- VamanaIndex::load v2-aware dispatch (load_v2_raw) tests ----

    #[cfg(feature = "mmap")]
    #[test]
    fn load_reads_v2_segments_after_save_atomic() {
        // load() must detect the v2 commit magic and raw-load the segments with no
        // corpus and no rebuild, preserving search results.
        let vectors = rand_unit_vectors(40, 8, 21);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let original = VamanaIndex::build(&vectors, cfg).unwrap();

        let dir = tempfile::tempdir().unwrap();
        original.save_atomic(dir.path()).unwrap();

        let loaded = VamanaIndex::load(dir.path()).expect("load must read v2 segments");
        assert_eq!(loaded.num_vectors(), original.num_vectors());

        let query = rand_unit_vectors(1, 8, 321);
        assert_eq!(
            original.search(&query, 5).unwrap(),
            loaded.search(&query, 5).unwrap(),
            "v2 raw load must preserve search results"
        );
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_v2_matches_load_or_build_fast_path() {
        // The raw load() path and load_or_build()'s fast path must agree when the
        // corpus matches the committed segments.
        let vectors = rand_unit_vectors(30, 8, 55);
        let cfg = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let original = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        original.save_atomic(dir.path()).unwrap();

        let via_load = VamanaIndex::load(dir.path()).unwrap();
        let via_lob = VamanaIndex::load_or_build(dir.path(), &vectors, cfg).unwrap();

        let query = rand_unit_vectors(1, 8, 999);
        assert_eq!(
            via_load.search(&query, 5).unwrap(),
            via_lob.search(&query, 5).unwrap(),
            "raw load and load_or_build fast path must produce identical results"
        );
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_v2_rejects_torn_segment() {
        // A checksum mismatch on any segment must error (load never rebuilds).
        let vectors = rand_unit_vectors(20, 4, 8);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save_atomic(dir.path()).unwrap();

        // Flip one body byte of graph.bin; segment length is unchanged so only the
        // blake3 checksum gate can catch it.
        let mut gdata = fs::read(dir.path().join("graph.bin")).unwrap();
        gdata[8] ^= 0xFF;
        fs::write(dir.path().join("graph.bin"), &gdata).unwrap();

        assert!(matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

    #[cfg(feature = "mmap")]
    #[test]
    fn load_v2_rejects_missing_segment() {
        // A v2 commit whose backing segment is gone must error, not rebuild.
        let vectors = rand_unit_vectors(20, 4, 9);
        let cfg = VamanaConfig::with_dimensions(4)
            .with_max_degree(4)
            .with_search_list_size(8);
        let idx = VamanaIndex::build(&vectors, cfg).unwrap();
        let dir = tempfile::tempdir().unwrap();
        idx.save_atomic(dir.path()).unwrap();

        fs::remove_file(dir.path().join("lifecycle.bin")).unwrap();
        assert!(
            VamanaIndex::load(dir.path()).is_err(),
            "load must fail when a v2 segment is missing"
        );
    }
}
