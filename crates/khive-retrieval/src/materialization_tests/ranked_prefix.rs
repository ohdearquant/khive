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
