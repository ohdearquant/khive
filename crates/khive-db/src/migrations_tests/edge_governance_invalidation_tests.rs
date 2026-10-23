use super::edge_governance_schema::{fixture_edge, insert_decision, MIGRATION_SQL};
use crate::stores::graph::{
    edge_merge_rewire_statement, edge_replace_if_unchanged_statement,
    EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL,
};
use crate::{SqliteError, StorageBackend};
use chrono::{TimeZone, Utc};
use khive_storage::types::{
    DeleteMode, Edge, EdgeUpsertDisposition, EdgeUpsertRequest, SqlStatement,
};
use khive_storage::{GraphStore, Note, NoteStore};
use khive_types::EdgeRelation;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::sync::Arc;
use uuid::Uuid;

const MARKER_INSERT: &str = "INSERT INTO edge_governance_active
    (edge_id,decision_id,edge_namespace,source_id,target_id,relation,target_backend,stamped_at)
    VALUES (?1,?2,?3,?4,?5,?6,NULL,100)";

struct Fixture {
    backend: StorageBackend,
    graph: Arc<dyn GraphStore>,
    notes: Arc<dyn NoteStore>,
}

impl Fixture {
    fn new() -> Self {
        let backend = StorageBackend::memory().unwrap();
        backend.prepare_core_schema().unwrap();
        let graph = backend.graph().unwrap();
        let notes = backend.notes().unwrap();
        {
            let reader = backend.pool().reader().unwrap();
            assert_eq!(
                reader
                    .conn()
                    .pragma_query_value(None, "foreign_keys", |row| row.get::<_, i64>(0))
                    .unwrap(),
                1,
                "the delete witness must exercise the actual FK cascade"
            );
        }
        Self {
            backend,
            graph,
            notes,
        }
    }

    fn try_write<R>(
        &self,
        write: impl FnOnce(&Connection) -> rusqlite::Result<R>,
    ) -> Result<R, SqliteError> {
        self.backend
            .pool()
            .writer()?
            .transaction(|conn| write(conn).map_err(SqliteError::from))
    }

    fn write<R>(&self, write: impl FnOnce(&Connection) -> rusqlite::Result<R>) -> R {
        self.try_write(write).unwrap()
    }

    fn statement(&self, statement: SqlStatement) -> usize {
        self.write(|conn| {
            let mut prepared = conn.prepare(&statement.sql)?;
            crate::sql_bridge::bind_params(&mut prepared, &statement.params)?;
            prepared.raw_execute()
        })
    }

    async fn edge(&self) -> Edge {
        let source = Note::new("local", "memory", "source");
        let target = Note::new("local", "memory", "target");
        self.notes.upsert_note(source.clone()).await.unwrap();
        self.notes.upsert_note(target.clone()).await.unwrap();
        let mut edge = fixture_edge();
        edge.source_id = source.id;
        edge.target_id = target.id;
        self.graph.upsert_edge(edge.clone()).await.unwrap();
        edge
    }

    fn insert_marker(conn: &Connection, edge: &Edge, decision: &str) -> rusqlite::Result<usize> {
        conn.execute(
            MARKER_INSERT,
            rusqlite::params![
                Uuid::from(edge.id).to_string(),
                decision,
                edge.namespace,
                edge.source_id.to_string(),
                edge.target_id.to_string(),
                edge.relation.as_str(),
            ],
        )
    }

    fn stamp(&self, edge: &Edge, decision: &str) {
        // Fixture-only authority: no production stamping path is introduced.
        self.write(|conn| {
            insert_decision(conn, edge, decision)?;
            Self::insert_marker(conn, edge, decision)
        });
    }

    fn marker(&self, edge: &Edge) -> Option<String> {
        self.backend
            .pool()
            .reader()
            .unwrap()
            .conn()
            .query_row(
                "SELECT decision_id FROM edge_governance_active WHERE edge_id=?1",
                [Uuid::from(edge.id).to_string()],
                |row| row.get(0),
            )
            .optional()
            .unwrap()
    }

