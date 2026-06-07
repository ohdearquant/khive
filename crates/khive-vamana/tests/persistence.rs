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
