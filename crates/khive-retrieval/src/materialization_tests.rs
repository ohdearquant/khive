use std::cell::{Cell, RefCell};
use std::future::ready;

use super::*;

#[tokio::test]
async fn reserved_collections_follow_consumer_caps_with_maximum_candidate_input() {
    let validated = Cell::new(0);
    let batches = RefCell::new(Vec::new());
    let configured = MaterializationLimits::try_new(
        MAX_MATERIALIZATION_CANDIDATES,
        NonZeroUsize::new(31).unwrap(),
        7,
        11,
    )
    .unwrap();
    let input: Vec<RankedCandidate<usize, usize>> = (0..MAX_MATERIALIZATION_CANDIDATES)
        .map(|key| RankedCandidate { key, score: key })
        .collect();
    let result = materialize_ranked_prefix(
        input,
        7,
        NonZeroUsize::new(31).unwrap(),
        configured,
        |candidate| candidate.key,
        |_| {
            validated.set(validated.get() + 1);
            Ok::<_, &'static str>(())
        },
        |keys| {
            batches.borrow_mut().push((keys.len(), keys.capacity()));
            ready(Ok(keys.into_iter().map(|key| (key, ())).collect()))
        },
        |candidate, row| {
            assert!(row.is_some(), "fixture loader returns every requested key");
            if candidate.key < 100 {
                MaterializationDecision::<usize, Reason, &'static str>::Drop(Reason::Missing)
            } else {
                MaterializationDecision::Keep(candidate.key)
            }
        },
    )
    .await
    .unwrap();
    // These inspect the actual non-ZST Vec reservations owned by the
    // controller. They do not estimate a process peak or caller payloads.
    assert_eq!(result.accepted.len(), 7);
    assert!(result.accepted.capacity() <= configured.max_output_rows());
    assert_eq!(result.diagnostic_details.len(), 11);
    assert!(result.diagnostic_details.capacity() <= configured.max_diagnostic_details());
    assert_eq!(result.drop_counts.count(Reason::Missing), Some(100));
    assert!(result.diagnostics_truncated);
    assert_eq!(validated.get(), MAX_MATERIALIZATION_CANDIDATES);
    assert_eq!(
        batches
            .borrow()
            .iter()
            .map(|(len, _)| *len)
            .collect::<Vec<_>>(),
        [31, 31, 31, 31]
    );
    assert!(batches
        .borrow()
        .iter()
        .all(|(_, capacity)| *capacity <= configured.max_loader_batch_size().get()));
    assert_eq!(
        result
            .accepted
            .iter()
            .map(|item| (item.candidate.key, item.candidate.score, item.rank))
            .collect::<Vec<_>>(),
        (100..107)
            .enumerate()
            .map(|(i, key)| (key, key, i + 1))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        result
            .diagnostic_details
            .iter()
            .map(|detail| detail.candidate.key)
            .collect::<Vec<_>>(),
        (0..11).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn duplicate_taxonomy_variant_fails_ordinal_preflight() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum DuplicateReason {
        Only,
    }
    impl DropReason for DuplicateReason {
        const ALL: &'static [Self] = &[Self::Only, Self::Only];
        fn ordinal(self) -> usize {
            0
        }
    }
    let callbacks = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| {
            callbacks.set(callbacks.get() + 1);
            Ok::<_, &'static str>(())
        },
        |_| {
            callbacks.set(callbacks.get() + 1);
            ready(Ok::<Vec<(u8, ())>, &'static str>(vec![]))
        },
        |_, _| MaterializationDecision::<(), DuplicateReason, &'static str>::Keep(()),
    )
    .await;
    assert_eq!(
        result,
        Err(MaterializationError::InvalidDropTaxonomy {
            message: "drop taxonomy ordinals are not contiguous and ordered"
        })
    );
    assert_eq!(callbacks.get(), 0);
}

#[tokio::test]
async fn validators_precede_each_bounded_loader_batch_in_input_order() {
    let events = RefCell::new(Vec::new());
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3, 4, 5]),
        5,
        NonZeroUsize::new(2).unwrap(),
        limits(5),
        |candidate| candidate.key,
        |candidate| {
            events
                .borrow_mut()
                .push(format!("validate {}", candidate.key));
            Ok::<_, &'static str>(())
        },
        |keys| {
            events.borrow_mut().push(format!("load {keys:?}"));
            ready(Ok::<_, &'static str>(
                keys.into_iter().map(|key| (key, ())).collect(),
            ))
        },
        |candidate, _| {
            events
                .borrow_mut()
                .push(format!("classify {}", candidate.key));
            MaterializationDecision::<(), Reason, &'static str>::Keep(())
        },
    )
    .await
    .unwrap();
    assert_eq!(result.accepted.len(), 5);
    assert_eq!(
        *events.borrow(),
        [
            "validate 1",
            "validate 2",
            "load [1, 2]",
            "classify 1",
            "classify 2",
            "validate 3",
            "validate 4",
            "load [3, 4]",
            "classify 3",
            "classify 4",
            "validate 5",
            "load [5]",
            "classify 5"
        ]
    );
}