    fn invalidations(&self, decision: &str) -> Vec<(String, String, i64)> {
        let reader = self.backend.pool().reader().unwrap();
        let mut statement = reader.conn().prepare(
            "SELECT edge_id,cause,invalidated_at FROM edge_governance_invalidations WHERE decision_id=?1 ORDER BY invalidation_id",
        ).unwrap();
        let rows = statement
            .query_map([decision], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        rows
    }

    fn assert_invalidated(&self, edge: &Edge, decision: &str, cause: &str) {
        assert_eq!(self.marker(edge), None);
        let invalidations = self.invalidations(decision);
        assert_eq!(
            invalidations.len(),
            1,
            "exactly one invalidation for {decision}"
        );
        assert_eq!(invalidations[0].0, Uuid::from(edge.id).to_string());
        assert_eq!(invalidations[0].1, cause);
        assert!(invalidations[0].2 > 0, "microsecond timestamp recorded");
        let reader = self.backend.pool().reader().unwrap();
        assert_eq!(
            reader
                .conn()
                .query_row(
                    "SELECT COUNT(*) FROM edge_governance_decisions WHERE decision_id=?1",
                    [decision],
                    |row| row.get::<_, i64>(0)
                )
                .unwrap(),
            1
        );
        assert_eq!(
            reader
                .conn()
                .query_row("SELECT status FROM edge_governance_state", [], |row| row
                    .get::<_, String>(
                    0
                ))
                .unwrap(),
            "inactive"
        );
    }
}

#[test]
fn edge_governance_installed_trigger_manifest_matches_immutable_sql() {
    let fixture = Fixture::new();
    let mut expected = BTreeMap::new();
    for line in MIGRATION_SQL.lines() {
        if let Some(entry) = line.strip_prefix("-- ADR159-TRIGGER-SHA256 ") {
            let (name, digest) = entry.split_once(' ').expect("trigger name and SHA256");
            assert_eq!(digest.len(), 64);
            assert!(expected
                .insert(name.to_owned(), digest.to_owned())
                .is_none());
        }
    }
    assert!(
        !expected.is_empty(),
        "the migration must author its trigger manifest"
    );
    let reader = fixture.backend.pool().reader().unwrap();
    let mut statement = reader.conn().prepare(
        "SELECT name,sql FROM sqlite_schema WHERE type='trigger' AND name GLOB 'edge_governance_*' ORDER BY name",
    ).unwrap();
    let actual = statement
        .query_map([], |row| {
            let name: String = row.get(0)?;
            let sql: String = row.get(1)?;
            let normalized = sql.split_whitespace().collect::<Vec<_>>().join(" ");
            Ok((name, format!("{:x}", Sha256::digest(normalized.as_bytes()))))
        })
        .unwrap()
        .collect::<rusqlite::Result<BTreeMap<_, _>>>()
        .unwrap();
    assert_eq!(
        actual, expected,
        "installed trigger names and normalized SQL digests"
    );
}

#[tokio::test]
async fn edge_governance_generic_store_writes_do_not_stamp_metadata_claims() {
    let fixture = Fixture::new();
    let mut edge = fixture.edge().await;
    edge.metadata = Some(serde_json::json!({
        "edge_governance_active": {"decision_id":"caller-chosen"},
        "authorizer_id":"caller-chosen", "authority_action":"memory.supersede"
    }));
    fixture.graph.upsert_edge(edge.clone()).await.unwrap();
    assert!(fixture.graph.get_edge(edge.id).await.unwrap().is_some());
    let reader = fixture.backend.pool().reader().unwrap();
    for table in [
        "edge_governance_decisions",
        "edge_governance_active",
        "edge_governance_invalidations",
    ] {
        let count = reader
            .conn()
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        assert_eq!(count, 0, "generic writes cannot stamp {table}");
    }
}

#[tokio::test]
async fn edge_governance_uuid_preimage_transitions_invalidate_once() {
    for field in [
        "namespace",
        "source_id",
        "target_id",
        "relation",
        "target_backend",
    ] {
        let fixture = Fixture::new();
        let edge = fixture.edge().await;
        fixture.stamp(&edge, field);
        let mut changed = edge.clone();
        changed.updated_at = Utc.timestamp_micros(200).single().unwrap();
        match field {
            "namespace" => changed.namespace = "other".into(),
            "source_id" => changed.source_id = Uuid::new_v4(),
            "target_id" => changed.target_id = Uuid::new_v4(),
            "relation" => changed.relation = EdgeRelation::DerivedFrom,
            "target_backend" => changed.target_backend = Some("other-backend".into()),
            _ => unreachable!(),
        }
        assert_eq!(
            fixture.statement(edge_replace_if_unchanged_statement(
                &changed,
                edge.updated_at,
                None
            )),
            1
        );
        fixture.assert_invalidated(&edge, field, "endpoint_or_relation_or_backend_rewrite");
        let stored = fixture.graph.get_edge(edge.id).await.unwrap().unwrap();
        assert_eq!(stored.namespace, changed.namespace);
        assert_eq!(stored.source_id, changed.source_id);
        assert_eq!(stored.target_id, changed.target_id);
        assert_eq!(stored.relation, changed.relation);
        assert_eq!(stored.target_backend, changed.target_backend);
    }
}

#[tokio::test]
async fn edge_governance_natural_key_backend_rewrite_invalidates_incumbent() {
    let fixture = Fixture::new();
    let edge = fixture.edge().await;
    fixture.stamp(&edge, "natural-key");
    let mut incoming = edge.clone();
    incoming.id = Uuid::new_v4().into();
    incoming.target_backend = Some("remote".into());
    let result = fixture
        .graph
        .upsert_edge_observed(EdgeUpsertRequest {
            edge: incoming.clone(),
            resurrect: false,
        })
        .await
        .unwrap();
    assert_eq!(result.disposition, EdgeUpsertDisposition::Updated);
    assert_eq!(
        result.edge.id, edge.id,
        "natural-key incumbent identity survives"
    );
    assert_eq!(result.edge.target_backend, incoming.target_backend);
    assert!(fixture.graph.get_edge(incoming.id).await.unwrap().is_none());
    fixture.assert_invalidated(
        &edge,
        "natural-key",
        "endpoint_or_relation_or_backend_rewrite",
    );
}

#[tokio::test]
async fn edge_governance_merge_rewire_statement_does_not_transfer_authority() {
    for source_side in [true, false] {
        let fixture = Fixture::new();
        let edge = fixture.edge().await;
        fixture.stamp(&edge, "merge-rewire");
        let replacement = Note::new("local", "memory", "merge survivor");
        fixture
            .notes
            .upsert_note(replacement.clone())
            .await
            .unwrap();
        let source = if source_side {
            replacement.id
        } else {
            edge.source_id
        };
        let target = if source_side {
            edge.target_id
        } else {
            replacement.id
        };
        assert_eq!(
            fixture.statement(edge_merge_rewire_statement(
                &edge.namespace,
                edge.id.into(),
                source,
                target,
                200
            )),
            1
        );
        fixture.assert_invalidated(
            &edge,
            "merge-rewire",
            "endpoint_or_relation_or_backend_rewrite",
        );
        let stored = fixture.graph.get_edge(edge.id).await.unwrap().unwrap();
        assert_eq!((stored.source_id, stored.target_id), (source, target));
    }
}

#[tokio::test]
async fn edge_governance_merge_conflict_delete_preserves_only_survivor_marker() {
    let fixture = Fixture::new();
    let survivor = fixture.edge().await;
    let mut incoming = survivor.clone();
    incoming.id = Uuid::new_v4().into();
    let merged_note = Note::new("local", "memory", "merge source");
    incoming.source_id = merged_note.id;
    fixture.notes.upsert_note(merged_note).await.unwrap();
    fixture.graph.upsert_edge(incoming.clone()).await.unwrap();
    fixture.stamp(&survivor, "survivor");
    fixture.stamp(&incoming, "incoming");
    fixture.write(|conn| {
        let conflict: String = conn.query_row(
            "SELECT id FROM graph_edges WHERE namespace=?1 AND source_id=?2 AND target_id=?3 AND relation=?4",
            rusqlite::params![survivor.namespace,survivor.source_id.to_string(),survivor.target_id.to_string(),survivor.relation.as_str()],
            |row| row.get(0),
        )?;
        assert_eq!(conflict, Uuid::from(survivor.id).to_string());
        conn.execute(EDGE_SYMMETRIC_DELETE_NONCANONICAL_SQL, rusqlite::params![incoming.namespace,Uuid::from(incoming.id).to_string()])
    });
    fixture.assert_invalidated(&incoming, "incoming", "hard_delete");
    assert_eq!(fixture.marker(&survivor).as_deref(), Some("survivor"));
    assert!(fixture.invalidations("survivor").is_empty());
    assert!(fixture.graph.get_edge(incoming.id).await.unwrap().is_none());
    assert_eq!(
        fixture
            .graph
            .get_edge(survivor.id)
            .await
            .unwrap()
            .unwrap()
            .id,
        survivor.id
    );
}

#[tokio::test]
async fn edge_governance_edge_liveness_transitions_invalidate_both_directions() {
    let fixture = Fixture::new();
    let edge = fixture.edge().await;
    fixture.stamp(&edge, "edge-delete");
    assert!(fixture
        .graph
        .delete_edge(edge.id, DeleteMode::Soft)
        .await
        .unwrap());
    fixture.assert_invalidated(&edge, "edge-delete", "edge_liveness_transition");
    // A test-only stale marker on a tombstone exercises the defensive restore arm.
    fixture.stamp(&edge, "edge-restore");
    let restored = fixture
        .graph
        .upsert_edge_observed(EdgeUpsertRequest {
            edge: edge.clone(),
            resurrect: true,
        })
        .await
        .unwrap();
    assert_eq!(restored.disposition, EdgeUpsertDisposition::Resurrected);
    assert_eq!(restored.edge.id, edge.id);
    fixture.assert_invalidated(&edge, "edge-restore", "edge_liveness_transition");
}

#[tokio::test]
async fn edge_governance_same_key_resurrection_cannot_reuse_spent_decision() {
    let fixture = Fixture::new();
    let edge = fixture.edge().await;
    fixture.stamp(&edge, "spent");
    fixture
        .graph
        .delete_edge(edge.id, DeleteMode::Soft)
        .await
        .unwrap();
    let mut incoming = edge.clone();
    incoming.id = Uuid::new_v4().into();
    let restored = fixture
        .graph
        .upsert_edge_observed(EdgeUpsertRequest {
            edge: incoming,
            resurrect: true,
        })
        .await
        .unwrap()
        .edge;
    assert_eq!(restored.id, edge.id);
    assert_eq!(
        (
            restored.namespace.as_str(),
            restored.source_id,
            restored.target_id,
            restored.relation,
            restored.target_backend.as_deref()
        ),
        (
            edge.namespace.as_str(),
            edge.source_id,
            edge.target_id,
            edge.relation,
            None
        )
    );
    fixture.assert_invalidated(&edge, "spent", "edge_liveness_transition");
    let error = fixture
        .try_write(|conn| Fixture::insert_marker(conn, &restored, "spent"))
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("edge governance decision is spent"));
    assert_eq!(fixture.marker(&edge), None);
    fixture.stamp(&restored, "fresh");
    assert_eq!(fixture.marker(&edge).as_deref(), Some("fresh"));
    let error = fixture
        .try_write(|conn| {
            conn.execute(
                "UPDATE edge_governance_active SET decision_id='spent' WHERE decision_id='fresh'",
                [],
            )
        })
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("edge governance decision is spent"));
    assert_eq!(fixture.marker(&edge).as_deref(), Some("fresh"));
    assert_eq!(fixture.invalidations("spent").len(), 1);
}

