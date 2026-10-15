use super::{
    should_mark_cold, CandidateDispatch, DirectoryKind, DiscoveredKind, DiscoveryIndex,
    TrackedDirectory, COLD_FILE_PROBES_PER_TICK, COLD_STALE_POPS_PER_TICK,
    DIRECTORY_FORCE_RESCAN_PROBES, DIRECTORY_PROBES_PER_TICK,
    DIRECTORY_REFRESH_FAILURES_BEFORE_WARN, FILE_COLD_AGE, FILE_ERROR_POLLS_BEFORE_COLD,
    FILE_UNCHANGED_POLLS_BEFORE_COLD, FILE_UNCHANGED_POLLS_WITHOUT_MTIME,
};
use crate::mirror::ingest::MirrorStats;
use std::collections::HashSet;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn mark_cold(discovery: &mut DiscoveryIndex, path: &Path) {
    let file = discovery.files.get_mut(path).expect("tracked file");
    file.cold = true;
    file.unchanged_polls = FILE_UNCHANGED_POLLS_BEFORE_COLD;
    discovery.hot_files.remove(path);
    discovery.enqueue_cold(path);
}

#[test]
fn cold_scheduler_has_a_fixed_per_tick_file_probe_ceiling() {
    let mut discovery = DiscoveryIndex::default();
    let count = COLD_FILE_PROBES_PER_TICK * 8;
    for index in 0..count {
        let path = PathBuf::from(format!("/cold/session-{index}.jsonl"));
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        mark_cold(&mut discovery, &path);
    }

    let scheduled = discovery.schedule_files();
    assert_eq!(scheduled.len(), COLD_FILE_PROBES_PER_TICK);
    assert!(scheduled.iter().all(|file| file.was_cold));
    assert_eq!(discovery.tracked_files(), count);
}

#[test]
fn divergent_provider_limit_no_progress_does_not_block_next_candidate() {
    let start_offset = 41;
    let mut dispatch = CandidateDispatch::default();

    assert!(!dispatch.record(
        Ok(MirrorStats {
            inserted: 0,
            scanned: 0,
            new_offset: start_offset,
            ..Default::default()
        }),
        start_offset,
    ));
    assert!(dispatch.record(
        Ok(MirrorStats {
            inserted: 2,
            scanned: 2,
            new_offset: 97,
            ..Default::default()
        }),
        start_offset,
    ));

    let stats = dispatch.stats.expect("progressing provider candidate");
    assert_eq!(stats.new_offset, 97);
    assert_eq!(stats.inserted, 2);
}

#[test]
fn directory_scheduler_has_a_fixed_per_tick_metadata_ceiling() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let mut discovery = DiscoveryIndex::default();
    for index in 0..DIRECTORY_PROBES_PER_TICK * 4 {
        discovery.add_directory_tree(
            &temp.path().join(format!("missing-{index}")),
            DirectoryKind::Codex,
            true,
        );
    }

    let stats = discovery.probe_directories();
    assert_eq!(stats.metadata_probes, DIRECTORY_PROBES_PER_TICK);
    assert_eq!(stats.walks, 0);
}

#[test]
fn parent_mtime_change_prioritizes_a_cold_growing_file_in_that_cycle() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let project = temp.path().join("project-slug");
    std::fs::create_dir(&project).expect("project dir");
    let transcript = project.join("session.jsonl");
    std::fs::write(&transcript, "first\n").expect("transcript fixture");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeCodeRoot, true);
    mark_cold(&mut discovery, &transcript);

    OpenOptions::new()
        .append(true)
        .open(&transcript)
        .expect("open transcript")
        .write_all(b"second\n")
        .expect("grow transcript");
    discovery
        .directories
        .get_mut(&project)
        .expect("tracked project directory")
        .fingerprint = None;

    let stats = discovery.probe_directories();
    assert_eq!(stats.walks, 1);
    assert!(!discovery.hot_files.contains(&transcript));
    assert!(discovery
        .schedule_files()
        .iter()
        .any(|file| file.path == transcript && file.was_cold));
}

