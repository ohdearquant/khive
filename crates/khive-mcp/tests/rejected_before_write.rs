//! Named handler refusals keep their own no-write disposition through real request dispatch.

use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_pack_comm::CommPack;
use khive_pack_kg::KgPack;
use khive_pack_memory::MemoryPack;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, VerbRegistryBuilder};
use khive_storage::SqlStatement;
use serde_json::{json, Value};
use serial_test::serial;
use uuid::Uuid;

const ACTOR: &str = "actor:prewrite-refusal";

struct Fixture {
    runtime: KhiveRuntime,
    server: KhiveMcpServer,
    thread: String,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            events_split: None,
            actor_id: Some(ACTOR.into()),
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let mut builder = VerbRegistryBuilder::new();
        builder.with_actor_id(Some(ACTOR.into()));
        builder.with_default_namespace("local");
        builder.register_trusted(KgPack::new(runtime.clone()));
        builder.register_trusted(CommPack::new(runtime.clone()));
        builder.register_trusted(MemoryPack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().unwrap();
        registry
            .apply_schema_plans_with_map(&Default::default(), runtime.backend())
            .unwrap();
        let mut fixture = Self {
            runtime,
            server: KhiveMcpServer::from_registry(registry),
            thread: String::new(),
        };
        let entity = fixture
            .success(
                "create",
                json!({"kind":"concept", "name":"stored baseline", "skip_dedup_check":true}),
            )
            .await;
        let source_id = Uuid::parse_str(entity["id"].as_str().unwrap()).unwrap();
        assert_eq!(entity["id"], source_id.to_string());
        fixture
            .success("memory.remember", json!({"content":"stored memory baseline", "namespace":"local", "source_id":entity["id"]}))
            .await;
        let sent = fixture
            .success(
                "comm.send",
                json!({"to":"actor:other", "content":"stored thread baseline"}),
            )
            .await;
        fixture.thread = sent["thread_id"].as_str().unwrap().to_owned();
        let baseline = fixture.rows().await;
        for table in ["entities", "notes", "graph_edges"] {
            assert!(
                !baseline[table].as_array().unwrap().is_empty(),
                "populated {table}"
            );
        }
        fixture
    }

    async fn request(&self, verb: &str, args: Value) -> Value {
        let output = self
            .server
            .dispatch_request_local(RequestParams {
                ops: json!([{"tool":verb, "args":args}]).to_string(),
                presentation: Some("verbose".into()),
                format: Some("json".into()),
                ..Default::default()
            })
            .await
            .expect("handler errors belong to the request envelope");
        serde_json::from_str(&output).unwrap()
    }

    async fn success(&self, verb: &str, args: Value) -> Value {
        let response = self.request(verb, args).await;
        assert_eq!(response["summary"]["succeeded"], 1, "{response}");
        assert_eq!(response["results"][0]["ok"], true, "{response}");
        response["results"][0]["result"].clone()
    }

    async fn rows(&self) -> Value {
        let mut reader = self.runtime.sql().reader().await.unwrap();
        let mut snapshot = serde_json::Map::new();
        for (table, sql) in [
            ("entities", "SELECT * FROM entities ORDER BY id"),
            ("notes", "SELECT * FROM notes ORDER BY id"),
            (
                "graph_edges",
                "SELECT * FROM graph_edges ORDER BY namespace, id",
            ),
        ] {
            let rows = reader
                .query_all(SqlStatement::new(sql, vec![]))
                .await
                .unwrap();
            snapshot.insert(table.into(), serde_json::to_value(rows).unwrap());
        }
        Value::Object(snapshot)
    }

    async fn refuse_unchanged(&self, verb: &str, args: Value, disposition: &str) -> Value {
        let before = self.rows().await;
        let response = self.request(verb, args).await;
        assert_eq!(self.rows().await, before, "{verb}: domain rows changed");
        assert_eq!(response["summary"]["failed"], 1, "{response}");
        let entry = &response["results"][0];
        assert_eq!(entry["ok"], false, "{response}");
        assert_eq!(
            entry["domain_disposition"], disposition,
            "{verb}: entry disposition"
        );
        assert_eq!(
            entry["error"]["domain_disposition"], disposition,
            "{verb}: error disposition"
        );
        assert_eq!(entry["error"]["kind"], "runtime_error");
        assert!(entry.get("result").is_none());
        assert!(entry["error"].get("domain_result").is_none());
        entry["error"].clone()
    }

