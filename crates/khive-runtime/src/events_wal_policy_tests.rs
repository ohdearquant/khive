use std::ffi::OsString;

use super::*;
use crate::RuntimeError;
use khive_db::{SqliteError, WalCeilingSource};

#[test]
fn supervisor_argv_preserves_zero_and_nonzero_policy_sources() {
    let executable = Path::new("/fixture/kernel");
    let db = Path::new("/fixture/with spaces/events.db");
    let socket = Path::new("/fixture/with spaces/events.sock");
    let locks = Path::new("/fixture/with spaces/volume locks");
    for (source, wire_source) in [
        (WalCeilingSource::BackendField, "backend_field"),
        (WalCeilingSource::Environment, "environment"),
        (WalCeilingSource::Default, "default"),
    ] {
        for bytes in [0, 64 * 1024 * 1024] {
            let policy = WalCeilingPolicy { bytes, source };
            let expected: Vec<OsString> = vec![
                "events-daemon".into(),
                "--db".into(),
                db.as_os_str().to_owned(),
                "--socket".into(),
                socket.as_os_str().to_owned(),
                "--wal-ceiling-bytes".into(),
                bytes.to_string().into(),
                "--wal-ceiling-source".into(),
                wire_source.into(),
                "--disk-reserve-bytes".into(),
                "1073741824".into(),
                "--disk-guard-deadline-ms".into(),
                "2000".into(),
                "--disk-reserve-source".into(),
                "default".into(),
                "--disk-deadline-source".into(),
                "default".into(),
                "--disk-legacy-environment-present".into(),
                "false".into(),
                "--volume-lock-dir".into(),
                locks.as_os_str().to_owned(),
            ];
            // Every respawn rebuilds this command from the same resolved policy.
            for _ in 0..3 {
                let command = events_daemon_command(
                    executable,
                    db,
                    socket,
                    policy,
                    khive_db::EffectiveDiskGuardConfig::default(),
                    locks,
                );
                assert_eq!(command.get_program(), executable.as_os_str());
                assert_eq!(
                    command.get_args().map(OsString::from).collect::<Vec<_>>(),
                    expected,
                    "supervisor argv must carry both policy fields without splitting paths"
                );
            }
        }
    }
}

#[test]
fn explicit_direct_policy_refuses_offset_overflow_before_filesystem_access() {
    let root = tempfile::tempdir().expect("private fixture root");
    let policy = WalCeilingPolicy {
        bytes: u64::MAX,
        source: WalCeilingSource::BackendField,
    };
    for read_only in [false, true] {
        let path = root.path().join("uncreated").join("events.db");
        let error = direct_backend_with_policies(
            &path,
            read_only,
            Some(2),
            policy,
            Some(khive_db::EffectiveDiskGuardConfig::default()),
            None,
        )
        .err()
        .expect("overflow must refuse before opening an event backend");
        assert!(matches!(
            error,
            RuntimeError::Storage(khive_storage::StorageError::Driver { source, .. })
                if matches!(source.downcast_ref(), Some(SqliteError::WalCeilingOffsetOverflow { bytes: u64::MAX }))
        ));
        assert!(!path.parent().unwrap().exists());
    }
}

#[tokio::test]
async fn explicit_daemon_policy_refuses_offset_overflow_before_filesystem_access() {
    let root = tempfile::tempdir().expect("private fixture root");
    let db = root.path().join("uncreated-db").join("events.db");
    let socket = root.path().join("uncreated-socket").join("events.sock");
    let error = run_events_daemon_with_wal_ceiling(
        &db,
        &socket,
        WalCeilingPolicy {
            bytes: u64::MAX,
            source: WalCeilingSource::BackendField,
        },
    )
    .await
    .expect_err("overflow must refuse before event daemon path preparation");
    assert!(matches!(
        error.downcast_ref::<RuntimeError>(),
        Some(RuntimeError::Storage(khive_storage::StorageError::Driver { source, .. }))
            if matches!(source.downcast_ref(), Some(SqliteError::WalCeilingOffsetOverflow { bytes: u64::MAX }))
    ));
    assert!(!db.parent().unwrap().exists());
    assert!(!socket.parent().unwrap().exists());
}

#[test]
fn direct_events_registry_requires_equal_effective_disk_policy() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("events.db");
    let locks = root.path().join("locks");
    let policy = khive_db::EffectiveDiskGuardConfig {
        reserve_bytes: 123,
        guard_deadline_ms: 250,
        ..khive_db::EffectiveDiskGuardConfig::default()
    };
    let open = |disk| {
        direct_backend_with_policies(
            &db,
            false,
            Some(2),
            WalCeilingPolicy::default(),
            Some(disk),
            Some(locks.clone()),
        )
    };
    let first = open(policy).unwrap();
    assert_eq!(first.pool().effective_disk_guard_config(), Some(policy));
    let mut alternate = policy;
    alternate.reserve_source = khive_db::DiskGuardConfigSource::Backend;
    assert!(Arc::ptr_eq(&first, &open(alternate).unwrap()));
    for changed in [
        khive_db::EffectiveDiskGuardConfig {
            reserve_bytes: 124,
            ..policy
        },
        khive_db::EffectiveDiskGuardConfig {
            guard_deadline_ms: 251,
            ..policy
        },
    ] {
        let error = open(changed)
            .err()
            .expect("conflicting cached events policy must fail");
        assert!(error
            .to_string()
            .contains("different disk reserve/deadline policy"));
    }
    let error = direct_backend_with_policies(
        &db,
        false,
        Some(2),
        WalCeilingPolicy::default(),
        Some(policy),
        Some(root.path().join("other-locks")),
    )
    .err()
    .expect("a cached events pool must not serve another lock directory");
    assert!(error
        .to_string()
        .contains("different volume-lock directory"));
    assert!(Arc::ptr_eq(&first, &open(policy).unwrap()));
}

