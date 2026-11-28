use khive_pack_comm::CommPack;
use khive_pack_git::GitPack;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::note::NoteKeyAssignOutcome;
use khive_storage::types::{EdgeFilter, PageRequest};
use khive_storage::{EdgeRelation, Note};
use serde_json::{json, Value};
use uuid::Uuid;

#[path = "../../khive-runtime/tests/support/receipt_credentials.rs"]
mod receipt_credentials;

const ACTOR: &str = "fixture:grammar";
const SHA: &str = "4f7c2a901b8e6d53c04a9f72e15b6c830d2a7e94";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = receipt_credentials::with_receipt_credentials(
            KhiveRuntime::new(RuntimeConfig {
                db_path: None,
                events_split: None,
                actor_id: Some(ACTOR.into()),
                brain_profile: None,
                ..RuntimeConfig::no_embeddings()
            })
            .expect("private grammar runtime"),
        );
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.with_default_namespace("local");
        builder.register_trusted(KgPack::new(runtime.clone()));
        builder.register_trusted(GitPack::new(runtime.clone()));
        builder.register_trusted(CommPack::new(runtime.clone()));
        builder.register_trusted(MemoryPack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("generated grammar registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .expect("generated grammar schemas");
        Self { runtime, registry }
    }

    async fn dispatch(&self, verb: &str, params: Value) -> Value {
        let result = self.registry.dispatch(verb, params).await;
        assert!(
            result.is_ok(),
            "generated key runtime route must admit {verb}: {result:?}"
        );
        result.unwrap()
    }

    async fn note(&self, id: Uuid) -> Note {
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .expect("actual route stored its note")
    }

    async fn direct_assignment(&self, key: &str) {
        // This accessor returns the actual SQL store, without a runtime guard.
        let store = self
            .runtime
            .backend()
            .notes_for_namespace("grammar-direct")
            .unwrap();
        let note = Note::new("grammar-direct", "observation", "stored grammar witness")
            .with_name("persistent row")
            .with_properties(json!({"preserve": [1, "two", null]}));
        store.upsert_note(note.clone()).await.unwrap();
        let mut expected = store.get_note(note.id).await.unwrap().unwrap();
        let assigned = store
            .assign_note_key_if_unset(note.id, key, "grammar-direct")
            .await;
        assert!(
            matches!(assigned, Ok(NoteKeyAssignOutcome::Assigned)),
            "generated key direct assignment must admit its raw grammar: {assigned:?}"
        );
        expected.key = Some(key.into());
        expected.version += 1;
        assert_eq!(
            store.get_note(note.id).await.unwrap(),
            Some(expected.clone())
        );
        let found = store
            .get_live_notes_by_key("grammar-direct", key, Some("observation"))
            .await
            .unwrap();
        assert_eq!(
            found,
            vec![expected],
            "generated key must remain readable verbatim"
        );
    }

    async fn note_and_edge_snapshot(&self) -> Vec<(&'static str, usize, String)> {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut snapshot = Vec::new();
        for table in ["notes", "graph_edges"] {
            let rows = reader
                .query_all(khive_storage::SqlStatement {
                    sql: format!("SELECT * FROM {table} ORDER BY rowid"),
                    params: vec![],
                    label: None,
                })
                .await
                .unwrap();
            snapshot.push((table, rows.len(), format!("{rows:?}")));
        }
        snapshot
    }

    async fn one_relation(&self, source: Uuid, target: Uuid, relation: EdgeRelation) {
        let namespace = self.note(source).await.namespace;
        let token = self
            .runtime
            .authorize(Namespace::parse(&namespace).unwrap())
            .unwrap();
        let edges = self
            .runtime
            .graph(&token)
            .unwrap()
            .query_edges(
                EdgeFilter {
                    source_ids: vec![source],
                    target_ids: vec![target],
                    relations: vec![relation],
                    ..Default::default()
                },
                vec![],
                PageRequest {
                    limit: 10,
                    offset: 0,
                },
            )
            .await
            .unwrap()
            .items;
        assert_eq!(
            edges.len(),
            1,
            "generated runtime route retains its relationship"
        );
        assert_eq!(edges[0].source_id, source);
        assert_eq!(edges[0].target_id, target);
        assert_eq!(edges[0].relation, relation);
        assert_eq!(edges[0].namespace, namespace);
    }
}

