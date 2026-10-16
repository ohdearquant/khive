#[test]
fn evict_lru_only_touches_children_of_root() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "5");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

    let root = dir.path().join("scratch-root");
    std::fs::create_dir_all(&root).unwrap();
    let kept = make_owned_entry(&root, "3333333333333333", true);

    evict_lru(&root, &kept).expect("evict");
    assert!(kept.exists());

    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
}

#[test]
fn evict_lru_never_removes_a_foreign_directory_under_root() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    // Cap of 0 repos: without ownership filtering this would previously
    // have wiped out every child of root, including operator data.
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "0");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "0");

    let root = dir.path().join("scratch-root");
    std::fs::create_dir_all(&root).unwrap();
    let foreign = root.join("not-a-cache-entry");
    std::fs::create_dir_all(&foreign).unwrap();
    std::fs::write(foreign.join("important.txt"), b"do not delete me").unwrap();
    let kept = make_owned_entry(&root, "4444444444444444", true);

    evict_lru(&root, &kept).expect("evict");

    assert!(
        foreign.exists(),
        "a directory that doesn't look like a cache slot must survive eviction"
    );
    assert!(
        foreign.join("important.txt").exists(),
        "foreign directory contents must be untouched"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
}

#[test]
fn evict_lru_does_not_grow_registry_for_unrelated_scratch_root_children() {
    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path().join("scratch-root");
    std::fs::create_dir_all(&root).unwrap();
    let kept = make_owned_entry(&root, "4444444444444444", true);

    for index in 0..32 {
        std::fs::create_dir_all(root.join(format!("operator-data-{index}"))).unwrap();
    }

    let baseline = slot_lock_registry_len();
    evict_lru(&root, &kept).expect("evict");
    assert_eq!(
        slot_lock_registry_len(),
        baseline,
        "unrelated scratch-root children must not allocate slot locks"
    );
}

#[test]
fn evict_lru_never_removes_an_owned_looking_dir_missing_the_marker() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "0");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "0");

    let root = dir.path().join("scratch-root");
    std::fs::create_dir_all(&root).unwrap();
    // Has a .git dir and a valid cache-key-shaped name, but no marker --
    // e.g. a clone that failed after `clone()` but before `touch()`.
    let no_marker = make_owned_entry(&root, "5555555555555555", false);
    let kept = make_owned_entry(&root, "6666666666666666", true);

    let baseline = slot_lock_registry_len();
    evict_lru(&root, &kept).expect("evict");

    assert!(
        no_marker.exists(),
        "an owned-looking directory without the marker must survive eviction"
    );
    assert_eq!(
        slot_lock_registry_len(),
        baseline,
        "an unowned cache-shaped child must not allocate a slot lock"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
}
