//! Slow, opt-in acceptance test for SQLite's real free-space write reserve.
//! The database and filler live on a bounded mounted filesystem, never on the
//! developer's ordinary data volume.

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use khive_db::{ConnectionPool, PoolConfig, SqlBridge};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{SqlAccess, StorageError};
use rusqlite::{Connection, OpenFlags};

const MIB: u64 = 1024 * 1024;
const FLOOR_BYTES: u64 = 64 * MIB;
const STARTING_HEADROOM_BYTES: u64 = 16 * MIB;
const MAX_CONSTRAINED_VOLUME_BYTES: u64 = 512 * MIB;

struct ConstrainedVolume {
    mount: PathBuf,
    // The image is outside its mount. Keep it alive until after detach.
    _image_dir: Option<tempfile::TempDir>,
    detach: bool,
}

impl Drop for ConstrainedVolume {
    fn drop(&mut self) {
        if self.detach {
            let status = Command::new("/usr/bin/hdiutil")
                .arg("detach")
                .arg("-force")
                .arg(&self.mount)
                .status();
            if !matches!(&status, Ok(exit) if exit.success()) {
                if let Some(image_dir) = self._image_dir.take() {
                    let recovery_path = image_dir.path().to_path_buf();
                    std::mem::forget(image_dir);
                    eprintln!(
                        "WARNING: detach failed for {:?}: {status:?}; image preserved at {:?}",
                        self.mount, recovery_path
                    );
                }
            }
        }
    }
}

