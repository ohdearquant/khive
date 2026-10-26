#[cfg(unix)]
#[test]
fn untrusted_parent_directory_is_refused() {
    use std::os::unix::fs::PermissionsExt;
    // SQLite's open is path-based: the only sound defense against a
    // component swapped after validation is refusing directories other
    // local users can write. A group/other-writable parent is refused
    // before any open; an owner-only parent proceeds.
    let dir = tempfile::tempdir().unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let open_dir = dir.path().join("shared");
    std::fs::create_dir(&open_dir).unwrap();
    std::fs::set_permissions(&open_dir, std::fs::Permissions::from_mode(0o777)).unwrap();
    let refused = direct_backend_for(&open_dir.join("khive.db.events.db"));
    match refused {
        Ok(_) => panic!("a world-writable events directory must be refused"),
        Err(e) => assert!(
            e.to_string().contains("untrusted directory"),
            "refusal must name the directory trust rule: {e}"
        ),
    }
    // Control: an owner-only sibling directory passes the same gate.
    let safe_dir = dir.path().join("owned");
    std::fs::create_dir(&safe_dir).unwrap();
    std::fs::set_permissions(&safe_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    direct_backend_for(&safe_dir.join("khive.db.events.db"))
        .expect("an owner-only events directory must open");
}

#[cfg(unix)]
#[test]
fn harden_refuses_a_sidecar_symlink_at_use_time() {
    use std::os::unix::fs::PermissionsExt;
    // The hardening step must be coupled to its validation: it opens
    // each target with O_NOFOLLOW and chmods the returned handle, so a
    // symlink present AT USE TIME is refused by the open itself — no
    // lstat-then-chmod window — and the link's target keeps its mode.
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"v").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    let db = dir.path().join("db.events.db");
    std::fs::write(&db, b"").unwrap();
    let mut wal = db.as_os_str().to_os_string();
    wal.push("-wal");
    std::os::unix::fs::symlink(&victim, PathBuf::from(&wal)).unwrap();
    let result = harden_events_db_sidecars(&db);
    assert!(
        result.is_err(),
        "a symlinked -wal must be refused at hardening time"
    );
    let mode = victim.metadata().unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o644, "the link's target must keep its mode");
}

#[cfg(unix)]
#[test]
fn lock_symlink_is_refused_and_target_untouched() {
    use std::os::unix::fs::PermissionsExt;
    // The daemon lock open is pinned with O_NOFOLLOW and chmods its own
    // handle: a symlink planted at the lock name must refuse the guard,
    // and the link's target must keep its inode content and mode. A
    // plain lock in the same directory must still acquire — the control
    // that proves the refusal is the symlink, not a broken guard.
    let dir = tempfile::tempdir().unwrap();
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"v").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    let socket = dir.path().join("events.sock");
    std::os::unix::fs::symlink(&victim, socket.with_extension("lock")).unwrap();
    assert!(
        matches!(
            acquire_events_daemon_guard_outcome(&socket),
            EventsDaemonGuardAcquisition::HardeningRefused(_)
        ),
        "a symlinked lock entry must report a hardening refusal"
    );
    let mode = victim.metadata().unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o644, "the symlink's target must keep its mode");
    assert_eq!(std::fs::read(&victim).unwrap(), b"v");
    let clean = dir.path().join("clean.sock");
    assert!(
        matches!(
            acquire_events_daemon_guard_outcome(&clean),
            EventsDaemonGuardAcquisition::Held(_)
        ),
        "a plain lock path in the same directory must still acquire"
    );
}

#[cfg(unix)]
#[test]
fn planted_wal_symlink_is_refused_before_open() {
    // SQLite creates `-wal` without O_EXCL, so a planted `-wal` symlink
    // would redirect WAL writes; admission checks the sidecar suffixes
    // before the database is ever created or opened.
    let dir = tempfile::tempdir().unwrap();
    let _registry_guard = TestRegistryGuard::new(dir.path());
    let victim = dir.path().join("victim.txt");
    std::fs::write(&victim, b"w").unwrap();
    let sidecar = dir.path().join("db.events.db");
    let mut wal = sidecar.as_os_str().to_os_string();
    wal.push("-wal");
    std::os::unix::fs::symlink(&victim, PathBuf::from(&wal)).unwrap();
    assert!(direct_backend_for(&sidecar).is_err());
    assert!(
        !sidecar.exists(),
        "refusal must precede creation of the events database"
    );
}

#[cfg(unix)]
#[test]
fn hardening_refuses_a_directory_at_the_database_path_without_touching_it() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("events.db");
    std::fs::create_dir(&db).unwrap();
    std::fs::set_permissions(&db, std::fs::Permissions::from_mode(0o700)).unwrap();
    let err = harden_events_db_sidecars(&db).unwrap_err().to_string();
    assert!(err.contains("not a regular file"), "{err}");
    let mode = std::fs::metadata(&db).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o700, "the directory keeps its mode");
}

#[cfg(unix)]
#[test]
fn unopened_check_refuses_a_loosened_or_replaced_sidecar() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let db = dir.path().join("events.db");
    let wal = PathBuf::from(format!("{}-wal", db.display()));
    let shm = PathBuf::from(format!("{}-shm", db.display()));
    for path in [&db, &wal] {
        std::fs::write(path, b"").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let before_open = harden_events_db_sidecars(&db).unwrap();
    // Owner-only regular files, an absent `-shm`: nothing to refuse.
    verify_events_db_owner_only_unopened(&db, &before_open).unwrap();

    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o644)).unwrap();
    let err = verify_events_db_owner_only_unopened(&db, &before_open)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("events.db-wal") && err.contains("644"),
        "{err}"
    );
    std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();
    verify_events_db_owner_only_unopened(&db, &before_open).unwrap();

    // A link planted at the `-shm` name is refused as not a regular file,
    // and the check never followed it: the target keeps its mode.
    let victim = dir.path().join("victim");
    std::fs::write(&victim, b"").unwrap();
    std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o644)).unwrap();
    std::os::unix::fs::symlink(&victim, &shm).unwrap();
    let err = verify_events_db_owner_only_unopened(&db, &before_open)
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("events.db-shm") && err.contains("regular file"),
        "{err}"
    );
    assert_eq!(
        std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
        0o644
    );
}
