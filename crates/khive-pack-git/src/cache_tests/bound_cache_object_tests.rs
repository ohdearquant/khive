/// The install path must fail closed when a foreign (unowned) directory
/// occupies the slot pathname: staging plus a single `rename` refuses a
/// non-empty destination, so the foreign bytes survive and no ownership
/// marker is written. This is the second half of `ensure_clone_locked`'s
/// TOCTOU regression: a slot swapped for an empty directory between the
/// ownership decision and the fetch must (a) fail `revalidate_owned_slot`
/// and (b) even if a fetch were issued anyway, fail on the exact path
/// rather than discovering upward into an ancestor repository. The
/// ancestor here is a real git repository containing the cache root —
/// the shape under which `-C` discovery would have fetched into it.
#[test]
fn a_swapped_slot_is_refused_and_never_reaches_an_ancestor_repo() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");

    // A fetchable upstream, so that an upward-discovering `git -C` fetch
    // from inside the ancestor would SUCCEED (creating FETCH_HEAD) —
    // without it the old `-C` form fails for the wrong reason and this
    // test could not distinguish the fixed form from the broken one.
    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    test_git(&upstream, &["init", "-q"]);
    test_git(&upstream, &["config", "user.email", "t@example.com"]);
    test_git(&upstream, &["config", "user.name", "T"]);
    std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
    test_git(&upstream, &["add", "tracked.md"]);
    test_git(&upstream, &["commit", "-q", "-m", "commit A"]);

    // Ancestor repository enclosing the cache root, with that upstream
    // configured as origin.
    let ancestor = dir.path().join("ancestor");
    std::fs::create_dir_all(&ancestor).unwrap();
    test_git(&ancestor, &["init", "-q"]);
    test_git(
        &ancestor,
        &["remote", "add", "origin", upstream.to_str().expect("utf8")],
    );
    let root = ancestor.join("cache-root");
    std::fs::create_dir_all(&root).unwrap();
    let _scratch = ScratchRootGuard::set(&root);

    // The swapped-in slot: an empty directory where an owned clone stood.
    let slot = root.join("swapped-slot");
    std::fs::create_dir_all(&slot).unwrap();

    // (a) descriptor-bound revalidation refuses it.
    assert!(
        matches!(
            revalidate_owned_slot(&slot),
            Err(CacheError::UnsafeToReplace(_))
        ),
        "an empty directory at the slot pathname must fail revalidation"
    );

    // (b) even a command bound to the hostile directory itself (the
    // test-only constructor skips the ownership check layer (a) proves)
    // errors on the missing relative `.git` instead of discovering
    // upward into the ancestor.
    #[cfg(unix)]
    {
        assert!(
            fetch(&slot, &ValidatedSlot::for_test(&slot)).is_err(),
            "fetch against a vanished slot must fail loudly"
        );
    }
    assert!(
        !ancestor.join(".git").join("FETCH_HEAD").exists(),
        "the ancestor repository must be untouched by the failed fetch"
    );
}

