use super::{dry_run_report, CodeIngestReport};
use khive_db::StorageBackend;
use khive_pack_code::{ingest_findings_json, CodeIngestBatch, CodeIngestOptions};
use khive_storage::{SqlStatement, SqlValue};
use std::path::Path;

#[derive(Clone, Copy)]
enum Collision {
    Deleted,
    OtherNamespace,
}

fn mapped_fixture() -> CodeIngestBatch {
    let document = serde_json::to_vec(&serde_json::json!({
        "audit": {
            "date": "2026-07-11",
            "scope": "khive-pack-code",
            "repo": "ohdearquant/khive",
            "branch": "typed-dry-run-fixture",
            "commit": "abc1234",
            "standards_file": "docs/standards.md"
        },
        "findings": [{
            "id": "F-001",
            "title": "A populated collision fixture for the dry run report",
            "severity": "medium",
            "confidence": "high",
            "failure_scenario": "Re-ingesting the same mapped identifiers must skip their existing rows.",
            "evidence": "Private scratch-store fixture",
            "impact": "No production data is involved."
        }]
    }))
    .expect("serialize findings fixture");
    let batch = ingest_findings_json(
        &document,
        CodeIngestOptions {
            namespace: "local",
            observed_at: chrono::Utc::now(),
            source_run: Some("typed-dry-run-fixture"),
        },
    )
    .expect("map a real valid findings document");
    assert_eq!(batch.entities.len(), 1);
    assert_eq!(batch.notes.len(), 1);
    assert_eq!(batch.edges.len(), 1);
    batch
}

fn private_backend(db: &Path) -> StorageBackend {
    let backend = StorageBackend::sqlite_for_test(db).expect("open private test database");
    assert!(
        !backend.pool_arc().write_queue_active(),
        "the exact-child fixture disables writer tasks before opening its private backend"
    );
    backend
        .prepare_core_schema()
        .expect("prepare private core schema");
    backend.entities().expect("prepare entity store");
    backend.notes().expect("prepare note store");
    backend.graph().expect("prepare graph store");
    backend
}

async fn seed_collision(backend: &StorageBackend, batch: &CodeIngestBatch, collision: Collision) {
    let entities = backend.entities().expect("entity store");
    let notes = backend.notes().expect("note store");
    let graph = backend.graph().expect("graph store");
    entities
        .upsert_entity(batch.entities[0].clone())
        .await
        .expect("seed actual entity");
    notes
        .upsert_note(batch.notes[0].clone())
        .await
        .expect("seed actual note");
    graph
        .upsert_edge(batch.edges[0].clone())
        .await
        .expect("seed actual edge");
    let access = backend.sql();
    let mut writer = access.writer().await.expect("fixture writer");
    for (table, id) in [
        ("entities", batch.entities[0].id),
        ("notes", batch.notes[0].id),
        ("graph_edges", uuid::Uuid::from(batch.edges[0].id)),
    ] {
        let (assignment, value) = match collision {
            Collision::Deleted => ("deleted_at", SqlValue::Integer(1_700_000_000_000_000)),
            Collision::OtherNamespace => ("namespace", SqlValue::Text("another-namespace".into())),
        };
        let version = if table == "entities" {
            ", version = version + 1"
        } else {
            ""
        };
        let changed = writer
            .execute(SqlStatement {
                sql: format!("UPDATE {table} SET {assignment} = ?1{version} WHERE id = ?2"),
                params: vec![value, SqlValue::Uuid(id)],
                label: Some("typed dry-run collision fixture".into()),
            })
            .await
            .expect("prepare exactly one existing-row collision");
        assert_eq!(changed, 1, "the fixture must change one {table} row");
    }
    drop(writer);
    let entity = entities
        .get_entity_including_deleted(batch.entities[0].id)
        .await
        .unwrap()
        .unwrap();
    let note = notes
        .get_note_including_deleted(batch.notes[0].id)
        .await
        .unwrap()
        .unwrap();
    let edge = graph
        .get_edge_including_deleted(batch.edges[0].id)
        .await
        .unwrap()
        .unwrap();
    match collision {
        Collision::Deleted => {
            assert!(entity.deleted_at.is_some());
            assert!(note.deleted_at.is_some());
            assert!(edge.deleted_at.is_some());
        }
        Collision::OtherNamespace => {
            assert_eq!(entity.namespace, "another-namespace");
            assert_eq!(note.namespace, "another-namespace");
            assert_eq!(edge.namespace, "another-namespace");
        }
    }
}