    async fn stored_note(&self, id: &Value) -> khive_storage::Note {
        let id = id.as_str().unwrap().parse::<Uuid>().unwrap();
        let token = self.runtime.authorize(Namespace::local()).unwrap();
        self.runtime
            .notes(&token)
            .unwrap()
            .get_note(id)
            .await
            .unwrap()
            .unwrap()
    }
}

#[tokio::test]
#[serial(config_ledger)]
async fn comm_self_send_refusal_is_not_committed_and_preserves_stored_rows() {
    let fixture = Fixture::new().await;
    for (to, thread) in [
        (ACTOR.to_owned(), None),
        (format!(" {ACTOR} "), Some(&fixture.thread)),
    ] {
        let mut args = json!({"to":to,"content":"must not self-address"});
        if let Some(thread) = thread {
            args["thread_id"] = json!(thread);
        }
        let error = fixture
            .refuse_unchanged("comm.send", args, "not_committed")
            .await;
        let message = error["message"].as_str().unwrap();
        assert!(message.starts_with(&format!("invalid input: send: `to` ({ACTOR:?}) resolves to the sender's own actor identity ({ACTOR:?})")), "{message}");
        assert!(
            message.ends_with("resend with `self_send=true`."),
            "{message}"
        );
    }
    let accepted = fixture
        .success(
            "comm.send",
            json!({"to":ACTOR,"content":"intentional self-note","self_send":true}),
        )
        .await;
    let stored = fixture.stored_note(&accepted["full_id"]).await;
    assert_eq!(stored.content, "intentional self-note");
    assert_eq!(stored.properties.as_ref().unwrap()["from_actor"], ACTOR);
    assert_eq!(stored.properties.as_ref().unwrap()["to_actor"], ACTOR);
    fixture.server.shutdown_audit_batch().await.unwrap();
}

#[tokio::test]
#[serial(config_ledger)]
async fn remember_decode_refusals_are_not_committed_and_preserve_stored_rows() {
    let fixture = Fixture::new().await;
    for (args, expected) in [
        (
            json!({"content":"duplicate aliases", "key":"same", "idempotency_key":"same"}),
            "duplicate field `key`",
        ),
        (
            json!({"content":42}),
            "invalid type: integer `42`, expected a string",
        ),
        (json!({}), "missing field `content`"),
        (
            json!({"content":"unknown field", "unknown_parameter":true}),
            "unknown field `unknown_parameter`",
        ),
    ] {
        let error = fixture
            .refuse_unchanged("memory.remember", args, "not_committed")
            .await;
        let message = error["message"].as_str().unwrap();
        assert!(
            message.starts_with(&format!("invalid input: {expected}")),
            "{message}"
        );
    }
    for alias in ["key", "idempotency_key"] {
        let mut args = json!({"content":format!("accepted {alias}"), "namespace":"local"});
        args[alias] = json!(format!("accepted-{alias}"));
        let accepted = fixture.success("memory.remember", args).await;
        assert_eq!(
            fixture.stored_note(&accepted["id"]).await.content,
            format!("accepted {alias}")
        );
    }
    fixture.server.shutdown_audit_batch().await.unwrap();
}

#[tokio::test]
#[serial(config_ledger)]
async fn ordinary_handler_validation_remains_unknown_with_unchanged_stored_rows() {
    let fixture = Fixture::new().await;
    for (verb, args, message) in [
        (
            "comm.send",
            json!({"to":ACTOR,"content":" "}),
            "send: `content` must not be empty",
        ),
        (
            "memory.remember",
            json!({"content":" "}),
            "content must not be empty",
        ),
        (
            "memory.remember",
            json!({"content":"downstream salience", "salience":2.0}),
            "salience must be in [0, 1], got 2",
        ),
    ] {
        let error = fixture.refuse_unchanged(verb, args, "unknown").await;
        assert_eq!(error["message"], format!("invalid input: {message}"));
    }
    fixture.server.shutdown_audit_batch().await.unwrap();
}