#[tokio::test]
async fn edge_governance_endpoint_note_liveness_transitions_invalidate_both_directions() {
    for source_side in [true, false] {
        let fixture = Fixture::new();
        let edge = fixture.edge().await;
        let note_id = if source_side {
            edge.source_id
        } else {
            edge.target_id
        };
        let mut incident = fixture.edge().await;
        if source_side {
            incident.target_id = note_id;
        } else {
            incident.source_id = note_id;
        }
        fixture.graph.upsert_edge(incident.clone()).await.unwrap();
        let mut note = fixture.notes.get_note(note_id).await.unwrap().unwrap();
        fixture.stamp(&edge, "note-delete");
        fixture.stamp(&incident, "incident-delete");
        assert!(fixture
            .notes
            .delete_note(note_id, DeleteMode::Soft)
            .await
            .unwrap());
        fixture.assert_invalidated(&edge, "note-delete", "endpoint_note_liveness_transition");
        fixture.assert_invalidated(
            &incident,
            "incident-delete",
            "endpoint_note_liveness_transition",
        );
        // Restore detection remains effective even if an earlier bug left a marker.
        fixture.stamp(&edge, "note-restore");
        fixture.stamp(&incident, "incident-restore");
        note.content = "same ID, replacement content".into();
        note.deleted_at = None;
        fixture.notes.upsert_note(note).await.unwrap();
        fixture.assert_invalidated(&edge, "note-restore", "endpoint_note_liveness_transition");
        fixture.assert_invalidated(
            &incident,
            "incident-restore",
            "endpoint_note_liveness_transition",
        );
    }
}

