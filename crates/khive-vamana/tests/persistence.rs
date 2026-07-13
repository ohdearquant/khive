//! Integration tests for Vamana snapshot serialization, deserialization, and corruption handling.
//! Includes v2 persistence (KHVVAMG2) tests.

#[cfg(feature = "mmap")]
use std::fs;

use khive_vamana::{
    CorpusFingerprint, VamanaConfig, VamanaError, VamanaIndex, VAMANA_SNAPSHOT_FORMAT,
    VAMANA_SNAPSHOT_VERSION,
};
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

    let mut gdata = fs::read(dir.path().join("graph.bin")).unwrap();
    let mut offset = 16usize;
    'outer: for _node in 0..4usize {
        let degree = u32::from_le_bytes(gdata[offset..offset + 4].try_into().unwrap()) as usize;
        offset += 4;
        if degree > 0 {
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
    let loaded = VamanaIndex::load(dir.path()).unwrap();
    let query = rand_unit_vectors(1, 8, 77);
    let results = loaded.search(&query, 3).unwrap();
    assert!(!results.is_empty());
}

#[test]
fn snapshot_roundtrip_preserves_search_results() {
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
fn snapshot_rejects_bad_format() {
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
fn snapshot_rejects_id_count_mismatch() {
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
fn stale_snapshot_detected_by_fingerprint_mismatch() {
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

    let fp_after_change = CorpusFingerprint {
        vector_count: 10,
        dimensions: 4,
    };

    assert_ne!(
        snapshot.fingerprint, fp_after_change,
        "stale snapshot must be detected by fingerprint mismatch"
    );
    assert_eq!(
        snapshot.fingerprint, fp_at_build,
        "snapshot fingerprint must equal the build-time fingerprint"
    );
}

// ---- V2 persistence (KHVVAMG2) tests ----

/// Round-trip: save_atomic → load_or_build → all lifecycle state preserved.
#[cfg(feature = "mmap")]
#[test]
fn v2_roundtrip_preserves_lifecycle_state() {
    let dim = 8usize;
    let vectors = rand_unit_vectors(30, dim, 0xA2_01);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(6)
        .with_search_list_size(12);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();

    // Create some lifecycle state: tombstone two nodes, insert two new ones.
    idx.tombstone(0).unwrap();
    idx.tombstone(1).unwrap();
    let new_vec = rand_unit_vectors(1, dim, 0x999);
    idx.insert(&new_vec).unwrap();

    let tc = idx.tombstone_count();
    let live = idx.live_count();
    let ops = idx.ops_since_consolidation();

    // Compute the expected reverse adj as it will be after save_atomic/load:
    // save_atomic caps the medoid's forward list to max_degree before writing
    // graph.bin, and derives reverse adj from that capped view. So the restored
    // reverse adj reflects the capped graph, not the potentially-overflowed
    // in-memory adjacency.
    let max_degree = 6usize;
    let medoid = idx.graph().medoid() as usize;
    let adj = idx.graph().adjacency();
    let mut expected_rev: Vec<std::collections::BTreeSet<u32>> =
        vec![Default::default(); adj.len()];
    for (u, neighbors) in adj.iter().enumerate() {
        let effective: &[u32] = if u == medoid {
            &neighbors[..max_degree.min(neighbors.len())]
        } else {
            neighbors
        };
        for &v in effective {
            expected_rev[v as usize].insert(u as u32);
        }
    }

    let corpus = idx.vectors().unwrap().to_vec();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(6)
        .with_search_list_size(12);
    let loaded = VamanaIndex::load_or_build(dir.path(), &corpus, fallback).unwrap();

    assert_eq!(
        loaded.tombstone_count(),
        tc,
        "tombstone_count must be preserved"
    );
    assert_eq!(loaded.live_count(), live, "live_count must be preserved");
    assert_eq!(
        loaded.ops_since_consolidation(),
        ops,
        "ops_since_consolidation must be preserved"
    );
    // Verify the loaded reverse adj matches the expected capped version.
    let loaded_rev = loaded.graph().reverse_adjacency();
    for v in 0..loaded_rev.len() {
        let actual: std::collections::BTreeSet<u32> = loaded_rev[v].iter().copied().collect();
        assert_eq!(
            actual, expected_rev[v],
            "reverse_adj[{v}] must be preserved (capped to match graph.bin)"
        );
    }
}

/// Crash consistency: corrupt metadata.bin after writing segments → load_or_build rebuilds.
#[cfg(feature = "mmap")]
#[test]
fn v2_crash_corrupted_metadata_falls_back_to_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(20, dim, 0xA2_02);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // Corrupt the metadata.bin (the commit record).
    let meta_path = dir.path().join("metadata.bin");
    let mut meta = fs::read(&meta_path).unwrap();
    meta[8..16].fill(0xff); // trash the vectors_hash start
    fs::write(&meta_path, &meta).unwrap();

    // load_or_build should detect checksum mismatch and rebuild without error.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), idx.num_vectors());
    // Search must still work.
    let query = rand_unit_vectors(1, dim, 0xabc);
    assert!(!rebuilt.search(&query, 3).unwrap().is_empty());
}

/// Fingerprint mismatch: modify corpus → load_or_build triggers rebuild.
#[cfg(feature = "mmap")]
#[test]
fn v2_fingerprint_mismatch_triggers_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(15, dim, 0xA2_03);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // Construct a modified corpus (one extra vector).
    let mut new_corpus = vectors.clone();
    new_corpus.extend_from_slice(&rand_unit_vectors(1, dim, 0xfff));

    // load_or_build with modified corpus → fingerprint mismatch → rebuild.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &new_corpus, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), new_corpus.len() / dim);
    let query = rand_unit_vectors(1, dim, 0xbcd);
    assert!(!rebuilt.search(&query, 3).unwrap().is_empty());
}

/// FIX 1: A hub graph whose reverse-adj degree exceeds max_degree*4 must load successfully.
/// Before the fix, parse_lifecycle would reject valid hub reverse-adjacency lists with degree
/// > max_degree*4 even though inbound degree can legitimately reach num_vectors-1.
#[cfg(feature = "mmap")]
#[test]
fn v2_hub_graph_high_inbound_degree_loads_successfully() {
    // Use a large max_degree relative to n to avoid the constraint firing for normal nodes,
    // but keep n large enough that a hub node can have inbound degree >> max_degree*4.
    // With max_degree=2 and n=20, a hub can have up to 19 inbound edges (>> 2*4=8).
    let dim = 4usize;
    let n = 20usize;
    let vectors = rand_unit_vectors(n, dim, 0xB1_01);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(2)
        .with_search_list_size(4);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let corpus = idx.vectors().unwrap().to_vec();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // Reload must succeed even if the medoid's reverse-adj degree exceeds max_degree*4.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(2)
        .with_search_list_size(4);
    let loaded = VamanaIndex::load_or_build(dir.path(), &corpus, fallback).unwrap();

    // Forward and reverse adjacency must be mutually consistent.
    assert_rev_adj_consistent(loaded.graph());

    // Search must still work.
    let query = rand_unit_vectors(1, dim, 0xB1_02);
    assert!(!loaded.search(&query, 3).unwrap().is_empty());
}

/// FIX 3: metadata.bin with >=8 bytes of unknown/garbage magic causes load_or_build to
/// rebuild rather than return InvalidFormat. (VamanaIndex::load remains strict.)
#[cfg(feature = "mmap")]
#[test]
fn v2_garbage_magic_causes_load_or_build_to_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(12, dim, 0xB3_01);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // Overwrite metadata.bin with >=8 bytes of garbage (unknown magic).
    let garbage = b"UNKNOWNX__padding__more_bytes_here".to_vec();
    fs::write(dir.path().join("metadata.bin"), &garbage).unwrap();

    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    // Must NOT return Err — must rebuild and return a usable index.
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), idx.num_vectors());

    // metadata.bin must now have the KHVVAMG2 magic (rebuilt + saved).
    let meta = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(&meta[..8], b"KHVVAMG2", "rebuilt index must be saved as v2");

    // Search must work.
    let query = rand_unit_vectors(1, dim, 0xB3_02);
    assert!(!rebuilt.search(&query, 3).unwrap().is_empty());
}

