//! An in-memory pool must never initialize the writer-timeout sink —
//! there is no database file to name a `startup` row
//! after, and claiming the process-global slot first would permanently
//! starve out a later file-backed pool that could have supplied a real
//! identity. Isolated in its own integration-test binary (own process) so
//! the in-memory pool booting "first" is a guarantee, not a race against
//! whatever other test happens to run earliest in a shared process.

use std::sync::Arc;
use std::time::Duration;

use khive_db::{ConnectionPool, PoolConfig};

#[test]
fn in_memory_pool_first_then_file_backed_pool_second_carries_file_backed_identity() {
    let sink_dir = tempfile::tempdir().expect("tempdir");
    std::env::set_var("KHIVE_WRITER_TIMEOUT_SINK_DIR", sink_dir.path());

    // Boot an in-memory pool FIRST. This must be a complete no-op for the
    // sink: no directory resolution, no thread, no claimed slot.
    let memory_cfg = PoolConfig {
        path: None,
        ..PoolConfig::default()
    };
    let memory_pool =
        Arc::new(ConnectionPool::new(memory_cfg).expect("in-memory pool should open"));

    let ndjson_path = sink_dir.path().join("writer_timeouts.ndjson");
    assert!(
        !ndjson_path.exists(),
        "an in-memory pool must never create the sink's NDJSON file"
    );

    // Now boot a file-backed pool. This must be the one that actually wins
    // the process-global sink slot and names the startup row.
    let dir = tempfile::tempdir().unwrap();
    let db_path = dir.path().join("ordering_test.db");
    let file_cfg = PoolConfig {
        path: Some(db_path.clone()),
        ..PoolConfig::default()
    };
    let file_pool = Arc::new(ConnectionPool::new(file_cfg).expect("file-backed pool should open"));

    let canonical_db = db_path.canonicalize().unwrap_or(db_path);
    let db_marker = canonical_db.display().to_string();

    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(contents) = std::fs::read_to_string(&ndjson_path) {
            if contents.contains("\"kind\":\"startup\"") {
                assert!(
                    contents.contains(&db_marker),
                    "expected the startup row to carry the file-backed pool's own \
                     identity ({db_marker}), got: {contents}"
                );
                assert!(
                    !contents.contains("\"db\":\"memory\""),
                    "the in-memory pool must never have supplied the startup row's \
                     identity, got: {contents}"
                );
                break;
            }
        }
        if std::time::Instant::now() > deadline {
            panic!("no startup row appeared within 5s of the file-backed pool booting");
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Keep both pools alive until the assertions above have run so their
    // Drop impls can't be blamed for anything odd, though neither pool
    // actually owns the sink thread.
    drop(memory_pool);
    drop(file_pool);
}