fn add_absent_rows(batch: &mut CodeIngestBatch) {
    let mut entity = batch.entities[0].clone();
    entity.id = uuid::Uuid::new_v4();
    batch.entities.push(entity);
    let mut note = batch.notes[0].clone();
    note.id = uuid::Uuid::new_v4();
    batch.notes.push(note);
    let mut edge = batch.edges[0].clone();
    edge.id = uuid::Uuid::new_v4().into();
    batch.edges.push(edge);
}

fn report_counts(report: &CodeIngestReport, created: u64, skipped: u64) {
    assert!(report.dry_run);
    assert_eq!(
        report.entities_skipped_existing, skipped,
        "entity collision count"
    );
    assert_eq!(
        report.notes_skipped_existing, skipped,
        "note collision count"
    );
    assert_eq!(
        report.edges_skipped_existing, skipped,
        "edge collision count"
    );
    assert_eq!(report.entities_created, created, "entity absent count");
    assert_eq!(report.notes_created, created, "note absent count");
    assert_eq!(report.edges_created, created, "edge absent count");
}

fn source_files(db: &Path) -> Vec<(std::path::PathBuf, Option<Vec<u8>>)> {
    ["", "-wal", "-shm"]
        .into_iter()
        .map(|suffix| {
            let mut name = db.as_os_str().to_owned();
            name.push(suffix);
            let path = std::path::PathBuf::from(name);
            let bytes = match std::fs::read(&path) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("read private fixture {}: {error}", path.display()),
            };
            (path, bytes)
        })
        .collect()
}

async fn collision_report(collision: Collision) {
    let scratch = tempfile::tempdir().expect("private fixture directory");
    let db = scratch.path().join("collision.db");
    let mut batch = mapped_fixture();
    let backend = private_backend(&db);
    let healthy = dry_run_report(Some(&db), &batch)
        .await
        .expect("empty typed stores are readable");
    report_counts(&healthy, 1, 0);
    seed_collision(&backend, &batch, collision).await;
    // Queue-off setup has no asynchronous writer to outlive these handles.
    drop(backend);
    let before = source_files(&db);
    let existing = dry_run_report(Some(&db), &batch)
        .await
        .expect("existing rows remain collisions");
    assert_eq!(
        source_files(&db),
        before,
        "dry-run collision checks must not write the source or sidecars"
    );
    report_counts(&existing, 0, 1);
    add_absent_rows(&mut batch);
    let mixed = dry_run_report(Some(&db), &batch)
        .await
        .expect("mixed existing and missing rows");
    assert_eq!(
        source_files(&db),
        before,
        "mixed dry-run checks must not write the source or sidecars"
    );
    report_counts(&mixed, 1, 1);
}

#[serial_test::serial]
#[tokio::test]
async fn dry_run_counts_deleted_entity_note_and_edge_ids_as_existing() {
    if crate::test_process::run_in_child() {
        return;
    }
    std::env::set_var("KHIVE_WRITE_QUEUE", "0");
    collision_report(Collision::Deleted).await;
}

#[serial_test::serial]
#[tokio::test]
async fn dry_run_counts_other_namespace_entity_note_and_edge_ids_as_existing() {
    if crate::test_process::run_in_child() {
        return;
    }
    std::env::set_var("KHIVE_WRITE_QUEUE", "0");
    collision_report(Collision::OtherNamespace).await;
}
