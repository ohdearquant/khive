//! Policy-free materialization of a bounded ranked candidate prefix.

use std::collections::BTreeMap;
use std::future::Future;
use std::marker::PhantomData;
use std::num::NonZeroUsize;

/// Absolute v1 candidate ceiling.
pub const MAX_MATERIALIZATION_CANDIDATES: usize = 4_096;
/// Absolute v1 loader-batch ceiling.
pub const MAX_MATERIALIZATION_LOADER_BATCH: usize = 256;
/// Absolute v1 accepted-output ceiling.
pub const MAX_MATERIALIZATION_OUTPUTS: usize = 4_096;
/// Absolute v1 retained-diagnostic ceiling.
pub const MAX_MATERIALIZATION_DIAGNOSTICS: usize = 4_096;
/// Absolute v1 drop-taxonomy ceiling.
pub const MAX_MATERIALIZATION_DROP_REASONS: usize = 32;

/// Construction failure for [`MaterializationLimits`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MaterializationLimitError {
    /// A configured ceiling exceeds the portable v1 envelope.
    #[error("materialization {field} limit {requested} exceeds v1 maximum {maximum}")]
    AboveV1Maximum {
        /// Limit field that failed validation.
        field: &'static str,
        /// Requested value.
        requested: usize,
        /// Absolute v1 maximum.
        maximum: usize,
    },
}

/// Caller-selected ceilings for one materialization consumer.
///
/// Private fields ensure every instance has passed [`Self::try_new`].
/// These bounds cover controller-owned collections; caller-owned key, score,
/// row and output payloads, and allocations inside callbacks, remain caller
/// responsibilities.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaterializationLimits {
    max_candidates: usize,
    max_loader_batch_size: NonZeroUsize,
    max_output_rows: usize,
    max_diagnostic_details: usize,
}

impl MaterializationLimits {
    /// Validate and construct consumer-specific limits.
    pub fn try_new(
        max_candidates: usize,
        max_loader_batch_size: NonZeroUsize,
        max_output_rows: usize,
        max_diagnostic_details: usize,
    ) -> Result<Self, MaterializationLimitError> {
        for (field, requested, maximum) in [
            ("candidates", max_candidates, MAX_MATERIALIZATION_CANDIDATES),
            (
                "loader_batch_size",
                max_loader_batch_size.get(),
                MAX_MATERIALIZATION_LOADER_BATCH,
            ),
            ("output_rows", max_output_rows, MAX_MATERIALIZATION_OUTPUTS),
            (
                "diagnostic_details",
                max_diagnostic_details,
                MAX_MATERIALIZATION_DIAGNOSTICS,
            ),
        ] {
            if requested > maximum {
                return Err(MaterializationLimitError::AboveV1Maximum {
                    field,
                    requested,
                    maximum,
                });
            }
        }
        Ok(Self {
            max_candidates,
            max_loader_batch_size,
            max_output_rows,
            max_diagnostic_details,
        })
    }

    /// Maximum supplied candidates for this consumer.
    pub const fn max_candidates(self) -> usize {
        self.max_candidates
    }

    /// Maximum rows in one loader callback.
    pub const fn max_loader_batch_size(self) -> NonZeroUsize {
        self.max_loader_batch_size
    }

    /// Maximum accepted output rows.
    pub const fn max_output_rows(self) -> usize {
        self.max_output_rows
    }

    /// Maximum retained per-drop diagnostic details.
    pub const fn max_diagnostic_details(self) -> usize {
        self.max_diagnostic_details
    }
}

/// One unique candidate in caller-provided total order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RankedCandidate<Key, Score> {
    /// Correlation key used by the loader.
    pub key: Key,
    /// Caller-owned score retained unchanged on acceptance.
    pub score: Score,
}

/// Closed caller-owned drop taxonomy.
pub trait DropReason: Copy + Eq + 'static {
    /// Every variant exactly once, in ordinal order.
    const ALL: &'static [Self];

    /// Fixed zero-based index into [`Self::ALL`], determined only by `self`.
    fn ordinal(self) -> usize;
}

