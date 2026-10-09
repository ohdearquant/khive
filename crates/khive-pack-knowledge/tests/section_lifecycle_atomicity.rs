use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const CONTENT: &str = "dense sparse retrieval corpus benchmark search latency gradient descent transformer attention vector index nearest neighbor ranking fusion pipeline embedding rerank cosine similarity";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            default_namespace: khive_runtime::Namespace::local(),
            visible_namespaces: Vec::new(),
            allowed_outbound_namespaces: Vec::new(),
            embedding_model: None,
            additional_embedding_models: Vec::new(),
            wal_ceiling_bytes: 0,
            wal_ceiling_configured_bytes: 0,
            wal_ceiling_source: Default::default(),
            wal_ceiling_env_raw: None,
            disk_guard_environment: Default::default(),
            disk_guard_config: None,
            volume_lock_dir: None,
            credentials: Vec::new(),
            visibility_receipts: None,
            packs: vec!["kg".into(), "knowledge".into()],
            actor_id: None,
            brain_profile: None,
            brain: Default::default(),
            blob: Default::default(),
            mounts: Vec::new(),
            events_split: None,
            ..RuntimeConfig::no_embeddings()
        })
        .expect("private memory runtime");
        assert!(runtime.backend().pool().canonical_path().is_none());
        assert!(runtime.backend_data_dir().is_none());
        assert!(runtime.backend_ann_root().is_none());
        assert!(runtime.default_embedder_name().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some("section-lifecycle-fixture".into()));
        builder.with_default_namespace("local");
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().expect("registry");
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self { runtime, registry }
    }

    async fn dispatch(&self, verb: &str, args: Value) -> Result<Value, RuntimeError> {
        self.registry.dispatch(verb, args).await
    }

    async fn execute(&self, query: &str, params: Vec<SqlValue>) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute(SqlStatement::new(query, params))
            .await
            .unwrap();
    }

    async fn script(&self, script: String) {
        self.runtime
            .sql()
            .writer()
            .await
            .unwrap()
            .execute_script(script)
            .await
            .unwrap();
    }

    async fn seed(&self, slug: &str) -> String {
        self.dispatch(
            "knowledge.upsert_atoms",
            json!({"atoms": [{
                "slug": slug, "name": slug, "content": CONTENT,
                "properties": {"untouched": "preserve me", "dispute_count": 0}
            }]}),
        )
        .await
        .unwrap();
        self.dispatch(
            "knowledge.edit",
            json!({"id": slug, "sections": [
                {"section_type": "overview", "content": CONTENT},
                {"section_type": "formalism", "content": format!("Independent section. {CONTENT}")}
            ]}),
        )
        .await
        .unwrap();
        self.atom(slug).await["id"].as_str().unwrap().to_owned()
    }

    async fn atom(&self, reference: &str) -> Value {
        self.dispatch(
            "knowledge.get",
            json!({"id": reference, "include_sections": true}),
        )
        .await
        .unwrap()
    }

    async fn snapshot(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut rows = Vec::new();
        for query in [
            "SELECT * FROM knowledge_atoms ORDER BY id",
            "SELECT * FROM knowledge_sections ORDER BY id",
            "SELECT * FROM fts_knowledge_data ORDER BY id",
            "SELECT * FROM fts_knowledge_idx ORDER BY segid, term",
            "SELECT * FROM fts_sections_data ORDER BY id",
            "SELECT * FROM fts_sections_idx ORDER BY segid, term",
        ] {
            rows.push(
                serde_json::to_value(
                    reader
                        .query_all(SqlStatement::new(query, vec![]))
                        .await
                        .unwrap(),
                )
                .unwrap(),
            );
        }
        json!(rows)
    }
}

#[derive(Clone, Copy, Debug)]
enum Transition {
    Challenge,
    Accept,
    Reject,
}

impl Transition {
    fn verb(self) -> &'static str {
        match self {
            Self::Challenge => "knowledge.challenge",
            _ => "knowledge.adjudicate",
        }
    }

    fn args(self, id: &str) -> Value {
        let mut args = json!({"atom_id": id, "section_type": "overview"});
        match self {
            Self::Challenge => (),
            Self::Accept => args["resolution"] = json!("accept"),
            Self::Reject => args["resolution"] = json!("reject"),
        }
        args
    }

    fn zero_message(self) -> &'static str {
        match self {
            Self::Challenge => "section not found, already disputed, or deprecated",
            _ => "section not found or not in disputed state",
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    CounterAbort,
    CounterIgnore,
    StatusAbort,
    StatusIgnore,
}

