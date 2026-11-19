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
