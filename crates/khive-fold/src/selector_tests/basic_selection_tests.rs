#[test]
fn empty_input() {
    let inputs: Vec<SelectorInput<()>> = vec![];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert!(out.selected.is_empty());
    assert_eq!(out.total_size, 0);
    assert_eq!(out.budget, 1000);
}

#[test]
fn packs_highest_scores_first() {
    let inputs = vec![
        input("a", 100, 0.5),
        input("b", 100, 0.9),
        input("c", 100, 0.7),
    ];
    let out = GreedySelector.select(inputs, 200, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "b");
    assert_eq!(out.selected[1].id, "c");
    assert_eq!(out.total_size, 200);
}

#[test]
fn respects_budget() {
    let inputs = vec![
        input("a", 300, 0.9),
        input("b", 300, 0.8),
        input("c", 300, 0.7),
    ];
    let out = GreedySelector.select(inputs, 500, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.total_size, 300);
}

#[test]
fn filters_below_min_score() {
    let inputs = vec![
        input("a", 10, 0.8),
        input("b", 10, 0.1),
        input("c", 10, 0.5),
    ];
    let out = GreedySelector.select(inputs, 1000, &weights(0.3)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "c");
}

#[test]
fn filters_nan_and_inf() {
    let inputs = vec![
        input("nan", 10, f32::NAN),
        input("inf", 10, f32::INFINITY),
        input("neg_inf", 10, f32::NEG_INFINITY),
        input("ok", 10, 0.5),
    ];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 1);
    assert_eq!(out.selected[0].id, "ok");
}

#[test]
fn tie_break_size_ascending() {
    let inputs = vec![input("big", 200, 0.5), input("small", 50, 0.5)];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert_eq!(out.selected[0].id, "small");
    assert_eq!(out.selected[1].id, "big");
}

#[test]
fn tie_break_id_ascending() {
    let inputs = vec![input("z", 100, 0.5), input("a", 100, 0.5)];
    let out = GreedySelector.select(inputs, 1000, &weights(0.0)).unwrap();
    assert_eq!(out.selected[0].id, "a");
    assert_eq!(out.selected[1].id, "z");
}

#[test]
fn skips_oversized_items_takes_smaller() {
    let inputs = vec![
        input("huge", 900, 0.9),
        input("small1", 40, 0.3),
        input("small2", 40, 0.2),
    ];
    let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.selected[0].id, "small1");
    assert_eq!(out.selected[1].id, "small2");
    assert_eq!(out.total_size, 80);
}

#[test]
fn zero_budget() {
    let inputs = vec![input("a", 1, 0.9)];
    let out = GreedySelector.select(inputs, 0, &weights(0.0)).unwrap();
    assert!(out.selected.is_empty());
}

#[test]
fn deterministic_across_input_order() {
    let a = vec![
        input("x", 50, 0.7),
        input("y", 50, 0.7),
        input("z", 50, 0.7),
    ];
    let b = vec![
        input("z", 50, 0.7),
        input("x", 50, 0.7),
        input("y", 50, 0.7),
    ];
    let out_a = GreedySelector.select(a, 100, &weights(0.0)).unwrap();
    let out_b = GreedySelector.select(b, 100, &weights(0.0)).unwrap();
    let ids_a: Vec<&str> = out_a.selected.iter().map(|i| i.id.as_str()).collect();
    let ids_b: Vec<&str> = out_b.selected.iter().map(|i| i.id.as_str()).collect();
    assert_eq!(ids_a, ids_b);
    assert_eq!(ids_a, vec!["x", "y"]);
}

#[test]
fn exact_budget_fit() {
    let inputs = vec![input("a", 50, 0.9), input("b", 50, 0.8)];
    let out = GreedySelector.select(inputs, 100, &weights(0.0)).unwrap();
    assert_eq!(out.selected.len(), 2);
    assert_eq!(out.total_size, 100);
}