#[test]
fn cold_round_robin_is_a_fallback_when_append_does_not_change_parent_mtime() {
    let mut discovery = DiscoveryIndex::default();
    let count = COLD_FILE_PROBES_PER_TICK * 3 + 1;
    let target = PathBuf::from(format!("/cold/session-{}.jsonl", count - 1));

    for index in 0..count {
        let path = PathBuf::from(format!("/cold/session-{index}.jsonl"));
        discovery.add_file(path.clone(), DiscoveredKind::ChatGptExport, false);
        mark_cold(&mut discovery, &path);
    }

    let mut found_on_cycle = None;
    for cycle in 1..=4 {
        let scheduled = discovery.schedule_files();
        if scheduled.iter().any(|file| file.path == target) {
            found_on_cycle = Some(cycle);
            break;
        }
        for file in scheduled {
            discovery.record_unchanged(
                &file.path,
                file.was_cold,
                Some(UNIX_EPOCH),
                SystemTime::now(),
            );
        }
    }

    assert_eq!(found_on_cycle, Some(4));
}

#[test]
fn missing_configured_root_stays_tracked_until_it_appears() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let root = temp.path().join("codex-sessions");
    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&root, DirectoryKind::Codex, true);

    discovery.probe_directories();
    assert!(discovery.directories.contains_key(&root));

    let day = root.join("2026/08/02");
    std::fs::create_dir_all(&day).expect("date tree");
    let transcript =
        day.join("rollout-2026-08-02T12-00-00-019a731e-4a58-71b1-a71f-a8d2f9782113.jsonl");
    std::fs::write(&transcript, "{}\n").expect("codex transcript");

    discovery.probe_directories();
    assert!(discovery.files.contains_key(&transcript));
    assert!(discovery.hot_files.contains(&transcript));
}

#[cfg(unix)]
#[test]
fn tracked_directory_replaced_by_symlink_is_removed_on_next_refresh() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let tracked = temp.path().join("tracked");
    let moved = temp.path().join("moved");
    let outside = tempfile::TempDir::new().expect("outside dir");
    std::fs::create_dir(&tracked).expect("tracked dir");
    let transcript = tracked.join("session.jsonl");
    std::fs::write(&transcript, "inside\n").expect("inside transcript");
    std::fs::write(outside.path().join("session.jsonl"), "outside\n").expect("outside transcript");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&tracked, DirectoryKind::ClaudeCodeProject, false);
    assert!(discovery.files.contains_key(&transcript));
    assert!(discovery
        .schedule_files()
        .iter()
        .any(|file| file.path == transcript));

    std::fs::rename(&tracked, &moved).expect("move tracked dir");
    std::os::unix::fs::symlink(outside.path(), &tracked).expect("replace with symlink");
    discovery.probe_directories();

    assert!(!discovery.directories.contains_key(&tracked));
    assert!(!discovery.files.contains_key(&transcript));
    assert!(!discovery
        .schedule_files()
        .iter()
        .any(|file| file.path == transcript));
    assert!(discovery.take_removed_files().contains(&transcript));
}

#[test]
fn nested_configured_root_keeps_identity_across_ancestor_remove_and_recreate() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let ancestor = temp.path().join("exports");
    let nested = ancestor.join("claude-ai");
    std::fs::create_dir_all(&nested).expect("nested export root");
    let export = nested.join("conversations.json");
    std::fs::write(&export, "[]").expect("export fixture");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&ancestor, DirectoryKind::ChatGptExport, true);
    discovery.add_directory_tree(&nested, DirectoryKind::ClaudeAiExport, true);

    std::fs::remove_dir_all(&nested).expect("remove nested configured root");
    discovery
        .directories
        .get_mut(&ancestor)
        .expect("tracked ancestor")
        .fingerprint = None;
    discovery.probe_directories();

    let missing_root = discovery
        .directories
        .get(&nested)
        .expect("configured nested root remains scheduled");
    assert!(missing_root.pinned);
    assert!(missing_root.kinds.contains(&DirectoryKind::ClaudeAiExport));

    std::fs::create_dir_all(&nested).expect("recreate nested configured root");
    std::fs::write(&export, "[]").expect("recreated export fixture");
    discovery
        .directories
        .get_mut(&ancestor)
        .expect("tracked ancestor")
        .fingerprint = None;
    discovery.probe_directories();

    let kinds = &discovery
        .files
        .get(&export)
        .expect("recreated export discovered")
        .kinds;
    assert!(kinds.contains(&DiscoveredKind::ClaudeAiExport));
}

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