#[tokio::test]
async fn invalid_candidate_in_current_batch_prevents_the_entire_loader_call() {
    let loaded = Cell::new(false);
    let result = materialize_ranked_prefix(
        candidates(&[1, 2]),
        1,
        NonZeroUsize::new(2).unwrap(),
        limits(2),
        |candidate| candidate.key,
        |candidate| {
            if candidate.key == 2 {
                Err("batch invalid")
            } else {
                Ok(())
            }
        },
        |_| {
            loaded.set(true);
            ready(Ok::<_, &'static str>(vec![(1_u8, ())]))
        },
        |_, _| MaterializationDecision::<(), Reason, &'static str>::Keep(()),
    )
    .await;
    assert_eq!(result, Err(MaterializationError::Caller("batch invalid")));
    assert!(!loaded.get());
}

#[tokio::test]
async fn equal_order_keys_are_rejected_even_when_candidate_keys_are_unique() {
    let loaded = Cell::new(false);
    let result = materialize_ranked_prefix(
        candidates(&[1, 2]),
        2,
        NonZeroUsize::new(1).unwrap(),
        limits(2),
        |_| 7_u8,
        |_| Ok::<_, &'static str>(()),
        |_| {
            loaded.set(true);
            ready(Ok::<Vec<(u8, ())>, &'static str>(vec![]))
        },
        |_, _| MaterializationDecision::<(), Reason, &'static str>::Keep(()),
    )
    .await;
    assert_eq!(
        result,
        Err(MaterializationError::NonMonotonicOrder {
            previous_index: 0,
            index: 1
        })
    );
    assert!(!loaded.get());
}

#[tokio::test]
async fn classifier_fatal_preserves_the_error_and_stops_later_candidates() {
    let classified = RefCell::new(Vec::new());
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3]),
        3,
        NonZeroUsize::new(3).unwrap(),
        limits(3),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<Vec<(u8, ())>, &'static str>(vec![])),
        |candidate, _| {
            classified.borrow_mut().push(candidate.key);
            if candidate.key == 2 {
                MaterializationDecision::<(), Reason, &'static str>::Fatal(
                    "classification integrity",
                )
            } else {
                MaterializationDecision::Drop(Reason::Missing)
            }
        },
    )
    .await;
    assert_eq!(
        result,
        Err(MaterializationError::Caller("classification integrity"))
    );
    assert_eq!(*classified.borrow(), [1, 2]);
}

#[tokio::test]
async fn taxonomy_over_32_variants_refuses_before_callbacks() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct WideReason(u8);
    impl DropReason for WideReason {
        const ALL: &'static [Self] = &[
            Self(0),
            Self(1),
            Self(2),
            Self(3),
            Self(4),
            Self(5),
            Self(6),
            Self(7),
            Self(8),
            Self(9),
            Self(10),
            Self(11),
            Self(12),
            Self(13),
            Self(14),
            Self(15),
            Self(16),
            Self(17),
            Self(18),
            Self(19),
            Self(20),
            Self(21),
            Self(22),
            Self(23),
            Self(24),
            Self(25),
            Self(26),
            Self(27),
            Self(28),
            Self(29),
            Self(30),
            Self(31),
            Self(32),
        ];
        fn ordinal(self) -> usize {
            self.0 as usize
        }
    }
    let callbacks = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| {
            callbacks.set(callbacks.get() + 1);
            Ok::<_, &'static str>(())
        },
        |_| {
            callbacks.set(callbacks.get() + 1);
            ready(Ok::<Vec<(u8, ())>, &'static str>(vec![]))
        },
        |_, _| MaterializationDecision::<(), WideReason, &'static str>::Keep(()),
    )
    .await;
    assert_eq!(
        result,
        Err(MaterializationError::InvalidDropTaxonomy {
            message: "drop taxonomy exceeds 32 variants"
        })
    );
    assert_eq!(callbacks.get(), 0);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Reason {
    Missing,
    Filtered,
}

