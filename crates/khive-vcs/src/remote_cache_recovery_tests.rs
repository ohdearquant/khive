use super::{publish_remote_cache, MetaJson, PublishFailAt, RemoteName};
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
