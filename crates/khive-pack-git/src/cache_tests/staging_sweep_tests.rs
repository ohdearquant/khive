#[test]
fn stale_staging_sweep_removes_an_abandoned_wrapper_lacking_a_lock_file_once_old() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, _held) = make_staging_wrapper(root.path(), false);
    std::fs::create_dir_all(wrapper.join("repo/partial.git/objects")).expect("nested payload");
    std::fs::write(wrapper.join("repo/partial.git/objects/pack"), b"partial")
        .expect("write orphan payload");
    let observed_mtime = std::fs::symlink_metadata(&wrapper)
        .expect("wrapper metadata")
        .modified()
        .expect("wrapper mtime");

    let removed = reap_stale_staging(
        root.path(),
        observed_mtime + std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(1),
    )
    .expect("reap stale staging directory");

    assert_eq!(removed, 1);
    assert!(
        !wrapper.exists(),
        "abandoned staging payload must be reclaimed"
    );
}

#[test]
fn staging_sweep_skips_wrapper_removed_before_mtime_read() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, _held) = make_staging_wrapper(root.path(), false);
    let mut mtime_reads = 0;
    let removed = reap_stale_staging_with(
        root.path(),
        SystemTime::now(),
        Duration::from_secs(1),
        |path| {
            assert_eq!(path, wrapper.as_path());
            mtime_reads += 1;
            std::fs::remove_dir_all(path).expect("concurrent cleanup removes admitted wrapper");
            std::fs::symlink_metadata(path).and_then(|m| m.modified())
        },
    )
    .expect("a wrapper gone during liveness classification is skipped");

    assert_eq!(mtime_reads, 1, "the real sweep must reach the second stat");
    assert_eq!(removed, 0, "another cleanup's removal is not counted");
    assert_eq!(
        std::fs::symlink_metadata(&wrapper).unwrap_err().kind(),
        std::io::ErrorKind::NotFound
    );
}

#[test]
fn staging_sweep_preserves_wrapper_mtime_errors() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, _held) = make_staging_wrapper(root.path(), false);
    let payload = wrapper.join("repo").join("payload");
    std::fs::write(&payload, b"keep").expect("fixture payload");
    let mut mtime_reads = 0;
    let error = reap_stale_staging_with(
        root.path(),
        SystemTime::now(),
        Duration::from_secs(1),
        |path| {
            assert_eq!(path, wrapper.as_path());
            mtime_reads += 1;
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "injected wrapper mtime denial",
            ))
        },
    )
    .expect_err("non-NotFound errors must still fail the sweep");

    match error {
        CacheError::Io(error) => {
            assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
            let message = error.to_string();
            assert!(message.contains("reap_stale_staging: wrapper mtime"));
            assert!(message.contains(&wrapper.display().to_string()));
            assert!(message.contains("injected wrapper mtime denial"));
        }
        other => panic!("expected wrapper mtime I/O error, got {other:?}"),
    }
    assert_eq!(mtime_reads, 1);
    assert!(wrapper.is_dir());
    assert_eq!(std::fs::read(payload).unwrap(), b"keep");
}

/// Blocking-finding acceptance test: a wrapper whose lock is still held
/// by a live handle must survive the sweep no matter how far past the
/// age fence it is -- age alone must never be the deletion criterion.
#[test]
fn stale_staging_sweep_preserves_a_wrapper_whose_lock_is_still_held_past_the_age_fence() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, held) = make_staging_wrapper(root.path(), true);
    let _held = held.expect("lock guard");

    let far_future = SystemTime::now() + std::time::Duration::from_secs(365 * 24 * 60 * 60);
    let removed = reap_stale_staging(root.path(), far_future, std::time::Duration::from_secs(1))
        .expect("sweep around a live wrapper");

    assert_eq!(removed, 0);
    assert!(
        wrapper.exists(),
        "a wrapper whose lock is still held by a live process must survive, \
             even a full year past the age fence"
    );
}

/// The flip side of the test above: liveness, not freshness, is what
/// matters. An abandoned wrapper (its lock file exists but nothing
/// holds the lock) is reclaimed even when it was created moments ago.
#[test]
fn stale_staging_sweep_removes_an_abandoned_wrapper_even_when_fresh() {
    let root = tempfile::tempdir().expect("tempdir");
    let (wrapper, _id, held) = make_staging_wrapper(root.path(), true);
    // Simulate the owning process dying: release the lock (dropping the
    // handle is exactly what the kernel does on process exit/kill).
    drop(held);

    let removed = reap_stale_staging(
        root.path(),
        SystemTime::now(),
        std::time::Duration::from_secs(24 * 60 * 60),
    )
    .expect("sweep an abandoned-but-fresh wrapper");

    assert_eq!(removed, 1);
    assert!(
        !wrapper.exists(),
        "an abandoned wrapper must be reclaimed even when it is not old"
    );
}

