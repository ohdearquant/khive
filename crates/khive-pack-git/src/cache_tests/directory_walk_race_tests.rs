/// Issue #842 macOS ENOENT flake family: a descendant disappearing
/// mid-walk must shrink the total, not abort with `NotFound`. See
/// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
#[test]
fn dir_size_tolerates_a_subdirectory_removed_mid_walk() {
    for _ in 0..200 {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().to_path_buf();
        let victim = root.join("victim");
        std::fs::create_dir_all(&victim).unwrap();
        for i in 0..64 {
            std::fs::write(victim.join(format!("f{i}.txt")), b"0123456789").unwrap();
        }
        // A wide fan of siblings so the walk still has entries left on
        // its stack (and is plausibly still inside `victim`) at the
        // instant the other thread deletes it.
        for i in 0..64 {
            let sibling = root.join(format!("sibling{i}"));
            std::fs::create_dir_all(&sibling).unwrap();
            std::fs::write(sibling.join("s.txt"), b"0123456789").unwrap();
        }

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let walk_root = root.clone();
        let walk_barrier = barrier.clone();
        let walker = std::thread::spawn(move || {
            walk_barrier.wait();
            dir_size(&walk_root)
        });
        let delete_victim = victim.clone();
        let deleter = std::thread::spawn(move || {
            barrier.wait();
            let _ = std::fs::remove_dir_all(&delete_victim);
        });

        let result = walker.join().expect("walker thread");
        deleter.join().expect("deleter thread");

        assert!(
            result.is_ok(),
            "dir_size must tolerate a subdirectory vanishing mid-walk, got {result:?}"
        );
    }
}

/// Companion to the test above (PR #847): the walk root itself
/// vanishing must error, not tolerate like a descendant. See
/// crates/khive-pack-git/docs/api/cache.md#test-module-notes.
#[test]
fn dir_size_errors_when_the_root_is_removed_mid_walk() {
    let mut saw_error = false;
    for _ in 0..500 {
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("slot");
        std::fs::create_dir_all(&root).unwrap();

        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let walk_root = root.clone();
        let walk_barrier = barrier.clone();
        let walker = std::thread::spawn(move || {
            walk_barrier.wait();
            dir_size(&walk_root)
        });
        let delete_root = root.clone();
        let deleter = std::thread::spawn(move || {
            barrier.wait();
            let _ = std::fs::remove_dir(&delete_root);
        });

        let result = walker.join().expect("walker thread");
        deleter.join().expect("deleter thread");

        match result {
            Ok(_) => continue, // walker won the race this round; try again
            Err(CacheError::Io(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                saw_error = true;
            }
            Err(e) => panic!("unexpected error kind from a vanished root: {e:?}"),
        }
    }
    assert!(
        saw_error,
        "root-vanish-mid-walk race was never hit across 500 iterations; \
             widen the fixture or investigate the barrier timing"
    );
}
