#[test]
fn persistently_erroring_hot_file_is_demoted_to_cold_and_recovers() {
    let mut discovery = DiscoveryIndex::default();
    let path = PathBuf::from("/broken/session.jsonl");
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
    assert!(discovery.hot_files.contains(&path));

    // Below the threshold the file stays hot.
    for _tick in 1..FILE_ERROR_POLLS_BEFORE_COLD {
        assert!(!discovery.record_error_poll(&path));
        assert!(discovery.hot_files.contains(&path));
        assert!(!discovery.files[&path].cold);
    }
    // Crossing the threshold demotes to cold exactly once (the one-shot
    // warn tick) and enqueues it for the ordinary cold cadence.
    assert!(discovery.record_error_poll(&path));
    assert!(!discovery.hot_files.contains(&path));
    assert!(discovery.files[&path].cold);
    assert!(discovery.cold_enqueued.contains(&path));

    // Further error ticks keep it cold without re-crossing.
    assert!(!discovery.record_error_poll(&path));

    // Recovery reactivates through the normal hot path and resets the
    // streak.
    discovery.reactivate_file(&path);
    assert!(discovery.hot_files.contains(&path));
    assert!(!discovery.files[&path].cold);
    for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
        assert!(!discovery.record_error_poll(&path));
    }
    assert!(discovery.record_error_poll(&path));
}

#[test]
fn empty_advance_candidate_does_not_mask_inserting_provider() {
    let start_offset = 10;
    let mut dispatch = CandidateDispatch::default();

    // A wrong provider can advance the cursor while inserting nothing
    // (a cursor-only pass over unparseable lines); that must not end
    // dispatch and hide the provider that parses the same bytes.
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset + 40,
            ..Default::default()
        }),
        start_offset,
    ));
    assert!(dispatch.record(
        Ok(MirrorStats {
            inserted: 3,
            scanned: 3,
            new_offset: start_offset + 40,
            ..Default::default()
        }),
        start_offset,
    ));
    let stats = dispatch.stats.expect("inserting provider recorded");
    assert_eq!(stats.inserted, 3);
    assert_eq!(stats.new_offset, start_offset + 40);
}

/// Regression for the recording-precedence finding: a no-progress
/// candidate recorded first must not hide a later empty advance, or the
/// in-memory offset stays behind a durably committed cursor.
#[test]
fn later_empty_advance_replaces_earlier_no_progress_record() {
    let start_offset = 100;
    let mut dispatch = CandidateDispatch::default();

    // Candidate A: no progress (whole-file ceiling below the current
    // length). Recorded only because nothing else is recorded yet.
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset,
            ..Default::default()
        }),
        start_offset,
    ));

    // Candidate B: empty advance past start_offset. Must replace A's
    // non-advancing record so the in-memory offset follows the cursor.
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset + 64,
            ..Default::default()
        }),
        start_offset,
    ));

    let stats = dispatch.stats.expect("empty advance recorded");
    assert_eq!(
        stats.new_offset,
        start_offset + 64,
        "the advancing result wins over the earlier no-progress record"
    );
    assert_eq!(stats.inserted, 0);
}

/// An advancing record is kept against a later empty advance (first
/// advancing record wins; an inserting candidate ends dispatch anyway).
#[test]
fn first_advancing_record_is_kept_against_later_empty_advance() {
    let start_offset = 50;
    let mut dispatch = CandidateDispatch::default();

    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset + 30,
            ..Default::default()
        }),
        start_offset,
    ));
    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset + 90,
            ..Default::default()
        }),
        start_offset,
    ));

    assert_eq!(
        dispatch
            .stats
            .expect("first advancing record kept")
            .new_offset,
        start_offset + 30
    );
}
