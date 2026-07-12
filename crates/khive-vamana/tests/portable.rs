use khive_vamana::{VamanaConfig, VamanaError, VamanaIndex};
use rand::{rngs::StdRng, Rng, SeedableRng};

fn corpus(rows: usize, dimensions: usize) -> Vec<f32> {
    let mut rng = StdRng::seed_from_u64(0x110a_110a);
    let mut vectors = Vec::with_capacity(rows * dimensions);
    for _ in 0..rows {
        let mut vector: Vec<f32> = (0..dimensions)
            .map(|_| rng.gen_range(-1.0f32..1.0))
            .collect();
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        for value in &mut vector {
            *value /= norm;
        }
        vectors.extend(vector);
    }
    vectors
}

fn config(dimensions: usize) -> VamanaConfig {
    VamanaConfig::with_dimensions(dimensions)
        .with_max_degree(8)
        .with_search_list_size(16)
}

#[test]
fn portable_round_trip_preserves_lifecycle_graph_ids_and_search() {
    let dimensions = 12;
    let vectors = corpus(48, dimensions);
    let mut index = VamanaIndex::build(&vectors, config(dimensions)).unwrap();
    index.tombstone(3).unwrap();
    index.tombstone(29).unwrap();

    let ids: Vec<(u32, String)> = (0..index.num_vectors() as u32)
        .rev()
        .filter(|ordinal| !index.is_tombstoned(*ordinal))
        .map(|ordinal| (ordinal, format!("doc-{ordinal}")))
        .collect();
    let query = &vectors[11 * dimensions..12 * dimensions];
    let expected_search = index.search(query, 12).unwrap();
    let bytes = index.to_bytes(&ids).unwrap();

    let (restored, restored_ids) = VamanaIndex::from_bytes(&bytes).unwrap();
    let mut expected_ids = ids;
    expected_ids.sort_unstable_by_key(|(ordinal, _)| *ordinal);
    assert_eq!(restored_ids, expected_ids);
    assert_eq!(restored.graph().adjacency(), index.graph().adjacency());
    for (restored, original) in restored
        .graph()
        .reverse_adjacency()
        .iter()
        .zip(index.graph().reverse_adjacency())
    {
        let mut restored = restored.clone();
        let mut original = original.clone();
        restored.sort_unstable();
        original.sort_unstable();
        assert_eq!(restored, original);
    }
    assert_eq!(restored.vectors().unwrap(), index.vectors().unwrap());
    assert_eq!(restored.tombstone_count(), 2);
    assert!(restored.is_tombstoned(3));
    assert!(restored.is_tombstoned(29));
    assert_eq!(restored.search(query, 12).unwrap(), expected_search);
}

#[test]
fn portable_container_without_ids_returns_an_empty_mapping() {
    let dimensions = 4;
    let vectors = corpus(12, dimensions);
    let index = VamanaIndex::build(&vectors, config(dimensions)).unwrap();
    let bytes = index.to_bytes(&[]).unwrap();
    let (restored, ids) = VamanaIndex::from_bytes(&bytes).unwrap();
    assert!(ids.is_empty());
    assert_eq!(
        restored.search(&vectors[..dimensions], 5).unwrap(),
        index.search(&vectors[..dimensions], 5).unwrap()
    );
}

#[test]
fn portable_container_rejects_framing_and_checksum_corruption() {
    let dimensions = 4;
    let vectors = corpus(12, dimensions);
    let index = VamanaIndex::build(&vectors, config(dimensions)).unwrap();
    let bytes = index.to_bytes(&[]).unwrap();

    let mut bad_magic = bytes.clone();
    bad_magic[0] ^= 0xff;
    assert!(matches!(
        VamanaIndex::from_bytes(&bad_magic),
        Err(VamanaError::InvalidFormat { .. })
    ));

    let mut bad_version = bytes.clone();
    bad_version[8..12].copy_from_slice(&2u32.to_le_bytes());
    assert!(matches!(
        VamanaIndex::from_bytes(&bad_version),
        Err(VamanaError::InvalidFormat { .. })
    ));

    assert!(matches!(
        VamanaIndex::from_bytes(&bytes[..bytes.len() - 1]),
        Err(VamanaError::InvalidFormat { .. })
    ));

    let mut bad_checksum = bytes;
    let last = bad_checksum.len() - 1;
    bad_checksum[last] ^= 0xff;
    assert!(matches!(
        VamanaIndex::from_bytes(&bad_checksum),
        Err(VamanaError::InvalidFormat { .. })
    ));
}

#[test]
fn fixed_seed_build_has_feature_independent_graph_and_search_digest() {
    let dimensions = 12;
    let vectors = corpus(48, dimensions);
    let index = VamanaIndex::build(&vectors, config(dimensions)).unwrap();
    let mut bytes = index.to_bytes(&[]).unwrap();
    for query_ordinal in [0usize, 7, 19, 41] {
        let query = &vectors[query_ordinal * dimensions..(query_ordinal + 1) * dimensions];
        for (ordinal, distance) in index.search(query, 10).unwrap() {
            bytes.extend_from_slice(&ordinal.to_le_bytes());
            bytes.extend_from_slice(&distance.to_bits().to_le_bytes());
        }
    }
    assert_eq!(
        blake3::hash(&bytes).to_hex().as_str(),
        "19d4c2097fbd58d54920724d5a2749370166e345406e06b80fc71e745b52feee"
    );
}