/// The descriptor layer binds the git command to the directory OBJECT
/// validated by `revalidate_owned_slot`, not to the pathname: after
/// validation, the slot pathname is swapped for a symlink pointing at an
/// ancestor repository — the shape an absolute `--git-dir` would follow.
/// The bound fetch must land in the validated (renamed-aside) directory
/// and never touch the ancestor.
#[cfg(unix)]
#[test]
fn a_bound_command_follows_the_validated_object_not_the_swapped_pathname() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");

    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    test_git(&upstream, &["init", "-q"]);
    test_git(&upstream, &["config", "user.email", "t@example.com"]);
    test_git(&upstream, &["config", "user.name", "T"]);
    std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
    test_git(&upstream, &["add", "tracked.md"]);
    test_git(&upstream, &["commit", "-q", "-m", "commit A"]);
    let url = upstream.to_str().expect("utf8");

    // Ancestor repository with the same fetchable origin, so that a
    // symlink-following fetch would SUCCEED into it — without it the
    // broken form fails for the wrong reason.
    let ancestor = dir.path().join("ancestor");
    std::fs::create_dir_all(&ancestor).unwrap();
    test_git(&ancestor, &["init", "-q"]);
    test_git(&ancestor, &["remote", "add", "origin", url]);
    let root = ancestor.join("cache-root");
    std::fs::create_dir_all(&root).unwrap();
    let _scratch = ScratchRootGuard::set(&root);

    // A genuine owned slot.
    let slot = root.join("owned-slot");
    clone(url, &slot, clone_max_bytes()).expect("clone slot");
    std::fs::write(slot.join(MARKER_FILE), b"").expect("marker");

    let validated = revalidate_owned_slot(&slot).expect("owned slot validates");

    // Post-validation swap: the pathname now points at the ancestor.
    let moved = root.join("owned-slot-moved");
    std::fs::rename(&slot, &moved).expect("move validated dir aside");
    std::os::unix::fs::symlink(&ancestor, &slot).expect("symlink swap");

    fetch(&slot, &validated).expect("bound fetch follows the validated object");

    assert!(
        !ancestor.join(".git").join("FETCH_HEAD").exists(),
        "the ancestor repository must never receive the fetch"
    );
    assert!(
        moved.join(".git").join("FETCH_HEAD").exists(),
        "the fetch must land in the directory that was validated"
    );
}

/// The descriptor layer binds git to the validated `.git` OBJECT, not to
/// the name `.git`: after validation, the slot's `.git` CHILD ENTRY is
/// swapped for a symlink pointing at an ancestor repository's `.git` — the
/// shape a re-resolved relative `--git-dir .git` would follow. Binding the
/// parent slot alone left this open (the parent pathname is unchanged, so
/// only the child entry moves); the fetch must land in the validated
/// `.git` and never touch the ancestor.
#[cfg(unix)]
#[test]
fn a_bound_command_follows_the_validated_git_object_not_a_swapped_git_child() {
    if crate::test_process::run_in_child() {
        return;
    }

    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().expect("tempdir");

    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    test_git(&upstream, &["init", "-q"]);
    test_git(&upstream, &["config", "user.email", "t@example.com"]);
    test_git(&upstream, &["config", "user.name", "T"]);
    std::fs::write(upstream.join("tracked.md"), "content\n").unwrap();
    test_git(&upstream, &["add", "tracked.md"]);
    test_git(&upstream, &["commit", "-q", "-m", "commit A"]);
    let url = upstream.to_str().expect("utf8");

    // Ancestor repository with the same fetchable origin, so that a
    // symlink-following fetch would SUCCEED into it — without it the
    // broken form fails for the wrong reason.
    let ancestor = dir.path().join("ancestor");
    std::fs::create_dir_all(&ancestor).unwrap();
    test_git(&ancestor, &["init", "-q"]);
    test_git(&ancestor, &["remote", "add", "origin", url]);
    let root = ancestor.join("cache-root");
    std::fs::create_dir_all(&root).unwrap();
    let _scratch = ScratchRootGuard::set(&root);

    // A genuine owned slot.
    let slot = root.join("owned-slot");
    clone(url, &slot, clone_max_bytes()).expect("clone slot");
    std::fs::write(slot.join(MARKER_FILE), b"").expect("marker");

    let validated = revalidate_owned_slot(&slot).expect("owned slot validates");

    // Post-validation swap of the CHILD ENTRY: move the real `.git` aside
    // and point the name `.git` at the ancestor's `.git`. The slot
    // pathname itself is untouched, so a parent-only binding still lands
    // here and re-resolves `.git` by name.
    let real_git = slot.join(".git");
    let moved_git = slot.join(".git-moved");
    std::fs::rename(&real_git, &moved_git).expect("move validated .git aside");
    std::os::unix::fs::symlink(ancestor.join(".git"), &real_git).expect("symlink .git swap");

    fetch(&slot, &validated).expect("bound fetch follows the validated .git object");

    assert!(
        !ancestor.join(".git").join("FETCH_HEAD").exists(),
        "the ancestor repository must never receive the fetch"
    );
    assert!(
        moved_git.join("FETCH_HEAD").exists(),
        "the fetch must land in the .git object that was validated"
    );
}
