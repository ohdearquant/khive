use std::ffi::OsString;

use super::*;
use crate::RuntimeError;
use khive_db::{SqliteError, WalCeilingSource};

#[test]
fn supervisor_argv_preserves_zero_and_nonzero_policy_sources() {
    let executable = Path::new("/fixture/kernel");
    let db = Path::new("/fixture/with spaces/events.db");
    let socket = Path::new("/fixture/with spaces/events.sock");
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
            ];
            // Every respawn rebuilds this command from the same resolved policy.
            for _ in 0..3 {
                let command = events_daemon_command(executable, db, socket, policy);
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
        let error =
            direct_backend_with_max_readers_and_wal_ceiling(&path, read_only, Some(2), policy)
                .err()
                .expect("overflow must refuse before opening an event backend");
        assert!(matches!(
            error,
            RuntimeError::Sqlite(SqliteError::WalCeilingOffsetOverflow { bytes: u64::MAX })
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
        Some(RuntimeError::Sqlite(
            SqliteError::WalCeilingOffsetOverflow { bytes: u64::MAX }
        ))
    ));
    assert!(!db.parent().unwrap().exists());
    assert!(!socket.parent().unwrap().exists());
}
