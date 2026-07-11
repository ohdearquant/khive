//! Black-box CLI tests for `kkernel code-ingest` (khive#848 F7).
//!
//! Drives the actual compiled `kkernel` binary (`CARGO_BIN_EXE_kkernel`)
//! rather than calling `code_ingest_batch` in-tree: the helper-level unit
//! tests in `crates/kkernel/src/code_ingest.rs` cover the mapping logic, but
//! only a real subprocess exercises clap argument parsing, `main`'s
//! subcommand dispatch, the process exit code, and — most importantly — the
//! filesystem side effects (or lack thereof) of `--dry-run` and a rejected
//! document, which is exactly what round 1 of the PR #848 review found
//! missing.

use std::path::Path;
use std::process::Command;

use khive_db::StorageBackend;
use khive_storage::{SqlStatement, SqlValue};

fn kkernel_bin() -> &'static str {
    env!("CARGO_BIN_EXE_kkernel")
}

/// Query substrate row counts directly over the sqlite file rather than
/// through `kkernel exec`: `exec`'s config resolution rejects an explicit
/// `--db` when the ambient `$HOME/.khive/config.toml` declares `[[backends]]`
/// (a real constraint on developer machines with a multi-backend khive
/// setup), which is orthogonal to what this test verifies. Reading the file
/// straight is also a more direct proof of "reached storage" than routing
/// through another CLI surface's own config layer.
async fn substrate_counts(db: &Path) -> (u64, u64, u64) {
    let backend = StorageBackend::sqlite_read_only(db).expect("open scratch db read-only");
    let sql = backend.sql();
    let mut reader = sql.reader().await.expect("reader");

    async fn count(reader: &mut dyn khive_storage::SqlReader, table: &str) -> u64 {
        match reader
            .query_scalar(SqlStatement {
                sql: format!("SELECT COUNT(*) FROM {table} WHERE deleted_at IS NULL"),
                params: vec![],
                label: None,
            })
            .await
            .expect("count query")
        {
            Some(SqlValue::Integer(n)) => n as u64,
            other => panic!("unexpected count() result: {other:?}"),
        }
    }

    let entities = count(reader.as_mut(), "entities").await;
    let notes = count(reader.as_mut(), "notes").await;
    let edges = count(reader.as_mut(), "graph_edges").await;
    (entities, notes, edges)
}

/// Fetch the single `finding` note's `properties.source_run` value from the
/// given namespace, directly over sqlite (see `substrate_counts` for why).
async fn finding_source_run(db: &Path, namespace: &str) -> String {
    let backend = StorageBackend::sqlite_read_only(db).expect("open scratch db read-only");
    let sql = backend.sql();
    let mut reader = sql.reader().await.expect("reader");
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT properties FROM notes WHERE namespace = ?1 AND kind = 'finding' \
                  AND deleted_at IS NULL"
                .to_string(),
            params: vec![SqlValue::Text(namespace.to_string())],
            label: None,
        })
        .await
        .expect("query finding note")
        .expect("expected exactly one finding note in the given namespace");
    let properties = match row.get("properties") {
        Some(SqlValue::Text(s)) => s.clone(),
        other => panic!("unexpected properties column: {other:?}"),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&properties).expect("properties must be valid JSON");
    parsed["source_run"]
        .as_str()
        .expect("finding note must carry a source_run property")
        .to_string()
}

