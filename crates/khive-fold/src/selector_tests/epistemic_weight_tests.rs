#[test]
fn epistemic_weight_zero_preserves_behavior() {
    // With epistemic_weight=0, result must be identical to the default (no epistemic).
    let make = || {
        vec![
            input_with_gain("a", 100, 0.9, 10.0),
            input_with_gain("b", 100, 0.8, 0.0),
            input_with_gain("c", 100, 0.7, 5.0),
        ]
    };
    let w_default = SelectorWeights {
        ..Default::default()
    };
    let w_zero = SelectorWeights {
        epistemic_weight: 0.0,
        ..Default::default()
    };
    let out_d = GreedySelector.select(make(), 200, &w_default).unwrap();
    let out_z = GreedySelector.select(make(), 200, &w_zero).unwrap();
    let ids_d: Vec<&str> = out_d.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_z: Vec<&str> = out_z.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_d, ids_z);
    // Pure score order: a (0.9), b (0.8).
    assert_eq!(ids_d, vec!["a", "b"]);
}

#[test]
fn epistemic_weight_positive_reorders_by_gain() {
    // a: score=0.5, gain=10.0  → effective = 0.5 + 1.0 * 10.0 = 10.5
    // b: score=0.9, gain=0.0   → effective = 0.9 + 1.0 * 0.0  = 0.9
    // With epistemic_weight=1.0, a should be selected first.
    let inputs = vec![
        input_with_gain("a", 100, 0.5, 10.0),
        input_with_gain("b", 100, 0.9, 0.0),
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 100, &w).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
}

#[test]
fn information_gain_none_equivalent_to_zero() {
    // None and Some(0.0) must produce identical ordering.
    let with_none = vec![
        input("a", 100, 0.9), // information_gain: None
        input("b", 100, 0.8),
    ];
    let with_zero = vec![
        input_with_gain("a", 100, 0.9, 0.0),
        input_with_gain("b", 100, 0.8, 0.0),
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        ..Default::default()
    };
    let out_none = GreedySelector.select(with_none, 200, &w).unwrap();
    let out_zero = GreedySelector.select(with_zero, 200, &w).unwrap();
    let ids_none: Vec<&str> = out_none.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_zero: Vec<&str> = out_zero.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_none, ids_zero);
}

#[test]
fn epistemic_weight_works_with_diversity_bias() {
    // Combines epistemic and diversity: the effective score incorporates both.
    // a: score=0.5, gain=10.0, category=x → base effective = 0.5 + 1.0 * 10.0 = 10.5
    // b: score=0.8, gain=0.0,  category=x → base effective = 0.8
    // c: score=0.3, gain=0.0,  category=y → base effective = 0.3
    // Budget=200, bias=0.5: a selected first (10.5 wins), then after a is in x,
    // b's diversity penalty is 0.8*(1-0.5*1/2)=0.8*0.75=0.6 vs c at 0.3 — b wins.
    let inputs = vec![
        {
            let mut i = input_with_gain("a", 100, 0.5, 10.0);
            i.category = Some("x".to_string());
            i
        },
        {
            let mut i = input_with_gain("b", 100, 0.8, 0.0);
            i.category = Some("x".to_string());
            i
        },
        {
            let mut i = input_with_gain("c", 100, 0.3, 0.0);
            i.category = Some("y".to_string());
            i
        },
    ];
    let w = SelectorWeights {
        epistemic_weight: 1.0,
        diversity_bias: 0.5,
        ..Default::default()
    };
    let out = GreedySelector.select(inputs, 200, &w).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    // b (eff=0.8*0.75=0.6) > c (eff=0.3) after a is placed in category x.
    assert_eq!(out.selected[1].id, "b");
}
