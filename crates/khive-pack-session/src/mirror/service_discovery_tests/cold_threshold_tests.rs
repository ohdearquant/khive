#[test]
fn old_unchanged_files_become_cold_only_after_the_threshold() {
    let now = SystemTime::now();
    let old = now.checked_sub(FILE_COLD_AGE).expect("old timestamp");
    assert!(!should_mark_cold(
        FILE_UNCHANGED_POLLS_BEFORE_COLD - 1,
        Some(old),
        now
    ));
    assert!(should_mark_cold(
        FILE_UNCHANGED_POLLS_BEFORE_COLD,
        Some(old),
        now
    ));
    assert!(!should_mark_cold(
        FILE_UNCHANGED_POLLS_BEFORE_COLD,
        Some(now),
        now
    ));
}

#[test]
fn future_mtime_falls_back_to_unchanged_poll_threshold() {
    let now = SystemTime::now();
    let future = now
        .checked_add(Duration::from_secs(3600))
        .expect("future timestamp");
    assert!(
        !should_mark_cold(FILE_UNCHANGED_POLLS_WITHOUT_MTIME - 1, Some(future), now),
        "below the no-mtime threshold an unusable mtime must not mark cold"
    );
    assert!(
        should_mark_cold(FILE_UNCHANGED_POLLS_WITHOUT_MTIME, Some(future), now),
        "a future-dated mtime must fall back to the unchanged-poll threshold"
    );
}

#[test]
fn regressing_candidate_offset_is_never_recorded() {
    let start_offset = 100;
    let mut dispatch = CandidateDispatch::default();

    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 4,
            scanned: 4,
            new_offset: start_offset - 60,
            ..Default::default()
        }),
        start_offset,
    ));
    assert!(
        dispatch.stats.is_none(),
        "a regressing cursor must not be recorded"
    );

    assert!(dispatch.record(
        Ok(MirrorStats {
            inserted: 1,
            scanned: 1,
            new_offset: start_offset + 50,
            ..Default::default()
        }),
        start_offset,
    ));
    assert_eq!(
        dispatch
            .stats
            .expect("advancing candidate recorded")
            .new_offset,
        150
    );
}
