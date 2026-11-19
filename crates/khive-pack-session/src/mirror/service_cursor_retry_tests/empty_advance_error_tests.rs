#[tokio::test]
async fn failed_empty_advance_commit_counts_as_error_for_cold_demotion() {
    let (rt, _dir) = runtime_without_schema();
    let path = PathBuf::from("/projects/blank.jsonl");
    let mut discovery = DiscoveryIndex::default();
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

    let (stats, had_errors) = finalize_dispatch_stats(
        &rt,
        &path,
        0,
        false,
        Some(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: 64,
            ..Default::default()
        }),
        false,
    )
    .await;
    assert!(stats.is_none(), "failed commit must hold back the offset");
    assert!(had_errors, "failed commit must become an error poll");

    for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
        assert!(!tally_dispatch_errors(
            &mut discovery,
            &path,
            false,
            had_errors
        ));
    }
    assert!(tally_dispatch_errors(
        &mut discovery,
        &path,
        false,
        had_errors
    ));
    assert!(discovery.files[&path].cold);
}

/// ADR-080 regression: under overlapping-root dispatch, one candidate
/// reports a genuine skipped-oversized-line advance while a sibling
/// candidate errors on the same span. The skip is source-independent —
/// the line was too large for any parser before one ever ran — so it
/// must commit despite the sibling's error, instead of being discarded
/// and re-read (bounded but forever) on every later poll.
#[tokio::test]
async fn oversized_skip_advance_commits_despite_a_sibling_candidate_error() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let path = PathBuf::from("/projects/oversized.jsonl");
    let start_offset = 0u64;

    let mut dispatch = CandidateDispatch::default();

    // Candidate A: the oversized-unterminated-line skip. Bytes consumed,
    // nothing inserted, but source-independent.
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: 4096,
            skipped_oversized_bytes: true,
            replay_mismatches: 0,
            file_identity: None,
        }),
        start_offset,
    ));
    // Candidate B: an unrelated provider candidate errors on this pass.
    assert!(!dispatch.record(Err(RuntimeError::Internal("boom".into())), start_offset,));

    let CandidateDispatch { stats, errors } = dispatch;
    let had_errors = !errors.is_empty();
    assert!(had_errors, "the sibling error must be recorded");

    let (stats, had_errors) =
        finalize_dispatch_stats(&rt, &path, start_offset, false, stats, had_errors).await;

    let stats = stats.expect(
        "a skipped-oversized-line advance must commit even when a sibling \
             candidate errored on the same span",
    );
    assert_eq!(stats.new_offset, 4096);
    assert!(
        had_errors,
        "the sibling's error is still reported for cold-demotion accounting"
    );
    assert_eq!(
        cursor_row_offset(&rt, &path).await,
        Some(4096),
        "the cursor for the oversized file must have durably advanced, so the \
             next pass starts past the discarded prefix instead of re-reading it"
    );
}

/// A pass that skipped an oversized line but also scanned an ordinary line
/// is not source-independent: the ordinary line is exactly what the
/// erroring sibling might have parsed, so the veto still holds and the
/// bounded span is re-read on a later pass.
#[tokio::test]
async fn mixed_oversized_skip_and_scanned_line_keeps_the_error_veto() {
    let (rt, _dir) = runtime_without_schema();
    apply_session_schema(&rt).await;
    let path = PathBuf::from("/projects/mixed.jsonl");
    let start_offset = 0u64;

    let mut dispatch = CandidateDispatch::default();
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 1,
            new_offset: 4200,
            skipped_oversized_bytes: true,
            replay_mismatches: 0,
            file_identity: None,
        }),
        start_offset,
    ));
    assert!(!dispatch.record(Err(RuntimeError::Internal("boom".into())), start_offset,));

    let CandidateDispatch { stats, errors } = dispatch;
    let (stats, had_errors) =
        finalize_dispatch_stats(&rt, &path, start_offset, false, stats, !errors.is_empty()).await;

    assert!(
        stats.is_none(),
        "an advance that also scanned an ordinary line stays vetoed while a sibling errored"
    );
    assert!(had_errors);
    assert_eq!(
        cursor_row_offset(&rt, &path).await,
        None,
        "no cursor row may be written for a contested span"
    );
}
