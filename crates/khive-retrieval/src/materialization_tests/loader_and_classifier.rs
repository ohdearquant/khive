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
    let order_key_calls = Cell::new(0);
    let result = materialize_ranked_prefix(
        candidates(&[1]),
        1,
        NonZeroUsize::new(1).unwrap(),
        limits(1),
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
            MaterializationDecision::<(), BadReason, &'static str>::Keep(())
        },
    )
    .await;
    assert!(matches!(
        result,
        Err(MaterializationError::InvalidDropTaxonomy { .. })
    ));
    assert_eq!(calls.get(), 0);
    assert_eq!(order_key_calls.get(), 0, "refusal precedes order_key");
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
