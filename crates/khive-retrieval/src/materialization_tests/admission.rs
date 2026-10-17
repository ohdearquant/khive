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
    assert_eq!(order_key_calls.get(), 0, "refusal precedes order_key");
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
    assert_eq!(order_key_calls.get(), 0, "refusal precedes order_key");
}
