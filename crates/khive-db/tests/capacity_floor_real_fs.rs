//! Slow, opt-in acceptance test for SQLite's real free-space write reserve.
//! The database and filler live on a bounded mounted filesystem, never on the
//! developer's ordinary data volume.

#![cfg(target_os = "linux")]

use std::error::Error;
use std::fs::{self, File};
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
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
    _fixture: tempfile::TempDir,
}

fn verify_isolation(mount: &Path) -> Result<(), String> {
    let metadata = fs::metadata(mount).map_err(|error| error.to_string())?;
    if !metadata.is_dir() {
        return Err("constrained mount must be a directory".into());
    }
    let workspace = std::env::current_dir().map_err(|error| error.to_string())?;
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .ok_or("HOME is unavailable; cannot prove volume isolation")?;
    for protected in [Path::new("/"), workspace.as_path(), home.as_path()] {
        let protected_metadata = fs::metadata(protected).map_err(|error| error.to_string())?;
        if metadata.dev() == protected_metadata.dev() {
            return Err(format!(
                "constrained device matches protected filesystem {}",
                protected.display()
            ));
        }
    }
    let total = fs4::total_space(mount).map_err(|error| error.to_string())?;
    if !(FLOOR_BYTES + 32 * MIB..=MAX_CONSTRAINED_VOLUME_BYTES).contains(&total) {
        return Err(format!(
            "constrained device size must be 96..512 MiB, got {total}"
        ));
    }
    Ok(())
}

fn constrained_volume() -> Option<ConstrainedVolume> {
    let Some(mount) = std::env::var_os("KHIVE_TEST_CONSTRAINED_MOUNT") else {
        eprintln!(
            "SKIP ADR154_CAPACITY: run scripts/test-sqlite-capacity-linux.sh \
             with the compiled test binary"
        );
        return None;
    };
    let mount = PathBuf::from(mount);
    if let Err(reason) = verify_isolation(&mount) {
        eprintln!("SKIP ADR154_CAPACITY: {reason}");
        return None;
    }
    let fixture = tempfile::Builder::new()
        .prefix("khive-capacity-")
        .tempdir_in(&mount)
        .expect("private fixture on verified isolated device");
    Some(ConstrainedVolume {
        mount: fixture.path().to_path_buf(),
        _fixture: fixture,
    })
}

fn fill_to_headroom(mount: &Path, floor: u64) {
    verify_isolation(mount).expect("isolation must still hold immediately before filling");
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
    let mut written = 0_u64;
    loop {
        let available = fs4::available_space(mount).expect("available space after fill");
        if available <= target {
            assert!(
                available > floor + 8 * MIB,
                "filler crossed the floor without room for several SQLite writes"
            );
            break;
        }
        written += block.len() as u64;
        assert!(
            written <= total,
            "constrained filesystem did not account for bounded filler allocation"
        );
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

/// The runner mounts a bounded tmpfs in a private Linux mount namespace.
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
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", sink_dir.path());
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_HEARTBEAT_MS", "100");

    let db_path = volume.mount.join("capacity.db");
    let pool = Arc::new(
        ConnectionPool::new(PoolConfig {
            path: Some(db_path.clone()),
            write_queue_enabled: Some(false),
            disk_guard_config: Some(
                khive_db::DiskGuardEnvironment::default()
                    .resolve(Some(FLOOR_BYTES), Some(2_000))
                    .unwrap(),
            ),
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
            .execute_batch(include_str!("fixtures/capacity-floor.sql"))
            .expect("schema creation");
    }
    assert_eq!(
        pool.try_checkpoint_nowait()
            .unwrap()
            .truncate()
            .unwrap()
            .busy,
        0
    );

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
        .query_row("SELECT generation FROM payloads WHERE id = 1", [], |row| {
            row.get(0)
        })
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
                sql: "UPDATE payloads SET bytes = ?1, generation = ?2 WHERE id = 1".into(),
                params: vec![
                    SqlValue::Blob(vec![(writes % 255 + 1) as u8; MIB as usize]),
                    SqlValue::Integer(writes + 1),
                ],
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
        .query_row("SELECT generation FROM payloads WHERE id = 1", [], |row| {
            row.get(0)
        })
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
    let checkpoint = pool
        .try_checkpoint_nowait()
        .expect("floor-bypassed checkpoint capability")
        .truncate()
        .expect("checkpoint after reader release");
    assert_eq!(
        checkpoint.busy, 0,
        "checkpoint must complete after the reader leaves"
    );
    assert!(
        fs4::available_space(&volume.mount).unwrap() > FLOOR_BYTES,
        "truncation of repeated-update WAL must restore admission headroom"
    );
    let mut writer = bridge
        .writer()
        .await
        .expect("ordinary admission recovers after checkpoint");
    assert_eq!(
        writer
            .execute(SqlStatement {
                sql: "UPDATE payloads SET generation = generation + 1 WHERE id = 1".into(),
                params: vec![],
                label: Some("real_capacity_floor_recovery".into()),
            })
            .await
            .expect("ordinary write after recovery"),
        1
    );
    drop(writer);
    let reader = pool.reader().unwrap();
    let generation: i64 = reader
        .query_row("SELECT generation FROM payloads WHERE id = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(generation, writes + 1);
    // libtest prints the test name on the line this output starts, so begin
    // with a newline to keep the marker on a line of its own.
    println!("\nADR154_CAPACITY_PASS");
}

#[test]
fn constrained_fixture_rejects_the_workspace_volume_without_writing() {
    let workspace = std::env::current_dir().unwrap();
    assert!(verify_isolation(&workspace)
        .unwrap_err()
        .contains("protected filesystem"));
}
