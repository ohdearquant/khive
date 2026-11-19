use khive_vamana::{VamanaConfig, VamanaError, VamanaIndex};

#[test]
fn allocating_search_is_available_from_the_production_library() {
    let index = VamanaIndex::build(
        &[1.0, 0.0, 0.0, 1.0, -1.0, 0.0],
        VamanaConfig::with_dimensions(2).with_max_degree(2),
    )
    .unwrap();
    let actual = index.search_allocating(&[1.0, 0.0], 10).unwrap();
    assert_eq!(actual.len(), 3);
    assert_eq!(actual[0], (0, 0.0));
    let bits = |pairs: Vec<(u32, f32)>| {
        pairs
            .into_iter()
            .map(|(id, distance)| (id, distance.to_bits()))
            .collect::<Vec<_>>()
    };
    assert_eq!(bits(actual), bits(index.search(&[1.0, 0.0], 10).unwrap()));
}

#[test]
fn allocating_and_pooled_search_preserve_validation_order() {
    let index = VamanaIndex::build(
        &[1.0, 0.0, 0.0, 1.0, -1.0, 0.0],
        VamanaConfig::with_dimensions(2).with_max_degree(2),
    )
    .unwrap();
    for allocating in [false, true] {
        let search = |query: &[f32], k| {
            if allocating {
                index.search_allocating(query, k)
            } else {
                index.search(query, k)
            }
        };
        for k in [0, 1] {
            assert!(matches!(
                search(&[f32::NAN], k),
                Err(VamanaError::DimensionMismatch {
                    expected: 2,
                    actual: 1
                })
            ));
        }
        for nonfinite in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
            assert!(search(&[nonfinite, 0.0], 0).unwrap().is_empty());
            assert!(matches!(
                search(&[nonfinite, 0.0], 1),
                Err(VamanaError::NonFiniteFloat { location, .. }) if location == "search query"
            ));
        }
        assert!(search(&[1.0, 0.0], 0).unwrap().is_empty());
    }
}
