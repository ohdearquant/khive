//! Vamana index: build, search, save/load, and snapshot serialization.

use std::{
    fs::{self, File},
    path::Path,
};

use bytemuck::cast_slice;
use memmap2::MmapOptions;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};

use crate::{
    config::VamanaConfig,
    distance::l2_squared,
    error::{Result, VamanaError},
    graph::{is_tombstoned_bit, robust_prune_inner, sort_dedup_u32, VamanaGraph, VisitedSet},
};

const METADATA_MAGIC: &[u8; 8] = b"KHVVAMM1";
const GRAPH_MAGIC: &[u8; 8] = b"KHVVAMG1";

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
    Mmap { mmap: memmap2::Mmap, len_f32: usize },
}

impl std::fmt::Debug for VectorStorage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Owned(v) => write!(f, "Owned(len={})", v.len()),
            Self::Mmap { len_f32, .. } => write!(f, "Mmap(len_f32={len_f32})"),
        }
    }
}

impl VectorStorage {
    fn as_slice(&self) -> Result<&[f32]> {
        match self {
            Self::Owned(v) => Ok(v.as_slice()),
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

/// An in-memory Vamana ANN index over pre-normalized vectors.
#[derive(Debug)]
pub struct VamanaIndex {
    vectors: VectorStorage,
    graph: VamanaGraph,
    config: VamanaConfig,
    num_vectors: usize,
    dimensions: usize,
    // ---- PR2: lifecycle fields (ADR-052 §2) ----
    /// Bit-packed tombstone marks. Bit `i` set ⇒ node `i` is soft-deleted.
    /// `Vec<u64>` with manual manipulation; no `bitvec` crate dependency (OQ3 resolution).
    tombstones: Vec<u64>,
    /// Count of currently tombstoned nodes.
    tombstone_count: usize,
    /// Cumulative delete+insert churn since the last consolidation.
    ops_since_consolidation: usize,
    /// Recycled ordinal slots from previous tombstone calls; consumed by insert (PR3).
    free_slots: Vec<u32>,
    /// Trigger tau for consolidation: fire when `ops_since_consolidation >= consolidation_tau`.
    /// Field on `VamanaIndex`, not `VamanaConfig` — this is operational policy, not topology (OQ5).
    consolidation_tau: usize,
}

struct IndexMetadata {
    num_vectors: usize,
    dimensions: usize,
    max_degree: usize,
    search_list_size: usize,
    alpha: f64,
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

        let graph = VamanaGraph::build(vectors, &config)?;
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
        })
    }

    /// Search for `k` nearest neighbors. Errors if dimension mismatch or non-finite query values.
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
        let result = self.graph.greedy_search(
            self.vectors()?,
            self.dimensions,
            query,
            k,
            self.config.search_list_size,
            &mut visited,
            tombstones,
        )?;

        let mut output = result.results;
        output.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
            a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
        });
        Ok(output)
    }

    /// Persist the index to `path` (a directory); writes `metadata.bin`, `graph.bin`, `vectors.bin`.
    pub fn save(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        write_metadata(&path.join("metadata.bin"), self)?;
        write_graph(&path.join("graph.bin"), &self.graph, self.config.max_degree)?;
        write_vectors(&path.join("vectors.bin"), self.vectors()?)?;
        Ok(())
    }

    /// Load an index from a directory previously written by [`VamanaIndex::save`].
    pub fn load(path: &Path) -> Result<Self> {
        let meta = read_metadata(&path.join("metadata.bin"))?;
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
        })
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
        // max_degree by 1 (see medoid-pin comment in insert()). Capping here
        // ensures the snapshot satisfies from_snapshot()'s degree constraint.
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

    /// Promote `VectorStorage::Mmap` to `Owned` by copying the mapping into a `Vec<f32>`.
    ///
    /// Called as the first statement of both `insert` and `consolidate` so that all
    /// subsequent vector reads/writes operate on a mutable owned buffer. If already
    /// `Owned`, this is a no-op. O(N × dim) once per promotion; the next `save`
    /// creates a fresh mmap at next load.
    fn ensure_owned(&mut self) -> Result<()> {
        if let VectorStorage::Mmap { .. } = &self.vectors {
            let owned: Vec<f32> = self.vectors.as_slice()?.to_vec();
            self.vectors = VectorStorage::Owned(owned);
        }
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
                VectorStorage::Mmap { .. } => {
                    return Err(VamanaError::invalid_format(
                        "insert: unexpected Mmap after ensure_owned".into(),
                    ))
                }
            }
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
                VectorStorage::Mmap { .. } => {
                    return Err(VamanaError::invalid_format(
                        "insert: unexpected Mmap after ensure_owned".into(),
                    ))
                }
            }
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
            // Pruning j's adjacency to make room is what caused the round-1 and
            // round-2 orphan/disconnect defects.
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
            // The medoid may transiently exceed max_degree by 1 due to this pin.
            // This is resolved at serialization time (save()/to_snapshot()): before
            // writing, the medoid's adjacency is capped to max_degree by truncating
            // to the first max_degree entries so the written graph satisfies all
            // loader degree constraints. See write_graph() and to_snapshot().
            //
            // If the medoid overflow edge (medoid→ordinal) is the one dropped at
            // serialization time, ordinal will not be searchable after load — but
            // it IS searchable in the live in-memory index. A subsequent consolidate()
            // rebuilds all back-edges and restores full reachability.
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

        self.graph = new_graph;
        self.vectors = VectorStorage::Owned(new_vecs);
        self.num_vectors = m;
        self.tombstones = tombstone_words_for(m);
        self.tombstone_count = 0;
        self.free_slots.clear();
        self.ops_since_consolidation = 0;

        Ok(new_to_old)
    }

    /// Soft-delete the node at `node_id` with eager Wolverine 2-hop repair (ADR-052 §2).
    ///
    /// For each live in-neighbor `p` of `node_id`, the repair rebuilds `p`'s adjacency
    /// by running RobustPrune over the union of `node_id`'s out-neighbors and `p`'s
    /// current neighbors (minus `node_id`). All tombstoned candidates are excluded.
    /// `reverse_adj` is updated in lockstep on every rewire.
    ///
    /// If `node_id` was the medoid, a new medoid is elected (centroid-nearest live node).
    ///
    /// Returns an error without mutating any state if the op would leave zero live nodes.
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

    /// Tombstone a batch without Wolverine in-neighbor rewiring. Test support only.
    ///
    /// Sets tombstone bits and clears each deleted node's own forward adjacency (updating
    /// `reverse_adj` in lockstep), but does NOT reselect in-neighbor lists via RobustPrune.
    /// The medoid is re-elected once at the end if it falls in the batch.
    ///
    /// Used by the OQ1 empirical drift test to build a genuine no-repair control: search
    /// still skips tombstoned nodes via the `Option<&[u64]>` guard in `greedy_search_inner`,
    /// but in-neighbors that previously pointed to deleted nodes are NOT rewired, so the
    /// graph retains dead-end paths that Wolverine would have bypassed.
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

