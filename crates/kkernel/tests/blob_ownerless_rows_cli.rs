#![cfg(unix)]

use std::process::Command;

#[test]
fn current_quiescent_member_prints_a_complete_json_report() {
    let dir = tempfile::tempdir().expect("isolated operator fixture");
    let database = dir.path().join("main.db");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").expect("empty config");
    let backend =
        khive_db::StorageBackend::sqlite_for_test(&database).expect("create fixture database");
    backend
        .prepare_core_schema()
        .expect("migrate fixture database");
    drop(backend);

    let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args([
            "blob",
            "ownerless-rows",
            "--db",
            database.to_str().expect("utf8 database path"),
            "--config",
            config.to_str().expect("utf8 config path"),
        ])
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_CONFIG")
        .env("KHIVE_VOLUME_LOCK_DIR", dir.path().join("volume-locks"))
        .output()
        .expect("run ownerless report binary");
    assert!(
        output.status.success(),
        "quiescent member should report: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("one complete JSON report");
    assert_eq!(report["members"][0]["names"], serde_json::json!(["main"]));
    assert_eq!(
        report["members"][0]["canonical_path"],
        database.canonicalize().unwrap().to_str().unwrap()
    );
    assert_eq!(report["counters"]["scanned"], 0);
    assert_eq!(report["rows"], serde_json::json!([]));
}

#[test]
fn live_member_refuses_without_printing_a_partial_report() {
    let dir = tempfile::tempdir().expect("isolated operator fixture");
    let database = dir.path().join("main.db");
    let config = dir.path().join("config.toml");
    std::fs::write(&config, "").expect("empty config");

    let connection = rusqlite::Connection::open(&database).expect("open fixture database");
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0; \
             CREATE TABLE fixture (id INTEGER PRIMARY KEY); INSERT INTO fixture VALUES (1);",
        )
        .expect("leave committed WAL frames under a live connection");
    let sidecar = database.with_file_name("main.db-shm");
    assert!(
        sidecar.exists(),
        "live WAL must have its writable SHM sidecar"
    );

    let output = Command::new(env!("CARGO_BIN_EXE_kkernel"))
        .args([
            "blob",
            "ownerless-rows",
            "--db",
            database.to_str().expect("utf8 database path"),
            "--config",
            config.to_str().expect("utf8 config path"),
        ])
        .env_remove("KHIVE_DB")
        .env_remove("KHIVE_CONFIG")
        .env("KHIVE_VOLUME_LOCK_DIR", dir.path().join("volume-locks"))
        .output()
        .expect("run ownerless report binary");
    assert!(!output.status.success(), "live member must refuse");
    assert!(output.stdout.is_empty(), "refusal cannot print report rows");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for phrase in ["main.db", "close every live writer", "frozen snapshot"] {
        assert!(stderr.contains(phrase), "missing {phrase:?}: {stderr}");
    }
}