#[tokio::test]
async fn edge_governance_hard_delete_records_before_fk_cascade_and_keeps_history() {
    let fixture = Fixture::new();
    let edge = fixture.edge().await;
    fixture.stamp(&edge, "hard-edge");
    assert!(fixture
        .graph
        .delete_edge(edge.id, DeleteMode::Hard)
        .await
        .unwrap());
    fixture.assert_invalidated(&edge, "hard-edge", "hard_delete");
    assert!(fixture
        .graph
        .get_edge_including_deleted(edge.id)
        .await
        .unwrap()
        .is_none());
    let reader = fixture.backend.pool().reader().unwrap();
    let fk_cascade: bool = reader.conn().query_row("SELECT EXISTS(SELECT 1 FROM pragma_foreign_key_list('edge_governance_active') WHERE \"table\"='graph_edges' AND on_delete='CASCADE')", [], |row| row.get(0)).unwrap();
    assert!(
        fk_cascade,
        "this witness includes the active-marker FK cascade"
    );
}

#[tokio::test]
async fn edge_governance_hard_note_delete_and_edge_cascade_record_only_once() {
    for endpoint in ["source", "target", "self-loop"] {
        let fixture = Fixture::new();
        let mut edge = fixture.edge().await;
        if endpoint == "self-loop" {
            edge.target_id = edge.source_id;
            fixture.graph.upsert_edge(edge.clone()).await.unwrap();
        }
        fixture.stamp(&edge, "hard-note");
        let note_id = if endpoint == "target" {
            edge.target_id
        } else {
            edge.source_id
        };
        assert!(fixture
            .notes
            .delete_note(note_id, DeleteMode::Hard)
            .await
            .unwrap());
        fixture.assert_invalidated(&edge, "hard-note", "hard_delete");
        // The runtime's later incident-edge cascade must not append a second row,
        // including when both endpoint predicates match the same active row.
        fixture.graph.purge_incident_edges(note_id).await.unwrap();
        fixture.assert_invalidated(&edge, "hard-note", "hard_delete");
        assert!(fixture
            .graph
            .get_edge_including_deleted(edge.id)
            .await
            .unwrap()
            .is_none());
    }
}

