#[test]
fn remove_file_clears_scheduler_state_and_readd_starts_clean() {
    let mut discovery = DiscoveryIndex::default();
    let path = PathBuf::from("/cold/session.jsonl");
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
    mark_cold(&mut discovery, &path);
    assert!(discovery.cold_enqueued.contains(&path));

    discovery.remove_file(&path, false);
    // Queue residue is invalidated lazily: the entry may still sit in
    // the cold queue, but it must never schedule again, and the enqueue
    // flags are cleared so a re-add starts clean.
    assert!(!discovery.cold_enqueued.contains(&path));
    assert!(!discovery.priority_cold_enqueued.contains(&path));
    assert!(discovery
        .schedule_files()
        .iter()
        .all(|file| file.path != path));

    // A re-added path is scheduled on the next tick instead of being
    // shadowed by leftover queue state.
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
    mark_cold(&mut discovery, &path);
    assert!(discovery
        .schedule_files()
        .iter()
        .any(|file| file.path == path && file.was_cold));
}

#[test]
fn remove_directory_tree_clears_directory_schedule_state() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let root = temp.path().join("projects");
    std::fs::create_dir_all(&root).expect("root dir");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&root, DirectoryKind::ClaudeCodeRoot, false);
    assert!(discovery.directory_enqueued.contains(&root));

    discovery.remove_directory_tree(&root, false);
    // Queue residue is invalidated lazily: the entry may still sit in
    // the queue, but an untracked path is skipped on pop, and the
    // enqueue flag stays set until that pop so a re-add cannot enqueue
    // a duplicate.
    assert!(discovery.directory_enqueued.contains(&root));

    // A re-added directory is probed exactly once on the next tick
    // instead of twice (once per queue entry).
    discovery.add_directory_tree(&root, DirectoryKind::ClaudeCodeRoot, false);
    let stats = discovery.probe_directories();
    assert_eq!(stats.metadata_probes, 1);
}

#[test]
fn removed_files_are_reported_once_for_cursor_cleanup() {
    let mut discovery = DiscoveryIndex::default();
    let path = PathBuf::from("/exports/conversations.json");
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);

    discovery.remove_file(&path, false);
    assert_eq!(discovery.take_removed_files(), vec![path.clone()]);
    assert!(discovery.take_removed_files().is_empty());

    // A pinned file that survives removal is not reported as removed.
    discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, true);
    discovery.remove_file(&path, true);
    assert!(discovery.files.contains_key(&path));
    assert!(discovery.take_removed_files().is_empty());
}

#[test]
fn pinned_missing_file_stays_tracked_and_hot_for_retry() {
    let mut discovery = DiscoveryIndex::default();
    let pinned = PathBuf::from("/exports/conversations.json");
    discovery.add_file(pinned.clone(), DiscoveredKind::ChatGptExport, true);

    discovery.record_probe_error(&pinned, false, true);
    assert!(discovery.files.contains_key(&pinned));
    assert!(discovery.hot_files.contains(&pinned));
    assert!(discovery
        .schedule_files()
        .iter()
        .any(|file| file.path == pinned && !file.was_cold));
}

#[test]
fn transient_not_found_gets_one_grace_probe_before_removal() {
    let mut discovery = DiscoveryIndex::default();
    let transient = PathBuf::from("/projects/gone.jsonl");
    discovery.add_file(transient.clone(), DiscoveredKind::ChatGptExport, false);

    // One NotFound (an atomic replace or FS hiccup) keeps the file and
    // its cursor so a promptly recreated path resumes from its old
    // offset instead of being reseeded to EOF.
    discovery.record_probe_error(&transient, false, true);
    assert!(discovery.files.contains_key(&transient));
    assert!(discovery.hot_files.contains(&transient));
    assert!(discovery.take_removed_files().is_empty());

    // A successful probe in between resets the streak entirely.
    discovery.record_unchanged(&transient, false, None, SystemTime::now());
    discovery.record_probe_error(&transient, false, true);
    assert!(discovery.files.contains_key(&transient));

    // A second consecutive NotFound removes the file and queues cursor
    // cleanup.
    discovery.record_probe_error(&transient, false, true);
    assert!(!discovery.files.contains_key(&transient));
    assert!(!discovery.hot_files.contains(&transient));
    assert_eq!(discovery.take_removed_files(), vec![transient.clone()]);
}

#[test]
fn stale_cold_queue_residue_is_pop_bounded_and_hot_files_still_scheduled() {
    let mut discovery = DiscoveryIndex::default();

    // A large backlog of stale entries at the queue head: files that
    // were cold, enqueued, then reactivated (e.g. growth noticed via a
    // directory priority sweep) — their queue entries are invalidated
    // lazily instead of by an O(queue) purge on reactivation.
    let stale_count = COLD_STALE_POPS_PER_TICK * 5;
    for index in 0..stale_count {
        let path = PathBuf::from(format!("/stale/session-{index}.jsonl"));
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        mark_cold(&mut discovery, &path);
        discovery.reactivate_file(&path);
    }
    // Fresh cold files behind the residue.
    let fresh_count = COLD_FILE_PROBES_PER_TICK * 3;
    let first_fresh = PathBuf::from("/cold/session-0.jsonl");
    for index in 0..fresh_count {
        let path = PathBuf::from(format!("/cold/session-{index}.jsonl"));
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        mark_cold(&mut discovery, &path);
    }
    // A hot file must still be scheduled in the same tick.
    let hot = PathBuf::from("/hot/session.jsonl");
    discovery.add_file(hot.clone(), DiscoveredKind::ChatGptExport, false);

    let queue_before = discovery.cold_queue.len();
    assert_eq!(queue_before, stale_count + fresh_count);

    let mut scheduled = discovery.schedule_files();
    assert!(
        scheduled
            .iter()
            .any(|file| file.path == hot && !file.was_cold),
        "hot scheduling is unaffected by the residue backlog"
    );
    // One tick cannot drain the arbitrary stale backlog: with residue
    // at the queue head, total pops are bounded by the stale-pop budget
    // plus the productive probe budget.
    let drained = queue_before - discovery.cold_queue.len();
    assert!(drained <= COLD_STALE_POPS_PER_TICK + COLD_FILE_PROBES_PER_TICK);
    assert!(
        discovery.cold_queue.len() > fresh_count / 2,
        "most of the backlog survives one tick"
    );

    // Bounded lag, no starvation: the residue drains at the stale-pop
    // rate and the fresh cold files are scheduled once it clears.
    let mut ticks = 1;
    while !scheduled.iter().any(|file| file.path == first_fresh) {
        scheduled = discovery.schedule_files();
        ticks += 1;
        assert!(
            ticks <= (stale_count / COLD_STALE_POPS_PER_TICK) + 2,
            "stale residue must drain at the bounded per-tick rate"
        );
    }
    assert!(
        scheduled.iter().filter(|file| file.was_cold).count() <= COLD_FILE_PROBES_PER_TICK,
        "the productive metadata-probe ceiling holds on the catch-up tick"
    );
}
