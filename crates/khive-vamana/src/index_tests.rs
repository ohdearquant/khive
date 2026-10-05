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

/// SQ8 recall@10 must stay within 0.02 of the exact-f32 oracle (ADR-052 §1 Step 2). See
/// crates/khive-vamana/docs/index.md#test-fixture-notes.
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

        let gt = exact_search(vecs, DIM, q, K, None);
        let gt_ids: std::collections::HashSet<u32> = gt.iter().map(|(id, _)| *id).collect();

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

        let sq8_result = index.search(q, K).expect("sq8 search failed");
        let sq8_ids: std::collections::HashSet<u32> =
            sq8_result.iter().map(|(id, _)| *id).collect();
        sq8_total += sq8_ids.intersection(&gt_ids).count() as f64 / denom;
    }

    let f32_recall = f32_total / NUM_QUERIES as f64;
    let sq8_recall = sq8_total / NUM_QUERIES as f64;
    let delta = f32_recall - sq8_recall;

    println!(
            "sq8_recall_parity | f32_recall@10={f32_recall:.4}  sq8_recall@10={sq8_recall:.4}  delta={delta:.4}"
        );

    assert!(
            sq8_recall >= f32_recall - 0.02,
            "SQ8 recall@10 {sq8_recall:.4} is more than 0.02 below f32 recall@10 {f32_recall:.4} (delta={delta:.4})"
        );
    assert!(
        sq8_recall >= 0.80,
        "SQ8 recall@10 {sq8_recall:.4} < 0.80 — absolute floor violated"
    );
}

/// An OOD query must take the f32 fallback in `search()` rather than the SQ8 path,
/// which would return a different (wrong) nearest neighbor for this fixture
/// (ADR-052 §2). See crates/khive-vamana/docs/index.md#test-fixture-notes.
#[test]
fn sq8_ood_fallback_deterministic_ranking_flip() {
    use crate::graph::greedy_search_inner_sq8;

    const DIM: usize = 2;
    const N: usize = 10;

    // Fixed corpus (Python random.Random(seed=0)) verified to produce a ranking
    // flip between SQ8 (search_list_size=1) and exact f32; see docs/index.md.
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

    // OOD query (dim 0 far below corpus min): clamped SQ8 codes rank n1 closest;
    // exact f32 correctly ranks n6 closest. See docs/index.md#test-fixture-notes.
    let query = vec![-7.360_714_f32, 0.100_701_2];

    let cfg = VamanaConfig::with_dimensions(DIM)
        .with_max_degree(4)
        .with_search_list_size(4);
    let index = VamanaIndex::build(&corpus, cfg).expect("build failed");
    let vecs = index.vectors().expect("vectors");

    assert!(
        !index.gs_codec.is_in_distribution(&query),
        "query dim0={} must be below codec min≈{}; is_in_distribution must be false",
        query[0],
        index.gs_codec.min[0]
    );

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

    let gt = exact_search(vecs, DIM, &query, 1, None);
    let gt_top1 = gt[0].0;

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

    // Non-vacuous check: the fixture must still exhibit the flip.
    assert_ne!(
        sq8_top1, gt_top1,
        "SQ8-only path (sls=1) must miss the true nearest n{gt_top1} for this fixture \
             to be non-vacuous; got sq8=n{sq8_top1}. Fixture may need updating for this graph.",
    );

    assert_eq!(
        fallback_top1, gt_top1,
        "index.search() OOD fallback must return gt_top1=n{gt_top1}, got n{fallback_top1}; \
             removing the is_in_distribution→f32 branch at search() makes this test RED"
    );

    let allocating_top1 = index.search_allocating(&query, 1).expect("search failed")[0].0;
    assert_eq!(
        allocating_top1, gt_top1,
        "search_allocating() OOD fallback must return gt_top1=n{gt_top1}, got n{allocating_top1}"
    );
}

