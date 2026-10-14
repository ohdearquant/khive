use super::*;
use crate::fold::fold_fn;

#[test]
fn test_filter_fold() {
    let counter = fold_fn(|_ctx| 0usize, |count, _entry: &i32, _ctx| count + 1);
    let filtered = filter(counter, |e: &i32| *e % 2 == 0);
    let entries = [1, 2, 3, 4, 5, 6];
    let result = filtered.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, 3);
}

#[test]
fn test_map_fold() {
    let summer = fold_fn(|_ctx| 0i32, |sum, entry: &i32, _ctx| sum + entry);
    let doubled = map(summer, |e: &i32| e * 2);
    let entries = [1, 2, 3];
    let result = doubled.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, 12);
}

#[test]
fn test_dual_fold() {
    let summer = fold_fn(|_ctx| 0i32, |sum, entry: &i32, _ctx| sum + entry);
    let counter = fold_fn(|_ctx| 0usize, |count, _entry: &i32, _ctx| count + 1);
    let dual = DualFold::new(summer, counter);
    let entries = [1, 2, 3, 4, 5];
    let (sum_result, count_result) = dual.execute(entries.iter(), &FoldContext::new());
    assert_eq!(sum_result.state, 15);
    assert_eq!(count_result.state, 5);
}

#[test]
fn test_sequential_fold() {
    let counter = fold_fn(|_ctx| 0usize, |count, _entry: &i32, _ctx| count + 1);
    let summer = fold_fn(
        |ctx: &FoldContext| ctx.extra.get("count").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
        |sum, entry: &i32, _ctx| sum + entry,
    );
    let sequential = SequentialFold::new(counter, summer, |count, ctx| {
        let mut new_ctx = ctx.clone();
        *new_ctx.extra_mut() = serde_json::json!({"count": *count});
        new_ctx
    });
    let entries = [1, 2, 3];
    let (count_result, sum_result) = sequential.execute(entries.iter(), &FoldContext::new());
    assert_eq!(count_result.state, 3);
    assert_eq!(sum_result.state, 9);
}
