//! Core Fold trait

use std::marker::PhantomData;
use std::sync::Arc;

use crate::{FoldContext, FoldOutcome};

/// Core fold trait: collapses a sequence of entries into deterministic derived state.
pub trait Fold<L, S>: Send + Sync {
    /// Get the initial state before any entries are processed.
    fn init(&self, context: &FoldContext) -> S;

    /// Process a single entry and return the new state.
    fn reduce(&self, state: S, entry: &L, context: &FoldContext) -> S;

    /// Finalize the state after all entries are processed; default returns state unchanged.
    #[inline]
    fn finalize(&self, state: S, _context: &FoldContext) -> S {
        state
    }

    /// Derive state from an iterator of entries.
    fn derive<'a, I>(&self, entries: I, context: &FoldContext) -> FoldOutcome<S>
    where
        Self: Sized,
        I: IntoIterator<Item = &'a L>,
        L: 'a,
    {
        let mut state = self.init(context);
        let mut count = 0;

        for entry in entries {
            state = self.reduce(state, entry, context);
            count += 1;
        }

        FoldOutcome::new(self.finalize(state, context), count)
    }

    /// Derive state with a filter.
    fn derive_filtered<'a, I, F>(
        &self,
        entries: I,
        context: &FoldContext,
        filter: F,
    ) -> FoldOutcome<S>
    where
        Self: Sized,
        I: IntoIterator<Item = &'a L>,
        L: 'a,
        F: Fn(&L) -> bool,
    {
        let mut state = self.init(context);
        let mut count = 0;

        for entry in entries {
            if filter(entry) {
                state = self.reduce(state, entry, context);
                count += 1;
            }
        }

        FoldOutcome::new(self.finalize(state, context), count)
    }
}

/// Failure returned by fallible fold operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum FoldFailure {
    /// The supplied state variant does not match the fold variant.
    #[error("Fold state mismatch: expected {expected}, got {actual}")]
    StateMismatch {
        /// State variant expected by the fold.
        expected: &'static str,
        /// State variant supplied by the caller.
        actual: &'static str,
    },
}

/// Fallible fold step API for reducers that can reject invalid state shapes.
pub trait TryFold<L, S>: Fold<L, S> {
    /// Process a single entry and return an error instead of panicking.
    fn try_step(&self, state: S, entry: &L, context: &FoldContext) -> Result<S, FoldFailure>;
}

impl<L, S, T> Fold<L, S> for Box<T>
where
    T: Fold<L, S> + ?Sized,
{
    #[inline]
    fn init(&self, context: &FoldContext) -> S {
        (**self).init(context)
    }

    #[inline]
    fn reduce(&self, state: S, entry: &L, context: &FoldContext) -> S {
        (**self).reduce(state, entry, context)
    }

    #[inline]
    fn finalize(&self, state: S, context: &FoldContext) -> S {
        (**self).finalize(state, context)
    }
}

impl<L, S, T> TryFold<L, S> for Box<T>
where
    T: TryFold<L, S> + ?Sized,
{
    #[inline]
    fn try_step(&self, state: S, entry: &L, context: &FoldContext) -> Result<S, FoldFailure> {
        (**self).try_step(state, entry, context)
    }
}

impl<L, S, T> Fold<L, S> for Arc<T>
where
    T: Fold<L, S> + ?Sized,
{
    #[inline]
    fn init(&self, context: &FoldContext) -> S {
        (**self).init(context)
    }

    #[inline]
    fn reduce(&self, state: S, entry: &L, context: &FoldContext) -> S {
        (**self).reduce(state, entry, context)
    }

    #[inline]
    fn finalize(&self, state: S, context: &FoldContext) -> S {
        (**self).finalize(state, context)
    }
}

impl<L, S, T> TryFold<L, S> for Arc<T>
where
    T: TryFold<L, S> + ?Sized,
{
    #[inline]
    fn try_step(&self, state: S, entry: &L, context: &FoldContext) -> Result<S, FoldFailure> {
        (**self).try_step(state, entry, context)
    }
}

