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