fn id(value: &Value) -> Uuid {
    let text = value.as_str().expect("full route identifier");
    let id = Uuid::parse_str(text).expect("canonical UUID");
    assert_eq!(id.to_string(), text);
    id
}

#[tokio::test]
async fn generated_commit_key_is_admitted_by_store_and_ingest_create_route() {
    let fixture = Fixture::new();
    let key = format!("git.commit:{SHA}");
    fixture.direct_assignment(&key).await;
    // Commit ingestion submits this kind, key and SHA through KG create.
    let params = json!({
        "kind": "commit", "key": key, "content": "Document a stable interface",
        "properties": {"sha": SHA, "parents": []}, "embed": false,
    });
    let created = fixture.dispatch("create", params.clone()).await;
    let stored = fixture.note(id(&created["id"])).await;
    assert_eq!(stored.kind, "commit");
    assert_eq!(stored.key.as_deref(), Some(key.as_str()));
    assert_eq!(stored.properties.as_ref().unwrap()["sha"], SHA);
    let replay = fixture.dispatch("create", params).await;
    assert_eq!(replay["created"], false);
    assert_eq!(id(&replay["id"]), stored.id);
    assert_eq!(fixture.note(stored.id).await, stored);
}

#[tokio::test]
async fn generated_memory_create_key_is_admitted_by_store_and_batch_route() {
    let fixture = Fixture::new();
    let key = "field-survey:create:7";
    fixture.direct_assignment(key).await;
    let params = json!({
        "batch": "field-survey", "dry_run": false,
        "rows": [{"action": "create", "part": 7, "content": "Reed warblers nest by water",
                  "properties": {"who": "field observer"}}],
    });
    let result = fixture.dispatch("memory.apply_batch", params.clone()).await;
    assert_eq!(result["entries"][0]["outcome"], "written");
    let stored = fixture.note(id(&result["entries"][0]["new_id"])).await;
    assert_eq!(stored.kind, "memory");
    assert_eq!(stored.namespace, ACTOR);
    assert_eq!(stored.key.as_deref(), Some(key));
    let replay = fixture.dispatch("memory.apply_batch", params).await;
    assert_eq!(replay["entries"][0]["outcome"], "deduped");
    assert_eq!(id(&replay["entries"][0]["new_id"]), stored.id);
    assert_eq!(fixture.note(stored.id).await, stored);
}

#[tokio::test]
async fn generated_memory_supersede_key_is_admitted_by_store_and_batch_route() {
    let fixture = Fixture::new();
    let seed = fixture
        .dispatch(
            "memory.remember",
            json!({
                "namespace": ACTOR, "content": "The survey counted three nests",
            }),
        )
        .await;
    let old = id(&seed["id"]);
    let key = format!("field-survey:{old}:9");
    fixture.direct_assignment(&key).await;
    let params = json!({
        "batch": "field-survey", "dry_run": false,
        "rows": [{"action": "supersede", "id": old.to_string(), "part": 9,
                  "content": "The survey counted four nests", "properties": {"who": "field observer"}}],
    });
    let result = fixture.dispatch("memory.apply_batch", params.clone()).await;
    assert_eq!(result["entries"][0]["outcome"], "written");
    let stored = fixture.note(id(&result["entries"][0]["new_id"])).await;
    assert_eq!(stored.key.as_deref(), Some(key.as_str()));
    assert_eq!(stored.namespace, ACTOR);
    assert_eq!(
        stored.properties.as_ref().unwrap()["supersedes"],
        old.to_string()
    );
    fixture
        .one_relation(stored.id, old, EdgeRelation::Supersedes)
        .await;
    let replay = fixture.dispatch("memory.apply_batch", params).await;
    assert_eq!(replay["entries"][0]["outcome"], "deduped");
    assert_eq!(id(&replay["entries"][0]["new_id"]), stored.id);
    assert_eq!(fixture.note(stored.id).await, stored);
    fixture
        .one_relation(stored.id, old, EdgeRelation::Supersedes)
        .await;
}