#[test]
fn export_directory_becoming_a_file_keeps_pinned_identity_for_reversion() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let root = temp.path().join("conversations.json");
    std::fs::create_dir(&root).expect("root directory");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&root, DirectoryKind::ChatGptExport, true);

    // The configured root is replaced by a regular export file.
    std::fs::remove_dir(&root).expect("remove root directory");
    std::fs::write(&root, "[]").expect("export file fixture");
    discovery.probe_directories();

    let file = discovery.files.get(&root).expect("export file tracked");
    assert!(file.pinned);
    assert!(file.kinds.contains(&DiscoveredKind::ChatGptExport));
    let directory = discovery
        .directories
        .get(&root)
        .expect("directory identity retained while the path is a file");
    assert!(directory.pinned);
    assert!(directory.kinds.contains(&DirectoryKind::ChatGptExport));

    // The path reverts to a directory containing an export file.
    std::fs::remove_file(&root).expect("remove export file");
    std::fs::create_dir(&root).expect("recreate root directory");
    std::fs::write(root.join("conversations.json"), "[]").expect("nested export fixture");
    discovery.probe_directories();

    let directory = discovery
        .directories
        .get(&root)
        .expect("configured root still scheduled after reversion");
    assert!(directory.pinned);
    assert!(directory.kinds.contains(&DirectoryKind::ChatGptExport));
    assert!(
        !discovery.files.contains_key(&root),
        "the stale file record from the file phase must not shadow the directory"
    );
    assert!(discovery
        .files
        .contains_key(&root.join("conversations.json")));
}

#[test]
fn pinned_file_placeholder_at_startup_reverts_to_a_directory() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let root = temp.path().join("codex-sessions");
    std::fs::write(&root, "unexpected regular file").expect("startup file fixture");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&root, DirectoryKind::Codex, true);

    let placeholder = discovery
        .directories
        .get(&root)
        .expect("pinned file gets a directory placeholder");
    assert!(placeholder.pinned);
    assert!(placeholder.kinds.contains(&DirectoryKind::Codex));
    assert!(!discovery.files.contains_key(&root));

    std::fs::remove_file(&root).expect("remove startup file");
    std::fs::create_dir(&root).expect("recreate configured root as directory");
    let day = root.join("2026/08/05");
    std::fs::create_dir_all(&day).expect("date directory");
    let transcript =
        day.join("rollout-2026-08-05T12-00-00-019a731e-4a58-71b1-a71f-a8d2f9782113.jsonl");
    std::fs::write(&transcript, "{}\n").expect("reversion transcript");

    discovery.probe_directories();

    let directory = discovery
        .directories
        .get(&root)
        .expect("placeholder remains scheduled after reversion");
    assert!(directory.pinned);
    assert!(discovery.files.contains_key(&transcript));
}

#[test]
fn force_rescan_without_fingerprint_change_does_not_reprioritize_cold_files() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let project = temp.path().join("project-slug");
    std::fs::create_dir(&project).expect("project dir");
    let transcript = project.join("session.jsonl");
    std::fs::write(&transcript, "first\n").expect("transcript fixture");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeCodeRoot, true);
    mark_cold(&mut discovery, &transcript);

    // Drain the startup enqueue with an unchanged probe.
    discovery.probe_directories();

    discovery
        .directories
        .get_mut(&project)
        .expect("tracked project directory")
        .unchanged_probes = DIRECTORY_FORCE_RESCAN_PROBES;

    let stats = discovery.probe_directories();
    assert_eq!(stats.walks, 1);
    assert!(
        discovery.priority_cold_queue.is_empty(),
        "a forced rescan of an unchanged directory carries no change signal"
    );
    assert!(discovery.cold_enqueued.contains(&transcript));
}