#[test]
fn writable_direct_events_open_requires_a_volume_lock_directory() {
    let root = tempfile::tempdir().unwrap();
    let db = root.path().join("events.db");
    let writable = direct_backend_with_policies(
        &db,
        false,
        Some(2),
        WalCeilingPolicy::default(),
        Some(khive_db::EffectiveDiskGuardConfig::default()),
        None,
    )
    .err()
    .expect("a writable open without a lock directory must refuse");
    assert!(
        writable.to_string().contains("KHIVE_VOLUME_LOCK_DIR"),
        "{writable}"
    );
    assert!(!db.exists(), "the refusal must precede any file creation");
}

#[test]
fn supervisor_carries_captured_disk_policy_and_lock_path_verbatim() {
    let disk = khive_db::EffectiveDiskGuardConfig {
        reserve_bytes: 0,
        guard_deadline_ms: 321,
        reserve_source: khive_db::DiskGuardConfigSource::Backend,
        deadline_source: khive_db::DiskGuardConfigSource::LegacyEnvironment,
        legacy_environment_present: true,
    };
    let locks = Path::new("/fixture/with spaces/volume locks");
    let command = events_daemon_command(
        Path::new("/fixture/kernel"),
        Path::new("/fixture/events.db"),
        Path::new("/fixture/events.sock"),
        WalCeilingPolicy::default(),
        disk,
        locks,
    );
    let args: Vec<_> = command.get_args().collect();
    for (key, value) in [
        ("--disk-reserve-bytes", "0"),
        ("--disk-guard-deadline-ms", "321"),
        ("--disk-reserve-source", "backend"),
        ("--disk-deadline-source", "legacy_environment"),
        ("--disk-legacy-environment-present", "true"),
        ("--volume-lock-dir", "/fixture/with spaces/volume locks"),
    ] {
        let at = args.iter().position(|arg| *arg == key).unwrap();
        assert_eq!(args[at + 1], value);
    }
}

#[test]
fn secondary_runtime_events_inherit_actual_main_disk_policy_and_lock_directory() {
    let root = tempfile::tempdir().unwrap();
    let main_policy = khive_db::EffectiveDiskGuardConfig {
        reserve_bytes: 123,
        guard_deadline_ms: 250,
        ..khive_db::EffectiveDiskGuardConfig::default()
    };
    let secondary_policy = khive_db::EffectiveDiskGuardConfig {
        reserve_bytes: 456,
        guard_deadline_ms: 350,
        ..main_policy
    };
    let main = Arc::new(
        StorageBackend::sqlite_with_max_readers_and_policies(
            root.path().join("main.db"),
            Some(2),
            WalCeilingPolicy::default(),
            main_policy,
            root.path().join("main-locks"),
        )
        .unwrap(),
    );
    let secondary = Arc::new(
        StorageBackend::sqlite_with_max_readers_and_policies(
            root.path().join("secondary.db"),
            Some(2),
            WalCeilingPolicy::default(),
            secondary_policy,
            root.path().join("secondary-locks"),
        )
        .unwrap(),
    );
    main.prepare_core_schema().unwrap();
    secondary.prepare_core_schema().unwrap();
    let sidecar = root.path().join("events.db");
    let config = crate::RuntimeConfig {
        db_path: Some(root.path().join("secondary.db")),
        backend_id: crate::BackendId::parse("secondary").unwrap(),
        disk_guard_config: Some(secondary_policy),
        volume_lock_dir: Some(root.path().join("stale-config-locks")),
        events_split: Some(EventsSplitConfig {
            db_path: sidecar.clone(),
            socket_path: None,
        }),
        ..crate::RuntimeConfig::no_embeddings()
    };
    let runtime = crate::KhiveRuntime::from_backend(secondary, config).with_core_backend(main);
    assert_eq!(runtime.events_disk_guard_policy().unwrap(), main_policy);
    assert_eq!(
        runtime.events_volume_lock_dir(),
        Some(root.path().join("main-locks"))
    );
    runtime.raw_events_for_namespace("local").unwrap();
    let lane = direct_backend_with_policies(
        &sidecar,
        false,
        Some(2),
        WalCeilingPolicy::default(),
        Some(main_policy),
        runtime.events_volume_lock_dir(),
    )
    .unwrap();
    assert_eq!(lane.pool().effective_disk_guard_config(), Some(main_policy));
    assert_eq!(
        lane.pool().config().volume_lock_dir.as_deref(),
        Some(root.path().join("main-locks").as_path())
    );
}