#[test]
fn staging_sweep_preserves_foreign_nested_and_nondirectory_entries_even_when_stale() {
    let root = tempfile::tempdir().expect("tempdir");
    let namespace_root = ensure_staging_namespace(root.path()).expect("namespace");

    let live_id = Uuid::new_v4();
    let live_wrapper = namespace_root.join(live_id.to_string());
    std::fs::create_dir_all(&live_wrapper).expect("create live wrapper");

    let foreign = namespace_root.join("not-a-canonical-uuid");
    std::fs::create_dir_all(&foreign).expect("create foreign-named dir");
    let staging_file = namespace_root.join(Uuid::new_v4().to_string());
    std::fs::write(&staging_file, b"operator file").expect("write uuid-shaped file");
    let nested = namespace_root
        .join("operator-owned")
        .join(Uuid::new_v4().to_string());
    std::fs::create_dir_all(&nested).expect("create nested uuid-shaped dir");

    // Every entry above is missing its lock file, so the fallback age
    // check applies -- drive `now` far enough past `max_age` that every
    // candidate would be reclaimed by age alone. Only the containment,
    // name-shape, and type checks may save them (regression coverage
    // for the bug where a future-dated fixture never reached those
    // checks at all).
    let far_future = SystemTime::now() + std::time::Duration::from_secs(365 * 24 * 60 * 60);
    let removed = reap_stale_staging(root.path(), far_future, std::time::Duration::from_secs(1))
        .expect("scan namespace entries");

    assert_eq!(
        removed, 1,
        "only the canonical-UUID live wrapper is reclaimed"
    );
    assert!(!live_wrapper.exists());
    assert!(foreign.is_dir(), "a non-UUID name is not staging-shaped");
    assert!(staging_file.is_file(), "the sweep removes directories only");
    assert!(
        nested.is_dir(),
        "the sweep never descends below the namespace root"
    );
}

/// An interrupted `delete_verified_owned_entry` leaves its renamed
/// `trash-<uuid>` slot behind with no lock file. The sweep must reclaim
/// it once past the age fence (or the deletion path reintroduces the
/// unreclaimable-residue class this module closes), must preserve it
/// while fresh (an in-flight recursive delete), and must never touch a
/// trash-prefixed name whose suffix is not a canonical UUID.
#[test]
fn staging_sweep_reclaims_old_trash_residue_but_preserves_fresh_and_lookalikes() {
    let root = tempfile::tempdir().expect("tempdir");
    let namespace_root = ensure_staging_namespace(root.path()).expect("namespace");

    let old_trash = namespace_root.join(format!("trash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(old_trash.join("repo/.git/objects")).expect("old trash payload");
    let lookalike = namespace_root.join("trash-not-a-canonical-uuid");
    std::fs::create_dir_all(&lookalike).expect("create trash lookalike");
    let observed_mtime = std::fs::symlink_metadata(&old_trash)
        .expect("trash metadata")
        .modified()
        .expect("trash mtime");

    let removed = reap_stale_staging(
        root.path(),
        observed_mtime + std::time::Duration::from_secs(2),
        std::time::Duration::from_secs(1),
    )
    .expect("reap trash residue");
    assert_eq!(removed, 1, "only the canonical trash residue is reclaimed");
    assert!(!old_trash.exists(), "aged trash residue must be reclaimed");
    assert!(
        lookalike.is_dir(),
        "a non-canonical trash suffix is not cache-owned"
    );

    let fresh_trash = namespace_root.join(format!("trash-{}", Uuid::new_v4()));
    std::fs::create_dir_all(fresh_trash.join("repo/.git")).expect("fresh trash payload");
    let fresh_mtime = std::fs::symlink_metadata(&fresh_trash)
        .expect("fresh trash metadata")
        .modified()
        .expect("fresh trash mtime");
    let removed = reap_stale_staging(
        root.path(),
        fresh_mtime + std::time::Duration::from_secs(1),
        std::time::Duration::from_secs(60),
    )
    .expect("scan fresh trash residue");
    assert_eq!(removed, 0, "an in-flight deletion must survive the sweep");
    assert!(
        fresh_trash.is_dir(),
        "fresh trash residue is protected by the age fence"
    );
}
