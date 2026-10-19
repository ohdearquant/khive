use super::*;
use crate::RuntimeConfig;
use khive_storage::error::SqliteWriteStage;
use khive_types::Namespace;
use serde_json::{json, Value};

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().expect("temporary import database");
        let runtime = KhiveRuntime::new_for_test(RuntimeConfig {
            db_path: Some(directory.path().join("conflicts.db")),
            ..RuntimeConfig::no_embeddings()
        })
        .expect("runtime");
        runtime.install_kind_registry(vec!["concept".into()], vec![]);
        let token = runtime
            .authorize(Namespace::parse("local").unwrap())
            .unwrap();
        runtime.entities(&token).unwrap();
        runtime.graph(&token).unwrap();
        runtime.text(&token).unwrap();
        runtime.events(&token).unwrap();
        Self {
            runtime,
            token,
            _directory: directory,
        }
    }

    fn archive(&self) -> KgArchive {
        let timestamp = chrono::DateTime::from_timestamp_micros(1_700_000_000_000_000).unwrap();
        KgArchive {
            format: "khive-kg".into(),
            version: "0.1".into(),
            namespace: "local".into(),
            exported_at: timestamp,
            entities: (1..=2)
                .map(|id| ExportedEntity {
                    id: Uuid::from_u128(id),
                    kind: "concept".into(),
                    entity_type: None,
                    name: format!("Archive{id}"),
                    description: None,
                    properties: Some(json!({"imported": true})),
                    tags: vec![],
                    created_at: timestamp,
                    updated_at: timestamp,
                })
                .collect(),
            edges: vec![ExportedEdge {
                edge_id: Uuid::from_u128(101),
                source: Uuid::from_u128(1),
                target: Uuid::from_u128(2),
                relation: khive_storage::EdgeRelation::Supports,
                weight: 0.7,
                properties: None,
                created_at: timestamp,
                updated_at: timestamp,
            }],
        }
    }

    async fn row_image(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut result = serde_json::Map::new();
        for (table, sql) in [
            ("entities", "SELECT * FROM entities ORDER BY namespace,id"),
            (
                "graph_edges",
                "SELECT * FROM graph_edges ORDER BY namespace,id",
            ),
            ("events", "SELECT * FROM events ORDER BY namespace,id"),
            ("notes", "SELECT * FROM notes ORDER BY namespace,id"),
            (
                "fts_entities",
                "SELECT rowid,* FROM fts_entities ORDER BY rowid",
            ),
            (
                "fts_entities_rowids",
                "SELECT * FROM fts_entities_rowids ORDER BY namespace,subject_id",
            ),
        ] {
            let rows = reader
                .query_all(SqlStatement::new(sql, vec![]))
                .await
                .unwrap();
            result.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(result)
    }
}

#[tokio::test]
async fn import_rechecks_destination_before_any_dml_and_preserves_the_racing_winner() {
    let fixture = Fixture::new();
    let archive = fixture.archive();
    let prepared = fixture
        .runtime
        .prepare_import(
            &archive,
            &fixture.token,
            ImportKindPolicy::Strict,
            ImportOptions::default(),
        )
        .await
        .unwrap();
    let mut winner = archive_entity(&archive.entities[0], "local");
    winner.name = "RacingWinner".into();
    winner.properties = Some(json!({"winner": true}));
    fixture
        .runtime
        .entities(&fixture.token)
        .unwrap()
        .upsert_entity(winner)
        .await
        .unwrap();
    let before = fixture.row_image().await;
    let error = fixture
        .runtime
        .apply_prepared_import(&fixture.token, prepared)
        .await
        .expect_err("a stale preflight must refuse, not overwrite or skip the winner");
    assert!(matches!(error, RuntimeError::InvalidInput(message)
        if message.contains("destination changed during preflight")));
    assert_eq!(
        fixture.row_image().await,
        before,
        "the winner, absent second entity, graph, FTS and events must all remain unchanged"
    );
}

