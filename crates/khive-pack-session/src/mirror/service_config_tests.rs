#[cfg(windows)]
use super::{ingest, DirectoryFingerprint};
use super::{parse_mirror_poll_secs, DirectoryKind, DiscoveredKind, DiscoveryIndex};

/// Regression for PACKSESSION-AUD-002: `KHIVE_MIRROR_POLL_SECS=0` used to
/// produce a hot polling loop via `Duration::from_secs(0)`. Explicit zero
/// must now be rejected back to the documented default, and the default
/// must remain distinguishable from a non-numeric value.
#[test]
fn poll_secs_zero_is_rejected_and_default_remains_two_seconds() {
    assert_eq!(
        parse_mirror_poll_secs(None),
        2,
        "missing value defaults to 2s"
    );
    assert_eq!(
        parse_mirror_poll_secs(Some("abc")),
        2,
        "non-numeric value defaults to 2s"
    );
    assert_eq!(
        parse_mirror_poll_secs(Some("0")),
        2,
        "explicit zero must be rejected back to the default, not accepted as a hot loop"
    );
    assert_eq!(
        parse_mirror_poll_secs(Some("1")),
        1,
        "valid nonzero value is honored"
    );
    assert_eq!(
        parse_mirror_poll_secs(Some("5")),
        5,
        "valid nonzero value is honored"
    );
}

#[test]
fn claude_ai_scanner_accepts_only_nested_conversations_json() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let nested = temp.path().join("download").join("claude-export");
    std::fs::create_dir_all(&nested).expect("nested export dir");
    let export = nested.join("conversations.json");
    std::fs::write(&export, "[]").expect("export fixture");
    std::fs::write(nested.join("conversations-copy.json"), "[]").expect("non-matching fixture");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeAiExport, true);
    assert_eq!(discovery.files.len(), 1);
    assert!(matches!(
        discovery
            .files
            .get(&export)
            .and_then(|file| file.kinds.first()),
        Some(&DiscoveredKind::ClaudeAiExport)
    ));
}

#[test]
fn overlapping_export_roots_retain_both_parser_candidates() {
    let temp = tempfile::TempDir::new().expect("temp dir");
    let export = temp.path().join("conversations.json");
    std::fs::write(&export, "[]").expect("export fixture");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(temp.path(), DirectoryKind::ChatGptExport, true);
    discovery.add_directory_tree(temp.path(), DirectoryKind::ClaudeAiExport, true);
    discovery.probe_directories();

    let kinds = &discovery.files.get(&export).expect("tracked export").kinds;
    assert_eq!(kinds.len(), 2);
    assert!(matches!(
        kinds.first(),
        Some(&DiscoveredKind::ChatGptExport)
    ));
    assert!(matches!(
        kinds.get(1),
        Some(&DiscoveredKind::ClaudeAiExport)
    ));
}

#[cfg(unix)]
#[test]
fn discovery_ignores_symlinked_transcript_outside_root() {
    let root = tempfile::TempDir::new().expect("root");
    let outside = tempfile::NamedTempFile::new().expect("outside transcript");
    let link = root.path().join("linked.jsonl");
    std::os::unix::fs::symlink(outside.path(), &link).expect("file symlink");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(root.path(), DirectoryKind::ClaudeCodeProject, true);
    assert!(!discovery.files.contains_key(&link));
}

#[cfg(unix)]
#[test]
fn configured_symlinked_export_root_is_refused_before_polling() {
    let root = tempfile::TempDir::new().expect("root");
    let target = tempfile::NamedTempFile::new().expect("export target");
    std::fs::write(target.path(), "[]").expect("export fixture");
    let link = root.path().join("conversations.json");
    std::os::unix::fs::symlink(target.path(), &link).expect("export symlink");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_export_root(
        &link,
        DirectoryKind::ChatGptExport,
        DiscoveredKind::ChatGptExport,
    );
    assert!(discovery.files.is_empty());
    assert!(discovery.directories.is_empty());
    assert!(discovery.schedule_files().is_empty());
}

