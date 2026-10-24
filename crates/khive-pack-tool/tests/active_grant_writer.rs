use std::any::Any;

use khive_pack_kg::KgPack;
use khive_pack_tool::{
    policy::{active_grant_on, decide},
    RegistryPin, ToolPack,
};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use khive_storage::{SqlStatement, SqlValue};
use serde_json::{json, Value};

const NOW: i64 = 100;
const ACTOR: &str = "agent:requester";
const TOOL: &str = "unregistered";

fn statement(sql: &str, params: Vec<SqlValue>) -> SqlStatement {
    SqlStatement {
        sql: sql.into(),
        params,
        label: None,
    }
}

fn text(value: &str) -> SqlValue {
    SqlValue::Text(value.to_string())
}

struct Fixture {
    rt: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    fn new() -> Self {
        let rt = KhiveRuntime::memory().expect("private memory runtime");
        assert!(rt.sql().database_path().is_none());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(rt.clone()));
        builder.register(ToolPack::new(rt.clone()));
        let registry = builder.build().expect("tool registry");
        registry
            .apply_schema_plans_with_map(&Default::default(), rt.backend())
            .expect("actual tool schema");
        rt.install_edge_rules(registry.all_edge_rules());
        Self { rt, registry }
    }

    async fn write(&self, sql: &str, params: Vec<SqlValue>) -> u64 {
        self.rt
            .sql()
            .writer()
            .await
            .expect("fixture writer")
            .execute(statement(sql, params))
            .await
            .expect("fixture DML")
    }

    async fn seed(&self, id: &str, namespace: &str, actor: &str, tool: &str, requested_at: i64) {
        assert_eq!(self.write(
            "INSERT INTO tool_grants (id, namespace, actor, tool, status, requested_at) VALUES (?1, ?2, ?3, ?4, 'granted', ?5)",
            vec![text(id), text(namespace), text(actor), text(tool), SqlValue::Integer(requested_at)],
        ).await, 1);
    }

    async fn on(
        &self,
        namespace: &str,
        actor: &str,
        tool: &str,
        now: i64,
        pin: Option<&RegistryPin>,
    ) -> Result<Option<String>, RuntimeError> {
        let mut writer = self
            .rt
            .sql()
            .writer()
            .await
            .expect("caller-supplied writer");
        active_grant_on(writer.as_mut(), namespace, actor, tool, now, pin).await
    }

    async fn expect_selection(
        &self,
        namespace: &str,
        actor: &str,
        tool: &str,
        pin: Option<&RegistryPin>,
        expected: Option<&str>,
    ) {
        let selected = self
            .on(namespace, actor, tool, NOW, pin)
            .await
            .expect("valid writer grant row");
        assert_eq!(
            selected.as_deref(),
            expected,
            "independent writer expectation for {namespace}/{actor}/{tool}"
        );
        let decision = decide(&self.rt, namespace, actor, tool, None, pin)
            .await
            .expect("valid reader grant row");
        assert_eq!(
            decision.grant_id.as_deref(),
            expected,
            "independent decide expectation"
        );
        assert_eq!(selected, decision.grant_id, "reader/writer grant parity");
        assert_eq!(
            decision.source,
            if expected.is_some() {
                "grant"
            } else {
                "default"
            }
        );
        assert_eq!(
            decision.decision,
            if expected.is_some() { "allow" } else { "ask" }
        );
    }

    async fn registration(&self, name: &str) -> RegistryPin {
        let registered = self.registry.dispatch("tool.register", json!({
            "name": name, "source": "mcp:fixture", "side_effect": "write", "trust": "first_party",
            "schema": {"type": "object", "properties": {"value": {"type": "string"}}},
        })).await.expect("real registry object");
        let id = registered["tool"]["full_id"]
            .as_str()
            .expect("registered id")
            .parse()
            .expect("registered UUID");
        let token = self.rt.authorize(Namespace::local()).expect("local token");
        let entity = self
            .rt
            .get_entity(&token, id)
            .await
            .expect("actual registry entity");
        RegistryPin::from_entity(&entity).expect("actual registration pin")
    }

    async fn repin(&self, id: &str, pin: Option<&RegistryPin>) {
        let (registry_id, digest) = pin.map_or((SqlValue::Null, SqlValue::Null), |pin| {
            (
                text(&pin.registry_id().to_string()),
                text(pin.definition_digest()),
            )
        });
        assert_eq!(self.write(
            "UPDATE tool_grants SET registry_id=?1, definition_digest=?2, invalidated_by_registry_id=NULL, invalidated_at=NULL WHERE id=?3",
            vec![registry_id, digest, text(id)],
        ).await, 1);
    }

    async fn row(&self, id: &str) -> Value {
        let listed = self
            .registry
            .dispatch("tool.requests", json!({}))
            .await
            .expect("stored grants");
        listed["requests"]
            .as_array()
            .expect("grant rows")
            .iter()
            .find(|row| row["id"] == id)
            .expect("actual grant remains present")
            .clone()
    }

    async fn storage_class(&self, column: &str, id: &str) -> String {
        let mut reader = self.rt.sql().reader().await.expect("storage-class reader");
        let row = reader
            .query_row(statement(
                &format!("SELECT typeof({column}) AS storage_class FROM tool_grants WHERE id=?1"),
                vec![text(id)],
            ))
            .await
            .expect("storage-class query")
            .expect("selected row");
        row.text("storage_class")
            .expect("actual storage class")
            .to_string()
    }
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn writer_grant_status_expiry_and_invalidation_match_decide() {
    let f = Fixture::new();
    let invalidator = f.registration("unrelated-registry-object").await;
    f.seed("state-grant", "local", ACTOR, TOOL, 1).await;
    f.expect_selection("local", ACTOR, TOOL, None, Some("state-grant"))
        .await;
    for (sql, params, expected) in [
        (
            "UPDATE tool_grants SET status='revoked' WHERE id='state-grant'",
            vec![],
            None,
        ),
        (
            "UPDATE tool_grants SET expires_at=1 WHERE id='state-grant'",
            vec![],
            None,
        ),
        (
            "UPDATE tool_grants SET expires_at=?1 WHERE id='state-grant'",
            vec![SqlValue::Integer(i64::MAX)],
            Some("state-grant"),
        ),
        (
            "UPDATE tool_grants SET invalidated_by_registry_id=?1 WHERE id='state-grant'",
            vec![text(&invalidator.registry_id().to_string())],
            None,
        ),
        (
            "UPDATE tool_grants SET invalidated_at=1 WHERE id='state-grant'",
            vec![],
            None,
        ),
    ] {
        f.write("UPDATE tool_grants SET status='granted', expires_at=NULL, invalidated_by_registry_id=NULL, invalidated_at=NULL WHERE id='state-grant'", vec![]).await;
        assert_eq!(f.write(sql, params).await, 1);
        f.expect_selection("local", ACTOR, TOOL, None, expected)
            .await;
    }
    f.write("UPDATE tool_grants SET status='granted', expires_at=100, invalidated_by_registry_id=NULL, invalidated_at=NULL WHERE id='state-grant'", vec![]).await;
    assert_eq!(
        f.on("local", ACTOR, TOOL, 99, None)
            .await
            .unwrap()
            .as_deref(),
        Some("state-grant")
    );
    assert_eq!(
        f.on("local", ACTOR, TOOL, 100, None).await.unwrap(),
        None,
        "expiry equality is not active"
    );
    assert_eq!(f.on("local", ACTOR, TOOL, 101, None).await.unwrap(), None);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn writer_grant_matches_both_pin_fields_and_unregistered_state() {
    let f = Fixture::new();
    f.seed("pinned-grant", "local", ACTOR, TOOL, 1).await;
    f.expect_selection("local", ACTOR, TOOL, None, Some("pinned-grant"))
        .await;
    let pin = f.registration(TOOL).await;
    let invalidated = f.row("pinned-grant").await;
    assert_eq!(
        invalidated["invalidated_by_registry_id"],
        pin.registry_id().to_string()
    );
    assert!(invalidated["invalidated_at"].is_string());
    f.expect_selection("local", ACTOR, TOOL, Some(&pin), None)
        .await;
    f.expect_selection("local", ACTOR, TOOL, None, None).await;

    f.repin("pinned-grant", Some(&pin)).await;
    f.expect_selection("local", ACTOR, TOOL, Some(&pin), Some("pinned-grant"))
        .await;
    f.expect_selection("local", ACTOR, TOOL, None, None).await;
    let changed = RegistryPin::from_canonical_bytes(
        pin.registry_id(),
        khive_types::canonical_json_bytes(&json!({
            "source": "mcp:replacement", "side_effect": "write", "trust": "first_party",
            "schema": {"type": "object", "properties": {"value": {"type": "string"}}},
        }))
        .expect("canonical policy inputs"),
    );
    assert_eq!(changed.registry_id(), pin.registry_id());
    assert_ne!(changed.definition_digest(), pin.definition_digest());
    f.expect_selection("local", ACTOR, TOOL, Some(&changed), None)
        .await;
    let other = f.registration("other-registry-object").await;
    assert_ne!(other.registry_id(), pin.registry_id());
    assert_eq!(other.definition_digest(), pin.definition_digest());
    f.expect_selection("local", ACTOR, TOOL, Some(&other), None)
        .await;

    f.repin("pinned-grant", Some(&changed)).await;
    f.expect_selection("local", ACTOR, TOOL, Some(&changed), Some("pinned-grant"))
        .await;
    f.expect_selection("local", ACTOR, TOOL, Some(&pin), None)
        .await;
    let changed_id =
        RegistryPin::from_canonical_bytes(other.registry_id(), changed.canonical_bytes().to_vec());
    f.repin("pinned-grant", Some(&changed_id)).await;
    f.expect_selection(
        "local",
        ACTOR,
        TOOL,
        Some(&changed_id),
        Some("pinned-grant"),
    )
    .await;
    f.expect_selection("local", ACTOR, TOOL, Some(&changed), None)
        .await;

    for (registry_id, digest) in [
        (text(&pin.registry_id().to_string()), SqlValue::Null),
        (SqlValue::Null, text(pin.definition_digest())),
    ] {
        f.write(
            "UPDATE tool_grants SET registry_id=?1, definition_digest=?2 WHERE id='pinned-grant'",
            vec![registry_id, digest],
        )
        .await;
        f.expect_selection("local", ACTOR, TOOL, None, None).await;
        f.expect_selection("local", ACTOR, TOOL, Some(&pin), None)
            .await;
    }
    f.repin("pinned-grant", None).await;
    f.expect_selection("local", ACTOR, TOOL, None, Some("pinned-grant"))
        .await;
    f.expect_selection("local", ACTOR, TOOL, Some(&pin), None)
        .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn writer_grant_preserves_scope_patterns_and_stable_selection() {
    let f = Fixture::new();
    for (id, namespace, actor, tool, requested) in [
        ("old", "local", ACTOR, "alpha", 10),
        ("z-tie", "local", ACTOR, "alpha", 20),
        ("a-tie", "local", ACTOR, "alpha", 20),
        ("foreign-new", "other", "*", "*", 100),
        ("actor-new", "local", "agent:other", "alpha", 100),
        ("tool-new", "local", ACTOR, "beta", 100),
        ("wild-new", "local", "agent:*", "alp*", 30),
    ] {
        f.seed(id, namespace, actor, tool, requested).await;
    }
    f.expect_selection("local", ACTOR, "alpha", None, Some("wild-new"))
        .await;
    f.write(
        "UPDATE tool_grants SET status='revoked' WHERE id='wild-new'",
        vec![],
    )
    .await;
    for _ in 0..3 {
        f.expect_selection("local", ACTOR, "alpha", None, Some("a-tie"))
            .await;
    }
    f.expect_selection("other", ACTOR, "alpha", None, Some("foreign-new"))
        .await;
    f.expect_selection("local", "agent:other", "alpha", None, Some("actor-new"))
        .await;
    f.expect_selection("local", ACTOR, "beta", None, Some("tool-new"))
        .await;
    f.expect_selection("missing", ACTOR, "alpha", None, None)
        .await;
    f.expect_selection("local", "agent:REQUESTER", "alpha", None, None)
        .await;
    f.expect_selection("local", ACTOR, "ALPHA", None, None)
        .await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn writer_grant_refuses_a_selected_malformed_row() {
    let f = Fixture::new();
    f.seed("decode-grant", "local", ACTOR, TOOL, 1).await;
    f.expect_selection("local", ACTOR, TOOL, None, Some("decode-grant"))
        .await;
    for column in ["expires_at", "requested_at"] {
        f.write(
            &format!("UPDATE tool_grants SET {column}=?1 WHERE id='decode-grant'"),
            vec![text("not-a-time")],
        )
        .await;
        assert_eq!(f.storage_class(column, "decode-grant").await, "text");
        let writer_error = f
            .on("local", ACTOR, TOOL, NOW, None)
            .await
            .expect_err("selected malformed writer row must refuse");
        let reader_error = decide(&f.rt, "local", ACTOR, TOOL, Some("read"), None)
            .await
            .expect_err("selected malformed reader row must not become default allow");
        for error in [writer_error, reader_error] {
            let message = match error {
                RuntimeError::Internal(message) => message,
                other => panic!("strict grant decode must return Internal, got {other:?}"),
            };
            assert!(
                message.contains("invalid tool_grants row decode-grant"),
                "{message}"
            );
            assert!(
                message.contains(&format!("SQL column {column}")),
                "{message}"
            );
        }
        let restored = if column == "expires_at" {
            SqlValue::Null
        } else {
            SqlValue::Integer(1)
        };
        f.write(
            &format!("UPDATE tool_grants SET {column}=?1 WHERE id='decode-grant'"),
            vec![restored],
        )
        .await;
        f.expect_selection("local", ACTOR, TOOL, None, Some("decode-grant"))
            .await;
    }
}

async fn check_and_consume(f: &Fixture, marker: &str) -> (Option<String>, u64) {
    let marker = marker.to_string();
    let result = f.rt.sql().atomic_unit(Box::new(move |writer| Box::pin(async move {
        let outcome: Result<(Option<String>, u64), RuntimeError> = async {
            let grant = active_grant_on(writer, "local", ACTOR, TOOL, NOW, None).await?;
            let changed = if let Some(id) = grant.as_ref() {
                writer.execute(statement("UPDATE tool_grants SET decision_note=?1 WHERE namespace='local' AND id=?2", vec![text(&marker), text(id)])).await?
            } else { 0 };
            Ok((grant, changed))
        }.await;
        Ok(Box::new(outcome) as Box<dyn Any + Send>)
    }))).await.expect("check-and-consume atomic unit");
    let outcome = result
        .downcast::<Result<(Option<String>, u64), RuntimeError>>()
        .unwrap_or_else(|_| panic!("typed check-and-consume outcome"));
    (*outcome).expect("grant check inside the unit")
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn atomic_grant_check_consumes_only_while_the_grant_is_live() {
    let f = Fixture::new();
    f.seed("atomic-grant", "local", ACTOR, TOOL, 1).await;
    f.expect_selection("local", ACTOR, TOOL, None, Some("atomic-grant"))
        .await;
    let initial = f.row("atomic-grant").await;
    assert_eq!(initial["status"], "granted");
    assert!(initial["decision_note"].is_null());
    let consumed = check_and_consume(&f, "healthy-consumed").await;
    assert_eq!(consumed, (Some("atomic-grant".to_string()), 1));
    let healthy = f.row("atomic-grant").await;
    assert_eq!(healthy["status"], "granted");
    assert_eq!(healthy["decision_note"], "healthy-consumed");

    let revoked = f.rt.sql().atomic_unit(Box::new(|writer| Box::pin(async move {
        let changed = writer.execute(statement("UPDATE tool_grants SET status='revoked' WHERE namespace='local' AND id='atomic-grant'", vec![])).await?;
        Ok(Box::new(changed) as Box<dyn Any + Send>)
    }))).await.expect("prior revocation commits");
    assert_eq!(
        *revoked
            .downcast::<u64>()
            .unwrap_or_else(|_| panic!("typed revoke count")),
        1
    );
    let before_denied = f.row("atomic-grant").await;
    assert_eq!(before_denied["status"], "revoked");
    assert_eq!(before_denied["decision_note"], "healthy-consumed");
    assert_eq!(check_and_consume(&f, "must-not-consume").await, (None, 0));
    assert_eq!(f.row("atomic-grant").await, before_denied);
    f.expect_selection("local", ACTOR, TOOL, None, None).await;
}