impl DropReason for Reason {
    const ALL: &'static [Self] = &[Self::Missing, Self::Filtered];

    fn ordinal(self) -> usize {
        self as usize
    }
}

fn limits(details: usize) -> MaterializationLimits {
    MaterializationLimits::try_new(8, NonZeroUsize::new(4).unwrap(), 8, details).unwrap()
}

fn candidates(keys: &[u8]) -> Vec<RankedCandidate<u8, i16>> {
    keys.iter()
        .copied()
        .map(|key| RankedCandidate {
            key,
            score: 100 - i16::from(key),
        })
        .collect()
}

#[test]
fn limits_reject_every_dimension_above_the_v1_envelope() {
    for result in [
        MaterializationLimits::try_new(
            MAX_MATERIALIZATION_CANDIDATES + 1,
            NonZeroUsize::new(1).unwrap(),
            1,
            1,
        ),
        MaterializationLimits::try_new(
            1,
            NonZeroUsize::new(MAX_MATERIALIZATION_LOADER_BATCH + 1).unwrap(),
            1,
            1,
        ),
        MaterializationLimits::try_new(
            1,
            NonZeroUsize::new(1).unwrap(),
            MAX_MATERIALIZATION_OUTPUTS + 1,
            1,
        ),
        MaterializationLimits::try_new(
            1,
            NonZeroUsize::new(1).unwrap(),
            1,
            MAX_MATERIALIZATION_DIAGNOSTICS + 1,
        ),
    ] {
        assert!(matches!(
            result,
            Err(MaterializationLimitError::AboveV1Maximum { .. })
        ));
    }
}

#[test]
fn limits_accept_the_exact_v1_envelope() {
    let limits = MaterializationLimits::try_new(
        MAX_MATERIALIZATION_CANDIDATES,
        NonZeroUsize::new(MAX_MATERIALIZATION_LOADER_BATCH).unwrap(),
        MAX_MATERIALIZATION_OUTPUTS,
        MAX_MATERIALIZATION_DIAGNOSTICS,
    )
    .unwrap();
    assert_eq!(limits.max_candidates(), MAX_MATERIALIZATION_CANDIDATES);
    assert_eq!(
        limits.max_loader_batch_size().get(),
        MAX_MATERIALIZATION_LOADER_BATCH
    );
    assert_eq!(limits.max_output_rows(), MAX_MATERIALIZATION_OUTPUTS);
    assert_eq!(
        limits.max_diagnostic_details(),
        MAX_MATERIALIZATION_DIAGNOSTICS
    );
}

#[tokio::test]
async fn arbitrary_loader_order_compacts_missing_rows_stably() {
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3]),
        3,
        NonZeroUsize::new(3).unwrap(),
        limits(3),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |keys| {
            assert_eq!(keys, vec![1, 2, 3]);
            ready(Ok(vec![(3, "three"), (1, "one")]))
        },
        |_, row| match row {
            Some(row) => MaterializationDecision::Keep(row),
            None => MaterializationDecision::Drop(Reason::Missing),
        },
    )
    .await
    .unwrap();

    assert_eq!(
        result
            .accepted
            .iter()
            .map(|item| (
                item.candidate.key,
                item.candidate.score,
                item.rank,
                item.output
            ))
            .collect::<Vec<_>>(),
        vec![(1, 99, 1, "one"), (3, 97, 2, "three")]
    );
    assert_eq!(result.drop_counts.count(Reason::Missing), Some(1));
    assert_eq!(result.diagnostic_details[0].candidate.key, 2);
    assert!(!result.diagnostics_truncated);
}