#[cfg(unix)]
#[test]
fn pinned_export_file_replaced_by_symlink_is_not_rescheduled() {
    let root = tempfile::TempDir::new().expect("configured root");
    let root_path = std::fs::canonicalize(root.path()).expect("physical configured root");
    let outside = tempfile::TempDir::new().expect("outside root");
    let export = root_path.join("conversations.json");
    let outside_export = outside.path().join("conversations.json");
    std::fs::write(&export, "[]").expect("configured export");
    std::fs::write(&outside_export, "[{}]").expect("outside export");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&export, DirectoryKind::ChatGptExport, true);
    assert!(discovery.files.get(&export).is_some_and(|file| file.pinned));

    std::fs::remove_file(&export).expect("remove configured export");
    std::os::unix::fs::symlink(&outside_export, &export).expect("replace with symlink");
    discovery.add_directory_tree(&export, DirectoryKind::ChatGptExport, true);
    discovery.probe_directories();

    assert!(!discovery.files.contains_key(&export));
    assert!(discovery
        .directories
        .get(&export)
        .is_some_and(|dir| dir.pinned));
    assert!(discovery.schedule_files().is_empty());
    assert!(super::ingest::open_source_file_beneath(&root_path, &export, None).is_err());
}

#[cfg(unix)]
#[test]
fn initial_probe_refuses_a_linked_root_ancestor_in_a_writable_parent() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::TempDir::new().expect("fixture directory");
    let fixture = std::fs::canonicalize(temp.path()).expect("fixture anchor");
    let inside = fixture.join("inside");
    let root = inside.join("exports");
    std::fs::create_dir_all(&root).expect("source root");
    let export = root.join("conversations.json");
    std::fs::write(&export, b"[]").expect("source fixture");
    let (_, original_identity, _) =
        super::probe_source_file(&root, &export).expect("ordinary initial probe");
    let linked_ancestor = fixture.join("linked");
    std::os::unix::fs::symlink(&inside, &linked_ancestor).expect("fixture ancestor link");
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o777)).unwrap();
    let linked_root = linked_ancestor.join("exports");
    let linked_export = linked_root.join("conversations.json");

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&linked_root, DirectoryKind::ChatGptExport, true);
    assert!(discovery.files.contains_key(&linked_export));
    assert!(
        super::probe_source_file(&linked_root, &linked_export)
            .is_err_and(|e| e.to_string().contains("refused: parent_permissions")),
        "a link in an unsafe parent must refuse before the root identity is admitted"
    );
    let (_, reopened_identity, _) =
        super::probe_source_file(&root, &export).expect("ordinary root remains usable");
    assert_eq!(reopened_identity, original_identity);
    assert_eq!(std::fs::read(&export).expect("unchanged source"), b"[]");
}