/// V1 compat: v1-format save → load_or_build upgrades to v2.
#[cfg(feature = "mmap")]
#[test]
fn v2_upgrades_v1_format_to_v2() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(12, dim, 0xA2_04);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let dir = tempfile::tempdir().unwrap();
    // Use v1 save (writes KHVVAMM1 metadata.bin).
    idx.save(dir.path()).unwrap();

    // Verify it's v1 format.
    let meta = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(&meta[..8], b"KHVVAMM1", "should be v1 magic before upgrade");

    // load_or_build detects v1, loads it, then saves v2.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let upgraded = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(upgraded.num_vectors(), idx.num_vectors());

    // After upgrade, metadata.bin should be KHVVAMG2.
    let meta2 = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(&meta2[..8], b"KHVVAMG2", "should be v2 magic after upgrade");

    // lifecycle.bin must now exist.
    assert!(
        dir.path().join("lifecycle.bin").exists(),
        "lifecycle.bin must be written after upgrade"
    );

    // Search still works.
    let query = rand_unit_vectors(1, dim, 0xdef);
    let r1 = idx.search(&query, 3).unwrap();
    let r2 = upgraded.search(&query, 3).unwrap();
    assert_eq!(r1, r2, "search results must match after v1→v2 upgrade");
}

/// Search correctness: load via v2 → search returns same results as in-memory index.
#[cfg(feature = "mmap")]
#[test]
fn v2_search_correctness_matches_in_memory() {
    let dim = 8usize;
    let vectors = rand_unit_vectors(40, dim, 0xA2_05);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();

    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let loaded = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();

    let queries = rand_unit_vectors(5, dim, 0x4242);
    for i in 0..5 {
        let q = &queries[i * dim..(i + 1) * dim];
        let r1 = idx.search(q, 5).unwrap();
        let r2 = loaded.search(q, 5).unwrap();
        assert_eq!(
            r1, r2,
            "query {i}: v2-loaded search must match in-memory search"
        );
    }
}

