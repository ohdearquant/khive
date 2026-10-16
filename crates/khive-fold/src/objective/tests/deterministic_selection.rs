#[test]
fn test_deterministic_select_basic() {
    let objective = objective_fn(|c: &TestCandidate, _ctx: &ObjectiveContext| c.value as f64);

    let candidates = vec![
        TestCandidate::new(1),
        TestCandidate::new(5),
        TestCandidate::new(3),
    ];

    let selection = objective
        .select_deterministic(&candidates, &ObjectiveContext::new())
        .unwrap();

    assert_eq!(selection.item.value, 5);
    assert_eq!(selection.score, 5.0);
}

#[test]
fn test_deterministic_select_equal_scores_uses_uuid_tiebreaker() {
    let objective = objective_fn(|_c: &TestCandidate, _ctx: &ObjectiveContext| 1.0);

    let id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let id3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();

    let candidates = vec![
        TestCandidate::with_id(id2, 100),
        TestCandidate::with_id(id3, 200),
        TestCandidate::with_id(id1, 300),
    ];

    let selection = objective
        .select_deterministic(&candidates, &ObjectiveContext::new())
        .unwrap();

    assert_eq!(selection.item.id, id1);
    assert_eq!(selection.item.value, 300);
}

#[test]
fn test_deterministic_select_top_ordering() {
    let objective = objective_fn(|_c: &TestCandidate, _ctx: &ObjectiveContext| 1.0);

    let id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let id3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();

    let candidates = vec![
        TestCandidate::with_id(id3, 300),
        TestCandidate::with_id(id1, 100),
        TestCandidate::with_id(id2, 200),
    ];

    let top = objective.select_top_deterministic(&candidates, 3, &ObjectiveContext::new());

    assert_eq!(top.len(), 3);
    assert_eq!(top[0].item.id, id1);
    assert_eq!(top[1].item.id, id2);
    assert_eq!(top[2].item.id, id3);
}

#[test]
fn test_deterministic_reproducibility() {
    let objective = objective_fn(|_c: &TestCandidate, _ctx: &ObjectiveContext| 1.0);

    let id1 = Uuid::parse_str("00000000-0000-0000-0000-000000000001").unwrap();
    let id2 = Uuid::parse_str("00000000-0000-0000-0000-000000000002").unwrap();
    let id3 = Uuid::parse_str("00000000-0000-0000-0000-000000000003").unwrap();

    let candidates = vec![
        TestCandidate::with_id(id2, 1),
        TestCandidate::with_id(id3, 2),
        TestCandidate::with_id(id1, 3),
    ];

    for _ in 0..100 {
        let selection = objective
            .select_deterministic(&candidates, &ObjectiveContext::new())
            .unwrap();
        assert_eq!(selection.item.id, id1, "Determinism violated!");

        let top = objective.select_top_deterministic(&candidates, 3, &ObjectiveContext::new());
        assert_eq!(top[0].item.id, id1);
        assert_eq!(top[1].item.id, id2);
        assert_eq!(top[2].item.id, id3);
    }
}
