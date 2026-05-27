use std::{
    fs::{self, File},
    path::Path,
};

use bytemuck::cast_slice;
use memmap2::MmapOptions;
use rayon::prelude::*;

use crate::{
    config::VamanaConfig,
    distance::l2_squared,
    error::{Result, VamanaError},
    graph::{VamanaGraph, VisitedSet},
};

const METADATA_MAGIC: &[u8; 8] = b"KHVVAMM1";
const GRAPH_MAGIC: &[u8; 8] = b"KHVVAMG1";

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

impl VamanaIndex {
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

    pub fn save(&self, path: &Path) -> Result<()> {
        fs::create_dir_all(path)?;
        write_metadata(&path.join("metadata.bin"), self)?;
        write_graph(&path.join("graph.bin"), &self.graph)?;
        write_vectors(&path.join("vectors.bin"), self.vectors()?)?;
        Ok(())
    }

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

    pub fn recall_at_k(&self, queries: &[f32], k: usize) -> Result<f64> {
        if queries.is_empty() {
            return Err(VamanaError::EmptyInput);
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

        let total_recall: f64 = (0..num_queries)
            .map(|qi| {
                let query = &queries[qi * self.dimensions..(qi + 1) * self.dimensions];
                let exact = exact_search(vecs, self.dimensions, query, k);
                let ann = self.search(query, k).unwrap_or_default();

                let exact_ids: std::collections::HashSet<u32> =
                    exact.iter().map(|(id, _)| *id).collect();
                let ann_ids: std::collections::HashSet<u32> =
                    ann.iter().map(|(id, _)| *id).collect();

                let overlap = exact_ids.intersection(&ann_ids).count() as f64;
                overlap / denom
            })
            .sum();

        Ok(total_recall / num_queries as f64)
    }

    pub fn graph(&self) -> &VamanaGraph {
        &self.graph
    }

    pub fn config(&self) -> &VamanaConfig {
        &self.config
    }

    pub fn num_vectors(&self) -> usize {
        self.num_vectors
    }

    pub fn dimensions(&self) -> usize {
        self.dimensions
    }

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
    dists.sort_unstable_by(|(a_id, a_d), (b_id, b_d)| {
        a_d.total_cmp(b_d).then_with(|| a_id.cmp(b_id))
    });
    dists.truncate(k);
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
    let num_vectors = u64::from_le_bytes(data[8..16].try_into().unwrap()) as usize;
    let dimensions = u64::from_le_bytes(data[16..24].try_into().unwrap()) as usize;
    let max_degree = u64::from_le_bytes(data[24..32].try_into().unwrap()) as usize;
    let search_list_size = u64::from_le_bytes(data[32..40].try_into().unwrap()) as usize;
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
    let num_nodes = graph.node_count() as u32;
    let medoid = graph.medoid();

    let total_edges: usize = graph.adjacency().iter().map(|v| v.len()).sum();
    // magic(8) + num_nodes(4) + medoid(4) + per-node degree(4) + all edges(4 each)
    let capacity = 8 + 4 + 4 + num_nodes as usize * 4 + total_edges * 4;
    let mut buf = Vec::with_capacity(capacity);

    buf.extend_from_slice(GRAPH_MAGIC);
    buf.extend_from_slice(&num_nodes.to_le_bytes());
    buf.extend_from_slice(&medoid.to_le_bytes());

    for neighbors in graph.adjacency() {
        buf.extend_from_slice(&(neighbors.len() as u32).to_le_bytes());
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
        adjacency.push(neighbors);
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
    let byte_len = file.metadata()?.len() as usize;
    let expected_bytes = expected_len_f32 * std::mem::size_of::<f32>();
    if byte_len != expected_bytes {
        return Err(VamanaError::invalid_format(format!(
            "vectors.bin byte length {byte_len} != expected {expected_bytes}"
        )));
    }

    // SAFETY: The index exposes this mapping as read-only via `as_slice()`.
    // Callers must not mutate or truncate the vectors.bin file while this index is alive.
    let mmap = unsafe { MmapOptions::new().map(&file)? };

    Ok(VectorStorage::Mmap {
        mmap,
        len_f32: expected_len_f32,
    })
}

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
}

// ---- tempfile dependency shim ----
// The tempfile crate is used only in tests. Declare it as a dev-dependency below.