#[tokio::test]
async fn generated_comm_key_is_admitted_by_store_and_message_pair_route() {
    let fixture = Fixture::new();
    let key = format!("comm-v1:{}", json!(["local", ACTOR, "field-report"]));
    fixture.direct_assignment(&key).await;
    let params = json!({
        "to": "fixture:reader", "subject": "Field report", "content": "The survey is complete",
        "idempotency_key": "field-report",
    });
    let sent = fixture.dispatch("comm.send", params.clone()).await;
    let outbound = fixture.note(id(&sent["full_id"])).await;
    let inbound = fixture.note(id(&sent["recipient_id"])).await;
    assert_eq!(outbound.key.as_deref(), Some(key.as_str()));
    assert_eq!(
        outbound.properties.as_ref().unwrap()["inbound_ref"],
        inbound.id.to_string()
    );
    assert_eq!(
        inbound.properties.as_ref().unwrap()["outbound_ref"],
        outbound.id.to_string()
    );
    let replay = fixture.dispatch("comm.send", params).await;
    assert_eq!(replay["replayed"], true);
    assert_eq!(id(&replay["full_id"]), outbound.id);
    assert_eq!(fixture.note(outbound.id).await, outbound);
    assert_eq!(fixture.note(inbound.id).await, inbound);
}

#[tokio::test]
async fn generated_outbound_diagnostic_key_is_admitted_by_store_and_runtime_route() {
    let fixture = Fixture::new();
    let sent = fixture
        .dispatch(
            "comm.send",
            json!({
                "to": "fixture:reader", "content": "A message with an unresolved external identity",
            }),
        )
        .await;
    let outbound = id(&sent["full_id"]);
    let key = format!("outbound-email-external-id-unverifiable:{outbound}");
    fixture.direct_assignment(&key).await;
    let token = fixture.runtime.authorize(Namespace::local()).unwrap();
    let diagnostic = fixture
        .runtime
        .record_outbound_external_id_diagnostic(&token, outbound, "external identifier unavailable")
        .await;
    assert!(
        diagnostic.is_ok(),
        "generated diagnostic runtime route must admit: {diagnostic:?}"
    );
    let diagnostic = diagnostic.unwrap();
    assert_eq!(diagnostic.key.as_deref(), Some(key.as_str()));
    assert_eq!(
        diagnostic.properties.as_ref().unwrap()["offending_message_id"],
        outbound.to_string()
    );
    fixture
        .one_relation(diagnostic.id, outbound, EdgeRelation::Annotates)
        .await;
    let before = fixture.note_and_edge_snapshot().await;
    let replay = fixture
        .runtime
        .record_outbound_external_id_diagnostic(&token, outbound, "external identifier unavailable")
        .await;
    let diagnostic_id = diagnostic.id.to_string();
    // Only the public KG create handler translates this internal replay signal.
    assert!(
        matches!(&replay, Err(khive_runtime::RuntimeError::Khive(error))
            if error.kind() == khive_types::ErrorKind::Conflict
                && error.details().is_some_and(|details|
                    details.get("reason") == Some("key_conflict")
                    && details.get("equal") == Some("true")
                    && details.get("key") == Some(key.as_str())
                    && details.get("existing_id") == Some(diagnostic_id.as_str()))),
        "the internal diagnostic duplicate must identify its exact equal holder: {replay:?}"
    );
    assert_eq!(
        fixture.note(diagnostic.id).await,
        diagnostic,
        "the duplicate diagnostic attempt must preserve the complete stored note"
    );
    assert_eq!(
        fixture.note_and_edge_snapshot().await,
        before,
        "the duplicate diagnostic attempt must preserve every note and edge row and count"
    );
    fixture
        .one_relation(diagnostic.id, outbound, EdgeRelation::Annotates)
        .await;
}