#[tokio::test]
async fn import_late_native_constraint_rolls_back_entity_fts_and_event_writes_without_a_summary() {
    let fixture = Fixture::new();
    let archive = fixture.archive();
    let mut original = archive_entity(&archive.entities[0], "local");
    original.name = "StoredBeforeImport".into();
    original.properties = Some(json!({"stored": true}));
    fixture
        .runtime
        .entities(&fixture.token)
        .unwrap()
        .upsert_entity(original)
        .await
        .unwrap();
    {
        let mut writer = fixture.runtime.sql().writer().await.unwrap();
        writer
            .execute_script(
                "CREATE TRIGGER import_late_native_failure BEFORE INSERT ON graph_edges \
             BEGIN SELECT RAISE(ABORT, 'bounded import failure control'); END;"
                    .into(),
            )
            .await
            .unwrap();
    }
    let before = fixture.row_image().await;
    let error = fixture
        .runtime
        .import_kg_with_options(
            &archive,
            &fixture.token,
            ImportKindPolicy::Strict,
            ImportOptions {
                on_conflict: ImportConflictMode::Update,
                ..ImportOptions::default()
            },
        )
        .await
        .expect_err("the final edge statement must propagate its native constraint failure");
    let RuntimeError::Storage(error) = error else {
        panic!("native storage failure was reclassified: {error:?}")
    };
    let failure = error
        .sqlite_write_failure()
        .expect("retain native write evidence");
    assert_eq!(failure.stage, SqliteWriteStage::Statement);
    assert_eq!(failure.primary_code, rusqlite::ffi::SQLITE_CONSTRAINT);
    assert_eq!(
        failure.extended_code,
        rusqlite::ffi::SQLITE_CONSTRAINT_TRIGGER
    );
    assert_eq!(fixture.row_image().await, before,
        "late SQL failure must restore the original entity/version and roll back new rows, FTS and events");
}

#[tokio::test]
async fn import_resolves_existing_note_event_and_edge_endpoints_and_guards_note_deletion() {
    let fixture = Fixture::new();
    let seed = fixture.archive();
    fixture
        .runtime
        .import_kg_with_options(
            &seed,
            &fixture.token,
            ImportKindPolicy::Strict,
            ImportOptions::default(),
        )
        .await
        .unwrap();
    let note = khive_storage::Note::new("local", "observation", "existing annotation source");
    fixture
        .runtime
        .notes(&fixture.token)
        .unwrap()
        .upsert_note(note.clone())
        .await
        .unwrap();
    let event = khive_storage::Event::new(
        "local",
        "import-control",
        khive_types::EventKind::Audit,
        khive_types::SubstrateKind::Entity,
        "fixture",
    );
    fixture
        .runtime
        .events(&fixture.token)
        .unwrap()
        .append_event(event.clone())
        .await
        .unwrap();
    let mut annotations = seed.clone();
    annotations.entities.clear();
    annotations.edges = [Uuid::from_u128(1), event.id, seed.edges[0].edge_id]
        .into_iter()
        .enumerate()
        .map(|(index, target)| ExportedEdge {
            edge_id: Uuid::from_u128(300 + index as u128),
            source: note.id,
            target,
            relation: khive_storage::EdgeRelation::Annotates,
            ..seed.edges[0].clone()
        })
        .collect();
    let preview = fixture
        .runtime
        .import_kg_with_options(
            &annotations,
            &fixture.token,
            ImportKindPolicy::Strict,
            ImportOptions {
                dry_run: true,
                ..ImportOptions::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(preview.edges_inserted, 3);
    assert_eq!(preview.edges_skipped, 0);
    let actual = fixture
        .runtime
        .import_kg_with_options(
            &annotations,
            &fixture.token,
            ImportKindPolicy::Strict,
            ImportOptions::default(),
        )
        .await
        .unwrap();
    assert_eq!(actual.edges_inserted, 3);
    for edge in &annotations.edges {
        let stored = fixture
            .runtime
            .get_edge(&fixture.token, edge.edge_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.source_id, note.id);
        assert_eq!(stored.target_id, edge.target);
        assert_eq!(stored.relation, khive_storage::EdgeRelation::Annotates);
    }

    let mut raced = annotations;
    raced.entities = vec![ExportedEntity {
        id: Uuid::from_u128(3),
        ..seed.entities[0].clone()
    }];
    raced.edges = vec![ExportedEdge {
        edge_id: Uuid::from_u128(400),
        source: note.id,
        target: Uuid::from_u128(2),
        relation: khive_storage::EdgeRelation::Annotates,
        ..seed.edges[0].clone()
    }];
    let prepared = fixture
        .runtime
        .prepare_import(
            &raced,
            &fixture.token,
            ImportKindPolicy::Strict,
            ImportOptions::default(),
        )
        .await
        .unwrap();
    {
        let mut writer = fixture.runtime.sql().writer().await.unwrap();
        // Deliberately leave updated_at/version unchanged: the deletion preimage
        // must itself protect endpoint liveness, rather than assume a revision bump.
        assert_eq!(
            writer
                .execute(SqlStatement::new(
                    "UPDATE notes SET deleted_at=1 WHERE id=?1",
                    vec![SqlValue::Text(note.id.to_string())],
                ))
                .await
                .unwrap(),
            1
        );
    }
    let before = fixture.row_image().await;
    let error = fixture
        .runtime
        .apply_prepared_import(&fixture.token, prepared)
        .await
        .expect_err("a deleted Note endpoint invalidates the whole prepared import");
    assert!(matches!(error, RuntimeError::InvalidInput(message)
        if message.contains("destination changed during preflight")));
    assert_eq!(fixture.row_image().await, before);
}