// ---- Fix 1: reverse-adj consistency after saturated insert ----

#[cfg(feature = "mmap")]
fn assert_rev_adj_consistent(graph: &khive_vamana::VamanaGraph) {
    let adj = graph.adjacency();
    let rev = graph.reverse_adjacency();
    let n = adj.len();
    let mut expected: Vec<std::collections::BTreeSet<u32>> = vec![Default::default(); n];
    for (u, neighbors) in adj.iter().enumerate() {
        for &v in neighbors {
            expected[v as usize].insert(u as u32);
        }
    }
    for v in 0..n {
        let actual: std::collections::BTreeSet<u32> = rev[v].iter().copied().collect();
        assert_eq!(actual, expected[v], "reverse_adj[{v}] inconsistent");
    }
}

#[cfg(feature = "mmap")]
#[test]
fn v2_reverse_adj_consistent_after_saturated_insert() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(8, dim, 0xA2_AA);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(2)
        .with_search_list_size(2);
    let mut idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let extra = rand_unit_vectors(4, dim, 0xA2_BB);
    for chunk in extra.chunks(dim) {
        idx.insert(chunk).unwrap();
    }
    let corpus = idx.vectors().unwrap().to_vec();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(2)
        .with_search_list_size(2);
    let loaded = VamanaIndex::load_or_build(dir.path(), &corpus, fallback).unwrap();
    assert_rev_adj_consistent(loaded.graph());
}

// ---- Fix 2: rebuild on clean first run and missing segments ----

#[cfg(feature = "mmap")]
#[test]
fn v2_empty_dir_falls_back_to_build() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(10, dim, 0xA2CC);
    let dir = tempfile::tempdir().unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(idx.num_vectors(), 10);
}

#[cfg(feature = "mmap")]
#[test]
fn v2_truncated_metadata_falls_back_to_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(10, dim, 0xA2DD);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();
    let mut garbage = b"KHVVAMG2".to_vec();
    garbage.extend_from_slice(&[0xffu8; 10]);
    fs::write(dir.path().join("metadata.bin"), &garbage).unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), 10);
}

#[cfg(feature = "mmap")]
#[test]
fn v2_missing_vectors_bin_falls_back_to_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(10, dim, 0xA2EE);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();
    fs::remove_file(dir.path().join("vectors.bin")).unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), 10);
}

#[cfg(feature = "mmap")]
#[test]
fn v2_missing_graph_bin_falls_back_to_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(10, dim, 0xA3_01);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();
    fs::remove_file(dir.path().join("graph.bin")).unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), 10);
}

#[cfg(feature = "mmap")]
#[test]
fn v2_missing_lifecycle_bin_falls_back_to_rebuild() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(10, dim, 0xA3_02);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();
    fs::remove_file(dir.path().join("lifecycle.bin")).unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), 10);
}

// ---- Fix 4: checksum-valid but semantically-corrupt reverse_adj triggers rebuild ----

