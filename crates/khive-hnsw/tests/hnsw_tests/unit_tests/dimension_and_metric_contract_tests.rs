#[test]
fn test_dimension_mismatch() {
    let mut index = HnswIndex::new(3);
    let id = make_id(1);

    // Wrong dimension should error
    let result = index.insert(id, vec![1.0, 0.0]);
    assert!(result.is_err());

    // Insert correct dimension
    index.insert(id, vec![1.0, 0.0, 0.0]).expect("insert");

    // Search with wrong dimension should error
    let result = index.search(&[1.0, 0.0], 1);
    assert!(result.is_err());
}

#[test]
fn test_empty_search() {
    let index = HnswIndex::new(3);
    let results = index.search(&[1.0, 0.0, 0.0], 10).expect("search empty");
    assert!(results.is_empty());
}

#[test]
fn test_dot_product_metric() {
    let mut config = HnswConfig::with_dimensions(2);
    config.metric = DistanceMetric::Dot;
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    let id2 = make_id(2);

    index.insert(id1, vec![1.0, 0.0]).expect("insert id1");
    index.insert(id2, vec![2.0, 0.0]).expect("insert id2");

    // For dot product, [2,0] . [1,0] = 2 > [1,0] . [1,0] = 1
    let results = index.search(&[1.0, 0.0], 2).expect("search");
    assert_eq!(results[0].0, id2);
}

#[test]
fn exact_scan_dot_tail_and_batch_have_the_same_score() {
    let vectors = [
        (make_id(1), vec![0.6, 0.8]),
        (make_id(2), vec![0.0, 1.0]),
        (make_id(3), vec![-1.0, 0.0]),
        (make_id(4), vec![0.0, -1.0]),
        (make_id(5), vec![1.0, 0.0]),
    ];

    for metric in [DistanceMetric::Dot, DistanceMetric::Cosine] {
        for best_in_tail in [true, false] {
            let mut config = HnswConfig::with_dimensions(2);
            config.metric = metric;
            let mut index = HnswIndex::with_config(config);
            let order = if best_in_tail {
                [0, 1, 2, 3, 4]
            } else {
                [4, 0, 1, 2, 3]
            };
            for position in order {
                let (id, vector) = &vectors[position];
                index.insert(*id, vector.clone()).expect("insert");
            }

            let results = index.search(&[1.0, 0.0], 1).expect("search");
            assert_eq!(results[0].0, make_id(5), "{metric:?}, tail={best_in_tail}");
            assert_eq!(
                results[0].1.to_f64(),
                1.0,
                "{metric:?}, tail={best_in_tail}"
            );
        }
    }
}

#[test]
fn test_euclidean_metric() {
    let mut config = HnswConfig::with_dimensions(2);
    config.metric = DistanceMetric::L2;
    let mut index = HnswIndex::with_config(config);

    let id1 = make_id(1);
    let id2 = make_id(2);

    index.insert(id1, vec![1.0, 0.0]).expect("insert id1");
    index.insert(id2, vec![10.0, 0.0]).expect("insert id2");

    // Euclidean: closer = higher score
    let results = index.search(&[0.0, 0.0], 2).expect("search");
    assert_eq!(results[0].0, id1); // Closer to origin
}
