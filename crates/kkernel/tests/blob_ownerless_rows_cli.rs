#![cfg(unix)]

use std::process::Command;

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
        .output()
        .expect("run ownerless report binary");
    assert!(!output.status.success(), "live member must refuse");
    assert!(output.stdout.is_empty(), "refusal cannot print report rows");
    let stderr = String::from_utf8_lossy(&output.stderr);
    for phrase in ["main.db", "close every live writer", "frozen snapshot"] {
        assert!(stderr.contains(phrase), "missing {phrase:?}: {stderr}");
    }
}
