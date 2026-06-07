//! Fold composition utilities

use crate::{Fold, FoldContext, FoldOutcome};

/// Sequential fold — run one fold, then use its output to inform another.
pub struct SequentialFold<L, S1, S2, F1, F2, M>
where
    F1: Fold<L, S1>,
    F2: Fold<L, S2>,
    M: Fn(&S1, &FoldContext) -> FoldContext,
{
    first: F1,
    second: F2,
    context_mapper: M,
    _phantom: std::marker::PhantomData<(L, S1, S2)>,
}

impl<L, S1, S2, F1, F2, M> SequentialFold<L, S1, S2, F1, F2, M>
where
    F1: Fold<L, S1>,
    F2: Fold<L, S2>,
    M: Fn(&S1, &FoldContext) -> FoldContext,
{
    /// Create a sequential fold.
    pub fn new(first: F1, second: F2, context_mapper: M) -> Self {
        Self {
            first,
            second,
            context_mapper,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Execute the sequential fold.
    pub fn execute<'a, I>(
        &self,
        entries: I,
        context: &FoldContext,
    ) -> (FoldOutcome<S1>, FoldOutcome<S2>)
    where
        I: IntoIterator<Item = &'a L> + Clone,
        L: 'a,
    {
        let result1 = self.first.derive(entries.clone(), context);
        let context2 = (self.context_mapper)(&result1.state, context);
        let result2 = self.second.derive(entries, &context2);
        (result1, result2)
    }
}

/// Dual fold — run two independent folds over the same entries sequentially.
pub struct DualFold<L, S1, S2, F1, F2>
where
    F1: Fold<L, S1>,
    F2: Fold<L, S2>,
{
    fold1: F1,
    fold2: F2,
    _phantom: std::marker::PhantomData<(L, S1, S2)>,
}

impl<L, S1, S2, F1, F2> DualFold<L, S1, S2, F1, F2>
where
    F1: Fold<L, S1>,
    F2: Fold<L, S2>,
{
    /// Create a dual fold.
    pub fn new(fold1: F1, fold2: F2) -> Self {
        Self {
            fold1,
            fold2,
            _phantom: std::marker::PhantomData,
        }
    }

    /// Execute both folds over the same entries.
    pub fn execute<'a, I>(
        &self,
        entries: I,
        context: &FoldContext,
    ) -> (FoldOutcome<S1>, FoldOutcome<S2>)
    where
        I: IntoIterator<Item = &'a L> + Clone,
        L: 'a,
    {
        let result1 = self.fold1.derive(entries.clone(), context);
        let result2 = self.fold2.derive(entries, context);
        (result1, result2)
    }
}

/// Filter fold — only process entries matching a predicate.
pub struct FilterFold<L, S, F, P>
where
    F: Fold<L, S>,
    P: Fn(&L) -> bool,
{
    inner: F,
    predicate: P,
    _phantom: std::marker::PhantomData<(L, S)>,
}

impl<L, S, F, P> FilterFold<L, S, F, P>
where
    F: Fold<L, S>,
    P: Fn(&L) -> bool,
{
    /// Create a filter fold.
    pub fn new(inner: F, predicate: P) -> Self {
        Self {
            inner,
            predicate,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<L, S, F, P> Fold<L, S> for FilterFold<L, S, F, P>
where
    L: Send + Sync,
    S: Send + Sync,
    F: Fold<L, S>,
    P: Fn(&L) -> bool + Send + Sync,
{
    fn init(&self, context: &FoldContext) -> S {
        self.inner.init(context)
    }

    fn reduce(&self, state: S, entry: &L, context: &FoldContext) -> S {
        if (self.predicate)(entry) {
            self.inner.reduce(state, entry, context)
        } else {
            state
        }
    }

    fn finalize(&self, state: S, context: &FoldContext) -> S {
        self.inner.finalize(state, context)
    }
}

/// Map fold — transform entries before folding.
pub struct MapFold<L1, L2, S, F, M>
where
    F: Fold<L2, S>,
    M: Fn(&L1) -> L2,
{
    inner: F,
    mapper: M,
    _phantom: std::marker::PhantomData<(L1, L2, S)>,
}

impl<L1, L2, S, F, M> MapFold<L1, L2, S, F, M>
where
    F: Fold<L2, S>,
    M: Fn(&L1) -> L2,
{
    /// Create a map fold.
    pub fn new(inner: F, mapper: M) -> Self {
        Self {
            inner,
            mapper,
            _phantom: std::marker::PhantomData,
        }
    }
}

impl<L1, L2, S, F, M> Fold<L1, S> for MapFold<L1, L2, S, F, M>
where
    L1: Send + Sync,
    L2: Send + Sync,
    S: Send + Sync,
    F: Fold<L2, S>,
    M: Fn(&L1) -> L2 + Send + Sync,
{
    fn init(&self, context: &FoldContext) -> S {
        self.inner.init(context)
    }

    fn reduce(&self, state: S, entry: &L1, context: &FoldContext) -> S {
        let mapped = (self.mapper)(entry);
        self.inner.reduce(state, &mapped, context)
    }

    fn finalize(&self, state: S, context: &FoldContext) -> S {
        self.inner.finalize(state, context)
    }
}

/// Helper to create a filter fold.
pub fn filter<L, S, F, P>(inner: F, predicate: P) -> FilterFold<L, S, F, P>
where
    F: Fold<L, S>,
    P: Fn(&L) -> bool,
{
    FilterFold::new(inner, predicate)
}

/// Helper to create a map fold.
pub fn map<L1, L2, S, F, M>(inner: F, mapper: M) -> MapFold<L1, L2, S, F, M>
where
    F: Fold<L2, S>,
    M: Fn(&L1) -> L2,
{
    MapFold::new(inner, mapper)
}

#[cfg(test)]
mod tests {
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
}