/// Regression test for the bidirectional-consistency check added to load_v2_fast.
///
/// This test targets the NEW VALIDATOR inside load_v2_fast, NOT the blake3/commit-record
/// gate.  The lifecycle.bin is mutated so that reverse_adj[0] contains a phantom source
/// node that has no u->0 forward edge in graph.bin.  The blake3 hash in metadata.bin is
/// then RECOMPUTED over the mutated lifecycle.bin so the checksum gate passes and the
/// corrupt bytes reach the new bidirectional check.
///
/// Asserts both recovery paths:
///   (a) load_or_build REBUILDS — fresh KHVVAMG2 written, a query returns sane results.
///   (b) VamanaIndex::load returns InvalidFormat (strict path, no fallback).
#[cfg(feature = "mmap")]
#[test]
fn v2_corrupt_reverse_adj_not_inverse_of_graph_triggers_rebuild() {
    let dim = 4usize;
    let n = 10usize;
    let vectors = rand_unit_vectors(n, dim, 0xA3_10);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // --- Step 1: find a phantom_src that has no forward edge to node 0. ---
    // We pick the first node u != 0 such that adjacency[u] does NOT contain 0.
    let adj = idx.graph().adjacency();
    let phantom_src: u32 = (1..n as u32)
        .find(|&u| !adj[u as usize].contains(&0))
        .expect("must find a node with no 0 in its forward adjacency for a 10-node graph");

    // --- Step 2: parse lifecycle.bin and inject phantom_src into reverse_adj[0]. ---
    let mut lifecycle_bytes = fs::read(dir.path().join("lifecycle.bin")).unwrap();

    // Skip magic (8) + ts_words_count (8) + ts_words*8 (ts_words * 8) + fs_count (8)
    // + free_slots (fs_count * 4) + ops (8) + rev_num_nodes (8)
    // to reach the per-node reverse-adj data.
    let magic_len = 8usize;
    let ts_words = u64::from_le_bytes(
        lifecycle_bytes[magic_len..magic_len + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    let after_tombstones = magic_len + 8 + ts_words * 8;
    let fs_count = u64::from_le_bytes(
        lifecycle_bytes[after_tombstones..after_tombstones + 8]
            .try_into()
            .unwrap(),
    ) as usize;
    let after_free_slots = after_tombstones + 8 + fs_count * 4;
    // Skip ops (8) + rev_num_nodes (8).
    let node0_offset = after_free_slots + 8 + 8;

    // Read current degree of node 0.
    let degree0 = u32::from_le_bytes(
        lifecycle_bytes[node0_offset..node0_offset + 4]
            .try_into()
            .unwrap(),
    ) as usize;

    // Verify phantom_src is not already in reverse_adj[0] (must not create a dup).
    let neighbors_start = node0_offset + 4;
    let neighbors_end = neighbors_start + degree0 * 4;
    let already_present = lifecycle_bytes[neighbors_start..neighbors_end]
        .chunks_exact(4)
        .any(|b| u32::from_le_bytes(b.try_into().unwrap()) == phantom_src);
    assert!(
        !already_present,
        "phantom_src {phantom_src} is already in reverse_adj[0] — pick a different seed"
    );

    // Inject phantom_src: increment degree and append the new neighbor ID.
    // We write the new degree (degree0 + 1) then insert the new neighbor at the end of
    // node 0's neighbor list.  All subsequent bytes (nodes 1..n) shift right by 4 bytes.
    let new_degree = (degree0 + 1) as u32;
    lifecycle_bytes[node0_offset..node0_offset + 4].copy_from_slice(&new_degree.to_le_bytes());
    let insert_at = neighbors_end;
    lifecycle_bytes.splice(insert_at..insert_at, phantom_src.to_le_bytes());

    // --- Step 3: recompute blake3(mutated lifecycle) and patch metadata.bin. ---
    // This ensures the checksum gate in load_or_build passes; the NEW BIDIRECTIONAL CHECK
    // inside load_v2_fast is what the corrupt bytes must reach and trip.
    let new_lhash = *blake3::hash(&lifecycle_bytes).as_bytes();
    let mut meta_bytes = fs::read(dir.path().join("metadata.bin")).unwrap();
    // lifecycle_hash is at offset 8 (magic) + 32 (vectors_hash) + 32 (graph_hash) = 72.
    const LIFECYCLE_HASH_OFFSET: usize = 8 + 32 + 32;
    meta_bytes[LIFECYCLE_HASH_OFFSET..LIFECYCLE_HASH_OFFSET + 32].copy_from_slice(&new_lhash);

    fs::write(dir.path().join("lifecycle.bin"), &lifecycle_bytes).unwrap();
    fs::write(dir.path().join("metadata.bin"), &meta_bytes).unwrap();

    // --- Step 4 (b): VamanaIndex::load (strict path) must return InvalidFormat. ---
    // Test the strict path FIRST while the corrupt state is still on disk.  load_or_build
    // (step 4a) overwrites lifecycle.bin with a fresh rebuild, so the strict-path check
    // must come before the permissive one.
    assert!(
        matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ),
        "VamanaIndex::load must return InvalidFormat on corrupt reverse_adj \
         (no recovery on the strict path)"
    );

    // --- Step 4 (a): load_or_build must REBUILD, not propagate the error. ---
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(
        rebuilt.num_vectors(),
        n,
        "rebuilt index must have all vectors"
    );

    // The rebuilt snapshot must be a fresh KHVVAMG2 with consistent reverse_adj.
    let meta_after = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(
        &meta_after[..8],
        b"KHVVAMG2",
        "rebuilt index must be saved as v2"
    );
    assert_rev_adj_consistent(rebuilt.graph());

    // A query must return sane (non-empty) results.
    let query = rand_unit_vectors(1, dim, 0xA3_11);
    assert!(
        !rebuilt.search(&query, 3).unwrap().is_empty(),
        "rebuilt index must answer queries"
    );
}

// ---- Fix 3: staged v2new segments do not corrupt v1 restore ----

#[cfg(feature = "mmap")]
#[test]
fn v2_v1_metadata_with_staged_v2_segments_not_torn() {
    let dim = 4usize;
    let vectors = rand_unit_vectors(12, dim, 0xA3_03);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Save as v1 format (KHVVAMM1).
    idx.save(dir.path()).unwrap();
    // Simulate half-done save_atomic: .v2new files exist but metadata.bin was not renamed.
    fs::write(dir.path().join("vectors.bin.v2new"), b"garbage").unwrap();
    fs::write(dir.path().join("graph.bin.v2new"), b"garbage").unwrap();
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let loaded = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(loaded.num_vectors(), idx.num_vectors());
    let query = rand_unit_vectors(1, dim, 0xA3_04);
    assert!(!loaded.search(&query, 3).unwrap().is_empty());
}

// ---- Fix 5: parse_lifecycle count-overflow: huge ts_words / fs_count must not panic ----

/// Helper: write `body` as lifecycle.bin, recompute its blake3, and patch the
/// lifecycle_hash field in metadata.bin so the checksum gate passes.  Returns the
/// mutated on-disk state ready for load_or_build / VamanaIndex::load.
#[cfg(feature = "mmap")]
fn install_corrupt_lifecycle(dir: &std::path::Path, lifecycle_body: &[u8]) {
    let lhash = *blake3::hash(lifecycle_body).as_bytes();
    fs::write(dir.join("lifecycle.bin"), lifecycle_body).unwrap();

    let mut meta = fs::read(dir.join("metadata.bin")).unwrap();
    // lifecycle_hash is at offset 8 (magic) + 32 (vectors_hash) + 32 (graph_hash) = 72.
    const LIFECYCLE_HASH_OFFSET: usize = 8 + 32 + 32;
    meta[LIFECYCLE_HASH_OFFSET..LIFECYCLE_HASH_OFFSET + 32].copy_from_slice(&lhash);
    fs::write(dir.join("metadata.bin"), &meta).unwrap();
}

/// Regression for overflow hardening in parse_lifecycle.
///
/// Targets the `checked_mul` guards added for ts_words and fs_count.  Without the fix
/// the multiply wraps to a small value on release builds, the bounds check passes
/// spuriously, and the subsequent loop panics on an out-of-range slice read.  With the
/// fix parse_lifecycle returns InvalidFormat before the loop, load_or_build rebuilds, and
/// load returns InvalidFormat — no panic on either path.
///
/// The lifecycle.bin blob is crafted so the blake3/commit-record gate passes (hash
/// recomputed and patched into metadata.bin); the overflowing multiply is the gate
/// that must trip, not the checksum.
#[cfg(feature = "mmap")]
#[test]
fn v2_parse_lifecycle_huge_ts_words_returns_invalid_format_not_panic() {
    let dim = 4usize;
    let n = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xA4_01);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // Craft a lifecycle.bin: magic + ts_words = u64::MAX/2, then nothing.
    // ts_words * 8 wraps to a small value on release (old code); checked_mul traps it.
    let mut lifecycle_body = b"KHVVLIF1".to_vec();
    lifecycle_body.extend_from_slice(&(u64::MAX / 2).to_le_bytes()); // ts_words
    install_corrupt_lifecycle(dir.path(), &lifecycle_body);

    // Strict path: InvalidFormat, no panic.
    assert!(
        matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ),
        "load must return InvalidFormat, not panic, on huge ts_words"
    );

    // Permissive path: load_or_build rebuilds successfully, no panic.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), n);
    let meta_after = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(&meta_after[..8], b"KHVVAMG2");
}

