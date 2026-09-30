use super::{publish_remote_cache, MetaJson, PublishFailAt, RemoteName, REMOTE_BACKUP_OWNER_FILE};
use std::path::{Path, PathBuf};

fn fixture_meta(label: &str) -> MetaJson {
    MetaJson {
        fetched_at: "2000-01-01T00:00:00Z".into(),
        git_ref: label.into(),
        commit_sha: "0".repeat(40),
        content_hash: format!("sha256:{}", "0".repeat(64)),
    }
}

fn publish(root: &Path, name: &str, label: &str) -> PathBuf {
    assert!(
        RemoteName::parse(name).is_ok(),
        "fixture name must be public-API-valid"
    );
    publish_remote_cache(root, name, &[], &[], &fixture_meta(label), None)
        .expect("publish complete synthetic generation")
}

fn snapshot(path: &Path) -> Vec<Vec<u8>> {
    ["entities.ndjson", "edges.ndjson", "meta.json"]
        .iter()
        .map(|name| std::fs::read(path.join(name)).expect("generation member remains"))
        .collect()
}

fn distinct_remote_name() -> String {
    format!("upstream.replaced-{}", u64::from(std::process::id()) + 1)
}

fn contributor_directory(root: &Path) -> PathBuf {
    let directory = root.join("upstream.replaced~7");
    std::fs::create_dir_all(&directory).unwrap();
    std::fs::write(directory.join("notes.md"), "contributor content\n").unwrap();
    directory
}

#[test]
fn backup_named_contributor_directory_survives_with_existing_cache() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    let target = publish(&root, "upstream", "old-target");
    let contributor = contributor_directory(&root);

    publish(&root, "upstream", "new-target");

    assert_eq!(
        std::fs::read_to_string(contributor.join("notes.md")).unwrap(),
        "contributor content\n"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&snapshot(&target)[2]).unwrap()["git_ref"],
        "new-target"
    );
}

#[test]
fn backup_named_contributor_directory_survives_without_cache() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    std::fs::create_dir_all(&root).unwrap();
    let contributor = contributor_directory(&root);

    let target = publish(&root, "upstream", "first-target");

    assert_eq!(
        std::fs::read_to_string(contributor.join("notes.md")).unwrap(),
        "contributor content\n"
    );
    assert!(!target.join("notes.md").exists());
    assert!(target.join("meta.json").exists());
}

#[test]
fn unrecognized_marker_does_not_authorize_backup_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    std::fs::create_dir_all(&root).unwrap();
    let contributor = contributor_directory(&root);
    std::fs::write(
        contributor.join(REMOTE_BACKUP_OWNER_FILE),
        "not a cache marker\n",
    )
    .unwrap();

    let target = publish(&root, "upstream", "first-target");

    assert!(contributor.join("notes.md").exists());
    assert!(contributor.join(REMOTE_BACKUP_OWNER_FILE).exists());
    assert!(!target.join("notes.md").exists());
}

#[test]
fn undecodable_or_oversized_backup_marker_stays_unowned_during_publish() {
    for marker_bytes in [vec![0xff, 0xfe], vec![b'x'; 1024 * 1024]] {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("remotes");
        std::fs::create_dir_all(&root).unwrap();
        let contributor = contributor_directory(&root);
        std::fs::write(contributor.join(REMOTE_BACKUP_OWNER_FILE), &marker_bytes).unwrap();

        let target = publish(&root, "upstream", "first-target");
        assert_eq!(
            std::fs::read(contributor.join(REMOTE_BACKUP_OWNER_FILE)).unwrap(),
            marker_bytes,
            "unverified contributor marker must remain untouched"
        );
        assert!(contributor.join("notes.md").exists());
        assert!(target.join("meta.json").exists());
    }
}

#[test]
fn marker_reader_stops_after_expected_length_plus_one() {
    let temp = tempfile::tempdir().unwrap();
    let marker = temp.path().join("marker");
    std::fs::write(&marker, vec![b'x'; 1024 * 1024]).unwrap();
    assert_eq!(super::read_bounded_marker(&marker, 17).unwrap().len(), 18);
}

#[test]
fn backup_path_collision_never_replaces_contributor_directory() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    let target = publish(&root, "upstream", "old-target");
    let backup = root.join(format!("upstream.replaced~{}", std::process::id()));
    std::fs::create_dir_all(&backup).unwrap();
    std::fs::write(backup.join("notes.md"), "contributor content\n").unwrap();

    let error = publish_remote_cache(
        &root,
        "upstream",
        &[],
        &[],
        &fixture_meta("new-target"),
        None,
    )
    .expect_err("an occupied backup path must stop the swap");

    assert!(error.to_string().contains("already exists"), "{error:#}");
    assert_eq!(
        std::fs::read_to_string(backup.join("notes.md")).unwrap(),
        "contributor content\n"
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&snapshot(&target)[2]).unwrap()["git_ref"],
        "old-target"
    );
}

#[test]
fn failed_publish_preserves_another_valid_remote() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    let target = publish(&root, "upstream", "old-target");
    let other = publish(&root, &distinct_remote_name(), "independent-remote");
    let target_before = snapshot(&target);
    let other_before = snapshot(&other);

    let error = publish_remote_cache(
        &root,
        "upstream",
        &[],
        &[],
        &fixture_meta("candidate"),
        Some(PublishFailAt::BeforeSwap),
    )
    .expect_err("reach the deliberate pre-swap refusal");
    assert!(
        error.to_string().contains("injected failure before swap"),
        "{error:#}"
    );
    assert_eq!(snapshot(&target), target_before);
    assert!(
        other.is_dir(),
        "failed publish deleted another valid remote cache"
    );
    assert_eq!(snapshot(&other), other_before);
}

#[test]
fn failed_first_publish_does_not_adopt_another_valid_remote() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    let other = publish(&root, &distinct_remote_name(), "independent-remote");
    let other_before = snapshot(&other);
    let target = root.join("upstream");
    assert!(!target.exists());

    let error = publish_remote_cache(
        &root,
        "upstream",
        &[],
        &[],
        &fixture_meta("candidate"),
        Some(PublishFailAt::BeforeSwap),
    )
    .expect_err("reach the deliberate pre-swap refusal");
    assert!(
        error.to_string().contains("injected failure before swap"),
        "{error:#}"
    );
    assert!(
        !target.exists(),
        "failed first publish adopted another remote's generation"
    );
    assert!(
        other.is_dir(),
        "another valid remote was moved out of its own namespace"
    );
    assert_eq!(snapshot(&other), other_before);
}

#[test]
fn successful_publish_preserves_another_valid_remote() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("remotes");
    let target = publish(&root, "upstream", "old-target");
    let other = publish(&root, &distinct_remote_name(), "independent-remote");
    let target_before = snapshot(&target);
    let other_before = snapshot(&other);

    publish(&root, "upstream", "new-target");
    assert_ne!(
        snapshot(&target),
        target_before,
        "control must replace target metadata"
    );
    assert!(
        other.is_dir(),
        "successful publish deleted another valid remote"
    );
    assert_eq!(snapshot(&other), other_before);
}