/// When two vectors collide to the same SQ8 code, greedy search and RobustPrune must
/// still rank/select them identically to the exact-f32 path (ADR-052 §2). See
/// crates/khive-vamana/docs/index.md#test-fixture-notes.
#[test]
fn sq8_equal_code_collision_correctness() {
    use crate::graph::{greedy_search_inner_sq8, robust_prune_inner, robust_prune_inner_sq8};
    use khive_quant::GsSq8Codec;

    const DIM: usize = 1;
    let vectors: Vec<f32> = vec![0.0, 0.001, 0.9];
    let codec = GsSq8Codec::train_flat(&vectors, DIM);
    let encoded: Vec<_> = (0..3)
        .map(|i| codec.encode(&vectors[i * DIM..(i + 1) * DIM]))
        .collect();

    assert_eq!(
        encoded[0].codes, encoded[1].codes,
        "vectors 0 and 1 must collide in u8 code space for this test to be meaningful"
    );

    let n = 3usize;
    let adjacency: Vec<Vec<u32>> = vec![vec![1, 2], vec![0, 2], vec![0, 1]];
    let mut visited = crate::graph::VisitedSet::new(n);

    let query = vec![0.0f32; DIM];
    let query_enc = codec.encode(&query);

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

/// SQ8 RobustPrune's alpha predicate must use the exact-f32 RHS, not the SQ8-pool
/// distance, when node and candidates collide to the same code — the SQ8 distance is
/// 0 there so the strict-≤ check would never prune (ADR-052 §2). See
/// crates/khive-vamana/docs/index.md#test-fixture-notes.
#[test]
fn sq8_robust_prune_alpha_predicate_collision_regression() {
    use crate::graph::{robust_prune_inner, robust_prune_inner_sq8};
    use khive_quant::GsSq8Codec;

    const DIM: usize = 1;
    let vectors: Vec<f32> = vec![0.0, 0.001, 0.0018, 1.0];
    let codec = GsSq8Codec::train_flat(&vectors, DIM);
    let encoded: Vec<_> = (0..4)
        .map(|i| codec.encode(&vectors[i * DIM..(i + 1) * DIM]))
        .collect();

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

    let f32_result = robust_prune_inner(&vectors, DIM, 0, vec![1, 2], 1.2, 4);

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

    assert_eq!(
        f32_result,
        vec![1],
        "f32 RobustPrune must prune v2 from [v1,v2]; got {f32_result:?}"
    );

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

/// A checkpoint publication is an event, even when its semantic payload is
/// byte-for-byte identical to the preceding checkpoint. Long-lived mmap
/// readers need an identity that changes on every rotation so they can
/// release the now-unlinked predecessor files (#2081).
#[cfg(feature = "mmap")]
#[test]
fn identical_checkpoint_rotations_have_distinct_commit_identities() {
    let vectors = rand_unit_vectors(10, 4, 2081);
    let cfg = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();

    idx.save_atomic(dir.path()).unwrap();
    let first = blake3::hash(&fs::read(dir.path().join("metadata.bin")).unwrap());

    idx.save_atomic(dir.path()).unwrap();
    let second = blake3::hash(&fs::read(dir.path().join("metadata.bin")).unwrap());

    assert_ne!(
        first, second,
        "every successful checkpoint rotation needs a distinct commit identity"
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
    overlap[second_offset_field..second_offset_field + 8].copy_from_slice(&first_payload_offset);
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

#[cfg(all(feature = "mmap", unix))]
#[test]
fn checkpoint_rejects_planted_links_at_lock_and_every_staging_name() {
    use std::os::unix::fs::symlink;

    let vectors = rand_unit_vectors(8, 4, 0x3282);
    let config = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let index = VamanaIndex::build(&vectors, config).unwrap();

    for name in [
        ".checkpoint.lock",
        "vectors.bin.v2new",
        "graph.bin.v2new",
        "lifecycle.bin.v2new",
        "codes.bin.v2new",
        "metadata.bin.tmp",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::write(&sentinel, b"outside bytes must remain intact").unwrap();
        symlink(&sentinel, dir.path().join(name)).unwrap();

        assert!(
            index.save_atomic(dir.path()).is_err(),
            "planted symlink at {name} must stop checkpoint publication"
        );
        assert_eq!(
            fs::read(&sentinel).unwrap(),
            b"outside bytes must remain intact",
            "checkpoint staging must never write through {name}"
        );
    }

    let parent = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let linked_directory = parent.path().join("linked-index");
    symlink(outside.path(), &linked_directory).unwrap();
    assert!(index.save_atomic(&linked_directory).is_err());
    assert!(!outside.path().join("metadata.bin").exists());

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("vectors.bin.v2new"), b"stale regular file").unwrap();
    index
        .save_atomic(dir.path())
        .expect("regular stale staging files remain replaceable");
    assert_eq!(
        VamanaIndex::load(dir.path()).unwrap().num_vectors(),
        index.num_vectors()
    );
}

#[cfg(feature = "mmap")]
#[test]
fn overlapping_checkpoint_writers_linearize_sequence_validation() {
    let vectors = rand_unit_vectors(30, 8, 0x1138_0200);
    let stale_vectors = rand_unit_vectors(20, 8, 0x1138_0100);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut newer = VamanaIndex::build(&vectors, config.clone()).unwrap();
    let mut stale = VamanaIndex::build(&stale_vectors, config).unwrap();
    newer.set_last_applied_seq(Some(200));
    stale.set_last_applied_seq(Some(100));

    let dir = tempfile::tempdir().unwrap();
    let newer_path = dir.path().to_path_buf();
    let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let newer_handle = std::thread::spawn(move || {
        newer.save_atomic_with_lock_hook(&newer_path, |lock| {
            lock.lock()?;
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    locked_rx.recv().unwrap();

    // `stale`'s hook probes `try_lock()` while `newer` holds the lock, so a `WouldBlock`
    // here proves genuine contention rather than a timing guess. See
    // crates/khive-vamana/docs/index.md#concurrency-test-harness.
    enum ProbeOutcome {
        Contended,
        Uncontended,
        ProbeFailed(String),
    }
    let stale_path = dir.path().to_path_buf();
    let (contended_tx, contended_rx) = std::sync::mpsc::sync_channel(0);
    let (result_tx, result_rx) = std::sync::mpsc::sync_channel(0);
    let stale_handle = std::thread::spawn(move || {
        let result = stale.save_atomic_with_lock_hook(&stale_path, |lock| match lock.try_lock() {
            Ok(()) => {
                contended_tx.send(ProbeOutcome::Uncontended).unwrap();
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                contended_tx.send(ProbeOutcome::Contended).unwrap();
                lock.lock()?;
                Ok(())
            }
            Err(std::fs::TryLockError::Error(err)) => {
                contended_tx
                    .send(ProbeOutcome::ProbeFailed(err.to_string()))
                    .unwrap();
                Err(err.into())
            }
        });
        result_tx.send(result).unwrap();
    });
    // Timeout guards only against the hook never running (e.g. a pre-probe panic).
    match contended_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("contention probe never signaled within 60s")
    {
        ProbeOutcome::Contended => {}
        ProbeOutcome::Uncontended => {
            panic!("lock probe observed no contention; newer writer should have held the lock")
        }
        ProbeOutcome::ProbeFailed(err) => panic!("lock probe failed: {err}"),
    }

    release_tx.send(()).unwrap();
    newer_handle.join().unwrap().unwrap();
    let stale_result = result_rx
        .recv_timeout(std::time::Duration::from_secs(10))
        .unwrap();
    assert!(matches!(
        stale_result,
        Err(VamanaError::CheckpointSequenceRegression {
            candidate: Some(100),
            incumbent: 200,
        })
    ));
    stale_handle.join().unwrap();

    let persisted = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(persisted.last_applied_seq(), Some(200));
    assert_eq!(persisted.vectors().unwrap(), vectors);
}

/// The build pool bound: half the machine, at least one, and an override that
/// cannot break a build by being wrong.
#[cfg(feature = "parallel")]
#[test]
fn build_threads_default_to_half_the_machine_and_survive_a_bad_override() {
    assert_eq!(resolve_build_threads(None, 10), 5);
    // Odd counts round up: on a 1-core machine the answer must still be 1.
    assert_eq!(resolve_build_threads(None, 9), 5);
    assert_eq!(resolve_build_threads(None, 1), 1);
    assert_eq!(resolve_build_threads(None, 0), 1);
    assert_eq!(resolve_build_threads(Some(" 3 "), 10), 3);
    // A build is not gated on the knob parsing: zero, negative and garbage
    // all fall back to the default rather than to zero threads or an error.
    assert_eq!(resolve_build_threads(Some("0"), 10), 5);
    assert_eq!(resolve_build_threads(Some("-2"), 10), 5);
    assert_eq!(resolve_build_threads(Some("half"), 10), 5);
    assert_eq!(resolve_build_threads(Some(""), 10), 5);
}

/// The pool builds are installed into is actually bounded to that number, so
/// every nested `par_iter` in graph construction inherits the bound rather
/// than the global one-thread-per-core pool.
#[cfg(feature = "parallel")]
#[test]
fn the_build_pool_is_bounded_to_the_resolved_thread_count() {
    let pool = build_pool().expect("build pool");
    assert_eq!(pool.current_num_threads(), build_thread_count());
    assert!(
        pool.current_num_threads()
            <= std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1),
        "the build pool must never exceed the machine"
    );
    // Observed from inside the pool, which is what a build sees.
    let observed = pool.install(rayon::current_num_threads);
    assert_eq!(observed, build_thread_count());
}

/// A reader must not be admitted into `save_atomic`'s critical section, because
/// every rename that publishes a checkpoint happens inside it: `metadata.bin` as the
/// commit record first, then the four segment files. A reader admitted between those
/// renames reads a new commit record against stale segments — the state seen in
/// production as `lifecycle.bin rev_num_nodes N != num_vectors M` and `v2 codes
/// segment checksum mismatch` — and every caller's recovery from it is a rebuild,
/// which publishes, which tears the next reader.
///
/// The probe uses `try_lock_shared` while the writer holds the lock, so `WouldBlock`
/// is evidence of real exclusion rather than a timing guess.
#[cfg(feature = "mmap")]
#[test]
fn a_reader_cannot_enter_the_publication_critical_section() {
    let vectors = rand_unit_vectors(30, 8, 0x1138_0300);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let mut index = VamanaIndex::build(&vectors, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    index.set_last_applied_seq(Some(1));
    index.save_atomic(dir.path()).unwrap();
    index.set_last_applied_seq(Some(2));

    let writer_path = dir.path().to_path_buf();
    let (locked_tx, locked_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let writer_handle = std::thread::spawn(move || {
        index.save_atomic_with_lock_hook(&writer_path, |lock| {
            lock.lock()?;
            locked_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    locked_rx.recv().unwrap();

    enum ProbeOutcome {
        Contended,
        Uncontended,
        ProbeFailed(String),
    }
    let reader_path = dir.path().to_path_buf();
    let (probe_tx, probe_rx) = std::sync::mpsc::sync_channel(0);
    let reader_handle = std::thread::spawn(move || {
        VamanaIndex::load_with_lock_hook(&reader_path, |lock| match lock.try_lock_shared() {
            Ok(()) => {
                probe_tx.send(ProbeOutcome::Uncontended).unwrap();
                Ok(())
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                probe_tx.send(ProbeOutcome::Contended).unwrap();
                lock.lock_shared()?;
                Ok(())
            }
            Err(std::fs::TryLockError::Error(err)) => {
                probe_tx
                    .send(ProbeOutcome::ProbeFailed(err.to_string()))
                    .unwrap();
                Err(err.into())
            }
        })
    });
    match probe_rx
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("reader probe never signaled within 60s")
    {
        ProbeOutcome::Contended => {}
        ProbeOutcome::Uncontended => {
            panic!("a reader entered the publication critical section")
        }
        ProbeOutcome::ProbeFailed(err) => panic!("reader lock probe failed: {err}"),
    }

    release_tx.send(()).unwrap();
    writer_handle.join().unwrap().unwrap();
    // The reader was delayed, not failed: it completes once publication ends, and
    // what it gets is the finished checkpoint rather than a torn one.
    let loaded = reader_handle.join().unwrap().unwrap();
    assert_eq!(loaded.last_applied_seq(), Some(2));
    assert_eq!(loaded.vectors().unwrap(), vectors);
}

/// The reader lock is shared, so it excludes publication and nothing else. Without
/// this the fix would trade a rebuild storm for a read convoy across every process
/// on the index root.
#[cfg(feature = "mmap")]
#[test]
fn concurrent_readers_do_not_exclude_each_other() {
    let vectors = rand_unit_vectors(24, 8, 0x1138_0400);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let index = VamanaIndex::build(&vectors, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    index.save_atomic(dir.path()).unwrap();

    let first_path = dir.path().to_path_buf();
    let (holding_tx, holding_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let first = std::thread::spawn(move || {
        VamanaIndex::load_with_lock_hook(&first_path, |lock| {
            lock.lock_shared()?;
            holding_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        })
    });
    holding_rx.recv().unwrap();

    let second_path = dir.path().to_path_buf();
    let (probe_tx, probe_rx) = std::sync::mpsc::sync_channel(0);
    let second = std::thread::spawn(move || {
        VamanaIndex::load_with_lock_hook(&second_path, |lock| {
            let acquired = lock.try_lock_shared().is_ok();
            probe_tx.send(acquired).unwrap();
            if acquired {
                Ok(())
            } else {
                lock.lock_shared().map_err(Into::into)
            }
        })
    });
    assert!(
        probe_rx
            .recv_timeout(std::time::Duration::from_secs(60))
            .expect("second reader never probed within 60s"),
        "a shared publication lock must not exclude another reader"
    );

    release_tx.send(()).unwrap();
    assert_eq!(first.join().unwrap().unwrap().vectors().unwrap(), vectors);
    assert_eq!(second.join().unwrap().unwrap().vectors().unwrap(), vectors);
}

/// A historical directory without `.checkpoint.lock` remains loadable, and
/// its first reader creates the publication lock before opening segments.
#[cfg(feature = "mmap")]
#[test]
fn load_succeeds_when_no_publication_lock_file_exists() {
    let vectors = rand_unit_vectors(16, 8, 0x1138_0500);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let index = VamanaIndex::build(&vectors, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    index.save_atomic(dir.path()).unwrap();

    let lock_path = dir.path().join(".checkpoint.lock");
    assert!(lock_path.exists(), "save_atomic must create the lock file");
    fs::remove_file(&lock_path).unwrap();

    let loaded = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(loaded.vectors().unwrap(), vectors);
    assert!(lock_path.exists(), "first load must join the lock protocol");
}

#[cfg(feature = "mmap")]
#[test]
fn load_or_build_creates_directory_before_first_lock() {
    let vectors = rand_unit_vectors(16, 8, 0x1138_0509);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("new-index");
    assert!(!path.exists());

    let loaded = VamanaIndex::load_or_build(&path, &vectors, config).unwrap();
    assert_eq!(loaded.vectors().unwrap(), vectors);
    assert!(path.join(".checkpoint.lock").exists());
}

/// The first load of a legacy v1 directory must exclude the first publisher,
/// just like a load of a directory whose lock file already exists. The hook
/// holds the reader before any segment read; the writer probes the actual
/// file lock, so the control does not depend on thread scheduling.
#[cfg(feature = "mmap")]
#[test]
fn first_legacy_load_excludes_first_publication() {
    for lock_already_present in [false, true] {
        let old_vectors = rand_unit_vectors(16, 8, 0x1138_0510);
        let new_vectors = rand_unit_vectors(16, 8, 0x1138_0511);
        let config = VamanaConfig::with_dimensions(8)
            .with_max_degree(8)
            .with_search_list_size(16);
        let old = VamanaIndex::build(&old_vectors, config.clone()).unwrap();
        let mut new = VamanaIndex::build(&new_vectors, config).unwrap();
        new.set_last_applied_seq(Some(2));
        let dir = tempfile::tempdir().unwrap();
        old.save(dir.path()).unwrap();
        let lock_path = dir.path().join(".checkpoint.lock");
        if !lock_already_present {
            fs::remove_file(&lock_path).unwrap();
        }

        let reader_path = dir.path().to_path_buf();
        let (reader_locked_tx, reader_locked_rx) = std::sync::mpsc::sync_channel(0);
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
        let reader = std::thread::spawn(move || {
            VamanaIndex::load_with_lock_hook(&reader_path, |lock| {
                lock.lock_shared()?;
                reader_locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
        });
        reader_locked_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("first reader never acquired a publication lock");
        assert!(lock_path.exists());

        let writer_path = dir.path().to_path_buf();
        let (probe_tx, probe_rx) = std::sync::mpsc::sync_channel(0);
        let writer = std::thread::spawn(move || {
            new.save_atomic_with_lock_hook(&writer_path, |lock| match lock.try_lock() {
                Ok(()) => {
                    probe_tx.send(false).unwrap();
                    Ok(())
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    probe_tx.send(true).unwrap();
                    lock.lock().map_err(Into::into)
                }
                Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
            })
        });
        let contended = probe_rx
            .recv_timeout(std::time::Duration::from_secs(10))
            .expect("first publisher never probed the publication lock");
        release_tx.send(()).unwrap();
        let loaded = reader.join().unwrap().unwrap();
        writer.join().unwrap().unwrap();
        assert!(contended, "first publisher must wait for the first reader");
        assert_eq!(loaded.vectors().unwrap(), old_vectors);
        assert_eq!(
            VamanaIndex::load(dir.path()).unwrap().vectors().unwrap(),
            new_vectors
        );
    }
}

#[cfg(feature = "mmap")]
#[test]
fn read_only_legacy_load_preserves_missing_lock() {
    let vectors = rand_unit_vectors(16, 8, 0x1138_0520);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let index = VamanaIndex::build(&vectors, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    index.save(dir.path()).unwrap();
    let lock_path = dir.path().join(".checkpoint.lock");
    fs::remove_file(&lock_path).unwrap();

    let loaded = VamanaIndex::load_with_lock_hooks(
        dir.path(),
        |lock| lock.lock_shared().map_err(Into::into),
        |_| Err(std::io::ErrorKind::PermissionDenied.into()),
        || {},
    )
    .unwrap();
    assert_eq!(loaded.vectors().unwrap(), vectors);
    assert!(
        !lock_path.exists(),
        "read-only fallback must not create a lock"
    );
}

#[cfg(all(feature = "mmap", windows))]
#[test]
fn windows_unlocked_load_identity_uses_stable_handle_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("identity.bin");
    fs::write(&path, b"first").unwrap();
    let first = publication_file_identity(&path, &fs::metadata(&path).unwrap()).unwrap();
    fs::write(&path, b"longer-second-value").unwrap();
    let second = publication_file_identity(&path, &fs::metadata(&path).unwrap()).unwrap();
    assert_eq!(
        (first.volume, first.file_index),
        (second.volume, second.file_index)
    );
    assert_ne!(first.len, second.len);
    let replacement = dir.path().join("replacement.bin");
    fs::write(&replacement, b"longer-second-value").unwrap();
    let distinct =
        publication_file_identity(&replacement, &fs::metadata(&replacement).unwrap()).unwrap();
    assert_eq!(second.volume, distinct.volume);
    assert_ne!(second.file_index, distinct.file_index);
}

#[cfg(all(feature = "mmap", unix))]
#[test]
fn actual_read_only_legacy_directory_remains_loadable() {
    use std::os::unix::fs::PermissionsExt as _;

    // Root can still create a file in mode 0555, so the injected error
    // test above is the deterministic fallback arm for that environment.
    if unsafe { libc::geteuid() } == 0 {
        return;
    }
    let vectors = rand_unit_vectors(16, 8, 0x1138_0521);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let index = VamanaIndex::build(&vectors, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    index.save(dir.path()).unwrap();
    let lock_path = dir.path().join(".checkpoint.lock");
    fs::remove_file(&lock_path).unwrap();
    let original = fs::metadata(dir.path()).unwrap().permissions();
    fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
    let loaded = VamanaIndex::load(dir.path());
    let lock_created = lock_path.exists();
    fs::set_permissions(dir.path(), original).unwrap();
    assert!(
        !lock_created,
        "read-only load must leave the directory unchanged"
    );
    assert_eq!(loaded.unwrap().vectors().unwrap(), vectors);
}

#[cfg(feature = "mmap")]
#[test]
fn read_only_fallback_refuses_a_changed_segment_generation() {
    let old_vectors = rand_unit_vectors(16, 8, 0x1138_0530);
    let new_vectors = rand_unit_vectors(16, 8, 0x1138_0531);
    let config = VamanaConfig::with_dimensions(8)
        .with_max_degree(8)
        .with_search_list_size(16);
    let old = VamanaIndex::build(&old_vectors, config.clone()).unwrap();
    let new = VamanaIndex::build(&new_vectors, config).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let next_dir = tempfile::tempdir().unwrap();
    old.save(dir.path()).unwrap();
    new.save(next_dir.path()).unwrap();
    fs::remove_file(dir.path().join(".checkpoint.lock")).unwrap();
    let replacement_graph = fs::read(next_dir.path().join("graph.bin")).unwrap();
    let destination = dir.path().to_path_buf();

    let result = VamanaIndex::load_with_lock_hooks(
        dir.path(),
        |lock| lock.lock_shared().map_err(Into::into),
        |_| Err(std::io::ErrorKind::PermissionDenied.into()),
        move || {
            let staged = destination.join("graph.bin.next");
            fs::write(&staged, replacement_graph).unwrap();
            fs::rename(staged, destination.join("graph.bin")).unwrap();
        },
    );
    assert!(matches!(
        result,
        Err(VamanaError::InvalidFormat { reason })
            if reason.contains("publication changed during read-only unlocked load")
    ));
}

#[cfg(feature = "mmap")]
#[test]
fn checkpoint_publication_repairs_structurally_corrupt_incumbent() {
    // Checksum-valid-but-structurally-corrupt incumbent (see
    // crates/khive-vamana/docs/index.md#test-fixture-notes): lifecycle.bin's
    // reverse_adj is out of sync with graph.bin despite matching blake3 checksums.
    let vectors = rand_unit_vectors(20, 4, 0x2200_0100);
    let cfg = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut incumbent = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
    incumbent.set_last_applied_seq(Some(500));

    let dir = tempfile::tempdir().unwrap();
    incumbent.save_atomic(dir.path()).unwrap();

    // Inject a phantom in-neighbor that passes parse_lifecycle's shape checks but is
    // not a real predecessor in graph.bin, then re-sign metadata.bin to match.
    let metadata_bytes = fs::read(dir.path().join("metadata.bin")).unwrap();
    let commit = parse_v2_commit(&metadata_bytes).unwrap();
    let lifecycle_bytes = fs::read(dir.path().join("lifecycle.bin")).unwrap();
    let mut lifecycle = parse_lifecycle(
        &lifecycle_bytes,
        commit.index_meta.num_vectors,
        commit.index_meta.max_degree,
    )
    .unwrap();
    let num_vectors = commit.index_meta.num_vectors;
    // Search for a destination with a free reverse-adjacency slot and a source
    // provably absent from both the persisted list and the true predecessors,
    // so the injected phantom is unambiguous. Panics loudly here if none exists
    // rather than deep inside the corruption path.
    let (dest, phantom) = (0..num_vectors)
        .find_map(|dest| {
            let reverse = &lifecycle.reverse_adj[dest];
            if reverse.len() >= num_vectors.saturating_sub(1) {
                return None;
            }
            (0..num_vectors)
                .filter(|&candidate| candidate != dest)
                .map(|candidate| candidate as u32)
                .find(|candidate| {
                    !reverse.contains(candidate)
                        && !incumbent.graph.adjacency()[*candidate as usize]
                            .contains(&(dest as u32))
                })
                .map(|candidate| (dest, candidate))
        })
        .expect(
            "some destination node must have both a free reverse-adjacency slot and a \
                 source absent from its reverse list and forward predecessors",
        );
    lifecycle.reverse_adj[dest].push(phantom);

    let corrupt_lifecycle_bytes = encode_lifecycle(
        &lifecycle.tombstones,
        &lifecycle.free_slots,
        &lifecycle.reverse_adj,
        lifecycle.ops_since_consolidation,
    );
    fs::write(dir.path().join("lifecycle.bin"), &corrupt_lifecycle_bytes).unwrap();
    let corrupt_lifecycle_hash = *blake3::hash(&corrupt_lifecycle_bytes).as_bytes();

    write_v2_commit_full(
        &dir.path().join("metadata.bin"),
        &commit.vectors_hash,
        &commit.graph_hash,
        &corrupt_lifecycle_hash,
        &V2CorpusFingerprint {
            vector_count: commit.fingerprint.vector_count,
            dimensions: commit.fingerprint.dimensions,
            content_hash: commit.fingerprint.content_hash,
        },
        commit.index_meta.num_vectors,
        commit.index_meta.dimensions,
        commit.index_meta.max_degree,
        commit.index_meta.search_list_size,
        commit.index_meta.alpha,
        commit.last_applied_seq,
        commit.codes_hash.as_ref(),
        None,
    )
    .unwrap();

    // Passes every checksum but must still fail load_v2_fast's bidirectional check.
    assert!(matches!(
        VamanaIndex::load(dir.path()),
        Err(VamanaError::InvalidFormat { .. })
    ));

    // A corrupt incumbent is not a legitimate barrier to a lower-sequence repair.
    let repair_vectors = rand_unit_vectors(15, 4, 0x2200_0200);
    let mut repair = VamanaIndex::build(&repair_vectors, cfg).unwrap();
    repair.set_last_applied_seq(Some(100));
    repair
        .save_atomic(dir.path())
        .expect("repair checkpoint below a structurally corrupt incumbent must publish");

    let persisted = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(persisted.last_applied_seq(), Some(100));
    assert_eq!(persisted.vectors().unwrap(), repair_vectors);
}

#[cfg(feature = "mmap")]
#[test]
fn directory_load_rejects_invalid_free_slots_and_allows_repair() {
    let vectors = rand_unit_vectors(8, 4, 0x3285);
    let config = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    for (case, slots) in [
        ("live", vec![1]),
        ("out of range", vec![8]),
        ("duplicate", vec![0, 0]),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let mut incumbent = VamanaIndex::build(&vectors, config.clone()).unwrap();
        incumbent.tombstone(0).unwrap();
        incumbent.set_last_applied_seq(Some(500));
        incumbent.save_atomic(dir.path()).unwrap();

        let metadata = fs::read(dir.path().join("metadata.bin")).unwrap();
        let commit = parse_v2_commit(&metadata).unwrap();
        let lifecycle_bytes = fs::read(dir.path().join("lifecycle.bin")).unwrap();
        let mut lifecycle = parse_lifecycle(
            &lifecycle_bytes,
            commit.index_meta.num_vectors,
            commit.index_meta.max_degree,
        )
        .unwrap();
        lifecycle.free_slots = slots;
        let corrupted = encode_lifecycle(
            &lifecycle.tombstones,
            &lifecycle.free_slots,
            &lifecycle.reverse_adj,
            lifecycle.ops_since_consolidation,
        );
        fs::write(dir.path().join("lifecycle.bin"), &corrupted).unwrap();
        let hash = *blake3::hash(&corrupted).as_bytes();
        write_v2_commit_full(
            &dir.path().join("metadata.bin"),
            &commit.vectors_hash,
            &commit.graph_hash,
            &hash,
            &V2CorpusFingerprint {
                vector_count: commit.fingerprint.vector_count,
                dimensions: commit.fingerprint.dimensions,
                content_hash: commit.fingerprint.content_hash,
            },
            commit.index_meta.num_vectors,
            commit.index_meta.dimensions,
            commit.index_meta.max_degree,
            commit.index_meta.search_list_size,
            commit.index_meta.alpha,
            commit.last_applied_seq,
            commit.codes_hash.as_ref(),
            None,
        )
        .unwrap();

        let load = VamanaIndex::load(dir.path());
        assert!(
            matches!(&load, Err(VamanaError::InvalidFormat { reason })
                    if reason.contains("invalid free slot")),
            "{case} free slot must fail at load, got {load:?}"
        );

        let mut repair = VamanaIndex::build(&vectors, config.clone()).unwrap();
        repair.set_last_applied_seq(Some(100));
        repair
            .save_atomic(dir.path())
            .expect("corrupt incumbent is no sequence barrier");
        assert_eq!(
            VamanaIndex::load(dir.path()).unwrap().last_applied_seq(),
            Some(100)
        );
    }
}

#[cfg(feature = "mmap")]
#[test]
fn checkpoint_publication_repairs_incumbent_with_malformed_codes_segment() {
    // codes.bin gets the same structural check as graph/lifecycle (see
    // crates/khive-vamana/docs/index.md#test-fixture-notes).
    let vectors = rand_unit_vectors(20, 4, 0x2200_0300);
    let cfg = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut incumbent = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
    incumbent.set_last_applied_seq(Some(500));

    let dir = tempfile::tempdir().unwrap();
    incumbent.save_atomic(dir.path()).unwrap();

    let metadata_bytes = fs::read(dir.path().join("metadata.bin")).unwrap();
    let commit = parse_v2_commit(&metadata_bytes).unwrap();

    let mut codes_bytes = fs::read(dir.path().join("codes.bin")).unwrap();
    codes_bytes[..8].copy_from_slice(b"CORRUPT!");
    fs::write(dir.path().join("codes.bin"), &codes_bytes).unwrap();
    let corrupt_codes_hash = *blake3::hash(&codes_bytes).as_bytes();

    write_v2_commit_full(
        &dir.path().join("metadata.bin"),
        &commit.vectors_hash,
        &commit.graph_hash,
        &commit.lifecycle_hash,
        &V2CorpusFingerprint {
            vector_count: commit.fingerprint.vector_count,
            dimensions: commit.fingerprint.dimensions,
            content_hash: commit.fingerprint.content_hash,
        },
        commit.index_meta.num_vectors,
        commit.index_meta.dimensions,
        commit.index_meta.max_degree,
        commit.index_meta.search_list_size,
        commit.index_meta.alpha,
        commit.last_applied_seq,
        Some(&corrupt_codes_hash),
        None,
    )
    .unwrap();

    // Passes every checksum but must still fail load_v2_fast's codes.bin header check.
    assert!(matches!(
        VamanaIndex::load(dir.path()),
        Err(VamanaError::InvalidFormat { .. })
    ));

    // A corrupt incumbent is not a legitimate barrier to a lower-sequence repair.
    let repair_vectors = rand_unit_vectors(15, 4, 0x2200_0400);
    let mut repair = VamanaIndex::build(&repair_vectors, cfg).unwrap();
    repair.set_last_applied_seq(Some(100));
    repair
        .save_atomic(dir.path())
        .expect("repair checkpoint below an incumbent with a malformed codes segment must publish");

    let persisted = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(persisted.last_applied_seq(), Some(100));
    assert_eq!(persisted.vectors().unwrap(), repair_vectors);
}

#[cfg(feature = "mmap")]
#[test]
fn checkpoint_guard_rejects_absurd_num_vectors_without_huge_allocation() {
    // Regression: parse_graph must preflight the declared node count against the
    // segment's actual byte length before allocating, or a forged num_nodes near
    // u32::MAX aborts the process. See crates/khive-vamana/docs/index.md#test-fixture-notes.
    let vectors = rand_unit_vectors(5, 4, 0x2200_0500);
    let cfg = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut incumbent = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
    incumbent.set_last_applied_seq(Some(500));

    let dir = tempfile::tempdir().unwrap();
    incumbent.save_atomic(dir.path()).unwrap();

    let metadata_bytes = fs::read(dir.path().join("metadata.bin")).unwrap();
    let commit = parse_v2_commit(&metadata_bytes).unwrap();

    let absurd_num_vectors = u32::MAX as usize;
    let mut malicious_graph = Vec::with_capacity(16);
    malicious_graph.extend_from_slice(GRAPH_MAGIC);
    malicious_graph.extend_from_slice(&(absurd_num_vectors as u32).to_le_bytes());
    malicious_graph.extend_from_slice(&0u32.to_le_bytes()); // medoid
    fs::write(dir.path().join("graph.bin"), &malicious_graph).unwrap();
    let malicious_graph_hash = *blake3::hash(&malicious_graph).as_bytes();

    write_v2_commit_full(
        &dir.path().join("metadata.bin"),
        &commit.vectors_hash,
        &malicious_graph_hash,
        &commit.lifecycle_hash,
        &V2CorpusFingerprint {
            vector_count: commit.fingerprint.vector_count,
            dimensions: commit.fingerprint.dimensions,
            content_hash: commit.fingerprint.content_hash,
        },
        absurd_num_vectors,
        commit.index_meta.dimensions,
        commit.index_meta.max_degree,
        commit.index_meta.search_list_size,
        commit.index_meta.alpha,
        commit.last_applied_seq,
        commit.codes_hash.as_ref(),
        None,
    )
    .unwrap();

    let repair_vectors = rand_unit_vectors(3, 4, 0x2200_0600);
    let mut repair = VamanaIndex::build(&repair_vectors, cfg).unwrap();
    repair.set_last_applied_seq(Some(100));
    let started = std::time::Instant::now();
    repair
        .save_atomic(dir.path())
        .expect("repair checkpoint below a structurally invalid incumbent must publish");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "guard must reject the forged incumbent cheaply, not attempt a huge allocation"
    );

    let persisted = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(persisted.last_applied_seq(), Some(100));
    assert_eq!(persisted.vectors().unwrap(), repair_vectors);
}

#[test]
fn parse_lifecycle_rejects_short_body_for_absurd_rev_num_nodes_without_huge_allocation() {
    // Same allocation-bomb class as the graph.bin guard, for lifecycle.bin's
    // rev_num_nodes field. See crates/khive-vamana/docs/index.md#test-fixture-notes.
    let absurd_num_vectors = u32::MAX as usize;
    let mut body = b"KHVVLIF1".to_vec();
    body.extend_from_slice(&0u64.to_le_bytes()); // ts_words
    body.extend_from_slice(&0u64.to_le_bytes()); // fs_count
    body.extend_from_slice(&0u64.to_le_bytes()); // ops_since_consolidation
    body.extend_from_slice(&(absurd_num_vectors as u64).to_le_bytes()); // rev_num_nodes

    let started = std::time::Instant::now();
    let result = parse_lifecycle(&body, absurd_num_vectors, 4);
    assert!(
        matches!(result, Err(VamanaError::InvalidFormat { .. })),
        "parse_lifecycle must reject a short body for an absurd rev_num_nodes"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(10),
        "guard must reject cheaply, not attempt a huge allocation"
    );
}

#[cfg(feature = "mmap")]
#[test]
fn parse_codes_bin_rejects_overflowing_shape_without_allocation() {
    // Regression: `dims * 4 + count * dims` must use checked arithmetic, or an
    // overflowing shape passes the length check and drives an enormous allocation.
    let dims = 1usize << 61;
    let count = 4usize;
    let mut data = Vec::with_capacity(CODES_HEADER_LEN);
    data.extend_from_slice(CODES_MAGIC);
    data.extend_from_slice(&(dims as u64).to_le_bytes());
    data.extend_from_slice(&(count as u64).to_le_bytes());
    data.extend_from_slice(&1.0f32.to_le_bytes()); // gs
    data.extend_from_slice(&0.0f32.to_le_bytes()); // anisotropy_ratio
    assert_eq!(data.len(), CODES_HEADER_LEN);

    let result = parse_codes_bin(&data, dims, count);
    assert!(
        matches!(result, Err(VamanaError::InvalidFormat { .. })),
        "parse_codes_bin must reject an overflowing shape as InvalidFormat"
    );
}

#[cfg(feature = "mmap")]
#[test]
fn checkpoint_publication_repairs_incumbent_with_wrong_length_vectors_segment() {
    // vectors.bin gets the same shape validation as graph/lifecycle (see
    // crates/khive-vamana/docs/index.md#test-fixture-notes).
    let vectors = rand_unit_vectors(20, 4, 0x2200_0700);
    let cfg = VamanaConfig::with_dimensions(4)
        .with_max_degree(4)
        .with_search_list_size(8);
    let mut incumbent = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
    incumbent.set_last_applied_seq(Some(500));

    let dir = tempfile::tempdir().unwrap();
    incumbent.save_atomic(dir.path()).unwrap();

    let metadata_bytes = fs::read(dir.path().join("metadata.bin")).unwrap();
    let commit = parse_v2_commit(&metadata_bytes).unwrap();

    let mut vectors_bytes = fs::read(dir.path().join("vectors.bin")).unwrap();
    let new_len = vectors_bytes.len() - 4;
    vectors_bytes.truncate(new_len);
    fs::write(dir.path().join("vectors.bin"), &vectors_bytes).unwrap();
    let corrupt_vectors_hash = *blake3::hash(&vectors_bytes).as_bytes();

    write_v2_commit_full(
        &dir.path().join("metadata.bin"),
        &corrupt_vectors_hash,
        &commit.graph_hash,
        &commit.lifecycle_hash,
        &V2CorpusFingerprint {
            vector_count: commit.fingerprint.vector_count,
            dimensions: commit.fingerprint.dimensions,
            content_hash: commit.fingerprint.content_hash,
        },
        commit.index_meta.num_vectors,
        commit.index_meta.dimensions,
        commit.index_meta.max_degree,
        commit.index_meta.search_list_size,
        commit.index_meta.alpha,
        commit.last_applied_seq,
        commit.codes_hash.as_ref(),
        None,
    )
    .unwrap();

    // Passes every checksum but must still fail load_v2_fast's byte-length check.
    assert!(matches!(
        VamanaIndex::load(dir.path()),
        Err(VamanaError::InvalidFormat { .. })
    ));

    // A corrupt incumbent is not a legitimate barrier to a lower-sequence repair.
    let repair_vectors = rand_unit_vectors(15, 4, 0x2200_0800);
    let mut repair = VamanaIndex::build(&repair_vectors, cfg).unwrap();
    repair.set_last_applied_seq(Some(100));
    repair.save_atomic(dir.path()).expect(
        "repair checkpoint below an incumbent with a wrong-length vectors segment must publish",
    );

    let persisted = VamanaIndex::load(dir.path()).unwrap();
    assert_eq!(persisted.last_applied_seq(), Some(100));
    assert_eq!(persisted.vectors().unwrap(), repair_vectors);
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