/// Fetch every persisted `finding` note's `(finding_id, audit_status,
/// kind_status)` triple in the given namespace, directly over sqlite (see
/// `substrate_counts` for why): proves the mapper's output actually reached
/// storage through the real CLI dispatch path, not just the in-tree mapper
/// unit tests.
async fn finding_status_pairs(db: &Path, namespace: &str) -> Vec<(String, String, String)> {
    let backend = StorageBackend::sqlite_read_only(db).expect("open scratch db read-only");
    let sql = backend.sql();
    let mut reader = sql.reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT properties FROM notes WHERE namespace = ?1 AND kind = 'finding' \
                  AND deleted_at IS NULL"
                .to_string(),
            params: vec![SqlValue::Text(namespace.to_string())],
            label: None,
        })
        .await
        .expect("query finding notes");
    rows.into_iter()
        .map(|row| {
            let properties = match row.get("properties") {
                Some(SqlValue::Text(s)) => s.clone(),
                other => panic!("unexpected properties column: {other:?}"),
            };
            let parsed: serde_json::Value =
                serde_json::from_str(&properties).expect("properties must be valid JSON");
            let finding_id = parsed["finding_id"]
                .as_str()
                .expect("finding note must carry finding_id")
                .to_string();
            let audit_status = parsed["audit_status"]
                .as_str()
                .expect("finding note must carry audit_status")
                .to_string();
            let kind_status = parsed["kind_status"]
                .as_str()
                .expect("finding note must carry kind_status")
                .to_string();
            (finding_id, audit_status, kind_status)
        })
        .collect()
}

fn write_three_status_findings(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("status_findings.json");
    std::fs::write(
        &path,
        r#"{
            "audit": {
                "date": "2026-07-11",
                "scope": "khive-pack-code",
                "repo": "ohdearquant/khive",
                "branch": "feat/adr085-code-ingest-admin",
                "commit": "abc1234",
                "standards_file": "docs/standards.md"
            },
            "findings": [
                {
                    "id": "F-STATUS-FIXED",
                    "title": "A finding the producer marked fixed",
                    "severity": "medium",
                    "confidence": "high",
                    "failure_scenario": "Reproduced by running kkernel code-ingest twice.",
                    "evidence": "code_ingest_cli.rs status test, fixed case",
                    "impact": "none, this is a test fixture",
                    "status": "fixed"
                },
                {
                    "id": "F-STATUS-FALSE-POSITIVE",
                    "title": "A finding the producer marked false_positive",
                    "severity": "low",
                    "confidence": "medium",
                    "failure_scenario": "Reproduced by running kkernel code-ingest twice.",
                    "evidence": "code_ingest_cli.rs status test, false_positive case",
                    "impact": "none, this is a test fixture",
                    "status": "false_positive"
                },
                {
                    "id": "F-STATUS-OPEN",
                    "title": "A finding the producer left open",
                    "severity": "high",
                    "confidence": "high",
                    "failure_scenario": "Reproduced by running kkernel code-ingest twice.",
                    "evidence": "code_ingest_cli.rs status test, open case",
                    "impact": "none, this is a test fixture",
                    "status": "open"
                }
            ]
        }"#,
    )
    .expect("write status findings.json fixture");
    path
}

fn write_valid_findings(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("findings.json");
    std::fs::write(
        &path,
        r#"{
            "audit": {
                "date": "2026-07-11",
                "scope": "khive-pack-code",
                "repo": "ohdearquant/khive",
                "branch": "feat/adr085-code-ingest-admin",
                "commit": "abc1234",
                "standards_file": "docs/standards.md"
            },
            "findings": [
                {
                    "id": "F-CLI-001",
                    "title": "Example finding for a black-box CLI test",
                    "severity": "medium",
                    "confidence": "high",
                    "failure_scenario": "Reproduced by running kkernel code-ingest twice.",
                    "evidence": "code_ingest_cli.rs test",
                    "impact": "none, this is a test fixture"
                }
            ]
        }"#,
    )
    .expect("write findings.json fixture");
    path
}

fn write_invalid_findings(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("bad.json");
    std::fs::write(
        &path,
        r#"{
            "audit": {
                "date": "2026-07-11",
                "scope": "x",
                "repo": "r",
                "branch": "b",
                "commit": "c",
                "standards_file": "s"
            },
            "findings": [
                {"id": "F-CLI-002", "title": "bad", "severity": "high", "confidence": "low"}
            ]
        }"#,
    )
    .expect("write invalid findings.json fixture");
    path
}

