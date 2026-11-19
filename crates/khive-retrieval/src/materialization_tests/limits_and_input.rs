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
    for (result, field, requested, maximum) in [
        (
            MaterializationLimits::try_new(
                MAX_MATERIALIZATION_CANDIDATES + 1,
                NonZeroUsize::new(1).unwrap(),
                1,
                1,
            ),
            "candidates",
            4_097,
            4_096,
        ),
        (
            MaterializationLimits::try_new(
                1,
                NonZeroUsize::new(MAX_MATERIALIZATION_LOADER_BATCH + 1).unwrap(),
                1,
                1,
            ),
            "loader_batch_size",
            257,
            256,
        ),
        (
            MaterializationLimits::try_new(
                1,
                NonZeroUsize::new(1).unwrap(),
                MAX_MATERIALIZATION_OUTPUTS + 1,
                1,
            ),
            "output_rows",
            4_097,
            4_096,
        ),
        (
            MaterializationLimits::try_new(
                1,
                NonZeroUsize::new(1).unwrap(),
                1,
                MAX_MATERIALIZATION_DIAGNOSTICS + 1,
            ),
            "diagnostic_details",
            4_097,
            4_096,
        ),
    ] {
        assert_eq!(
            result,
            Err(MaterializationLimitError::AboveV1Maximum {
                field,
                requested,
                maximum,
            })
        );
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
        let order_key_calls = Cell::new(0);
        let result = materialize_ranked_prefix(
            input,
            output_limit,
            batch_size,
            configured,
            |candidate| {
                order_key_calls.set(order_key_calls.get() + 1);
                candidate.key
            },
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
        assert_eq!(order_key_calls.get(), 0, "refusal precedes order_key");
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