#[cfg(unix)]
#[test]
fn initial_probe_refuses_a_root_ancestor_substituted_after_discovery() {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::{Arc, Barrier};

    let temp = tempfile::TempDir::new().expect("fixture directory");
    let fixture = std::fs::canonicalize(temp.path()).expect("fixture anchor");
    let ancestor = fixture.join("configured");
    let root = ancestor.join("exports");
    let protected_ancestor = fixture.join("protected");
    let protected_root = protected_ancestor.join("exports");
    std::fs::create_dir_all(&root).expect("configured source root");
    std::fs::create_dir_all(&protected_root).expect("protected fixture root");
    let export = root.join("conversations.json");
    let protected_export = protected_root.join("conversations.json");
    std::fs::write(&export, b"[]").expect("configured fixture");
    std::fs::write(&protected_export, b"[{}]").expect("protected fixture");
    let protected_bytes = std::fs::read(&protected_export).expect("protected bytes before");
    let protected_metadata =
        std::fs::metadata(&protected_export).expect("protected metadata before");
    let protected_identity = super::ingest::file_identity(
        &std::fs::File::open(&protected_export).expect("protected fixture handle"),
    )
    .expect("protected identity before");

    // Discovery is the pathname preflight. The first native probe has no directory witness
    // yet, so the root chain refuses the link substituted into this world-writable parent.
    std::fs::set_permissions(&fixture, std::fs::Permissions::from_mode(0o777)).unwrap();
    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&root, DirectoryKind::ChatGptExport, true);
    let scheduled = discovery.schedule_files();
    assert_eq!(scheduled.len(), 1);
    assert_eq!(scheduled[0].path, export);

    let barrier = Arc::new(Barrier::new(2));
    std::thread::scope(|scope| {
        let swap_barrier = Arc::clone(&barrier);
        let swap_ancestor = &ancestor;
        let saved_ancestor = fixture.join("configured-original");
        let swap_target = &protected_ancestor;
        let swap = scope.spawn(move || -> std::io::Result<()> {
            swap_barrier.wait();
            std::fs::rename(swap_ancestor, saved_ancestor)?;
            std::os::unix::fs::symlink(swap_target, swap_ancestor)
        });
        barrier.wait();
        swap.join()
            .expect("fixture substitution thread")
            .expect("fixture substitution");
        assert!(
            super::probe_source_file(&root, &scheduled[0].path)
                .is_err_and(|e| e.to_string().contains("refused: parent_permissions")),
            "initial admission must refuse a root ancestor substituted after discovery"
        );
    });

    let after = std::fs::metadata(&protected_export).expect("protected metadata after");
    assert_eq!(
        std::fs::read(&protected_export).expect("protected bytes after"),
        protected_bytes
    );
    assert_eq!(after.len(), protected_metadata.len());
    assert_eq!(
        after.modified().expect("mtime after"),
        protected_metadata.modified().expect("mtime before")
    );
    assert_eq!(
        super::ingest::file_identity(
            &std::fs::File::open(&protected_export).expect("protected fixture handle after"),
        )
        .expect("protected identity after"),
        protected_identity
    );
    assert_eq!(
        std::fs::read(fixture.join("configured-original/exports/conversations.json"))
            .expect("configured source remains intact"),
        b"[]"
    );
}

#[cfg(windows)]
#[test]
fn mirror_windows_junction_is_neither_discovered_nor_opened() {
    use std::os::windows::fs::MetadataExt;
    use std::process::Command;

    let temp = tempfile::TempDir::new().expect("tempdir");
    let root = temp.path().join("root");
    let outside = temp.path().join("outside");
    std::fs::create_dir_all(&root).expect("root");
    std::fs::create_dir_all(&outside).expect("outside");
    let inside_file = root.join("conversations.json");
    std::fs::write(&inside_file, "[]").expect("inside export");
    std::fs::write(outside.join("conversations.json"), "[]").expect("outside export");
    let junction = root.join("linked");
    let output = Command::new("cmd")
        .arg("/C")
        .arg("mklink")
        .arg("/J")
        .arg(&junction)
        .arg(&outside)
        .output()
        .expect("create junction");
    assert!(
        output.status.success(),
        "junction creation failed: {output:?}"
    );
    let metadata = std::fs::symlink_metadata(&junction).expect("junction metadata");
    assert_ne!(
        metadata.file_attributes()
            & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT,
        0
    );

    let mut discovery = DiscoveryIndex::default();
    discovery.add_directory_tree(&root, DirectoryKind::ChatGptExport, true);
    assert!(discovery.files.contains_key(&inside_file));
    assert!(!discovery.directories.contains_key(&junction));
    assert!(!discovery
        .files
        .contains_key(&junction.join("conversations.json")));
    let fingerprint =
        DirectoryFingerprint::from_metadata(&std::fs::metadata(&root).expect("root metadata"));
    discovery
        .refresh_directory(&root, &[DirectoryKind::ChatGptExport], fingerprint, true)
        .expect("refresh root");
    assert!(!discovery.directories.contains_key(&junction));
    assert!(
        ingest::open_source_file_beneath(&root, &junction.join("conversations.json"), None,)
            .is_err()
    );

    let mut configured = DiscoveryIndex::default();
    configured.add_export_root(
        &junction,
        DirectoryKind::ChatGptExport,
        DiscoveredKind::ChatGptExport,
    );
    assert!(configured.files.is_empty());
    assert!(configured.directories.is_empty());
}