#[cfg(feature = "mmap")]
#[test]
fn v2_parse_lifecycle_huge_fs_count_returns_invalid_format_not_panic() {
    let dim = 4usize;
    let n = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xA4_02);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // Craft a lifecycle.bin: magic + ts_words=0 (passes tombstone check) + fs_count = u64::MAX/2.
    // fs_count * 4 wraps on release (old code); checked_mul traps it.
    let mut lifecycle_body = b"KHVVLIF1".to_vec();
    lifecycle_body.extend_from_slice(&0u64.to_le_bytes()); // ts_words = 0
    lifecycle_body.extend_from_slice(&(u64::MAX / 2).to_le_bytes()); // fs_count
    install_corrupt_lifecycle(dir.path(), &lifecycle_body);

    // Strict path: InvalidFormat, no panic.
    assert!(
        matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ),
        "load must return InvalidFormat, not panic, on huge fs_count"
    );

    // Permissive path: load_or_build rebuilds successfully, no panic.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), n);
}

// ---- Fix 5 / R5: checked_add for offset + ts_bytes / offset + fs_bytes ----
//
// These two tests target the ADDITION overflow, not the multiplication.
// ts_words = usize::MAX/8 passes checked_mul (ts_bytes = usize::MAX-7), but
// offset (=16) + ts_bytes wraps to a small value without checked_add, causing the
// bounds check to pass spuriously and the subsequent loop to panic on an
// out-of-range slice read.  Same pattern for fs_count = usize::MAX/4.
//
// Both tests confirm the two recovery paths:
//   (a) VamanaIndex::load returns InvalidFormat (no panic).
//   (b) load_or_build rebuilds successfully (no panic).

