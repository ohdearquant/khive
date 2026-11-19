use super::{HnswConfig, HnswIndex};
use crate::{config::DistanceMetric, NodeId};
use rand::{rngs::StdRng, Rng, SeedableRng};

#[test]
fn seeded_rng_matches_rand_standard_stream() {
    for seed in [0, 42, u64::MAX] {
        let mut index = HnswIndex::with_config(HnswConfig::with_dimensions(3).with_seed(seed));
        let mut standard = StdRng::seed_from_u64(seed);
        // Mixed draws cross both the 16-word ChaCha block and 64-word buffer.
        for _ in 0..96 {
            assert_eq!(index.rng.next_u32(), standard.next_u32());
            assert_eq!(index.rng.next_u64(), standard.next_u64());
            let mut actual = [0; 5];
            let mut expected = [0; 5];
            index.rng.fill_bytes(&mut actual);
            standard.fill_bytes(&mut expected);
            assert_eq!(actual, expected);
        }
    }
}

#[test]
fn clone_preserves_partially_consumed_rng_stream() {
    for stream in [0, 0xfedc_ba98_7654_3210] {
        for consumed_words in [0, 1, 15, 16, 17, 63, 64, 65] {
            let mut original = HnswIndex::with_config(HnswConfig::with_dimensions(3).with_seed(42));
            original.rng.set_stream(stream);
            for _ in 0..consumed_words {
                let _ = original.rng.next_u32();
            }
            let position = original.rng.get_word_pos();
            assert_eq!(position, consumed_words as u128);
            let mut cloned = original.clone();
            assert_eq!(cloned.rng.get_seed(), original.rng.get_seed());
            assert_eq!(cloned.rng.get_stream(), stream);
            assert_eq!(cloned.rng.get_word_pos(), position);

            // Advance one side first: independent cursors must yield the same continuation.
            let expected: Vec<_> = (0..129).map(|_| original.rng.next_u64()).collect();
            assert_eq!(cloned.rng.get_word_pos(), position);
            let actual: Vec<_> = (0..129).map(|_| cloned.rng.next_u64()).collect();
            assert_eq!(actual, expected, "stream {stream}, word {position}");
            assert_eq!(cloned.rng.get_word_pos(), original.rng.get_word_pos());
        }
    }
}

fn insert(index: &mut HnswIndex, id: u8) {
    index
        .insert(
            NodeId::new([id; 16]),
            vec![f32::from(id), f32::from(id).powi(2), f32::from(id % 3)],
        )
        .unwrap();
}

#[test]
fn cloned_populated_index_preserves_insertion_continuation() {
    let mut config = HnswConfig::with_dimensions(3).with_seed(42);
    config.metric = DistanceMetric::L2;
    let mut original = HnswIndex::with_config(config);
    for id in 0..8 {
        insert(&mut original, id);
    }
    // Insertions consume f64 draws; leave an odd word cursor for the next draw.
    let _ = original.rng.next_u32();
    assert_eq!(original.rng.get_word_pos(), 17);
    let mut cloned = original.clone();
    for id in 8..40 {
        insert(&mut original, id);
        insert(&mut cloned, id);
    }
    assert_eq!(cloned.entry_point, original.entry_point);
    assert_eq!(cloned.max_level, original.max_level);
    assert_eq!(cloned.internal_to_id, original.internal_to_id);
    assert_eq!(cloned.nodes.len(), original.nodes.len());
    for (actual, expected) in cloned.nodes.iter().zip(&original.nodes) {
        assert_eq!(actual.max_layer, expected.max_layer);
        assert_eq!(actual.neighbors, expected.neighbors);
        assert_eq!(actual.vector, expected.vector);
    }
    assert_eq!(cloned.rng.next_u64(), original.rng.next_u64());
}
