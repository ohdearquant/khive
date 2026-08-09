//! Causal acceptance regression for #1844's database-volume reserve.
//!
//! The ordinary test suite cannot safely fill its host filesystem. The Linux
//! CI lane mounts a private 32 MiB tmpfs and passes its path through
//! `KHIVE_TEST_CONSTRAINED_FS_DIR`. Because this is an integration test,
//! `khive-db` is compiled as a normal dependency: the unit-test-only capacity
//! override is unavailable, so every refusal comes from the production
//! `fs4::available_space` sampler.

#[cfg(target_os = "linux")]
mod linux {
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use khive_db::{ConnectionPool, PoolConfig, SqliteError};
    use rusqlite::{Connection, OpenFlags};

    const MIB: u64 = 1024 * 1024;
    const MAX_TEST_VOLUME_BYTES: u64 = 64 * MIB;
    const RESERVE_BYTES: u64 = 4 * MIB;
    const PAYLOAD_BYTES: i64 = 256 * 1024;
    const MAX_INSERTS: i64 = 1024;

    struct CaseDir(PathBuf);

    impl Drop for CaseDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn constrained_root() -> Option<PathBuf> {
        std::env::var_os("KHIVE_TEST_CONSTRAINED_FS_DIR")
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    }

    fn is_sqlite_full(error: &rusqlite::Error) -> bool {
        matches!(
            error,
            rusqlite::Error::SqliteFailure(code, _)
                if code.code == rusqlite::ErrorCode::DiskFull
        )
    }

    #[test]
    fn real_volume_guard_refuses_before_filesystem_induced_sqlite_full_with_old_reader() {
        let Some(root) = constrained_root() else {
            eprintln!(
                "SKIP disk-reserve constrained-filesystem acceptance: \
                 KHIVE_TEST_CONSTRAINED_FS_DIR is unset (the Linux CI mount lane supplies it)"
            );
            return;
        };
        let total_bytes = fs4::total_space(&root).expect("measure constrained test filesystem");
        assert!(
            total_bytes <= MAX_TEST_VOLUME_BYTES,
            "refusing to run a disk-filling test on an unbounded volume: {} has {total_bytes} bytes",
            root.display()
        );
        assert!(
            total_bytes > RESERVE_BYTES * 2,
            "constrained volume must leave room for setup and a recovery reserve"
        );

        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after epoch")
            .as_nanos();
        let case_dir = root.join(format!(
            "khive-disk-reserve-{}-{unique}",
            std::process::id()
        ));
        std::fs::create_dir(&case_dir).expect("create isolated constrained-volume case");
        let _cleanup = CaseDir(case_dir.clone());
        let db_path = case_dir.join("guarded.db");

        let guarded = ConnectionPool::new(PoolConfig {
            path: Some(db_path.clone()),
            write_queue_enabled: Some(false),
            disk_reserve_bytes: RESERVE_BYTES,
            ..PoolConfig::default()
        })
        .expect("guarded pool opens while the bounded volume has headroom");
        guarded
            .writer()
            .expect("seed writer")
            .execute_batch(
                "CREATE TABLE payloads (
                     id INTEGER PRIMARY KEY AUTOINCREMENT,
                     payload BLOB NOT NULL
                 );
                 INSERT INTO payloads(payload) VALUES (zeroblob(16));",
            )
            .expect("seed WAL database");

        let old_reader = Connection::open_with_flags(
            &db_path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .expect("open old reader");
        old_reader
            .execute_batch("BEGIN DEFERRED")
            .expect("begin old snapshot");
        assert_eq!(
            old_reader
                .query_row("SELECT COUNT(*) FROM payloads", [], |row| row
                    .get::<_, i64>(0))
                .expect("pin reader snapshot"),
            1
        );

        // Open the reserve-disabled control while headroom is still ample. The
        // guarded loop below must be the only operation that drives this
        // bounded volume toward its reserve boundary.
        let control = ConnectionPool::new(PoolConfig {
            path: Some(db_path.clone()),
            write_queue_enabled: Some(false),
            disk_reserve_bytes: 0,
            ..PoolConfig::default()
        })
        .expect("reserve-disabled control opens before capacity pressure");

        let refusal = (0..MAX_INSERTS).find_map(|_| {
            let writer = match guarded.writer() {
                Ok(writer) => writer,
                Err(error @ SqliteError::DiskCapacityFloor { .. }) => return Some(error),
                Err(error) => panic!("unexpected guarded writer error: {error:?}"),
            };
            writer
                .execute(
                    "INSERT INTO payloads(payload) VALUES (zeroblob(?1))",
                    [PAYLOAD_BYTES],
                )
                .unwrap_or_else(|error| {
                    panic!("production guard must refuse before SQLite reaches FULL; got {error:?}")
                });
            None
        });
        let refusal = refusal.expect("bounded writes must cross the configured reserve");
        match refusal {
            SqliteError::DiskCapacityFloor {
                volume,
                available_bytes,
                reserve_bytes,
            } => {
                assert_eq!(PathBuf::from(volume), db_path.canonicalize().unwrap());
                assert_eq!(reserve_bytes, RESERVE_BYTES);
                assert!(available_bytes <= reserve_bytes);
                assert!(
                    available_bytes > 0,
                    "guard must preserve real filesystem headroom before SQLite FULL"
                );
            }
            other => panic!("expected capacity-floor refusal, got {other:?}"),
        }
        assert!(
            fs4::available_space(&case_dir).expect("measure preserved headroom") > 0,
            "capacity refusal must occur while the constrained filesystem still has space"
        );
        assert!(
            !old_reader.is_autocommit(),
            "the old reader must remain open while the guard refuses"
        );

        let control_writer = control.writer().expect("reserve-disabled writer");
        let sqlite_full = (0..MAX_INSERTS).find_map(|_| {
            match control_writer.execute(
                "INSERT INTO payloads(payload) VALUES (zeroblob(?1))",
                [PAYLOAD_BYTES],
            ) {
                Ok(_) => None,
                Err(error) if is_sqlite_full(&error) => Some(error),
                Err(error) => panic!("expected filesystem-induced SQLITE_FULL, got {error:?}"),
            }
        });
        assert!(
            sqlite_full.is_some(),
            "the same bounded filesystem must reach primary SQLITE_FULL when the guard is disabled"
        );
        assert!(
            !old_reader.is_autocommit(),
            "the old reader must remain pinned through the control's FULL boundary"
        );

        drop(control_writer);
        drop(control);
        old_reader
            .execute_batch("ROLLBACK")
            .expect("release old snapshot");
        drop(old_reader);
        drop(guarded);
    }
}

#[cfg(not(target_os = "linux"))]
#[test]
fn real_volume_guard_refuses_before_filesystem_induced_sqlite_full_with_old_reader() {
    eprintln!(
        "SKIP disk-reserve constrained-filesystem acceptance: bounded tmpfs lane is Linux-only"
    );
}