#[tokio::test]
async fn duplicate_and_non_monotonic_candidates_fail_before_callbacks() {
    let mut duplicate = candidates(&[1, 1]);
    // Its score order is strictly increasing under Reverse: uniqueness is
    // independently necessary, rather than merely another order failure.
    duplicate[1].score -= 1;
    for (case, input) in [duplicate, candidates(&[2, 1])].into_iter().enumerate() {
        let validator_calls = Cell::new(0);
        let loader_calls = Cell::new(0);
        let result = materialize_ranked_prefix(
            input,
            2,
            NonZeroUsize::new(1).unwrap(),
            limits(2),
            |candidate| std::cmp::Reverse(candidate.score),
            |_| {
                validator_calls.set(validator_calls.get() + 1);
                Ok::<_, &'static str>(())
            },
            |_| {
                loader_calls.set(loader_calls.get() + 1);
                ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new()))
            },
            |_, _| MaterializationDecision::<(), Reason, &'static str>::Drop(Reason::Missing),
        )
        .await;
        if case == 0 {
            assert_eq!(
                result,
                Err(MaterializationError::DuplicateCandidate {
                    first_index: 0,
                    duplicate_index: 1,
                })
            );
        } else {
            assert_eq!(
                result,
                Err(MaterializationError::NonMonotonicOrder {
                    previous_index: 0,
                    index: 1,
                })
            );
        }
        assert_eq!(validator_calls.get(), 0);
        assert_eq!(loader_calls.get(), 0);
    }
}

#[tokio::test]
async fn request_limits_fail_before_callbacks() {
    async fn assert_rejected(
        input: Vec<RankedCandidate<u8, i16>>,
        output_limit: usize,
        batch_size: NonZeroUsize,
        configured: MaterializationLimits,
        expected_field: &'static str,
    ) {
        let calls = Cell::new(0);
        let result = materialize_ranked_prefix(
            input,
            output_limit,
            batch_size,
            configured,
            |candidate| candidate.key,
            |_| {
                calls.set(calls.get() + 1);
                Ok::<_, &'static str>(())
            },
            |_| {
                calls.set(calls.get() + 1);
                ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new()))
            },
            |_, _| {
                calls.set(calls.get() + 1);
                MaterializationDecision::<(), Reason, &'static str>::Drop(Reason::Missing)
            },
        )
        .await;
        assert!(matches!(
            result,
            Err(MaterializationError::RequestExceedsLimit { field, .. })
                if field == expected_field
        ));
        assert_eq!(calls.get(), 0);
    }

    let configured =
        MaterializationLimits::try_new(2, NonZeroUsize::new(1).unwrap(), 2, 2).unwrap();
    assert_rejected(
        candidates(&[1, 2, 3]),
        2,
        NonZeroUsize::new(1).unwrap(),
        configured,
        "candidates",
    )
    .await;
    assert_rejected(
        candidates(&[1, 2]),
        2,
        NonZeroUsize::new(2).unwrap(),
        configured,
        "loader_batch_size",
    )
    .await;
    assert_rejected(
        candidates(&[1, 2]),
        3,
        NonZeroUsize::new(1).unwrap(),
        configured,
        "output_rows",
    )
    .await;
}

#[tokio::test]
async fn loader_structure_is_checked_before_any_classification() {
    for (case, rows) in [vec![(1, ()), (1, ())], vec![(9, ())]]
        .into_iter()
        .enumerate()
    {
        let classified = Cell::new(0);
        let result = materialize_ranked_prefix(
            candidates(&[1, 2]),
            2,
            NonZeroUsize::new(2).unwrap(),
            limits(2),
            |candidate| candidate.key,
            |_| Ok::<_, &'static str>(()),
            |_| ready(Ok::<_, &'static str>(rows.clone())),
            |_, _| {
                classified.set(classified.get() + 1);
                MaterializationDecision::<(), Reason, &'static str>::Keep(())
            },
        )
        .await;
        if case == 0 {
            assert_eq!(result, Err(MaterializationError::DuplicateLoaderKey));
        } else {
            assert_eq!(result, Err(MaterializationError::UnexpectedLoaderKey));
        }
        assert_eq!(classified.get(), 0);
    }
}

#[tokio::test]
async fn loader_row_count_is_bounded_before_correlation_or_classification() {
    let classified = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<_, &'static str>(vec![(1, ()), (1, ())])),
        |_, _| {
            classified.set(classified.get() + 1);
            MaterializationDecision::<(), Reason, &'static str>::Keep(())
        },
    )
    .await;
    assert_eq!(
        result,
        Err(MaterializationError::LoaderReturnedTooManyRows {
            returned: 2,
            requested: 1,
        })
    );
    assert_eq!(classified.get(), 0);
}