fn constrained_volume() -> Option<ConstrainedVolume> {
    if let Some(mount) = std::env::var_os("KHIVE_TEST_CONSTRAINED_MOUNT") {
        let mount = PathBuf::from(mount)
            .canonicalize()
            .expect("configured mount exists");
        assert!(mount.is_dir(), "configured mount must be a directory");
        return Some(ConstrainedVolume {
            mount,
            _image_dir: None,
            detach: false,
        });
    }

    #[cfg(target_os = "macos")]
    {
        let image_dir = tempfile::tempdir().expect("image host tempdir");
        let mount = image_dir.path().join("mount");
        fs::create_dir(&mount).expect("image mountpoint");
        let image = image_dir.path().join("capacity.sparseimage");
        let output = Command::new("/usr/bin/hdiutil")
            .args(["create", "-size", "256m", "-fs", "APFS", "-type", "SPARSE"])
            .arg(&image)
            .output()
            .expect("hdiutil create");
        assert!(
            output.status.success(),
            "hdiutil create failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        // Arm cleanup before attach: a failed command may still have mounted.
        let volume = ConstrainedVolume {
            mount,
            _image_dir: Some(image_dir),
            detach: true,
        };
        let output = Command::new("/usr/bin/hdiutil")
            .args(["attach", "-nobrowse", "-mountpoint"])
            .arg(&volume.mount)
            .arg(&image)
            .output()
            .expect("hdiutil attach");
        assert!(
            output.status.success(),
            "hdiutil attach failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        Some(volume)
    }

    #[cfg(not(target_os = "macos"))]
    {
        eprintln!(
            "SKIP: set KHIVE_TEST_CONSTRAINED_MOUNT to a pre-mounted <=512 MiB writable filesystem"
        );
        None
    }
}

fn fill_to_headroom(mount: &Path, floor: u64) {
    let total = fs4::total_space(mount).expect("constrained volume size");
    assert!(
        total <= MAX_CONSTRAINED_VOLUME_BYTES,
        "refusing to fill a volume larger than 512 MiB: {total} bytes"
    );
    let target = floor + STARTING_HEADROOM_BYTES;
    let initial = fs4::available_space(mount).expect("initial available space");
    assert!(
        initial > target + MIB,
        "constrained volume needs more than {target} free bytes, has {initial}"
    );

    let mut filler = File::create(mount.join("capacity-filler.bin")).expect("filler create");
    let block = vec![0xA5_u8; (4 * MIB) as usize];
    loop {
        let available = fs4::available_space(mount).expect("available space after fill");
        if available <= target {
            assert!(
                available > floor + 8 * MIB,
                "filler crossed the floor without room for several SQLite writes"
            );
            break;
        }
        filler.write_all(&block).expect("bounded filler write");
        filler.flush().expect("flush filler allocation");
        filler.sync_data().expect("reserve actual disk blocks");
    }
}

fn is_sqlite_full(error: &(dyn Error + 'static)) -> bool {
    let mut source = Some(error);
    while let Some(current) = source {
        if let Some(rusqlite::Error::SqliteFailure(code, _)) =
            current.downcast_ref::<rusqlite::Error>()
        {
            if code.code == rusqlite::ErrorCode::DiskFull {
                return true;
            }
        }
        source = current.source();
    }
    false
}

fn wait_for_sink(path: &Path, predicate: impl Fn(&str) -> bool) -> String {
    let deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let content = match fs::read_to_string(path) {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => panic!("cannot read sink {:?}: {error}", path),
        };
        if predicate(&content) {
            return content;
        }
        assert!(Instant::now() < deadline, "sink did not drain: {content}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn heartbeat_count(content: &str) -> usize {
    content
        .lines()
        .filter(|line| line.contains("\"kind\":\"heartbeat\""))
        .count()
}

/// Requires hdiutil on macOS, or KHIVE_TEST_CONSTRAINED_MOUNT on Linux.
/// Run: cargo test -p khive-db --test capacity_floor_real_fs -- --ignored
#[tokio::test]
#[ignore = "mounts and fills a real small filesystem; opt in on a disposable host"]
async fn refuses_before_sqlite_full_with_old_reader_and_recoverable_reserve() {
    let Some(volume) = constrained_volume() else {
        return;
    };
    let total = fs4::total_space(&volume.mount).expect("mounted filesystem size");
    assert!(
        total <= MAX_CONSTRAINED_VOLUME_BYTES,
        "refusing to use a volume larger than 512 MiB: {total} bytes"
    );
    let sink_dir = tempfile::tempdir().expect("sink host tempdir");
    std::env::set_var("KHIVE_DB_FREE_SPACE_FLOOR_BYTES", FLOOR_BYTES.to_string());
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", sink_dir.path());
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_HEARTBEAT_MS", "100");

    let db_path = volume.mount.join("capacity.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(db_path.clone()),
            write_queue_enabled: Some(false),
            ..PoolConfig::for_test()
        })
        .expect("pool opens before filling"),
    );
    pool.claim_checkpoint_ownership()
        .expect("disable implicit WAL checkpointing");
    {
        let writer = pool.writer().expect("schema writer");
        writer
            .conn()
            .execute_batch("CREATE TABLE payloads (id INTEGER PRIMARY KEY, bytes BLOB NOT NULL)")
            .expect("schema creation");
    }

    let sink_path = sink_dir
        .path()
        .join(format!("writer_timeouts.{}.ndjson", std::process::id()));
    wait_for_sink(&sink_path, |content| {
        content.contains("\"kind\":\"startup\"")
    });
    fill_to_headroom(&volume.mount, FLOOR_BYTES);

    let old_reader = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .expect("independent read-only connection");
    old_reader
        .execute_batch("BEGIN")
        .expect("old reader begins");
    let initial: i64 = old_reader
        .query_row("SELECT COUNT(*) FROM payloads", [], |row| row.get(0))
        .expect("establish old read snapshot");
    assert_eq!(initial, 0);

    let bridge = SqlBridge::new(Arc::clone(&pool), true);
    let mut writes = 0;
    let refusal = loop {
        assert!(
            writes < 128,
            "reserve did not refuse within 128 MiB of writes"
        );
        let mut writer = match bridge.writer().await {
            Ok(writer) => writer,
            Err(error @ StorageError::CapacityFloor { .. }) => break error,
            Err(error) if is_sqlite_full(&error) => {
                panic!("SQLITE_FULL before capacity-floor refusal: {error:?}")
            }
            Err(error) => panic!("writer admission failed unexpectedly: {error:?}"),
        };
        let result = writer
            .execute(SqlStatement {
                sql: "INSERT INTO payloads (bytes) VALUES (?1)".into(),
                params: vec![SqlValue::Blob(vec![0x5A; MIB as usize])],
                label: Some("real_capacity_floor_acceptance".into()),
            })
            .await;
        match result {
            Ok(1) => writes += 1,
            Ok(other) => panic!("one row expected per write, got {other}"),
            Err(error @ StorageError::CapacityFloor { .. }) => break error,
            Err(error) if is_sqlite_full(&error) => {
                panic!("SQLITE_FULL before capacity-floor refusal: {error:?}")
            }
            Err(error) => panic!("SQLite write failed unexpectedly: {error:?}"),
        }
    };
    assert!(writes > 0, "the old reader must overlap real WAL writes");
    let StorageError::CapacityFloor {
        available_bytes,
        floor_bytes,
        ..
    } = refusal
    else {
        unreachable!("loop only breaks on typed capacity refusal")
    };
    assert_eq!(floor_bytes, FLOOR_BYTES);
    assert!(available_bytes <= floor_bytes);

    let still_old: i64 = old_reader
        .query_row("SELECT COUNT(*) FROM payloads", [], |row| row.get(0))
        .expect("old snapshot remains readable");
    assert_eq!(still_old, 0, "old reader must pin its pre-write WAL view");

    // A subsequent heartbeat is a sink drain barrier: a sqlite_full event
    // queued before the refusal would have been written before that row.
    let before = fs::read_to_string(&sink_path).expect("read sink before drain barrier");
    let after = wait_for_sink(&sink_path, |content| {
        heartbeat_count(content) > heartbeat_count(&before)
    });
    assert!(
        !after.contains("\"kind\":\"sqlite_full\""),
        "capacity guard must refuse before SQLite exhausts the volume: {after}"
    );

    old_reader
        .execute_batch("ROLLBACK")
        .expect("release old reader");
    drop(old_reader);
    let checkpoint = Connection::open(&db_path).expect("checkpoint connection");
    let (busy, _log, _checkpointed): (i64, i64, i64) = checkpoint
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .expect("reserved space permits checkpoint after reader release");
    assert_eq!(busy, 0, "checkpoint must complete after the reader leaves");
}
