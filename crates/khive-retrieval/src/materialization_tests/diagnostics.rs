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
