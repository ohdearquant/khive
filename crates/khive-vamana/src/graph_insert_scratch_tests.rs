use super::{new_visited_set, VISITED_SET_ALLOCATIONS};
use crate::{VamanaConfig, VamanaError, VamanaIndex};
use rand::{rngs::StdRng, RngExt, SeedableRng};

fn fixture() -> (VamanaIndex, Vec<f32>) {
    let mut rng = StdRng::seed_from_u64(0x3775);
    let vectors: Vec<f32> = (0..24 * 4).map(|_| rng.random_range(-1.0..1.0)).collect();
    let config = VamanaConfig::with_dimensions(4)
        .with_max_degree(8)
        .with_search_list_size(16);
    (VamanaIndex::build(&vectors, config).unwrap(), vectors)
}

fn allocation_count() -> usize {
    VISITED_SET_ALLOCATIONS.with(|count| count.get())
}

fn non_medoid(index: &VamanaIndex) -> u32 {
    (0..index.num_vectors() as u32)
        .find(|&ordinal| ordinal != index.graph().medoid())
        .unwrap()
}

fn result_bits(results: Vec<(u32, f32)>) -> Vec<(u32, u32)> {
    results
        .into_iter()
        .map(|(ordinal, distance)| (ordinal, distance.to_bits()))
        .collect()
}

fn assert_same_index(left: &VamanaIndex, right: &VamanaIndex) {
    assert_eq!(left.num_vectors(), right.num_vectors());
    assert_eq!(left.tombstone_count(), right.tombstone_count());
    assert_eq!(
        left.ops_since_consolidation(),
        right.ops_since_consolidation()
    );
    assert_eq!(left.graph().medoid(), right.graph().medoid());
    assert_eq!(left.graph().adjacency(), right.graph().adjacency());
    assert_eq!(
        left.graph().reverse_adjacency(),
        right.graph().reverse_adjacency()
    );
    for ordinal in 0..left.num_vectors() as u32 {
        assert_eq!(left.is_tombstoned(ordinal), right.is_tombstoned(ordinal));
    }
    let vector_bits = |index: &VamanaIndex| {
        index
            .vectors()
            .unwrap()
            .iter()
            .map(|value| value.to_bits())
            .collect::<Vec<_>>()
    };
    assert_eq!(vector_bits(left), vector_bits(right));
}

#[test]
fn recycled_insert_reuses_warmed_visited_without_construction() {
    let (mut index, vectors) = fixture();
    let before_control = allocation_count();
    drop(new_visited_set(index.num_vectors()));
    assert_eq!(allocation_count(), before_control + 1);
    assert!(!index.search(&vectors[..4], 8).unwrap().is_empty());

    for vector in [
        [0.2, -0.3, 0.4, -0.5],
        [3.0, -2.0, 1.0, 0.0],
        [-0.4, 0.3, -0.2, 0.1],
    ] {
        let ordinal = non_medoid(&index);
        index.tombstone(ordinal).unwrap();
        let before_insert = allocation_count();
        assert_eq!(index.insert(&vector).unwrap(), ordinal);
        assert_eq!(
            allocation_count(),
            before_insert,
            "recycled insert allocated a tracker"
        );
        assert!(!index.search(&vector, 8).unwrap().is_empty());
        assert_eq!(allocation_count(), before_insert);
    }
}

#[test]
fn appended_insert_retains_resized_visited_for_the_next_search() {
    let (mut index, vectors) = fixture();
    index.search(&vectors[..4], 8).unwrap();
    for vector in [[0.2, -0.3, 0.4, -0.5], [3.0, -2.0, 1.0, 0.0]] {
        let ordinal = index.num_vectors() as u32;
        let before_insert = allocation_count();
        assert_eq!(index.insert(&vector).unwrap(), ordinal);
        assert_eq!(allocation_count(), before_insert + 1);
        assert!(!index.search(&vector, 8).unwrap().is_empty());
        assert_eq!(
            allocation_count(),
            before_insert + 1,
            "search allocated after insert had sized scratch"
        );
    }
}

#[test]
fn reused_and_fresh_insert_scratch_preserve_graph_and_result_bits() {
    let (mut cold, vectors) = fixture();
    let (mut warm, _) = fixture();
    let insertions = [
        [0.2, -0.3, 0.4, -0.5],
        [3.0, -2.0, 1.0, 0.0],
        [-0.4, 0.3, -0.2, 0.1],
        [0.0, 0.0, 0.0, 0.0],
    ];
    for (step, vector) in insertions.into_iter().enumerate() {
        warm.search(&vectors[..4], 8).unwrap();
        if step % 2 == 0 {
            let ordinal = non_medoid(&cold);
            cold.tombstone(ordinal).unwrap();
            warm.tombstone(ordinal).unwrap();
        }
        assert_eq!(cold.insert(&vector).unwrap(), warm.insert(&vector).unwrap());
        assert_same_index(&cold, &warm);
        for query in [&vectors[..4], vector.as_slice(), &[20.0; 4]] {
            assert_eq!(
                result_bits(cold.search(query, 24).unwrap()),
                result_bits(warm.search(query, 24).unwrap())
            );
        }
    }
}

#[test]
fn rejected_insert_keeps_state_and_does_not_construct_visited() {
    let (mut index, vectors) = fixture();
    let (unchanged, _) = fixture();
    index.search(&vectors[..4], 8).unwrap();
    let before = allocation_count();
    assert!(matches!(
        index.insert(&[f32::NAN]),
        Err(VamanaError::DimensionMismatch {
            expected: 4,
            actual: 1
        })
    ));
    assert_same_index(&index, &unchanged);
    assert_eq!(allocation_count(), before);
    for value in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        assert!(matches!(
            index.insert(&[value, 0.0, 0.0, 0.0]),
            Err(VamanaError::NonFiniteFloat { location, .. }) if location == "insert vector"
        ));
        assert_same_index(&index, &unchanged);
        assert_eq!(allocation_count(), before);
    }
}
