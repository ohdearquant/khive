use std::os::unix::fs::PermissionsExt;

fn directory_mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn assert_store_refusal(message: &str, db: &Path, component: &Path) {
    assert!(
        message.contains(&format!("events database {}", db.display())),
        "{message}"
    );
    assert!(
        message.contains(&component.display().to_string()),
        "{message}"
    );
    assert!(
        message.contains("remove group/other write access"),
        "{message}"
    );
    assert!(message.contains("move the store"), "{message}");
    assert!(
        message.contains("Only the first offending component"),
        "{message}"
    );
    assert!(
        message.contains("further untrusted ancestors or components may remain"),
        "{message}"
    );
    assert!(!message.contains("KHIVE_SOCKET"), "{message}");
    assert!(!message.contains("KHIVE_PID"), "{message}");
    assert!(!message.contains("socket directory"), "{message}");
    assert!(!message.contains("socket-path"), "{message}");
    for target in events_db_targets(db) {
        assert!(
            !target.exists(),
            "refusal must not create {}",
            target.display()
        );
    }
}

#[test]
fn direct_store_open_reports_database_parent_without_changing_permissions() {
    let root = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let _registry_guard = TestRegistryGuard::new(root.path());
    for mode in [0o775, 0o777, 0o1777] {
        let parent = root.path().join(format!("mode-{mode:o}"));
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
        let db = parent.join("events.db");
        for read_only in [false, true] {
            let result = if read_only {
                direct_backend_read_only_for(&db)
            } else {
                direct_backend_for(&db)
            };
            let error = result.err().expect("untrusted store must not open");
            let message = error.to_string();
            assert_store_refusal(&message, &db, &parent);
            assert!(
                message.contains(&format!(
                    "store directory {} is mode {mode:04o}",
                    parent.display()
                )),
                "{message}"
            );
            assert_eq!(directory_mode(&parent), mode);
        }
    }
}

#[test]
fn direct_store_open_reports_first_bad_ancestor_and_warns_more_may_follow() {
    let root = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let _registry_guard = TestRegistryGuard::new(root.path());
    let first = root.path().join("first");
    let second = first.join("second");
    let parent = second.join("private");
    std::fs::create_dir_all(&parent).unwrap();
    for (path, mode) in [(&first, 0o775), (&second, 0o777), (&parent, 0o700)] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }
    let db = parent.join("events.db");
    let error = direct_backend_for(&db)
        .err()
        .expect("first ancestor must refuse");
    let message = error.to_string();
    assert_store_refusal(&message, &db, &first);
    assert!(
        message.contains(&format!(
            "store-path ancestor {} is mode 0775",
            first.canonicalize().unwrap().display()
        )),
        "{message}"
    );
    assert_eq!(directory_mode(&first), 0o775);
    assert_eq!(directory_mode(&second), 0o777);
    assert_eq!(directory_mode(&parent), 0o700);

    // Only the fixture repairs the first failure; the next open must reveal the second.
    std::fs::set_permissions(&first, std::fs::Permissions::from_mode(0o755)).unwrap();
    let error = direct_backend_for(&db)
        .err()
        .expect("second ancestor must refuse");
    let message = error.to_string();
    assert_store_refusal(&message, &db, &second);
    assert!(
        message.contains(&format!(
            "store-path ancestor {} is mode 0777",
            second.canonicalize().unwrap().display()
        )),
        "{message}"
    );
    assert_eq!(directory_mode(&first), 0o755);
    assert_eq!(directory_mode(&second), 0o777);
    assert_eq!(directory_mode(&parent), 0o700);
}

#[test]
fn store_trust_keeps_private_parents_and_sticky_ancestors_acceptable() {
    let root = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let ancestor = root.path().join("sticky");
    std::fs::create_dir(&ancestor).unwrap();
    std::fs::set_permissions(&ancestor, std::fs::Permissions::from_mode(0o1777)).unwrap();
    for mode in [0o700, 0o755] {
        let parent = ancestor.join(format!("private-{mode:o}"));
        std::fs::create_dir(&parent).unwrap();
        std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(mode)).unwrap();
        let db = parent.join("events.db");
        ensure_events_db_parent_trusted(&db).expect("trusted parent below sticky ancestor");
        assert_eq!(directory_mode(&ancestor), 0o1777);
        assert_eq!(directory_mode(&parent), mode);
        assert!(
            !db.exists(),
            "the trust check itself does not create a database"
        );
    }
}

#[test]
fn missing_store_parent_reports_database_and_metadata_failure() {
    let root = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let parent = root.path().join("missing");
    let db = parent.join("events.db");
    let message = ensure_events_db_parent_trusted(&db)
        .unwrap_err()
        .to_string();
    assert_store_refusal(&message, &db, &parent);
    assert!(message.contains("cannot stat store directory"), "{message}");
    assert!(!parent.exists());
}

#[test]
fn socket_and_pid_refusals_keep_their_exact_parent_and_ancestor_diagnostics() {
    let root = crate::test_process::private_tempdir_builder()
        .tempdir()
        .unwrap();
    let parent = root.path().join("writable");
    let inner = parent.join("private");
    std::fs::create_dir_all(&inner).unwrap();
    std::fs::set_permissions(&parent, std::fs::Permissions::from_mode(0o775)).unwrap();
    std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o700)).unwrap();
    for (is_pid, directory_name, component_name, path_name, env_name) in [
        (
            false,
            "socket directory",
            "socket-path",
            "socket path",
            "KHIVE_SOCKET",
        ),
        (
            true,
            "PID-file directory",
            "PID-file-path",
            "PID-file path",
            "KHIVE_PID",
        ),
    ] {
        let check = |directory: &Path| {
            if is_pid {
                crate::daemon::ensure_pid_file_dir_is_trusted(&directory.join("daemon.pid"))
            } else {
                crate::daemon::ensure_socket_dir_is_trusted(directory)
            }
        };
        assert_eq!(
            check(&parent).unwrap_err().to_string(),
            format!(
                "refusing to start: {directory_name} {} for {env_name} is mode 0775 — writable \
             by group or other, so another local user could replace the rendezvous path. \
             Use a directory only you can write, or unset {env_name} for the default. \
             This daemon is not changing the permissions of a directory it does not own.",
                parent.display()
            )
        );
        assert_eq!(
            check(&inner).unwrap_err().to_string(),
            format!(
                "refusing to start: {component_name} ancestor {} for {env_name} is mode \
             0775 — writable by group or other without the sticky bit, so another \
             local user could rename the next path component and re-root the {path_name}. \
             Point {env_name} somewhere trusted end to end, or unset it for the default.",
                parent.canonicalize().unwrap().display()
            )
        );
        assert_eq!(directory_mode(&parent), 0o775);
        assert_eq!(directory_mode(&inner), 0o700);
    }
}
