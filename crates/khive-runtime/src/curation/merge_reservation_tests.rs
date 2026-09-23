//! ADR-115 A1 reservation witnesses through the runtime's public merge routes.

use super::*;
use crate::{Namespace, RuntimeConfig};
use khive_storage::{SqlStatement, StorageCapability, StorageError, WriterTaskRequestState};
use serde_json::json;

#[derive(Clone, Copy, Debug)]
enum Record {
    Entity,
    Note,
}

impl Record {
    fn table(self) -> &'static str {
        match self {
            Self::Entity => "entities",
            Self::Note => "notes",
        }
    }

    fn driver(self, source: impl std::error::Error + Send + Sync + 'static) -> StorageError {
        let (capability, operation) = match self {
            Self::Entity => (StorageCapability::Entities, "merge_entity"),
            Self::Note => (StorageCapability::Notes, "merge_note"),
        };
        StorageError::driver(capability, operation, source)
    }

    fn map_error(self, error: StorageError) -> RuntimeError {
        match self {
            Self::Entity => map_merge_entity_storage_error(error),
            Self::Note => map_merge_note_storage_error(error),
        }
    }
}

struct Fixture {
    runtime: KhiveRuntime,
    token: NamespaceToken,
    _directory: Option<tempfile::TempDir>,
}

impl Fixture {
    fn new(file_backed: bool) -> Self {
        let directory = file_backed.then(|| tempfile::tempdir().expect("fixture directory"));
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: directory.as_ref().map(|dir| dir.path().join("merge.db")),
            actor_id: Some("lambda:merge-reservation-fixture".into()),
            brain_profile: None,
            events_split: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private model-less runtime");
        let token = runtime
            .authorize(Namespace::local())
            .expect("fixture actor");
        // The owner runs this module in separate queue=0 and queue=1 processes.
        // Assert the actual route rather than letting both gates cover one mode.
        if let Ok(queue) = std::env::var("KHIVE_WRITE_QUEUE") {
            if queue == "0" || queue == "1" {
                assert_eq!(
                    runtime
                        .backend()
                        .pool_arc()
                        .writer_task_handle()
                        .unwrap()
                        .is_some(),
                    file_backed && queue == "1",
                    "fixture must exercise the requested writer route"
                );
            }
        }
        Self {
            runtime,
            token,
            _directory: directory,
        }
    }

    async fn seed(&self, record: Record, name: &str, properties: Value) -> Uuid {
        let id = match record {
            Record::Entity => {
                self.runtime
                    .create_entity(
                        &self.token,
                        "concept",
                        None,
                        name,
                        Some("searchable entity text"),
                        None,
                        vec![],
                    )
                    .await
                    .expect("clean entity seed")
                    .id
            }
            Record::Note => {
                self.runtime
                    .create_note(
                        &self.token,
                        "observation",
                        Some(name),
                        "searchable note text",
                        None,
                        None,
                        vec![],
                    )
                    .await
                    .expect("clean note seed")
                    .id
            }
        };
        // Privileged fixture corruption only: application create correctly refuses
        // the forged top-level stamp, so seed the historical preimage directly.
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.expect("fixture writer");
        writer
            .execute(SqlStatement {
                // Entity rows guard their version: every update advances it by one.
                sql: format!(
                    "UPDATE {} SET properties = ?1{} WHERE id = ?2",
                    record.table(),
                    match record {
                        Record::Entity => ", version = version + 1",
                        Record::Note => "",
                    }
                ),
                params: vec![
                    SqlValue::Text(properties.to_string()),
                    SqlValue::Text(id.to_string()),
                ],
                label: Some("test.merge_reservation.seed_preimage".into()),
            })
            .await
            .expect("seed stored property preimage");
        id
    }

    async fn edge(&self, record: Record, from: Uuid) -> Uuid {
        let anchor = self
            .runtime
            .create_entity(
                &self.token,
                "concept",
                None,
                "Merge anchor",
                None,
                None,
                vec![],
            )
            .await
            .expect("edge anchor");
        let relation = match record {
            Record::Entity => EdgeRelation::Extends,
            Record::Note => EdgeRelation::Annotates,
        };
        self.runtime
            .link(&self.token, from, anchor.id, relation, 0.7, None)
            .await
            .expect("edge that a successful merge would rewire");
        anchor.id
    }

    async fn merge(
        &self,
        record: Record,
        into: Uuid,
        from: Uuid,
        strategy: EntityDedupMergePolicy,
        dry_run: bool,
    ) -> RuntimeResult<MergeSummary> {
        match record {
            Record::Entity => {
                self.runtime
                    .merge_entity_with_reason(
                        &self.token,
                        into,
                        from,
                        strategy,
                        ContentMergeStrategy::Append,
                        dry_run,
                        None,
                    )
                    .await
            }
            Record::Note => {
                self.runtime
                    .merge_note_with_reason(
                        &self.token,
                        into,
                        from,
                        strategy,
                        ContentMergeStrategy::Append,
                        dry_run,
                        None,
                    )
                    .await
            }
        }
    }

