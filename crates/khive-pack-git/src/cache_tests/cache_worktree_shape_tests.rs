/// Issue #2104 defect 2: the slot is cloned `--filter=blob:none`, but a
/// checkout backfills every blob reachable at HEAD and undoes the filter.
/// Nothing reads this worktree — every consumer command is `rev-parse`,
/// `log`, or `remote` — so the materialized tree is pure cost.
///
/// The second half of this test is the one that matters. `--no-checkout`
/// alone does not hold: the previous `advance_to_fetched_tip` ran
/// `reset --hard`, which repopulates the worktree on the very next
/// `ensure_clone` and gives the bytes straight back. So the assertion
/// after the re-ensure is what fails if the ref update ever regresses to
/// a reset, and the assertion on the fresh clone alone would not catch it.
#[test]
fn cache_slot_never_materializes_a_working_tree() {
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

    // The upstream itself HAS a working tree, so an empty result from
    // `worktree_entries` below is a real absence and not a helper that
    // always returns nothing.
    assert!(
        worktree_entries(&upstream).contains(&"tracked.md".to_string()),
        "control: the upstream must have a materialized tree, else the \
             assertions below prove nothing"
    );

    let url = upstream.to_str().expect("utf8 path");
    let slot = ensure_clone(url).expect("initial clone");
    assert_eq!(
        worktree_entries(&slot),
        Vec::<String>::new(),
        "a fresh cache slot must not check out a working tree"
    );
    assert_eq!(
        test_head_sha(&slot),
        test_head_sha(&upstream),
        "HEAD must still resolve without a checkout"
    );

    std::fs::write(upstream.join("second.md"), "more\n").unwrap();
    test_git(&upstream, &["add", "second.md"]);
    test_git(&upstream, &["commit", "-q", "-m", "commit B"]);
    let upstream_tip = test_head_sha(&upstream);

    let slot2 = ensure_clone(url).expect("re-ensure existing slot");
    assert_eq!(slot2, slot, "same cache slot");
    assert_eq!(
        test_head_sha(&slot2),
        upstream_tip,
        "advancing to the fetched tip must still work without a checkout"
    );
    assert_eq!(
        worktree_entries(&slot2),
        Vec::<String>::new(),
        "advancing the slot must move a ref, not reset a working tree: a \
             `reset --hard` here repopulates the tree and undoes the blob \
             filter on every pass"
    );
}
