use std::process::Command;

use rusqlite::Connection;
use tempfile::TempDir;

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

#[test]
fn legacy_ledger_check_recommends_recreation() {
    let tmp = TempDir::new().expect("temp dir");
    let path = tmp.path().join("legacy.db");
    let conn = Connection::open(&path).expect("open legacy db");
    conn.execute_batch(
        "CREATE TABLE _schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        );
        INSERT INTO _schema_migrations (version, name, applied_at) VALUES
            (1, 'initial_schema', 0),
            (2, 'add_name_to_notes', 0),
            (22, 'knowledge_lifecycle_status', 0);",
    )
    .expect("create legacy migration ledger");
    drop(conn);

    let output = Command::new(kkernel_bin())
        .args(["db", "check", "--db"])
        .arg(&path)
        .arg("--human")
        .output()
        .expect("run db check");

    assert!(
        output.status.success(),
        "db check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 stdout");
    assert!(
        stdout.contains("predates the consolidated baseline"),
        "{stdout}"
    );
    assert!(stdout.contains("recreate"), "{stdout}");
    assert!(!stdout.contains("upgrade the binary"), "{stdout}");

    let output = Command::new(kkernel_bin())
        .args(["db", "check", "--db"])
        .arg(&path)
        .output()
        .expect("run JSON db check");
    assert!(
        output.status.success(),
        "JSON db check failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("utf8 JSON stdout");
    let report: serde_json::Value = serde_json::from_str(&stdout).expect("db check JSON");
    assert_eq!(report["current_version"], 22);
    assert_eq!(report["ahead"], false);
    assert_eq!(report["recreation_required"], true);
    assert!(
        report["guidance"]
            .as_str()
            .is_some_and(|guidance| guidance.contains("recreate")),
        "{report}"
    );
}
