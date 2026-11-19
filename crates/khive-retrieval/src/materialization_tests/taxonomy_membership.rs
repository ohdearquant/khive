#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ThirtyTwoReason {
    R00,
    R01,
    R02,
    R03,
    R04,
    R05,
    R06,
    R07,
    R08,
    R09,
    R10,
    R11,
    R12,
    R13,
    R14,
    R15,
    R16,
    R17,
    R18,
    R19,
    R20,
    R21,
    R22,
    R23,
    R24,
    R25,
    R26,
    R27,
    R28,
    R29,
    R30,
    R31,
}

impl DropReason for ThirtyTwoReason {
    const ALL: &'static [Self] = &[
        Self::R00,
        Self::R01,
        Self::R02,
        Self::R03,
        Self::R04,
        Self::R05,
        Self::R06,
        Self::R07,
        Self::R08,
        Self::R09,
        Self::R10,
        Self::R11,
        Self::R12,
        Self::R13,
        Self::R14,
        Self::R15,
        Self::R16,
        Self::R17,
        Self::R18,
        Self::R19,
        Self::R20,
        Self::R21,
        Self::R22,
        Self::R23,
        Self::R24,
        Self::R25,
        Self::R26,
        Self::R27,
        Self::R28,
        Self::R29,
        Self::R30,
        Self::R31,
    ];

    fn ordinal(self) -> usize {
        self as usize
    }
}

#[tokio::test]
async fn exactly_32_declared_reasons_accept_and_count_every_ordinal() {
    let order_key_calls = Cell::new(0);
    let validator_calls = Cell::new(0);
    let loader_calls = Cell::new(0);
    let classifier_calls = Cell::new(0);
    let input = (0_u8..32)
        .map(|key| RankedCandidate { key, score: key })
        .collect();
    let configured =
        MaterializationLimits::try_new(32, NonZeroUsize::new(8).unwrap(), 32, 32).unwrap();
    let result = materialize_ranked_prefix(
        input,
        32,
        NonZeroUsize::new(8).unwrap(),
        configured,
        |candidate| {
            order_key_calls.set(order_key_calls.get() + 1);
            candidate.key
        },
        |_| {
            validator_calls.set(validator_calls.get() + 1);
            Ok::<_, &'static str>(())
        },
        |keys| {
            loader_calls.set(loader_calls.get() + 1);
            ready(Ok::<_, &'static str>(
                keys.into_iter().map(|key| (key, ())).collect(),
            ))
        },
        |candidate, row| {
            assert!(row.is_some(), "valid setup returns each requested row");
            classifier_calls.set(classifier_calls.get() + 1);
            MaterializationDecision::<(), ThirtyTwoReason, &'static str>::Drop(
                ThirtyTwoReason::ALL[usize::from(candidate.key)],
            )
        },
    )
    .await
    .expect("all 32 declared variants fit the v1 taxonomy");
    assert!(result.accepted.is_empty());
    assert_eq!(result.drop_counts.total(), 32);
    for (ordinal, reason) in ThirtyTwoReason::ALL.iter().copied().enumerate() {
        assert_eq!(
            result.drop_counts.count(reason),
            Some(1),
            "ordinal {ordinal}"
        );
        assert_eq!(result.diagnostic_details[ordinal].reason, reason);
        assert_eq!(
            result.diagnostic_details[ordinal].candidate.key,
            ordinal as u8
        );
    }
    assert_eq!(result.diagnostic_details.len(), 32);
    assert!(!result.diagnostics_truncated);
    assert_eq!(order_key_calls.get(), 32);
    assert_eq!(validator_calls.get(), 32);
    assert_eq!(loader_calls.get(), 4);
    assert_eq!(classifier_calls.get(), 32);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InRangeReason {
    Declared,
    Omitted,
}

impl DropReason for InRangeReason {
    const ALL: &'static [Self] = &[Self::Declared];

    fn ordinal(self) -> usize {
        // Both map to an in-range slot; membership must also be checked.
        0
    }
}

#[tokio::test]
async fn classifier_refuses_an_in_range_undeclared_reason() {
    let classified = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |keys| {
            ready(Ok::<_, &'static str>(
                keys.into_iter().map(|key| (key, ())).collect(),
            ))
        },
        |_, row| {
            assert!(row.is_some(), "valid loader setup reaches classification");
            classified.set(classified.get() + 1);
            MaterializationDecision::<(), InRangeReason, &'static str>::Drop(InRangeReason::Omitted)
        },
    )
    .await;
    assert_eq!(classified.get(), 1);
    assert_eq!(
        result,
        Err(MaterializationError::InvalidDropTaxonomy {
            message: "classifier returned an undeclared drop reason",
        })
    );
}

#[tokio::test]
async fn drop_counts_returns_none_for_an_in_range_undeclared_reason() {
    // Construct counts through a legitimate declared drop. The separate
    // classifier-refusal witness must not supply an invalid result here.
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
        |candidate| candidate.key,
        |_| Ok::<_, &'static str>(()),
        |keys| {
            ready(Ok::<_, &'static str>(
                keys.into_iter().map(|key| (key, ())).collect(),
            ))
        },
        |_, row| {
            assert!(row.is_some());
            MaterializationDecision::<(), InRangeReason, &'static str>::Drop(
                InRangeReason::Declared,
            )
        },
    )
    .await
    .unwrap();
    assert_eq!(result.drop_counts.total(), 1);
    assert_eq!(result.drop_counts.count(InRangeReason::Declared), Some(1));
    assert_eq!(result.diagnostic_details[0].reason, InRangeReason::Declared);
    assert_eq!(result.drop_counts.count(InRangeReason::Omitted), None);
}
