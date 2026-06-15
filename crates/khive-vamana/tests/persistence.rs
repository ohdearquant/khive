//! Integration tests for Vamana snapshot serialization, deserialization, and corruption handling.
//! Includes v2 persistence (KHVVAMG2) tests.

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
    let rev_adj_snapshot: Vec<Vec<u32>> = idx.graph().reverse_adjacency().to_vec();

    let corpus = idx.vectors().unwrap().to_vec();
    let dir = tempfile::tempdir().unwrap();
    idx.save_atomic(dir.path()).unwrap();

    let loaded = VamanaIndex::load_or_build(dir.path(), &corpus).unwrap();

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
    assert_eq!(
        loaded.graph().reverse_adjacency(),
        rev_adj_snapshot.as_slice(),
        "reverse_adj must be preserved (no rebuild)"
    );
}

/// Crash consistency: corrupt metadata.bin after writing segments → load_or_build rebuilds.
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
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &vectors).unwrap();
    assert_eq!(rebuilt.num_vectors(), idx.num_vectors());
    // Search must still work.
    let query = rand_unit_vectors(1, dim, 0xabc);
    assert!(!rebuilt.search(&query, 3).unwrap().is_empty());
}

/// Fingerprint mismatch: modify corpus → load_or_build triggers rebuild.
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
    let rebuilt = VamanaIndex::load_or_build(dir.path(), &new_corpus).unwrap();
    assert_eq!(rebuilt.num_vectors(), new_corpus.len() / dim);
    let query = rand_unit_vectors(1, dim, 0xbcd);
    assert!(!rebuilt.search(&query, 3).unwrap().is_empty());
}

/// V1 compat: v1-format save → load_or_build upgrades to v2.
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
    let upgraded = VamanaIndex::load_or_build(dir.path(), &vectors).unwrap();
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

    let loaded = VamanaIndex::load_or_build(dir.path(), &vectors).unwrap();

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
