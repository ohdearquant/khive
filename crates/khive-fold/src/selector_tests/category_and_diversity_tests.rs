#[test]
fn category_weights_boost_preferred_category() {
    let inputs = vec![
        input_cat("a", 100, 0.9, "low"),
        input_cat("b", 100, 0.5, "high"),
    ];
    let w = SelectorWeights {
        category_weights: [("high".to_string(), 2.0f32), ("low".to_string(), 1.0f32)]
            .into_iter()
            .collect(),
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 100, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "b");
}

#[test]
fn category_weights_can_push_below_min_score() {
    let inputs = vec![
        input_cat("a", 10, 0.4, "bad"),
        input_cat("b", 10, 0.8, "good"),
    ];
    let w = SelectorWeights {
        min_score: 0.3,
        category_weights: [("bad".to_string(), 0.5f32)].into_iter().collect(),
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 1000, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "b");
}

#[test]
fn diversity_bias_zero_identical_to_greedy() {
    let make = || {
        vec![
            input_cat("a", 100, 0.9, "x"),
            input_cat("b", 100, 0.8, "x"),
            input_cat("c", 100, 0.7, "y"),
        ]
    };
    let w_greedy = SelectorWeights {
        ..Default::default()
    };
    let w_bias0 = SelectorWeights {
        diversity_bias: 0.0,
        ..Default::default()
    };
    let out_g = GreedySelector.select(make(), 200, &w_greedy).unwrap();
    let out_b = GreedySelector.select(make(), 200, &w_bias0).unwrap();
    let ids_g: Vec<&str> = out_g.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_b: Vec<&str> = out_b.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_g, ids_b);
}

#[test]
fn diversity_bias_prefers_different_categories() {
    let inputs = vec![
        input_cat("a", 100, 0.9, "x"),
        input_cat("b", 100, 0.8, "x"),
        input_cat("c", 100, 0.7, "y"),
    ];
    let w = SelectorWeights {
        diversity_bias: 1.0,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 200, &w).unwrap();
    assert_eq!(out.selected.len(), 2);
    let ids: Vec<&str> = out.selected.iter().map(|i| i.id.as_str()).collect();
    assert!(ids.contains(&"a"), "a should always be selected");
    assert!(
        ids.contains(&"c"),
        "c should be preferred over b due to diversity"
    );
}

#[test]
fn diversity_path_tie_breaks_id_ascending() {
    let inputs = vec![input("z", 1, 0.5), input("a", 1, 0.5)];
    let w = SelectorWeights {
        diversity_bias: 0.5,
        ..Default::default()
    };

    let out = GreedySelector.select(inputs, 1, &w).unwrap();

    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
}

#[test]
fn no_overflow_near_usize_max() {
    // Items with near-usize::MAX sizes must not overflow when checking budget.
    let large = usize::MAX - 1;
    let inputs = vec![
        SelectorInput {
            id: "a".to_string(),
            content: (),
            size: large,
            score: 0.9,
            category: None,
            information_gain: None,
            rank_score: None,
        },
        SelectorInput {
            id: "b".to_string(),
            content: (),
            size: 10,
            score: 0.8,
            category: None,
            information_gain: None,
            rank_score: None,
        },
    ];
    // Budget is 100 — only item "b" fits.
    let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "b");
}

#[test]
fn diversity_bias_no_categories_unaffected() {
    let inputs = vec![
        input("a", 100, 0.9),
        input("b", 100, 0.8),
        input("c", 100, 0.7),
    ];
    let w = SelectorWeights {
        diversity_bias: 1.0,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 200, &w).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "b");
}