/// Pack policy decision for one correlated candidate row.
#[derive(Debug, PartialEq, Eq)]
pub enum MaterializationDecision<Output, Reason, Error> {
    /// Retain the output and assign the next compact rank.
    Keep(Output),
    /// Omit the candidate and record a typed diagnostic.
    Drop(Reason),
    /// Stop immediately with the caller's error.
    Fatal(Error),
}

/// One retained accepted output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedItem<Key, Score, Output> {
    /// Original candidate, including its unchanged score.
    pub candidate: RankedCandidate<Key, Score>,
    /// Compact one-based result rank.
    pub rank: usize,
    /// Caller-owned output projection.
    pub output: Output,
}

/// One retained per-candidate drop diagnostic.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropDiagnostic<Key, Score, Reason> {
    /// Original candidate in input order.
    pub candidate: RankedCandidate<Key, Score>,
    /// Caller-owned typed reason.
    pub reason: Reason,
}

/// Fixed-capacity aggregate counters keyed by a caller's drop taxonomy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DropCounts<Reason> {
    counts: [usize; MAX_MATERIALIZATION_DROP_REASONS],
    marker: PhantomData<Reason>,
}

impl<Reason: DropReason> DropCounts<Reason> {
    /// Count drops for one reason.
    ///
    /// Returns `None` when `reason` is not a declared member of the closed
    /// taxonomy used to construct this result.
    pub fn count(&self, reason: Reason) -> Option<usize> {
        let ordinal = reason.ordinal();
        Reason::ALL
            .get(ordinal)
            .filter(|declared| **declared == reason)
            .map(|_| self.counts[ordinal])
    }

    /// Count all drops.
    pub fn total(&self) -> usize {
        self.counts.iter().sum()
    }
}

/// Successful bounded materialization result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MaterializedPrefix<Key, Score, Output, Reason> {
    /// Accepted outputs in original candidate order.
    pub accepted: Vec<MaterializedItem<Key, Score, Output>>,
    /// Aggregate typed drop counts.
    pub drop_counts: DropCounts<Reason>,
    /// First bounded drop details in candidate order.
    pub diagnostic_details: Vec<DropDiagnostic<Key, Score, Reason>>,
    /// Whether at least one detail was omitted by the configured detail cap.
    pub diagnostics_truncated: bool,
}

/// Structural or caller failure from ranked-prefix materialization.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MaterializationError<Error> {
    /// One request dimension exceeds its consumer-specific limit.
    #[error("materialization request {field} {requested} exceeds configured limit {limit}")]
    RequestExceedsLimit {
        /// Request dimension.
        field: &'static str,
        /// Requested value.
        requested: usize,
        /// Consumer limit.
        limit: usize,
    },
    /// A candidate key occurs more than once.
    #[error("duplicate materialization candidate at indexes {first_index} and {duplicate_index}")]
    DuplicateCandidate {
        /// First occurrence.
        first_index: usize,
        /// Duplicate occurrence.
        duplicate_index: usize,
    },
    /// Caller-provided order keys are not strictly increasing.
    #[error(
        "materialization candidate order is not strict at indexes {previous_index} and {index}"
    )]
    NonMonotonicOrder {
        /// Previous index.
        previous_index: usize,
        /// Current index.
        index: usize,
    },
    /// The caller's drop taxonomy is not a complete ordinal sequence.
    #[error("invalid materialization drop taxonomy: {message}")]
    InvalidDropTaxonomy {
        /// Stable validation explanation.
        message: &'static str,
    },
    /// A loader returned a key outside its requested batch.
    #[error("materialization loader returned an unexpected key")]
    UnexpectedLoaderKey,
    /// A loader returned one key more than once.
    #[error("materialization loader returned a duplicate key")]
    DuplicateLoaderKey,
    /// A loader returned more rows than the requested batch could contain.
    #[error(
        "materialization loader returned {returned} rows for a batch of {requested} candidates"
    )]
    LoaderReturnedTooManyRows {
        /// Number of keyed rows returned by the loader.
        returned: usize,
        /// Number of keys supplied to the loader.
        requested: usize,
    },
    /// Candidate validation, loading, or classification failed.
    #[error("materialization caller callback failed")]
    Caller(Error),
}