/// A boxed fold for dynamic dispatch.
pub type BoxedFold<L, S> = Box<dyn Fold<L, S> + Send + Sync>;

/// Helper to create a fold from closures.
pub struct FnFold<L, S, I, St, F>
where
    I: Fn(&FoldContext) -> S,
    St: Fn(S, &L, &FoldContext) -> S,
    F: Fn(S, &FoldContext) -> S,
{
    initial_fn: I,
    step_fn: St,
    finalize_fn: F,
    _phantom: PhantomData<(L, S)>,
}

impl<L, S, I, St, F> FnFold<L, S, I, St, F>
where
    I: Fn(&FoldContext) -> S,
    St: Fn(S, &L, &FoldContext) -> S,
    F: Fn(S, &FoldContext) -> S,
{
    /// Create a new FnFold.
    pub fn new(initial: I, step: St, finalize: F) -> Self {
        Self {
            initial_fn: initial,
            step_fn: step,
            finalize_fn: finalize,
            _phantom: PhantomData,
        }
    }
}

impl<L, S, I, St, F> Fold<L, S> for FnFold<L, S, I, St, F>
where
    L: Send + Sync,
    S: Send + Sync,
    I: Fn(&FoldContext) -> S + Send + Sync,
    St: Fn(S, &L, &FoldContext) -> S + Send + Sync,
    F: Fn(S, &FoldContext) -> S + Send + Sync,
{
    #[inline]
    fn init(&self, context: &FoldContext) -> S {
        (self.initial_fn)(context)
    }

    #[inline]
    fn reduce(&self, state: S, entry: &L, context: &FoldContext) -> S {
        (self.step_fn)(state, entry, context)
    }

    #[inline]
    fn finalize(&self, state: S, context: &FoldContext) -> S {
        (self.finalize_fn)(state, context)
    }
}

impl<L, S, I, St, F> TryFold<L, S> for FnFold<L, S, I, St, F>
where
    L: Send + Sync,
    S: Send + Sync,
    I: Fn(&FoldContext) -> S + Send + Sync,
    St: Fn(S, &L, &FoldContext) -> S + Send + Sync,
    F: Fn(S, &FoldContext) -> S + Send + Sync,
{
    #[inline]
    fn try_step(&self, state: S, entry: &L, context: &FoldContext) -> Result<S, FoldFailure> {
        Ok((self.step_fn)(state, entry, context))
    }
}

/// Create a fold from just initial and step functions (no finalize).
pub fn fold_fn<L, S, I, St>(initial: I, step: St) -> impl Fold<L, S>
where
    L: Send + Sync,
    S: Send + Sync,
    I: Fn(&FoldContext) -> S + Send + Sync,
    St: Fn(S, &L, &FoldContext) -> S + Send + Sync,
{
    FnFold::new(initial, step, |s, _| s)
}

/// A zero-allocation count fold.
#[derive(Debug, Clone, Copy)]
pub struct CountFold<L> {
    _phantom: PhantomData<fn(&L)>,
}

impl<L> CountFold<L> {
    /// Create a new count fold.
    #[must_use]
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }
}

impl<L> Default for CountFold<L> {
    fn default() -> Self {
        Self::new()
    }
}

impl<L> Fold<L, usize> for CountFold<L> {
    #[inline]
    fn init(&self, _context: &FoldContext) -> usize {
        0
    }

    #[inline]
    fn reduce(&self, state: usize, _entry: &L, _context: &FoldContext) -> usize {
        state.saturating_add(1)
    }
}

impl<L> TryFold<L, usize> for CountFold<L> {
    #[inline]
    fn try_step(
        &self,
        state: usize,
        entry: &L,
        context: &FoldContext,
    ) -> Result<usize, FoldFailure> {
        Ok(self.reduce(state, entry, context))
    }
}