#[cfg(feature = "mmap")]
#[test]
fn v2_parse_lifecycle_ts_add_overflow_returns_invalid_format_not_panic() {
    let dim = 4usize;
    let n = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xA5_01);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // ts_words = usize::MAX/8.
    //   checked_mul: (usize::MAX/8) * 8 = usize::MAX - 7  → fits in usize, passes mul check.
    //   offset (=16) + (usize::MAX - 7) wraps to 8 without checked_add → spurious pass.
    let ts_words = (usize::MAX / 8) as u64;
    let mut lifecycle_body = b"KHVVLIF1".to_vec();
    lifecycle_body.extend_from_slice(&ts_words.to_le_bytes());
    install_corrupt_lifecycle(dir.path(), &lifecycle_body);

    // Strict path: InvalidFormat (not panic).
    assert!(
        matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ),
        "load must return InvalidFormat, not panic, when offset + ts_bytes overflows"
    );

    // Permissive path: load_or_build rebuilds, no panic.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), n);
    let meta_after = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(&meta_after[..8], b"KHVVAMG2");
}

#[cfg(feature = "mmap")]
#[test]
fn v2_parse_lifecycle_fs_add_overflow_returns_invalid_format_not_panic() {
    let dim = 4usize;
    let n = 8usize;
    let vectors = rand_unit_vectors(n, dim, 0xA5_02);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    // ts_words=0 so tombstone section is skipped; fs_count = usize::MAX/4.
    //   checked_mul: (usize::MAX/4) * 4 = usize::MAX - 3  → fits in usize, passes mul check.
    //   offset (=24) + (usize::MAX - 3) wraps to 20 without checked_add → spurious pass.
    let fs_count = (usize::MAX / 4) as u64;
    let mut lifecycle_body = b"KHVVLIF1".to_vec();
    lifecycle_body.extend_from_slice(&0u64.to_le_bytes()); // ts_words = 0
    lifecycle_body.extend_from_slice(&fs_count.to_le_bytes());
    install_corrupt_lifecycle(dir.path(), &lifecycle_body);

    // Strict path: InvalidFormat (not panic).
    assert!(
        matches!(
            VamanaIndex::load(dir.path()),
            Err(VamanaError::InvalidFormat { .. })
        ),
        "load must return InvalidFormat, not panic, when offset + fs_bytes overflows"
    );

    // Permissive path: load_or_build rebuilds, no panic.
    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), n);
    let meta_after = fs::read(dir.path().join("metadata.bin")).unwrap();
    assert_eq!(&meta_after[..8], b"KHVVAMG2");
}