/// Materialize a bounded prefix of an already total-ordered candidate list.
///
/// `order_key` must expose the caller's existing **strictly increasing** total
/// order. For a descending score order, wrap the score in [`std::cmp::Reverse`]
/// and include the caller's stable ID tie-break. The controller validates the
/// complete bounded order and closed drop taxonomy before invoking the other
/// callbacks.
///
/// For each ordered batch, `candidate_validator` runs for every candidate
/// before `batch_loader` receives that batch's keys. The loader may return
/// keyed rows in arbitrary order and may omit keys; a missing key is passed to
/// `classifier` as `None`. Duplicate, unexpected, or excess rows are fatal
/// structural errors checked before any row in the batch is classified.
///
/// Classification stops at the `output_limit`th `Keep`. Later rows already
/// returned in that batch are ignored, while `candidate_validator` still
/// visits the remaining tail without further loader or classifier calls. The
/// validator and classifier are policy-only synchronous callbacks; all caller
/// I/O belongs in the bounded async loader.
#[allow(clippy::too_many_arguments)]
pub async fn materialize_ranked_prefix<
    Key,
    Score,
    OrderKey,
    Row,
    Output,
    Reason,
    Error,
    OrderFn,
    Validator,
    Loader,
    LoaderFuture,
    Classifier,
