use super::*;

#[test]
fn test_fold_fn() {
    let counter = fold_fn(|_ctx| 0usize, |count, _entry: &i32, _ctx| count + 1);
    let entries = [1, 2, 3, 4, 5];
    let result = counter.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, 5);
    assert_eq!(result.entries_processed, 5);
}

#[test]
fn test_fold_fn_sum() {
    let summer = fold_fn(|_ctx| 0i32, |sum, entry: &i32, _ctx| sum + entry);
    let entries = [1, 2, 3, 4, 5];
    let result = summer.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, 15);
}

#[test]
fn test_fold_filtered() {
    let summer = fold_fn(|_ctx| 0i32, |sum, entry: &i32, _ctx| sum + entry);
    let entries = [1, 2, 3, 4, 5, 6];
    let result = summer.derive_filtered(entries.iter(), &FoldContext::new(), |e| *e % 2 == 0);
    assert_eq!(result.state, 12);
    assert_eq!(result.entries_processed, 3);
}

#[test]
fn test_boxed_fold_derive() {
    // REASON: box_default fires because CountFold implements Default, but the test
    // explicitly exercises the BoxedFold type alias which requires Box::new().
    #[allow(clippy::box_default)]
    let counter: BoxedFold<i32, usize> = Box::new(CountFold::new());
    let entries = [1, 2, 3, 4];
    let result = counter.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, 4);
}

#[test]
fn test_common_fold_count() {
    let fold = CommonFold::<i32>::count();
    let entries = [1, 2, 3];
    let result = fold.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, CommonFoldState::Count(3));
}

#[test]
fn test_common_fold_sum() {
    let fold = CommonFold::<i32>::sum_i64(|value: &i32| *value as i64);
    let entries = [1, 2, 3];
    let result = fold.derive(entries.iter(), &FoldContext::new());
    assert_eq!(result.state, CommonFoldState::SumI64(6));
}

#[test]
fn count_folds_saturate_on_overflow() {
    let context = FoldContext::new();
    let entry = 1;

    let count = CountFold::new();
    assert_eq!(count.reduce(usize::MAX, &entry, &context), usize::MAX);

    let filtered = FilterCountFold::new(|_: &i32| true);
    assert_eq!(filtered.reduce(usize::MAX, &entry, &context), usize::MAX);
}

#[test]
fn sum_i64_fold_saturates_on_overflow() {
    let context = FoldContext::new();
    let fold = SumI64Fold::new(|value: &i64| *value);
    assert_eq!(fold.reduce(i64::MAX, &1, &context), i64::MAX);
}

#[test]
fn common_fold_try_step_mismatch_returns_error() {
    let context = FoldContext::new();
    let fold = CommonFold::<i32>::count();
    let err = TryFold::try_step(&fold, CommonFoldState::SumI64(0), &1, &context).unwrap_err();
    assert_eq!(
        err,
        FoldFailure::StateMismatch {
            expected: "Count",
            actual: "SumI64"
        }
    );
}

#[test]
fn test_any_fold() {
    let fold = AnyFold::new(|value: &i32| *value == 7);
    let entries = [1, 2, 7, 9];
    let result = fold.derive(entries.iter(), &FoldContext::new());
    assert!(result.state);
}

#[test]
fn fold_is_deterministic_no_timing() {
    // Same inputs must produce equal FoldOutcome (PartialEq holds).
    let fold = fold_fn(|_ctx| 0usize, |c, _: &i32, _ctx| c + 1);
    let entries = [1, 2, 3];
    let ctx = FoldContext::new();
    let a = fold.derive(entries.iter(), &ctx);
    let b = fold.derive(entries.iter(), &ctx);
    assert_eq!(a, b);
}