#[tokio::test]
async fn unexpected_loader_key_precedence_is_independent_of_row_order() {
    for rows in [
        vec![(1, ()), (1, ()), (9, ())],
        vec![(9, ()), (1, ()), (1, ())],
    ] {
        let classified = Cell::new(0);
        let result = materialize_ranked_prefix(
            candidates(&[1, 2, 3]),
            3,
            NonZeroUsize::new(3).unwrap(),
            limits(3),
            |candidate| candidate.key,
            |_| Ok::<_, &'static str>(()),
            |_| ready(Ok::<_, &'static str>(rows.clone())),
            |_, _| {
                classified.set(classified.get() + 1);
                MaterializationDecision::<(), Reason, &'static str>::Keep(())
            },
        )
        .await;
        assert_eq!(result, Err(MaterializationError::UnexpectedLoaderKey));
        assert_eq!(classified.get(), 0);
    }
}

#[tokio::test]
async fn invalid_drop_taxonomy_fails_before_callbacks() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum BadReason {
        Only,
    }

    impl DropReason for BadReason {
        const ALL: &'static [Self] = &[Self::Only];

        fn ordinal(self) -> usize {
            1
        }
    }

    let calls = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| {
            calls.set(calls.get() + 1);
            Ok::<_, &'static str>(())
        },
        |_| {
            calls.set(calls.get() + 1);
            ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new()))
        },
        |_, _| {
            calls.set(calls.get() + 1);
            MaterializationDecision::<(), BadReason, &'static str>::Keep(())
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(MaterializationError::InvalidDropTaxonomy { .. })
    ));
    assert_eq!(calls.get(), 0);
}

#[tokio::test]
async fn classifier_cannot_return_an_undeclared_reason() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum PartialReason {
        Declared,
        Omitted,
    }

    impl DropReason for PartialReason {
        const ALL: &'static [Self] = &[Self::Declared];

        fn ordinal(self) -> usize {
            self as usize
        }
    }

    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new())),
        |_, _| {
            MaterializationDecision::<(), PartialReason, &'static str>::Drop(PartialReason::Omitted)
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(MaterializationError::InvalidDropTaxonomy { .. })
    ));
}

#[tokio::test]
async fn an_empty_drop_taxonomy_is_valid_for_keep_only_consumers() {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum NoReason {}

    impl DropReason for NoReason {
        const ALL: &'static [Self] = &[];

        fn ordinal(self) -> usize {
            match self {}
        }
    }

    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<_, &'static str>(vec![(1, "row")])),
        |_, row| MaterializationDecision::<_, NoReason, &'static str>::Keep(row.unwrap()),
    )
    .await
    .unwrap();
    assert_eq!(result.accepted[0].output, "row");
    assert_eq!(result.drop_counts.total(), 0);
}

#[tokio::test]
async fn candidate_scores_do_not_need_to_be_clone() {
    #[derive(Debug, PartialEq, Eq)]
    struct NonCloneScore(u8);

    let result = materialize_ranked_prefix(
        vec![RankedCandidate {
            key: 1_u8,
            score: NonCloneScore(7),
        }],
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<_, &'static str>(vec![(1_u8, ())])),
        |_, _| MaterializationDecision::<_, Reason, &'static str>::Keep("kept"),
    )
    .await
    .unwrap();
    assert_eq!(result.accepted[0].candidate.score, NonCloneScore(7));
}

#[tokio::test]
async fn kth_keep_ignores_later_loaded_classifier_results_but_validates_the_batch() {
    let validated = RefCell::new(Vec::new());
    let classified = RefCell::new(Vec::new());
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3]),
        1,
        NonZeroUsize::new(3).unwrap(),
        limits(3),
        |candidate| candidate.key,
        |candidate| {
            validated.borrow_mut().push(candidate.key);
            Ok::<_, &'static str>(())
        },
        |_| ready(Ok(vec![(1, ()), (2, ()), (3, ())])),
        |candidate, _| {
            classified.borrow_mut().push(candidate.key);
            if candidate.key == 1 {
                MaterializationDecision::<&str, Reason, &'static str>::Keep("kept")
            } else {
                MaterializationDecision::<&str, Reason, &'static str>::Fatal("must be ignored")
            }
        },
    )
    .await;
    assert!(
        result.is_ok(),
        "later loaded classifier Fatal must be ignored: {result:?}"
    );
    let result = result.unwrap();
    assert_eq!(*validated.borrow(), vec![1, 2, 3]);
    assert_eq!(*classified.borrow(), vec![1]);
    assert_eq!(result.accepted[0].output, "kept");
    assert_eq!(result.drop_counts.total(), 0);
}