async fn check_transition(transition: Transition, fault: Fault) {
    let f = Fixture::new();
    let id = f.seed("lifecycle-target").await;
    let foreign_id = f.seed("foreign-sentinel").await;
    f.execute(
        "UPDATE knowledge_atoms SET namespace=?1 WHERE id=?2",
        vec![
            SqlValue::Text("foreign".into()),
            SqlValue::Text(foreign_id.clone()),
        ],
    )
    .await;
    f.execute(
        "UPDATE knowledge_sections SET namespace=?1 WHERE atom_id=?2",
        vec![
            SqlValue::Text("foreign".into()),
            SqlValue::Text(foreign_id.clone()),
        ],
    )
    .await;
    let foreign_before = f.atom(&foreign_id).await;
    assert_eq!(foreign_before["namespace"], "foreign");
    if !matches!(transition, Transition::Challenge) {
        let result = f
            .dispatch("knowledge.challenge", Transition::Challenge.args(&id))
            .await
            .unwrap();
        assert_eq!(result["disputed"], 1);
        assert_eq!(f.atom(&id).await["properties"]["dispute_count"], 1);
    }
    let before = f.snapshot().await;
    // The abort also proves the zero-row status path skips the counter write.
    if let Some(action) = match fault {
        Fault::CounterAbort | Fault::StatusIgnore => Some("RAISE(ABORT, 'counter-write-rejected')"),
        Fault::CounterIgnore => Some("RAISE(IGNORE)"),
        Fault::StatusAbort => None,
    } {
        f.script(format!("CREATE TRIGGER fail_counter BEFORE UPDATE OF properties ON knowledge_atoms WHEN OLD.id='{id}' BEGIN SELECT {action}; END;")).await;
    }
    if let Some(action) = match fault {
        Fault::StatusAbort => Some("RAISE(ABORT, 'status-write-rejected')"),
        Fault::StatusIgnore => Some("RAISE(IGNORE)"),
        Fault::CounterAbort | Fault::CounterIgnore => None,
    } {
        f.script(format!("CREATE TRIGGER fail_status BEFORE UPDATE OF status ON knowledge_sections WHEN OLD.atom_id='{id}' BEGIN SELECT {action}; END;")).await;
    }
    let error = f
        .dispatch(transition.verb(), transition.args(&id))
        .await
        .expect_err("injected refusal");
    match fault {
        Fault::StatusIgnore => assert!(
            matches!(error, RuntimeError::InvalidInput(ref message) if message == transition.zero_message()),
            "{error:?}"
        ),
        Fault::CounterIgnore => assert!(
            error
                .to_string()
                .contains("expected one parent atom, updated 0"),
            "{error}"
        ),
        Fault::CounterAbort => assert!(
            error.to_string().contains("counter-write-rejected"),
            "{error}"
        ),
        Fault::StatusAbort => assert!(
            error.to_string().contains("status-write-rejected"),
            "{error}"
        ),
    }
    assert_eq!(
        f.snapshot().await,
        before,
        "{transition:?}/{fault:?} must preserve both records and FTS"
    );
    f.script("DROP TRIGGER IF EXISTS fail_counter; DROP TRIGGER IF EXISTS fail_status;".into())
        .await;
    let result = f
        .dispatch(transition.verb(), transition.args(&id))
        .await
        .expect("retry after rollback");
    let (count_key, status, count) = match transition {
        Transition::Challenge => ("disputed", "disputed", 1),
        Transition::Accept => ("resolved", "verified", 0),
        Transition::Reject => ("resolved", "reviewed", 0),
    };
    assert_eq!(
        result[count_key], 1,
        "response counts the section, not both SQL writes"
    );
    let after = f.atom(&id).await;
    assert_eq!(
        after["properties"],
        json!({"untouched": "preserve me", "dispute_count": count})
    );
    let section = after["sections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["section_type"] == "overview")
        .unwrap();
    assert_eq!(section["status"], status);
    assert_eq!(f.atom(&foreign_id).await, foreign_before);
    let committed = f.snapshot().await;
    let duplicate = f
        .dispatch(transition.verb(), transition.args(&id))
        .await
        .expect_err("repeat keeps lifecycle conflict");
    assert!(
        matches!(duplicate, RuntimeError::InvalidInput(ref message) if message == transition.zero_message()),
        "{duplicate:?}"
    );
    assert_eq!(f.snapshot().await, committed);
}

#[tokio::test]
async fn counter_failure_rolls_back_status_and_allows_retry() {
    for transition in [
        Transition::Challenge,
        Transition::Accept,
        Transition::Reject,
    ] {
        check_transition(transition, Fault::CounterAbort).await;
    }
}

#[tokio::test]
async fn status_failure_preserves_counter_and_allows_retry() {
    for transition in [
        Transition::Challenge,
        Transition::Accept,
        Transition::Reject,
    ] {
        check_transition(transition, Fault::StatusAbort).await;
    }
}

#[tokio::test]
async fn zero_status_changes_skip_counter_and_preserve_conflict() {
    for transition in [
        Transition::Challenge,
        Transition::Accept,
        Transition::Reject,
    ] {
        check_transition(transition, Fault::StatusIgnore).await;
    }
}

#[tokio::test]
async fn zero_counter_changes_roll_back_status_and_allow_retry() {
    for transition in [
        Transition::Challenge,
        Transition::Accept,
        Transition::Reject,
    ] {
        check_transition(transition, Fault::CounterIgnore).await;
    }
}