/// Core Wolverine repair: rewire each live in-neighbor of `deleted` so it bypasses
/// the deleted node. Monotonic-path preservation: the new neighbor list is derived
/// from a RobustPrune over `deleted`'s out-neighbors ∪ the in-neighbor's current
/// neighbors (minus `deleted`), with all tombstoned candidates excluded.
///
/// `reverse_adj` is updated in lockstep on every rewire (the PR1 invariant).
///
/// # References
/// - Wolverine: PVLDB 18(7):2268-2280, VLDB 2025 (Liu/Zheng/Yue/Ruan/Zhou/Jensen)
/// - FreshDiskANN: SIGMOD 2022 (>95% recall at 20% deletion with eager repair)
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
    let mut dists: Vec<(u32, f32)> = (0..n as u32)
        .into_par_iter()
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

fn write_graph(path: &Path, graph: &VamanaGraph, max_degree: usize) -> Result<()> {
    let num_nodes = u32::try_from(graph.node_count()).map_err(|_| VamanaError::TooManyVectors {
        count: graph.node_count(),
    })?;
    let medoid = graph.medoid();

    // Cap the medoid's adjacency list at max_degree before serialization.
    // The medoid-pin in insert() may transiently allow the medoid to exceed
    // max_degree by 1 to ensure a freshly inserted node has an inbound edge
    // (see medoid-pin comment in insert()). We drop the overflow entry here
    // so the written graph satisfies the loader degree constraint.
    let medoid_adj_capped: Vec<u32>;
    let adjacency = graph.adjacency();
    let medoid_neighbors = &adjacency[medoid as usize];
    let medoid_capped: &[u32] = if medoid_neighbors.len() > max_degree {
        medoid_adj_capped = medoid_neighbors[..max_degree].to_vec();
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

    fs::write(path, &buf)?;
    Ok(())
}

fn read_graph(path: &Path, max_degree: usize, num_vectors: usize) -> Result<VamanaGraph> {
    let data = fs::read(path)?;
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

        if degree > max_degree {
            return Err(VamanaError::invalid_format(format!(
                "degree {degree} exceeds max_degree {max_degree}"
            )));
        }
        if offset + degree * 4 > data.len() {
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

fn write_vectors(path: &Path, vectors: &[f32]) -> Result<()> {
    let bytes: &[u8] = cast_slice(vectors);
    fs::write(path, bytes)?;
    Ok(())
}

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

// INLINE TEST JUSTIFICATION: Tests in this module exercise private helpers
// (`exact_search`, `write_metadata`, `read_metadata`, `write_graph`, `read_graph`,
// `mmap_vectors`) and the internal `VectorStorage` enum that cannot be accessed
// from `tests/`. Moving snapshot-corruption and save/load tests here avoids
// publishing test-only re-exports. The section is larger than 300 lines because
// each persistence and snapshot variant requires independent fixture setup.
#[cfg(test)]
mod tests {
    use super::*;
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

    #[test]
    fn load_rejects_bad_metadata_magic() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("metadata.bin"), b"BADMAGIC12345678").unwrap();
        assert!(matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ));
    }

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
}
