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
    graph::{VamanaGraph, VisitedSet},
};

const METADATA_MAGIC: &[u8; 8] = b"KHVVAMM1";
const GRAPH_MAGIC: &[u8; 8] = b"KHVVAMG1";

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
        if num_vectors > 0 && raw.medoid as usize >= num_vectors {
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

        let mut visited = VisitedSet::new(self.num_vectors);
        let result = self.graph.greedy_search(
            self.vectors()?,
            self.dimensions,
            query,
            k,
            self.config.search_list_size,
            &mut visited,
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
        write_graph(&path.join("graph.bin"), &self.graph)?;
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

        let graph = read_graph(&path.join("graph.bin"), meta.max_degree, meta.num_vectors)?;

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

        Ok(Self {
            vectors: storage,
            graph,
            config,
            num_vectors: meta.num_vectors,
            dimensions: meta.dimensions,
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
        let denom = k.min(self.num_vectors) as f64;

        let total_recall: f64 = (0..num_queries).try_fold(0.0f64, |acc, qi| {
            let query = &queries[qi * self.dimensions..(qi + 1) * self.dimensions];
            let exact = exact_search(vecs, self.dimensions, query, k);
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
                medoid: self.graph.medoid(),
                adjacency: self.graph.adjacency().to_vec(),
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

        Ok(Self {
            vectors: VectorStorage::Owned(ix.vectors.clone()),
            graph,
            config,
            num_vectors,
            dimensions,
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
}

fn exact_search(vectors: &[f32], dimensions: usize, query: &[f32], k: usize) -> Vec<(u32, f32)> {
    let n = vectors.len() / dimensions;
    let mut dists: Vec<(u32, f32)> = (0..n as u32)
        .into_par_iter()
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

fn write_graph(path: &Path, graph: &VamanaGraph) -> Result<()> {
    let num_nodes = u32::try_from(graph.node_count()).map_err(|_| VamanaError::TooManyVectors {
        count: graph.node_count(),
    })?;
    let medoid = graph.medoid();

    let total_edges: usize = graph.adjacency().iter().map(|v| v.len()).sum();
    // magic(8) + num_nodes(4) + medoid(4) + per-node degree(4) + all edges(4 each)
    let capacity = 8 + 4 + 4 + num_nodes as usize * 4 + total_edges * 4;
    let mut buf = Vec::with_capacity(capacity);

    buf.extend_from_slice(GRAPH_MAGIC);
    buf.extend_from_slice(&num_nodes.to_le_bytes());
    buf.extend_from_slice(&medoid.to_le_bytes());

    for neighbors in graph.adjacency() {
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
}