#[tokio::test]
async fn edge_governance_weight_metadata_and_same_value_assignments_keep_markers() {
    let fixture = Fixture::new();
    let mut edge = fixture.edge().await;
    fixture.stamp(&edge, "retained");
    edge.weight = 0.875;
    edge.metadata = Some(serde_json::json!({"ordinary":"changed"}));
    fixture.graph.upsert_edge(edge.clone()).await.unwrap();
    let note = fixture
        .notes
        .get_note(edge.target_id)
        .await
        .unwrap()
        .unwrap();
    fixture.notes.upsert_note(note).await.unwrap();
    fixture.write(|conn| conn.execute("UPDATE graph_edges SET namespace=namespace,source_id=source_id,target_id=target_id,relation=relation,target_backend=target_backend,deleted_at=deleted_at WHERE id=?1", [Uuid::from(edge.id).to_string()]));
    assert_eq!(fixture.marker(&edge).as_deref(), Some("retained"));
    assert!(fixture.invalidations("retained").is_empty());
    let stored = fixture.graph.get_edge(edge.id).await.unwrap().unwrap();
    assert_eq!(stored.weight, 0.875);
    assert_eq!(stored.metadata, edge.metadata);
}

#[tokio::test]
async fn edge_governance_ledger_failure_rolls_back_edge_mutation_and_marker_clear() {
    for (sql, note_endpoint, cause) in [
        (
            "UPDATE graph_edges SET source_id='changed' WHERE id=?1",
            false,
            "endpoint_or_relation_or_backend_rewrite",
        ),
        ("DELETE FROM graph_edges WHERE id=?1", false, "hard_delete"),
        (
            "UPDATE notes SET deleted_at=200 WHERE id=?1",
            true,
            "endpoint_note_liveness_transition",
        ),
        ("DELETE FROM notes WHERE id=?1", true, "hard_delete"),
    ] {
        let fixture = Fixture::new();
        let edge = fixture.edge().await;
        fixture.stamp(&edge, "ledger-failure");
        let snapshot = || {
            let reader = fixture.backend.pool().reader().unwrap();
            [
                "SELECT rowid,* FROM graph_edges ORDER BY id",
                "SELECT rowid,* FROM notes ORDER BY id",
                "SELECT * FROM edge_governance_decisions ORDER BY decision_id",
                "SELECT * FROM edge_governance_active ORDER BY edge_id",
                "SELECT * FROM edge_governance_invalidations ORDER BY invalidation_id",
                "SELECT * FROM edge_governance_state",
            ]
            .map(|query| {
                let mut statement = reader.conn().prepare(query).unwrap();
                let columns = statement.column_count();
                let values = statement
                    .query_map([], |row| {
                        (0..columns)
                            .map(|column| row.get::<_, rusqlite::types::Value>(column))
                            .collect::<rusqlite::Result<Vec<_>>>()
                    })
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap();
                values
            })
        };
        let before = snapshot();
        fixture.write(|conn| conn.execute_batch("CREATE TRIGGER refuse_fixture_invalidation BEFORE INSERT ON edge_governance_invalidations BEGIN SELECT RAISE(ABORT,'fixture ledger failure'); END"));
        let id = if note_endpoint {
            edge.target_id.to_string()
        } else {
            Uuid::from(edge.id).to_string()
        };
        let error = fixture
            .try_write(|conn| conn.execute(sql, [&id]))
            .unwrap_err();
        let native = match error {
            SqliteError::Rusqlite(source) | SqliteError::Write { source, .. } => source,
            error => panic!("expected native trigger failure, got {error}"),
        };
        assert_eq!(
            native.sqlite_error_code(),
            Some(rusqlite::ErrorCode::ConstraintViolation)
        );
        assert!(native.to_string().contains("fixture ledger failure"));
        assert_eq!(
            snapshot(),
            before,
            "native refusal must roll back graph, notes, history and projection: {sql}"
        );
        fixture.write(|conn| conn.execute_batch("DROP TRIGGER refuse_fixture_invalidation"));
        assert_eq!(fixture.write(|conn| conn.execute(sql, [&id])), 1);
        fixture.assert_invalidated(&edge, "ledger-failure", cause);
    }
}

