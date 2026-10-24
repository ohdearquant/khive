#[cfg(unix)]
#[test]
fn events_lock_preserves_search_only_parent_access() {
    use std::os::unix::fs::PermissionsExt;

    if crate::test_process::run_in_child() {
        return;
    }
    // Root bypasses the permission that distinguishes this arm. The owner
    // must record an ordinary nonroot run; a root skip is not its acceptance.
    // SAFETY: geteuid reads process identity and takes no pointers.
    if unsafe { libc::geteuid() } == 0 {
        eprintln!("SKIP: events lock search-only parent requires an ordinary nonroot actor");
        return;
    }
    struct RestorePermissions(PathBuf, std::fs::Permissions);
    impl Drop for RestorePermissions {
        fn drop(&mut self) {
            std::fs::set_permissions(&self.0, self.1.clone()).unwrap();
        }
    }
    let dir = tempfile::tempdir().unwrap();
    let parent = dir.path().join("search-only");
    std::fs::create_dir(&parent).unwrap();
    let _restore = RestorePermissions(
        parent.clone(),
        std::fs::metadata(&parent).unwrap().permissions(),
    );
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"victim-sentinel").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o300)).unwrap();
    assert_eq!(
        std::fs::File::open(&parent).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
    let socket = parent.join("events.sock");
    let held = match acquire_events_daemon_guard_outcome(&socket) {
        EventsDaemonGuardAcquisition::Held(guard) => guard,
        other => panic!("search-only parent must allow the plain lock: {other:?}"),
    };
    let lock = socket.with_extension("lock");
    assert_eq!(
        std::fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(matches!(
        acquire_events_daemon_guard_outcome(&socket),
        EventsDaemonGuardAcquisition::Contended
    ));
    drop(held);
    assert!(matches!(
        acquire_events_daemon_guard_outcome(&socket),
        EventsDaemonGuardAcquisition::Held(_)
    ));
    let linked_socket = parent.join("linked.sock");
    std::os::unix::fs::symlink(&victim, linked_socket.with_extension("lock")).unwrap();
    assert!(matches!(
        acquire_events_daemon_guard_outcome(&linked_socket),
        EventsDaemonGuardAcquisition::HardeningRefused(_)
    ));
    assert_eq!(std::fs::read(&victim).unwrap(), b"victim-sentinel");
    assert_eq!(
        std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
