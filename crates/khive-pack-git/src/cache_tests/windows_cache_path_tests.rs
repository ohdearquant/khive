/// Windows closes this race by preventing each rename, not by allowing
/// a rename and following an fd as the Unix regression above does.
#[cfg(windows)]
#[test]
fn issue2149_windows_pins_slot_and_git_child_through_fetch() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    for child in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (upstream, slot, foreign) = windows_fetchable_slot(dir.path());
        let validated = revalidate_owned_slot(&slot).expect("owned slot validates");
        let target = if child {
            slot.join(".git")
        } else {
            slot.clone()
        };
        let moved = target.with_file_name("moved-aside");
        assert!(
            std::fs::rename(&target, &moved).is_err(),
            "validated component must not be movable before git completes: {}",
            target.display()
        );
        assert!(target.is_dir());
        assert!(!moved.exists());
        // A wrong caller spelling must not override the validated path.
        // Foreign has a fetchable origin, so a pathname regression would
        // succeed there rather than fail for an unrelated setup reason.
        fetch(&foreign, &validated).expect("fetch into pinned slot");
        advance_to_fetched_tip(&foreign, &validated).expect("advance pinned refs");
        assert_eq!(head_sha(&slot), head_sha(&upstream));
        assert!(slot.join(".git/FETCH_HEAD").exists());
        assert!(!foreign.join(".git/FETCH_HEAD").exists());
        drop(validated);
        std::fs::rename(&target, &moved).expect("same rename succeeds after pin release");
    }
}

#[cfg(windows)]
#[test]
fn issue2149_windows_pins_every_git_path_ancestor() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    for parent in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let (upstream, slot, foreign) = windows_fetchable_slot(dir.path());
        let root = slot.parent().unwrap();
        let target = if parent { root.parent().unwrap() } else { root };
        let moved = target.with_file_name("ancestor-moved");
        let validated = revalidate_owned_slot(&slot).expect("owned slot validates");
        assert!(
            std::fs::rename(target, &moved).is_err(),
            "every directory in the Git command path must remain pinned"
        );
        fetch(&slot, &validated).unwrap();
        advance_to_fetched_tip(&slot, &validated).unwrap();
        assert_eq!(head_sha(&slot), head_sha(&upstream));
        assert!(!foreign.join(".git/FETCH_HEAD").exists());
        drop(validated);
        std::fs::rename(target, &moved).expect("ancestor can move after command pins drop");
    }
}

#[cfg(windows)]
#[test]
fn issue2149_windows_refuses_reparse_ownership_components() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    for component in ["slot", ".git", MARKER_FILE] {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let slot = make_owned_entry(&root, "aaaaaaaaaaaaaaaa", true);
        let target = if component == "slot" {
            slot.clone()
        } else {
            slot.join(component)
        };
        let moved = target.with_file_name("unowned-target");
        std::fs::rename(&target, &moved).unwrap();
        if component == MARKER_FILE {
            std::fs::write(&moved, b"foreign marker must survive").unwrap();
            std::os::windows::fs::symlink_file(&moved, &target).expect(
                "Windows CI needs Developer Mode or symlink privilege; do not skip this witness",
            );
        } else {
            std::os::windows::fs::symlink_dir(&moved, &target).expect(
                "Windows CI needs Developer Mode or symlink privilege; do not skip this witness",
            );
        }
        assert!(matches!(
            revalidate_owned_slot(&slot),
            Err(CacheError::UnsafeToReplace(_))
        ));
        assert!(moved.exists(), "revalidation never removes foreign data");
        if component == MARKER_FILE {
            assert_eq!(
                std::fs::read(&moved).unwrap(),
                b"foreign marker must survive"
            );
            std::fs::remove_file(&target).unwrap();
        } else {
            std::fs::remove_dir(&target).unwrap();
        }
    }
}

#[cfg(windows)]
#[test]
fn issue2149_windows_resolved_path_survives_a_scratch_alias_swap() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let (upstream, slot, foreign) = windows_fetchable_slot(dir.path());
    let alias = dir.path().join("scratch-alias");
    let foreign_root = dir.path().join("foreign-root");
    std::fs::create_dir_all(&foreign_root).unwrap();
    let foreign_slot = foreign_root.join(slot.file_name().unwrap());
    std::os::windows::fs::symlink_dir(&foreign, &foreign_slot)
        .expect("Windows CI needs Developer Mode or symlink privilege; do not skip this witness");
    std::os::windows::fs::symlink_dir(slot.parent().unwrap(), &alias)
        .expect("Windows CI needs Developer Mode or symlink privilege; do not skip this witness");
    let alias_slot = alias.join(slot.file_name().unwrap());
    let validated = revalidate_owned_slot(&alias_slot).expect("resolved scratch alias validates");
    // Pins cover the resolved Git path, not this original alias. Replacing
    // the alias must therefore succeed and still have no effect on Git.
    std::fs::remove_dir(&alias).unwrap();
    // The replacement has the same slot name and a fetchable repository,
    // so reusing the original spelling would mutate foreign FETCH_HEAD.
    std::os::windows::fs::symlink_dir(&foreign_root, &alias).unwrap();
    fetch(&alias_slot, &validated).unwrap();
    advance_to_fetched_tip(&alias_slot, &validated).unwrap();
    assert_eq!(head_sha(&slot), head_sha(&upstream));
    assert!(!foreign.join(".git/FETCH_HEAD").exists());
    drop(validated);
    std::fs::remove_dir(&alias).unwrap();
    std::fs::remove_dir(&foreign_slot).unwrap();
}

#[cfg(windows)]
#[test]
fn issue2149_windows_over_cap_cleanup_releases_pins() {
    if crate::test_process::run_in_child() {
        return;
    }
    let _guard = ENV_MUTEX.blocking_lock();
    let dir = tempfile::tempdir().unwrap();
    let upstream = dir.path().join("upstream");
    std::fs::create_dir_all(&upstream).unwrap();
    init_origin_with_one_commit(&upstream);
    let root = dir.path().join("cache-root");
    let _scratch = ScratchRootGuard::set(&root);
    let url = upstream.to_str().unwrap();
    for refetch in [false, true] {
        std::env::set_var(
            "KHIVE_GIT_DIGEST_CLONE_MAX_BYTES",
            DEFAULT_CLONE_MAX_BYTES.to_string(),
        );
        let slot = ensure_clone(url).expect("create a normal owned slot");
        std::env::set_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES", "1");
        let result = if refetch {
            refetch_clone(url)
        } else {
            ensure_clone(url)
        };
        assert!(
            matches!(result, Err(CacheError::CloneTooLarge { .. })),
            "over-cap cleanup must complete, got {result:?}"
        );
        assert!(
            !slot.exists(),
            "pins must not prevent owned over-cap removal"
        );
    }
    std::env::remove_var("KHIVE_GIT_DIGEST_CLONE_MAX_BYTES");
}