    async fn snapshot(&self) -> Value {
        // Prime each lazy schema before taking a snapshot, not in the assertion.
        self.runtime.entities(&self.token).unwrap();
        self.runtime.notes(&self.token).unwrap();
        self.runtime.graph(&self.token).unwrap();
        self.runtime.text(&self.token).unwrap();
        self.runtime.text_for_notes(&self.token).unwrap();
        self.runtime.events(&self.token).unwrap();
        let sql = self.runtime.sql();
        let mut reader = sql.reader().await.expect("snapshot reader");
        let mut snapshot = serde_json::Map::new();
        for table in [
            "entities",
            "notes",
            "graph_edges",
            "notes_seq",
            "attachments",
            "events",
            "fts_entities",
            "fts_notes",
        ] {
            let rows = reader
                .query_all(SqlStatement {
                    sql: format!("SELECT rowid, * FROM {table} ORDER BY rowid"),
                    params: vec![],
                    label: Some("test.merge_reservation.snapshot".into()),
                })
                .await
                .expect("domain and index snapshot");
            snapshot.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(snapshot)
    }

    async fn veto_domain_dml(&self) {
        let sql = self.runtime.sql();
        let mut writer = sql.writer().await.expect("fixture trigger writer");
        for table in ["entities", "notes", "graph_edges"] {
            for operation in ["INSERT", "UPDATE", "DELETE"] {
                writer
                    .execute(SqlStatement {
                        sql: format!(
                            "CREATE TRIGGER test_reservation_veto_{table}_{operation} \
                             BEFORE {operation} ON {table} BEGIN \
                             SELECT RAISE(ABORT, 'fixture forbids domain DML before refusal'); END"
                        ),
                        params: vec![],
                        label: Some("test.merge_reservation.veto_domain_dml".into()),
                    })
                    .await
                    .expect("fixture DML veto trigger");
            }
        }
    }
}

fn reserved_property_refusal() -> RuntimeError {
    crate::secret_gate::reject_reserved_secret_gate_property(Some(
        &json!({"khive:secret_gate": null}),
    ))
    .unwrap_err()
}

#[test]
fn merge_reservation_error_mapping_recovers_only_a_confirmed_rollback_refusal() {
    for record in [Record::Entity, Record::Note] {
        for wrapped in [false, true] {
            let mut error = record.driver(MergeSqlError::Refusal(reserved_property_refusal()));
            if wrapped {
                error = StorageError::WriterTaskRequestFailed {
                    request_state: WriterTaskRequestState::TransactionRolledBack,
                    source: Box::new(error),
                };
            }
            let error = record.map_error(error);
            assert!(matches!(error, RuntimeError::InvalidInput(_)), "{error:?}");
            assert_eq!(error.to_string(), reserved_property_refusal().to_string());
        }
    }
}

#[test]
fn merge_reservation_error_mapping_preserves_other_sources_and_request_states() {
    for record in [Record::Entity, Record::Note] {
        for request_state in [
            WriterTaskRequestState::NotStarted,
            WriterTaskRequestState::TransactionRolledBack,
            WriterTaskRequestState::SideEffectsUnknown,
        ] {
            let mut sources = vec![
                record.driver(MergeSqlError::Sqlite(SqliteError::InvalidData(
                    "fixture sqlite failure".into(),
                ))),
                record.driver(std::io::Error::other(
                    reserved_property_refusal().to_string(),
                )),
                StorageError::Pool {
                    operation: "fixture_commit".into(),
                    message: "fixture commit failure".into(),
                },
                // An unexpected nested envelope is not this adapter's known
                // writer-closure transport and must not be recursively erased.
                StorageError::WriterTaskRequestFailed {
                    request_state: WriterTaskRequestState::TransactionRolledBack,
                    source: Box::new(
                        record.driver(MergeSqlError::Refusal(reserved_property_refusal())),
                    ),
                },
            ];
            if request_state != WriterTaskRequestState::TransactionRolledBack {
                sources.push(record.driver(MergeSqlError::Refusal(reserved_property_refusal())));
            }
            for source in sources {
                let error = StorageError::WriterTaskRequestFailed {
                    request_state,
                    source: Box::new(source),
                };
                let before = format!("{error:?}");
                let RuntimeError::Storage(after) = record.map_error(error) else {
                    panic!("non-refusal and unconfirmed outcomes must retain storage metadata");
                };
                assert_eq!(format!("{after:?}"), before, "{record:?}/{request_state:?}");
            }

            let error = StorageError::WriterTaskTerminated { request_state };
            let before = format!("{error:?}");
            let RuntimeError::Storage(after) = record.map_error(error) else {
                panic!("terminal writer outcomes must not become semantic refusals");
            };
            assert_eq!(format!("{after:?}"), before);
        }
    }
}

#[tokio::test]
async fn merge_reservation_refuses_forged_source_and_target_before_domain_mutation() {
    // Checks remain inside the existing writer transaction, including dry-run.
    // Initialization and BEGIN/rollback are allowed; domain DML is not.
    let stamp = json!({"khive:secret_gate": {"forged": "private-fixture-posture"}});
    let cases = [
        (
            "copy source stamp",
            json!({"ordinary": true}),
            stamp.clone(),
            EntityDedupMergePolicy::Union,
        ),
        (
            "discard source stamp",
            json!("ordinary replacement"),
            stamp.clone(),
            EntityDedupMergePolicy::PreferInto,
        ),
        (
            "remove target stamp",
            stamp.clone(),
            Value::Null,
            EntityDedupMergePolicy::PreferFrom,
        ),
        (
            "replace target stamp",
            stamp,
            json!({"khive:secret_gate": {"forged": "replacement"}}),
            EntityDedupMergePolicy::PreferFrom,
        ),
        (
            "source null stamp",
            json!({"ordinary": true}),
            json!({"khive:secret_gate": null}),
            EntityDedupMergePolicy::PreferFrom,
        ),
        (
            "target null stamp",
            json!({"khive:secret_gate": null}),
            json!({"ordinary": true}),
            EntityDedupMergePolicy::PreferInto,
        ),
    ];
    for file_backed in [false, true] {
        for record in [Record::Entity, Record::Note] {
            for (label, into_properties, from_properties, strategy) in &cases {
                let fixture = Fixture::new(file_backed);
                let into = fixture.seed(record, "Into", into_properties.clone()).await;
                let from = fixture.seed(record, "From", from_properties.clone()).await;
                fixture.edge(record, from).await;
                let before = fixture.snapshot().await;
                fixture.veto_domain_dml().await;
                for dry_run in [true, false] {
                    let error = fixture
                        .merge(record, into, from, *strategy, dry_run)
                        .await
                        .expect_err("top-level reservation applies before every merge mutation");
                    let expected = crate::secret_gate::reject_reserved_secret_gate_property(Some(
                        &json!({"khive:secret_gate": null}),
                    ))
                    .unwrap_err();
                    assert!(
                        matches!(&error, RuntimeError::InvalidInput(_)),
                        "{record:?}/{label}/file={file_backed}/dry={dry_run}: {error:?}"
                    );
                    assert_eq!(error.to_string(), expected.to_string());
                    assert!(!error.to_string().contains("private-fixture-posture"));
                    assert_eq!(fixture.snapshot().await, before,
                        "{record:?}/{label}/file={file_backed}/dry={dry_run}: rows, indexes, edges and events must stay intact");
                }
            }
        }
    }
}

#[tokio::test]
async fn merge_reservation_preserves_ordinary_and_nested_property_merges() {
    for file_backed in [false, true] {
        for record in [Record::Entity, Record::Note] {
            for nested in [false, true] {
                let fixture = Fixture::new(file_backed);
                let into = fixture
                    .seed(record, "Into", json!({"keep": "ordinary"}))
                    .await;
                let properties = if nested {
                    json!({"ordinary": {"khive:secret_gate": "nested data"}})
                } else {
                    json!({"ordinary": "plain data"})
                };
                let from = fixture.seed(record, "From", properties.clone()).await;
                let anchor = fixture.edge(record, from).await;
                let summary = fixture
                    .merge(record, into, from, EntityDedupMergePolicy::Union, false)
                    .await
                    .expect("ordinary and nested properties are not reserved");
                assert_eq!(summary.kept_id, into);
                assert_eq!(summary.removed_id, from);
                assert_eq!(summary.edges_rewired, 1);
                let merged = match record {
                    Record::Entity => {
                        fixture
                            .runtime
                            .get_entity(&fixture.token, into)
                            .await
                            .unwrap()
                            .properties
                    }
                    Record::Note => {
                        fixture
                            .runtime
                            .notes(&fixture.token)
                            .unwrap()
                            .get_note(into)
                            .await
                            .unwrap()
                            .unwrap()
                            .properties
                    }
                }
                .unwrap();
                assert_eq!(merged["keep"], "ordinary");
                assert_eq!(merged["ordinary"], properties["ordinary"]);
                assert!(merged
                    .get(crate::secret_gate::RESERVED_SECRET_GATE_KEY)
                    .is_none());
                let edges = fixture
                    .runtime
                    .list_edges(
                        &fixture.token,
                        EdgeListFilter {
                            source_id: Some(into),
                            target_id: Some(anchor),
                            ..Default::default()
                        },
                        10,
                        0,
                    )
                    .await
                    .unwrap();
                assert_eq!(edges.len(), 1);
            }
        }
    }
}
