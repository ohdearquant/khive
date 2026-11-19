#[test]
fn test_simple_objective() {
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let candidates = vec![1, 5, 3, 8, 2];
    let selection = objective
        .select(&candidates, &ObjectiveContext::new())
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 8);
    assert_eq!(selection.score, 8.0);
    assert_eq!(selection.index, 3);
}

#[test]
fn test_threshold() {
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let candidates = vec![1, 5, 3, 8, 2];
    let context = ObjectiveContext::new().with_min_score(4.0);
    let selection = objective
        .select(&candidates, &context)
        .into_iter()
        .next()
        .unwrap();

    assert_eq!(*selection.item, 8);
    assert_eq!(selection.passed, 2);
}

#[test]
fn test_no_candidates() {
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let candidates: Vec<i32> = vec![];
    let result = objective.select(&candidates, &ObjectiveContext::new());

    assert!(result.is_empty());
}

#[test]
fn test_no_match() {
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let candidates = vec![1, 2, 3];
    let context = ObjectiveContext::new().with_min_score(10.0);
    let result = objective.select(&candidates, &context);

    assert!(result.is_empty());
}

#[test]
fn test_select_top() {
    let objective = objective_fn(|n: &i32, _ctx: &ObjectiveContext| *n as f64);

    let candidates = vec![1, 5, 3, 8, 2];
    let top = objective.select_top(&candidates, 3, &ObjectiveContext::new());

    assert_eq!(top.len(), 3);
    assert_eq!(*top[0].item, 8);
    assert_eq!(*top[1].item, 5);
    assert_eq!(*top[2].item, 3);
}