// ---- Issue #444: v2 commit config validation must feed the corrupt-snapshot rebuild path ----

/// Regression: a persisted v2 commit with an invalid embedded config (e.g. max_degree = 0)
/// used to surface as `VamanaError::InvalidConfig` from `load_v2_fast`'s `config.validate()`
/// call, which `load_or_build`'s rebuild match only catches for `InvalidFormat`. That let a
/// corrupt commit record propagate as an error out of `load_or_build` instead of rebuilding.
/// With the fix, `parse_v2_commit` validates the embedded config and maps failures to
/// `InvalidFormat`, so the existing corrupt-snapshot rebuild path handles it.
#[cfg(feature = "mmap")]
#[test]
fn load_or_build_rebuilds_when_v2_commit_config_invalid() {
    let dim = 8usize;
    let vectors = rand_unit_vectors(20, dim, 0xA6_44);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let idx = VamanaIndex::build(&vectors, cfg.clone()).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    let meta_path = dir.path().join("metadata.bin");
    let mut meta = fs::read(&meta_path).unwrap();
    meta[168..176].copy_from_slice(&0u64.to_le_bytes()); // malformed persisted max_degree
    fs::write(&meta_path, &meta).unwrap();

    assert!(matches!(
        VamanaIndex::load(dir.path()),
        Err(VamanaError::InvalidFormat { .. })
    ));

    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(8)
        .with_search_list_size(16);
    let loaded = VamanaIndex::load_or_build(dir.path(), &vectors, fallback.clone()).unwrap();
    assert_eq!(loaded.config(), &fallback);
    assert_eq!(loaded.num_vectors(), vectors.len() / dim);
}

// ---- Issue #435: v2 lifecycle parser must reject tombstone bits past num_vectors ----

/// Regression: parse_lifecycle resized the tombstone bitvec up to `needed_words` when it was
/// too short but never rejected (or masked) extra set bits at or beyond `num_vectors` when the
/// persisted word count already covered `needed_words`. Those out-of-range set bits were
/// counted into `tombstone_count` and silently treated as tombstoned nodes past the valid
/// node-id domain. With the fix, `parse_lifecycle` rejects tombstone bits set beyond
/// `num_vectors` with `InvalidFormat` instead of accepting the corrupt state.
#[cfg(feature = "mmap")]
#[test]
fn v2_parse_lifecycle_rejects_tombstone_bits_past_num_vectors_not_panic() {
    let dim = 4usize;
    let n = 9usize;
    let vectors = rand_unit_vectors(n, dim, 0xA6_35);
    let cfg = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let idx = VamanaIndex::build(&vectors, cfg).unwrap();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    let mut lifecycle_body = fs::read(dir.path().join("lifecycle.bin")).unwrap();
    assert_eq!(&lifecycle_body[..8], b"KHVVLIF1");
    assert_eq!(
        u64::from_le_bytes(lifecycle_body[8..16].try_into().unwrap()),
        1
    );
    // For num_vectors = 9, valid tombstone bits are 0..=8; this word sets only bits 9..=63.
    lifecycle_body[16..24].copy_from_slice(&0xffff_ffff_ffff_fe00u64.to_le_bytes());
    install_corrupt_lifecycle(dir.path(), &lifecycle_body);

    let load_result = std::panic::catch_unwind(|| VamanaIndex::load(dir.path()));
    assert!(matches!(
        load_result,
        Ok(Err(VamanaError::InvalidFormat { .. }))
    ));

    let fallback = VamanaConfig::with_dimensions(dim)
        .with_max_degree(4)
        .with_search_list_size(8);
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors, fallback).unwrap();
    assert_eq!(rebuilt.num_vectors(), n);
}
