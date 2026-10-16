/// Item 6: a mixed poll — one candidate's no-progress success plus
/// another candidate's error — must count toward demotion; otherwise the
/// file stays hot forever.
#[test]
fn mixed_no_progress_and_error_polls_count_toward_demotion() {
    let mut discovery = DiscoveryIndex::default();
    let path = PathBuf::from("/projects/mixed.jsonl");
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

    for _ in 1..FILE_ERROR_POLLS_BEFORE_COLD {
        assert!(
            !super::tally_dispatch_errors(&mut discovery, &path, false, true),
            "below the threshold no demotion"
        );
        assert!(!discovery.files[&path].cold);
    }
    assert!(
        super::tally_dispatch_errors(&mut discovery, &path, false, true),
        "the crossing tick reports the one-shot demotion warn"
    );
    assert!(
        discovery.files[&path].cold,
        "the mixed no-progress+error file is demoted to cold"
    );

    // A successful advance resets the streak.
    super::tally_dispatch_errors(&mut discovery, &path, true, false);
    assert_eq!(discovery.files[&path].consecutive_error_polls, 0);
}

#[test]
fn refresh_failure_counter_crosses_the_warn_threshold_once_per_episode() {
    let mut discovery = DiscoveryIndex::default();
    let path = PathBuf::from("/projects/wedged");
    discovery.directories.insert(
        path.clone(),
        TrackedDirectory {
            kinds: vec![DirectoryKind::ClaudeCodeRoot],
            fingerprint: None,
            entries: HashSet::new(),
            unchanged_probes: 0,
            pinned: false,
            refresh_failures: 0,
        },
    );

    for _ in 1..DIRECTORY_REFRESH_FAILURES_BEFORE_WARN {
        assert_ne!(
            discovery.record_refresh_failure(&path),
            DIRECTORY_REFRESH_FAILURES_BEFORE_WARN,
            "below the threshold the log stays at debug"
        );
    }
    assert_eq!(
        discovery.record_refresh_failure(&path),
        DIRECTORY_REFRESH_FAILURES_BEFORE_WARN,
        "exactly one escalation per episode"
    );
    assert_ne!(
        discovery.record_refresh_failure(&path),
        DIRECTORY_REFRESH_FAILURES_BEFORE_WARN,
        "subsequent failures in the same episode do not re-escalate"
    );

    discovery.clear_refresh_failures(&path);
    assert_eq!(
        discovery.directories[&path].refresh_failures, 0,
        "a successful walk resets the episode"
    );
}