fn code_ingest(args: &[&str]) -> std::process::Output {
    Command::new(kkernel_bin())
        .arg("code-ingest")
        .args(args)
        .env("KHIVE_NO_DAEMON", "1")
        .output()
        .expect("run kkernel code-ingest")
}

/// The `-wal` sidecar path SQLite uses alongside a WAL-mode database file.
fn wal_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-wal");
    std::path::PathBuf::from(name)
}

/// The `-shm` sidecar path SQLite uses alongside a WAL-mode database file.
fn shm_sidecar_path(db_path: &Path) -> std::path::PathBuf {
    let mut name = db_path.as_os_str().to_owned();
    name.push("-shm");
    std::path::PathBuf::from(name)
}

/// File names present directly under `dir`, used to prove a dry run creates
/// no new files anywhere in the target directory.
fn dir_entry_names(dir: &Path) -> std::collections::BTreeSet<String> {
    std::fs::read_dir(dir)
        .expect("read target dir")
        .map(|entry| {
            entry
                .expect("dir entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect()
}

/// (a) A fresh `--dry-run` against a nonexistent `--db` path must exit 0 and
/// leave that path nonexistent — no file, no directory, nothing.
#[test]
fn dry_run_against_nonexistent_db_leaves_it_nonexistent_and_exits_zero() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let findings = write_valid_findings(tmp.path());
    let db = tmp.path().join("does-not-exist").join("scratch.db");
    assert!(!db.exists());

    let output = code_ingest(&[
        findings.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--dry-run",
    ]);

    assert!(
        output.status.success(),
        "a valid document under --dry-run must exit 0; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !db.exists(),
        "--dry-run against a nonexistent db path must not create it or its parent"
    );

    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(report["dry_run"], serde_json::json!(true));
    assert_eq!(report["entities_created"], serde_json::json!(1));
    assert_eq!(report["notes_created"], serde_json::json!(1));
    assert_eq!(report["edges_created"], serde_json::json!(1));
}

/// (b) An invalid document against a nonexistent `--db` path must exit 1 and
/// leave that path nonexistent.
#[test]
fn invalid_document_against_nonexistent_db_leaves_it_nonexistent_and_exits_nonzero() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let findings = write_invalid_findings(tmp.path());
    let db = tmp.path().join("scratch.db");
    assert!(!db.exists());

    let output = code_ingest(&[findings.to_str().unwrap(), "--db", db.to_str().unwrap()]);

    assert!(
        !output.status.success(),
        "an invalid document must exit nonzero; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        !db.exists(),
        "rejecting an invalid document must leave the db path untouched"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("failed validation") || stderr.contains("failure_scenario"),
        "stderr must explain the validation failure: {stderr}"
    );
}

/// (c) An invalid document against an EXISTING db must exit 1 and leave the
/// db byte-identical — no migrations, no partial writes, nothing.
#[test]
fn invalid_document_against_existing_db_leaves_it_byte_identical() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let valid = write_valid_findings(tmp.path());
    let db = tmp.path().join("scratch.db");

    let seed = code_ingest(&[valid.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        seed.status.success(),
        "seeding the db with a valid ingest must succeed; stderr={}",
        String::from_utf8_lossy(&seed.stderr)
    );
    assert!(db.exists(), "seed ingest must create the db file");
    let bytes_before = std::fs::read(&db).expect("read db bytes before invalid ingest");

    let invalid = write_invalid_findings(tmp.path());
    let output = code_ingest(&[invalid.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        !output.status.success(),
        "an invalid document against an existing db must exit nonzero"
    );

    let bytes_after = std::fs::read(&db).expect("read db bytes after invalid ingest");
    assert_eq!(
        bytes_before, bytes_after,
        "rejecting an invalid document must not change a single byte of an existing db"
    );
}

/// (d) A normal (non-dry-run) ingest creates the expected entity/note/edge
/// rows, and re-running the same sweep is a content-derived-id no-op.
#[tokio::test]
async fn normal_ingest_creates_expected_entity_note_edge_rows() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let findings = write_valid_findings(tmp.path());
    let db = tmp.path().join("scratch.db");

    let output = code_ingest(&[
        findings.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--source-run",
        "cli-test-run",
    ]);
    assert!(
        output.status.success(),
        "normal ingest must succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(report["dry_run"], serde_json::json!(false));
    assert_eq!(report["entities_created"], serde_json::json!(1));
    assert_eq!(report["notes_created"], serde_json::json!(1));
    assert_eq!(report["edges_created"], serde_json::json!(1));

    let stats = substrate_counts(&db).await;
    assert_eq!(
        stats,
        (1, 1, 1),
        "expected (entities, notes, edges) = (1, 1, 1): {stats:?}"
    );

    // Re-ingesting the identical sweep must be a no-op (content-derived
    // UUIDv5 identity), never a duplicate write.
    let rerun = code_ingest(&[
        findings.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--source-run",
        "cli-test-run",
    ]);
    assert!(rerun.status.success());
    let rerun_report: serde_json::Value =
        serde_json::from_slice(&rerun.stdout).expect("stdout must be valid JSON");
    assert_eq!(rerun_report["entities_created"], serde_json::json!(0));
    assert_eq!(rerun_report["notes_created"], serde_json::json!(0));
    assert_eq!(rerun_report["edges_created"], serde_json::json!(0));
    assert_eq!(
        rerun_report["entities_skipped_existing"],
        serde_json::json!(1)
    );
    assert_eq!(rerun_report["notes_skipped_existing"], serde_json::json!(1));
    assert_eq!(rerun_report["edges_skipped_existing"], serde_json::json!(1));

    let stats_after_rerun = substrate_counts(&db).await;
    assert_eq!(
        stats_after_rerun, stats,
        "a re-ingest no-op must not change substrate counts"
    );
}

/// (e) `--db`, `--namespace`, and `--source-run` all reach storage: the
/// database is created at the given path, records land in the given
/// namespace, and the note's `source_run` property carries the given value.
#[tokio::test]
async fn db_namespace_and_source_run_reach_storage() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let findings = write_valid_findings(tmp.path());
    let db = tmp.path().join("nested").join("scratch.db");

    let output = code_ingest(&[
        findings.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--namespace",
        "cli-test-ns",
        "--source-run",
        "explicit-source-run-marker",
    ]);
    assert!(
        output.status.success(),
        "ingest must succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        db.exists(),
        "--db must be honored, including parent dir creation"
    );

    let (entities, notes, edges) = substrate_counts(&db).await;
    assert_eq!(
        (entities, notes, edges),
        (1, 1, 1),
        "the record must be reachable in the database --db pointed at"
    );

    let source_run = finding_source_run(&db, "cli-test-ns").await;
    assert_eq!(
        source_run, "explicit-source-run-marker",
        "--source-run must reach the persisted note's properties.source_run, under the \
         explicit --namespace"
    );
}

/// (f) A three-finding sweep carrying producer `status` values `fixed`,
/// `false_positive`, and `open` must land through the real CLI dispatch
/// path with the governed `(audit_status, kind_status)` pairs persisted:
/// `(fixed, resolved)`, `(false_positive, invalid)`, `(open, open)`. The
/// mapper-level unit tests in `khive-pack-code/tests/integration.rs` cover
/// the mapping itself; this covers the plumbing from that mapper's output
/// through to storage over the actual compiled binary (round 2 Finding 2).
#[tokio::test]
async fn status_mapping_reaches_storage_through_cli_dispatch() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let findings = write_three_status_findings(tmp.path());
    let db = tmp.path().join("scratch.db");

    let output = code_ingest(&[
        findings.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--source-run",
        "status-mapping-cli-test",
    ]);
    assert!(
        output.status.success(),
        "ingest must succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let report: serde_json::Value =
        serde_json::from_slice(&output.stdout).expect("stdout must be valid JSON");
    assert_eq!(report["notes_created"], serde_json::json!(3));

    let mut pairs = finding_status_pairs(&db, "local").await;
    pairs.sort();

    let mut expected = vec![
        (
            "F-STATUS-FIXED".to_string(),
            "fixed".to_string(),
            "resolved".to_string(),
        ),
        (
            "F-STATUS-FALSE-POSITIVE".to_string(),
            "false_positive".to_string(),
            "invalid".to_string(),
        ),
        (
            "F-STATUS-OPEN".to_string(),
            "open".to_string(),
            "open".to_string(),
        ),
    ];
    expected.sort();

    assert_eq!(
        pairs, expected,
        "persisted (audit_status, kind_status) pairs must match the governed mapping for \
         every producer status in the sweep"
    );
}

/// (g) The binary-boundary variant of the helper-level
/// `code_ingest_dry_run_against_existing_wal_db_leaves_sidecars_untouched`
/// test in `code_ingest.rs`: a `--dry-run` against a db held open by a live
/// writer connection, invoked through the actual compiled `kkernel` binary
/// rather than `code_ingest_batch` in-tree. The WAL mutation class is caused
/// by opening the db, so the invariant must be pinned at the binary boundary, not only at the helper function.
#[tokio::test]
async fn dry_run_against_existing_wal_db_held_open_by_a_writer_leaves_sidecars_untouched() {
    let tmp = tempfile::TempDir::new().expect("temp dir");
    let target_dir = tmp.path();
    let findings = write_valid_findings(target_dir);
    let db = target_dir.join("wal_scratch.db");

    let seed = code_ingest(&[findings.to_str().unwrap(), "--db", db.to_str().unwrap()]);
    assert!(
        seed.status.success(),
        "seeding the db with a valid ingest must succeed; stdout={} stderr={}",
        String::from_utf8_lossy(&seed.stdout),
        String::from_utf8_lossy(&seed.stderr)
    );

    let pin = StorageBackend::sqlite(&db).expect("open pin backend");
    {
        let sql = pin.sql();
        let mut writer = sql.writer().await.expect("pin writer");
        writer
            .execute_script(
                "CREATE TABLE IF NOT EXISTS wal_pin_probe(x INTEGER); \
                 INSERT INTO wal_pin_probe VALUES (1);"
                    .to_string(),
            )
            .await
            .expect("pin write to keep the wal open");
    }

    let wal_path = wal_sidecar_path(&db);
    let shm_path = shm_sidecar_path(&db);
    assert!(
        wal_path.exists(),
        "expected a live -wal sidecar before dry-run"
    );
    assert!(
        shm_path.exists(),
        "expected a live -shm sidecar before dry-run"
    );

    let db_before = std::fs::read(&db).expect("read db before dry run");
    let wal_before = std::fs::read(&wal_path).expect("read -wal before dry run");
    let shm_before = std::fs::read(&shm_path).expect("read -shm before dry run");
    let entries_before = dir_entry_names(target_dir);

    let output = code_ingest(&[
        findings.to_str().unwrap(),
        "--db",
        db.to_str().unwrap(),
        "--dry-run",
    ]);
    assert!(
        output.status.success(),
        "dry-run against an existing WAL db must exit 0; stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert!(
        wal_path.exists(),
        "the existing -wal sidecar must not disappear"
    );
    assert!(
        shm_path.exists(),
        "the existing -shm sidecar must not disappear"
    );

    let entries_after = dir_entry_names(target_dir);
    assert_eq!(
        entries_before, entries_after,
        "dry-run must not create any new file in the target dir"
    );

    let db_after = std::fs::read(&db).expect("read db after dry run");
    let wal_after = std::fs::read(&wal_path).expect("read -wal after dry run");
    let shm_after = std::fs::read(&shm_path).expect("read -shm after dry run");

    assert_eq!(
        db_before, db_after,
        "dry-run must not touch the main db file"
    );
    assert_eq!(
        wal_before, wal_after,
        "dry-run must not touch the existing -wal sidecar"
    );
    assert_eq!(
        shm_before, shm_after,
        "dry-run must not touch the existing -shm sidecar (Finding 1 regression)"
    );

    drop(pin);
}