>(
    candidates: Vec<RankedCandidate<Key, Score>>,
    output_limit: usize,
    loader_batch_size: NonZeroUsize,
    limits: MaterializationLimits,
    mut order_key: OrderFn,
    mut candidate_validator: Validator,
    mut batch_loader: Loader,
    mut classifier: Classifier,
) -> Result<MaterializedPrefix<Key, Score, Output, Reason>, MaterializationError<Error>>
where
    Key: Clone + Ord,
    OrderKey: Ord,
    Reason: DropReason,
    OrderFn: FnMut(&RankedCandidate<Key, Score>) -> OrderKey,
    Validator: FnMut(&RankedCandidate<Key, Score>) -> Result<(), Error>,
    Loader: FnMut(Vec<Key>) -> LoaderFuture,
    LoaderFuture: Future<Output = Result<Vec<(Key, Row)>, Error>>,
    Classifier: FnMut(
        &RankedCandidate<Key, Score>,
        Option<Row>,
    ) -> MaterializationDecision<Output, Reason, Error>,
{
    for (field, requested, limit) in [
        ("candidates", candidates.len(), limits.max_candidates()),
        (
            "loader_batch_size",
            loader_batch_size.get(),
            limits.max_loader_batch_size().get(),
        ),
        ("output_rows", output_limit, limits.max_output_rows()),
    ] {
        if requested > limit {
            return Err(MaterializationError::RequestExceedsLimit {
                field,
                requested,
                limit,
            });
        }
    }

    if Reason::ALL.len() > MAX_MATERIALIZATION_DROP_REASONS {
        return Err(MaterializationError::InvalidDropTaxonomy {
            message: "drop taxonomy exceeds 32 variants",
        });
    }
    for (ordinal, reason) in Reason::ALL.iter().copied().enumerate() {
        if reason.ordinal() != ordinal {
            return Err(MaterializationError::InvalidDropTaxonomy {
                message: "drop taxonomy ordinals are not contiguous and ordered",
            });
        }
    }

    // Validate the complete bounded input before any validator or loader work.
    // Borrowed map keys avoid cloning caller-owned candidate payloads.
    {
        let mut first_indexes = BTreeMap::<&Key, usize>::new();
        let mut previous_order = None;
        for (index, candidate) in candidates.iter().enumerate() {
            if let Some(first_index) = first_indexes.insert(&candidate.key, index) {
                return Err(MaterializationError::DuplicateCandidate {
                    first_index,
                    duplicate_index: index,
                });
            }

            let current_order = order_key(candidate);
            if previous_order
                .as_ref()
                .is_some_and(|previous| previous >= &current_order)
            {
                return Err(MaterializationError::NonMonotonicOrder {
                    previous_index: index - 1,
                    index,
                });
            }
            previous_order = Some(current_order);
        }
    }

    let accepted_capacity = output_limit.min(candidates.len());
    let diagnostic_capacity = limits.max_diagnostic_details().min(candidates.len());
    let mut accepted = Vec::with_capacity(accepted_capacity);
    let mut diagnostic_details = Vec::with_capacity(diagnostic_capacity);
    let mut counts = [0_usize; MAX_MATERIALIZATION_DROP_REASONS];
    let mut diagnostics_truncated = false;
    let mut remaining = candidates.into_iter();

    while accepted.len() < output_limit {
        let batch = remaining
            .by_ref()
            .take(loader_batch_size.get())
            .collect::<Vec<_>>();
        if batch.is_empty() {
            break;
        }

        for candidate in &batch {
            candidate_validator(candidate).map_err(MaterializationError::Caller)?;
        }

        let loader_keys = batch
            .iter()
            .map(|candidate| candidate.key.clone())
            .collect::<Vec<_>>();
        let rows = batch_loader(loader_keys)
            .await
            .map_err(MaterializationError::Caller)?;
        if rows.len() > batch.len() {
            return Err(MaterializationError::LoaderReturnedTooManyRows {
                returned: rows.len(),
                requested: batch.len(),
            });
        }

        // Correlate the entire returned batch before classifying any row.  The
        // index borrows candidate keys and the row slots stay batch-bounded.
        let mut indexes = BTreeMap::<&Key, usize>::new();
        for (index, candidate) in batch.iter().enumerate() {
            indexes.insert(&candidate.key, index);
        }
        let mut row_slots = (0..batch.len()).map(|_| None).collect::<Vec<Option<Row>>>();
        let mut saw_unexpected = false;
        let mut saw_duplicate = false;
        for (key, row) in rows {
            match indexes.get(&key).copied() {
                Some(index) if row_slots[index].is_none() => row_slots[index] = Some(row),
                Some(_) => saw_duplicate = true,
                None => saw_unexpected = true,
            }
        }
        drop(indexes);
        if saw_unexpected {
            return Err(MaterializationError::UnexpectedLoaderKey);
        }
        if saw_duplicate {
            return Err(MaterializationError::DuplicateLoaderKey);
        }

        for (candidate, row) in batch.into_iter().zip(row_slots) {
            if accepted.len() == output_limit {
                break;
            }
            match classifier(&candidate, row) {
                MaterializationDecision::Keep(output) => {
                    accepted.push(MaterializedItem {
                        candidate,
                        rank: accepted.len() + 1,
                        output,
                    });
                }
                MaterializationDecision::Drop(reason) => {
                    let ordinal = reason.ordinal();
                    if Reason::ALL.get(ordinal).copied() != Some(reason) {
                        return Err(MaterializationError::InvalidDropTaxonomy {
                            message: "classifier returned an undeclared drop reason",
                        });
                    }
                    counts[ordinal] += 1;
                    if diagnostic_details.len() < limits.max_diagnostic_details() {
                        diagnostic_details.push(DropDiagnostic { candidate, reason });
                    } else {
                        diagnostics_truncated = true;
                    }
                }
                MaterializationDecision::Fatal(error) => {
                    return Err(MaterializationError::Caller(error));
                }
            }
        }
    }

    // The loaded batch that produced K was already validated in full.  Only
    // candidates beyond that batch remain here, and they must never trigger I/O.
    for candidate in remaining {
        candidate_validator(&candidate).map_err(MaterializationError::Caller)?;
    }

    Ok(MaterializedPrefix {
        accepted,
        drop_counts: DropCounts {
            counts,
            marker: PhantomData,
        },
        diagnostic_details,
        diagnostics_truncated,
    })
}

#[cfg(test)]
#[path = "materialization_tests.rs"]
mod tests;
