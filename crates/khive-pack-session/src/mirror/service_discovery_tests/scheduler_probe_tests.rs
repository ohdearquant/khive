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