/// A zero-allocation count fold with a function-pointer predicate.
#[derive(Clone, Copy)]
pub struct FilterCountFold<L> {
    predicate: fn(&L) -> bool,
}

impl<L> std::fmt::Debug for FilterCountFold<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FilterCountFold").finish()
    }
}

impl<L> FilterCountFold<L> {
    /// Create a new filtered count fold.
    #[must_use]
    pub fn new(predicate: fn(&L) -> bool) -> Self {
        Self { predicate }
    }
}

impl<L> Fold<L, usize> for FilterCountFold<L> {
    #[inline]
    fn init(&self, _context: &FoldContext) -> usize {
        0
    }

    #[inline]
    fn reduce(&self, state: usize, entry: &L, _context: &FoldContext) -> usize {
        if (self.predicate)(entry) {
            state.saturating_add(1)
        } else {
            state
        }
    }
}

impl<L> TryFold<L, usize> for FilterCountFold<L> {
    #[inline]
    fn try_step(
        &self,
        state: usize,
        entry: &L,
        context: &FoldContext,
    ) -> Result<usize, FoldFailure> {
        Ok(self.reduce(state, entry, context))
    }
}

/// A zero-allocation i64 summation fold with a function-pointer projection.
#[derive(Clone, Copy)]
pub struct SumI64Fold<L> {
    project: fn(&L) -> i64,
}

impl<L> std::fmt::Debug for SumI64Fold<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SumI64Fold").finish()
    }
}

impl<L> SumI64Fold<L> {
    /// Create a new summation fold.
    #[must_use]
    pub fn new(project: fn(&L) -> i64) -> Self {
        Self { project }
    }
}

impl<L> Fold<L, i64> for SumI64Fold<L> {
    #[inline]
    fn init(&self, _context: &FoldContext) -> i64 {
        0
    }

    #[inline]
    fn reduce(&self, state: i64, entry: &L, _context: &FoldContext) -> i64 {
        state.saturating_add((self.project)(entry))
    }
}

impl<L> TryFold<L, i64> for SumI64Fold<L> {
    #[inline]
    fn try_step(&self, state: i64, entry: &L, context: &FoldContext) -> Result<i64, FoldFailure> {
        Ok(self.reduce(state, entry, context))
    }
}

/// A zero-allocation existential fold with a function-pointer predicate.
#[derive(Clone, Copy)]
pub struct AnyFold<L> {
    predicate: fn(&L) -> bool,
}

impl<L> std::fmt::Debug for AnyFold<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AnyFold").finish()
    }
}

impl<L> AnyFold<L> {
    /// Create a new existential fold.
    #[must_use]
    pub fn new(predicate: fn(&L) -> bool) -> Self {
        Self { predicate }
    }
}

impl<L> Fold<L, bool> for AnyFold<L> {
    #[inline]
    fn init(&self, _context: &FoldContext) -> bool {
        false
    }

    #[inline]
    fn reduce(&self, state: bool, entry: &L, _context: &FoldContext) -> bool {
        state || (self.predicate)(entry)
    }
}

impl<L> TryFold<L, bool> for AnyFold<L> {
    #[inline]
    fn try_step(&self, state: bool, entry: &L, context: &FoldContext) -> Result<bool, FoldFailure> {
        Ok(self.reduce(state, entry, context))
    }
}

/// Unified state returned by [`CommonFold`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommonFoldState {
    /// Count-like output.
    Count(usize),
    /// Summation output.
    SumI64(i64),
    /// Boolean existential output.
    Any(bool),
}

impl CommonFoldState {
    #[inline]
    fn kind(self) -> &'static str {
        match self {
            Self::Count(_) => "Count",
            Self::SumI64(_) => "SumI64",
            Self::Any(_) => "Any",
        }
    }
}