#[tokio::test]
async fn invalid_tail_after_k_fails_without_more_loader_io() {
    let loader_calls = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(3),
        |candidate| candidate.key,
        |candidate| {
            if candidate.key == 3 {
                Err("invalid tail")
            } else {
                Ok(())
            }
        },
        |keys| {
            loader_calls.set(loader_calls.get() + 1);
            ready(Ok(keys.into_iter().map(|key| (key, ())).collect()))
        },
        |_, _| MaterializationDecision::<(), Reason, &'static str>::Keep(()),
    )
    .await;
    assert_eq!(result, Err(MaterializationError::Caller("invalid tail")));
    assert_eq!(loader_calls.get(), 1);
}

#[tokio::test]
async fn earlier_loader_failure_wins_over_a_later_invalid_candidate() {
    let validated = RefCell::new(Vec::new());
    let result = materialize_ranked_prefix(
        candidates(&[1, 2]),
        2,
        NonZeroUsize::new(1).unwrap(),
        limits(2),
        |candidate| candidate.key,
        |candidate| {
            validated.borrow_mut().push(candidate.key);
            if candidate.key == 2 {
                Err("later invalid")
            } else {
                Ok(())
            }
        },
        |_| ready(Err::<Vec<(u8, ())>, _>("earlier loader")),
        |_, _| MaterializationDecision::<(), Reason, &'static str>::Keep(()),
    )
    .await;
    assert_eq!(result, Err(MaterializationError::Caller("earlier loader")));
    assert_eq!(*validated.borrow(), vec![1]);
}

#[tokio::test]
async fn diagnostic_details_truncate_without_changing_counts() {
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3]),
        3,
        NonZeroUsize::new(3).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new())),
        |candidate, _| {
            MaterializationDecision::<(), Reason, &'static str>::Drop(if candidate.key == 3 {
                Reason::Filtered
            } else {
                Reason::Missing
            })
        },
    )
    .await
    .unwrap();
    assert_eq!(result.drop_counts.count(Reason::Missing), Some(2));
    assert_eq!(result.drop_counts.count(Reason::Filtered), Some(1));
    assert_eq!(result.diagnostic_details.len(), 1);
    assert_eq!(result.diagnostic_details[0].candidate.key, 1);
    assert!(result.diagnostics_truncated);
}

#[tokio::test]
async fn zero_diagnostic_capacity_still_counts_and_marks_truncation() {
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(0),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |_| ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new())),
        |_, _| MaterializationDecision::<(), Reason, &'static str>::Drop(Reason::Missing),
    )
    .await
    .unwrap();
    assert_eq!(result.drop_counts.count(Reason::Missing), Some(1));
    assert!(result.diagnostic_details.is_empty());
    assert!(result.diagnostics_truncated);
}

#[tokio::test]
async fn zero_output_validates_the_whole_tail_without_loader_io() {
    let validated = RefCell::new(Vec::new());
    let loader_calls = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1, 2, 3]),
        0,
        NonZeroUsize::new(2).unwrap(),
        limits(0),
        |candidate| candidate.key,
        |candidate| {
            validated.borrow_mut().push(candidate.key);
            Ok::<_, &'static str>(())
        },
        |_| {
            loader_calls.set(loader_calls.get() + 1);
            ready(Ok::<Vec<(u8, ())>, &'static str>(Vec::new()))
        },
        |_, _| MaterializationDecision::<(), Reason, &'static str>::Keep(()),
    )
    .await
    .unwrap();
    assert!(result.accepted.is_empty());
    assert_eq!(*validated.borrow(), vec![1, 2, 3]);
    assert_eq!(loader_calls.get(), 0);
}