#[tokio::test]
async fn edge_governance_rebuild_excludes_spent_decision_after_preimage_recurs() {
    let fixture = Fixture::new();
    let edge = fixture.edge().await;
    fixture.stamp(&edge, "rebuild-spent");
    fixture
        .graph
        .delete_edge(edge.id, DeleteMode::Soft)
        .await
        .unwrap();
    fixture
        .graph
        .upsert_edge_observed(EdgeUpsertRequest {
            edge: edge.clone(),
            resurrect: true,
        })
        .await
        .unwrap();
    // Test-only reconstruction: no activation, receipt issuance, or public API.
    // ADR159 §1:154–161 / §Verification:553–571 requires the ledger to be
    // load-bearing after exact preimage and endpoint liveness have recurred.
    fixture.write(|conn| conn.execute(
        "INSERT INTO edge_governance_active
         (edge_id,decision_id,edge_namespace,source_id,target_id,relation,target_backend,stamped_at)
         SELECT d.edge_id,d.decision_id,d.edge_namespace,d.source_id,d.target_id,d.relation,d.target_backend,200
         FROM edge_governance_decisions AS d
         JOIN graph_edges AS e ON e.id=d.edge_id
         JOIN notes AS s ON s.id=d.source_id
         JOIN notes AS t ON t.id=d.target_id
         WHERE d.disposition='authorized' AND e.deleted_at IS NULL
           AND s.deleted_at IS NULL AND t.deleted_at IS NULL
           AND s.kind='memory' AND t.kind='memory'
           AND e.namespace IS d.edge_namespace AND e.source_id IS d.source_id
           AND e.target_id IS d.target_id AND e.relation IS d.relation
           AND e.target_backend IS d.target_backend AND e.target_backend IS NULL
           AND NOT EXISTS (SELECT 1 FROM edge_governance_invalidations AS i
                           WHERE i.decision_id=d.decision_id)", []));
    assert_eq!(
        fixture.marker(&edge),
        None,
        "a recurring preimage must not revive its spent decision"
    );
    fixture.assert_invalidated(&edge, "rebuild-spent", "edge_liveness_transition");
    fixture.stamp(&edge, "rebuild-fresh");
    assert_eq!(fixture.marker(&edge).as_deref(), Some("rebuild-fresh"));
}
