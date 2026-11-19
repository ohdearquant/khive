/// no-re-read guarantee: once the slot state is decided `Absent`, a
/// foreign directory appearing at the pathname must not be fetched
/// into, overwritten, or claimed.
#[test]
fn install_fresh_clone_refuses_a_foreign_occupied_slot() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    let _scratch = ScratchRootGuard::set(dir.path());

    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    test_git(&upstream, &["init", "-q"]);
    test_git(&upstream, &["config", "user.email", "t@example.com"]);
    test_git(&upstream, &["config", "user.name", "T"]);
    std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
    test_git(&upstream, &["add", "tracked.md"]);
    test_git(&upstream, &["commit", "-q", "-m", "commit A"]);
    let url = upstream.to_str().expect("utf8 path");

    // A foreign process's directory at the slot pathname: it has a
    // `.git` but no ownership marker, plus a sentinel byte the
    // assertions below prove survives.
    let root = scratch_root();
    prepare_cache_root(&root).expect("cache root");
    let repo_dir = root.join(cache_key(url));
    std::fs::create_dir_all(repo_dir.join(".git")).unwrap();
    std::fs::write(repo_dir.join("foreign.txt"), "foreign\n").unwrap();
    assert!(
        !is_owned_entry(&repo_dir),
        "fixture control: the occupying directory must be unowned"
    );

    install_fresh_clone(url, &root, &repo_dir, clone_max_bytes())
        .expect_err("install into an occupied foreign pathname must fail");
    assert_eq!(
        std::fs::read_to_string(repo_dir.join("foreign.txt")).expect("sentinel readable"),
        "foreign\n",
        "foreign bytes must survive a refused install"
    );
    assert!(
        !repo_dir.join(MARKER_FILE).exists(),
        "a refused install must never write the ownership marker"
    );
}

#[test]
fn is_owned_entry_rejects_non_cache_shapes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let root = dir.path();

    // Wrong length.
    let short = root.join("abc123");
    std::fs::create_dir_all(short.join(".git")).unwrap();
    std::fs::write(short.join(MARKER_FILE), b"").unwrap();
    assert!(!is_owned_entry(&short));

    // Uppercase hex (cache_key is always lowercase).
    let upper = root.join("ABCDEF0123456789");
    std::fs::create_dir_all(upper.join(".git")).unwrap();
    std::fs::write(upper.join(MARKER_FILE), b"").unwrap();
    assert!(!is_owned_entry(&upper));

    // Right shape but missing .git.
    let no_git = root.join("7777777777777777");
    std::fs::create_dir_all(&no_git).unwrap();
    std::fs::write(no_git.join(MARKER_FILE), b"").unwrap();
    assert!(!is_owned_entry(&no_git));

    let owned = make_owned_entry(root, "8888888888888888", true);
    assert!(is_owned_entry(&owned));
}

#[test]
fn dir_size_sums_file_bytes_recursively() {
    let dir = tempfile::tempdir().expect("tempdir");
    std::fs::write(dir.path().join("a.txt"), b"12345").unwrap();
    std::fs::create_dir_all(dir.path().join("sub")).unwrap();
    std::fs::write(dir.path().join("sub/b.txt"), b"1234567890").unwrap();
    assert_eq!(dir_size(dir.path()).unwrap(), 15);
}

/// PR #847: walk root vanishing must error, never launder to `Ok(0)`.
#[test]
fn dir_size_errors_when_the_root_itself_is_missing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let missing = dir.path().join("does-not-exist");
    let err = dir_size(&missing).expect_err("a missing root must error, not size to 0");
    assert!(
        matches!(&err, CacheError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
        "expected CacheError::Io(NotFound), got {err:?}"
    );
}

/// `keep` vanishing must propagate, not be treated as an empty slot.
#[test]
fn evict_lru_errors_when_keep_itself_is_missing() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS", "5");
    std::env::set_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES", "1000000000");

    let root = dir.path().join("scratch-root");
    std::fs::create_dir_all(&root).unwrap();
    let missing_keep = root.join("0000000000000000");

    let err = evict_lru(&root, &missing_keep).expect_err("a missing keep root must error");
    assert!(
        matches!(&err, CacheError::Io(e) if e.kind() == std::io::ErrorKind::NotFound),
        "expected CacheError::Io(NotFound), got {err:?}"
    );

    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_REPOS");
    std::env::remove_var("KHIVE_GIT_DIGEST_CACHE_MAX_BYTES");
}