/// Enum-dispatch fold for common allocation-free patterns without vtable overhead.
#[derive(Clone)]
pub enum CommonFold<L> {
    /// Count every entry.
    Count(CountFold<L>),
    /// Count entries matching a predicate.
    FilterCount(FilterCountFold<L>),
    /// Sum projected i64 values.
    SumI64(SumI64Fold<L>),
    /// Return whether any entry matches the predicate.
    Any(AnyFold<L>),
}

impl<L> std::fmt::Debug for CommonFold<L> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Count(_) => f.write_str("CommonFold::Count"),
            Self::FilterCount(_) => f.write_str("CommonFold::FilterCount"),
            Self::SumI64(_) => f.write_str("CommonFold::SumI64"),
            Self::Any(_) => f.write_str("CommonFold::Any"),
        }
    }
}

impl<L> CommonFold<L> {
    /// Create a counting common fold.
    #[must_use]
    pub fn count() -> Self {
        Self::Count(CountFold::new())
    }

    /// Create a filtered-count common fold.
    #[must_use]
    pub fn filter_count(predicate: fn(&L) -> bool) -> Self {
        Self::FilterCount(FilterCountFold::new(predicate))
    }

    /// Create an i64 summation common fold.
    #[must_use]
    pub fn sum_i64(project: fn(&L) -> i64) -> Self {
        Self::SumI64(SumI64Fold::new(project))
    }

    /// Create an existential common fold.
    #[must_use]
    pub fn any(predicate: fn(&L) -> bool) -> Self {
        Self::Any(AnyFold::new(predicate))
    }

    #[inline]
    fn expected_state_kind(&self) -> &'static str {
        match self {
            Self::Count(_) | Self::FilterCount(_) => "Count",
            Self::SumI64(_) => "SumI64",
            Self::Any(_) => "Any",
        }
    }

    /// Process a single entry and return an error if the state shape is invalid.
    pub fn try_step(
        &self,
        state: CommonFoldState,
        entry: &L,
        context: &FoldContext,
    ) -> Result<CommonFoldState, FoldFailure> {
        match (self, state) {
            (Self::Count(inner), CommonFoldState::Count(count)) => {
                Ok(CommonFoldState::Count(inner.reduce(count, entry, context)))
            }
            (Self::FilterCount(inner), CommonFoldState::Count(count)) => {
                Ok(CommonFoldState::Count(inner.reduce(count, entry, context)))
            }
            (Self::SumI64(inner), CommonFoldState::SumI64(sum)) => {
                Ok(CommonFoldState::SumI64(inner.reduce(sum, entry, context)))
            }
            (Self::Any(inner), CommonFoldState::Any(any)) => {
                Ok(CommonFoldState::Any(inner.reduce(any, entry, context)))
            }
            (kind, state) => Err(FoldFailure::StateMismatch {
                expected: kind.expected_state_kind(),
                actual: state.kind(),
            }),
        }
    }
}

impl<L> Fold<L, CommonFoldState> for CommonFold<L> {
    #[inline]
    fn init(&self, _context: &FoldContext) -> CommonFoldState {
        match self {
            Self::Count(_) | Self::FilterCount(_) => CommonFoldState::Count(0),
            Self::SumI64(_) => CommonFoldState::SumI64(0),
            Self::Any(_) => CommonFoldState::Any(false),
        }
    }

    /// Panics if `state` variant does not match `self`; use `try_step` for fallible version.
    #[inline]
    fn reduce(&self, state: CommonFoldState, entry: &L, context: &FoldContext) -> CommonFoldState {
        self.try_step(state, entry, context)
            .unwrap_or_else(|err| panic!("{err}"))
    }
}

impl<L> TryFold<L, CommonFoldState> for CommonFold<L> {
    #[inline]
    fn try_step(
        &self,
        state: CommonFoldState,
        entry: &L,
        context: &FoldContext,
    ) -> Result<CommonFoldState, FoldFailure> {
        CommonFold::try_step(self, state, entry, context)
    }
}

#[cfg(test)]
#[path = "fold_tests.rs"]
mod tests;
