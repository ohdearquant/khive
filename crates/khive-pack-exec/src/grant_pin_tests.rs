use super::*;
use khive_pack_blob::BlobPack;
use khive_pack_kg::KgPack;
use khive_pack_tool::ToolPack;
use khive_runtime::{RuntimeConfig, VerbRegistry, VerbRegistryBuilder};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_types::Namespace;

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
    token: NamespaceToken,
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("exec-root");
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            actor_id: None,
            exec: khive_runtime::engine_config::ExecSectionConfig {
                root: Some(root.to_string_lossy().into_owned()),
                ..Default::default()
            },
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        let token = runtime
            .authorize(Namespace::parse("local").unwrap())
            .unwrap();
        assert!(token.actor().is_anonymous());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(BlobPack::new(runtime.clone()));
        builder.register(ToolPack::new(runtime.clone()));
        builder.register(crate::ExecPack::new(runtime.clone()));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        Self {
            runtime,
            registry,
            token,
            _dir: dir,
            root,
        }
    }

    async fn call(&self, verb: &str, args: Value) -> Value {
        self.registry
            .dispatch(verb, args)
            .await
            .unwrap_or_else(|error| panic!("{verb}: {error}"))
    }

    async fn register_and_grant(&self) -> (khive_storage::Entity, Value) {
        self.call(
            "tool.register",
            json!({
                "name":"pin-shell", "kind":"tool", "source":"exec:/bin/sh",
                "side_effect":"write", "trust":"first_party",
                "schema":{"z":{"type":"string"}, "a":[{"y":2,"x":1},true,null]}
            }),
        )
        .await;
        let request = self
            .call(
                "tool.request",
                json!({"tool":"pin-shell", "actor":"agent:pin"}),
            )
            .await;
        let grant = self
            .call("tool.grant", json!({"id":request["request_id"]}))
            .await;
        let entity = khive_pack_tool::resolve_registered(&self.runtime, &self.token, "pin-shell")
            .await
            .unwrap();
        (entity, grant["grant"].clone())
    }

    async fn run_to_preflight_receipt(&self, expected_source: &str) -> Value {
        let error = self
            .registry
            .dispatch(
                "exec.run",
                json!({
                    "tool":"pin-shell", "actor":"agent:pin", "tree":"intentionally-invalid-tree"
                }),
            )
            .await
            .unwrap_err()
            .to_string();
        if expected_source == "grant" {
            assert!(
                error.contains("tree:"),
                "policy should allow before tree refusal: {error}"
            );
        } else {
            assert!(error.contains("= ask from default"), "{error}");
            assert!(
                !error.contains("tree:"),
                "pin mismatch must refuse before tree loading"
            );
        }
        let runs = self.call("exec.runs", json!({"actor":"agent:pin"})).await;
        let listed = &runs["runs"][0];
        let receipt = self.call("exec.receipt", json!({"id":listed["id"]})).await;
        assert_eq!(receipt["decision"]["source"], expected_source);
        assert!(!self.root.exists(), "preflight must not materialize a run");
        assert!(receipt.get("schema").is_none());
        assert!(receipt.get("definition_digest").is_none());
        assert!(receipt["decision"].get("definition_digest").is_none());
        receipt
    }

    async fn set_pin(&self, id: &Value, registry_id: Uuid, digest: &str) {
        let mut writer = self.runtime.sql().writer().await.unwrap();
        assert_eq!(writer.execute(SqlStatement {
            sql:"UPDATE tool_grants SET registry_id=?1, definition_digest=?2 WHERE namespace='local' AND id=?3".into(),
            params:vec![SqlValue::Text(registry_id.to_string()), SqlValue::Text(digest.into()), SqlValue::Text(id.as_str().unwrap().into())],
            label:Some("test_grant_pin_only_mutation".into()),
        }).await.unwrap(), 1);
    }
}

#[tokio::test]
async fn grant_and_exec_preflight_consume_identical_canonical_definition_bytes() {
    let f = Fixture::new();
    let (entity, grant) = f.register_and_grant().await;
    let receipt = f.run_to_preflight_receipt("grant").await;
    assert_eq!(receipt["decision"]["id"], grant["id"]);

    // Both helpers are used by the real consumers above, not by fixture setup.
    let granted = RegistryPin::from_entity(&entity).unwrap();
    let preflight = preflight_registry_pin(&entity).unwrap();
    assert_eq!(grant["registry_id"], entity.id.to_string());
    assert_eq!(grant["definition_digest"], granted.definition_digest());
    assert_eq!(granted.canonical_bytes(), preflight.canonical_bytes());
    assert_eq!(granted.canonical_bytes(), br#"{"schema":{"a":[{"x":1,"y":2},true,null],"z":{"type":"string"}},"side_effect":"write","source":"exec:/bin/sh","trust":"first_party"}"#);
    assert_eq!(granted.definition_digest(), preflight.definition_digest());

    // A structurally equal private pretty serializer disagrees as bytes.
    let private = serde_json::to_vec_pretty(&registry_policy_inputs(&entity)).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&private).unwrap(),
        registry_policy_inputs(&entity)
    );
    assert_ne!(granted.canonical_bytes(), private.as_slice());
    let private_pin = RegistryPin::from_canonical_bytes(entity.id, private);
    assert_ne!(granted.definition_digest(), private_pin.definition_digest());
}

#[tokio::test]
async fn exec_preflight_rejects_wrong_pin_and_recovers_without_receipt_changes() {
    let f = Fixture::new();
    let (entity, grant) = f.register_and_grant().await;
    let pin = RegistryPin::from_entity(&entity).unwrap();
    f.run_to_preflight_receipt("grant").await;
    f.set_pin(&grant["id"], Uuid::new_v4(), pin.definition_digest())
        .await;
    f.run_to_preflight_receipt("default").await;
    f.set_pin(&grant["id"], entity.id, pin.definition_digest())
        .await;
    f.run_to_preflight_receipt("grant").await;
    f.set_pin(&grant["id"], entity.id, "different-digest").await;
    f.run_to_preflight_receipt("default").await;
    let rows = f
        .call(
            "tool.requests",
            json!({"status":"granted", "actor":"agent:pin"}),
        )
        .await;
    assert_eq!(rows["requests"][0]["status"], "granted");
}

#[tokio::test]
async fn exec_preflight_validates_the_snapshot_that_selected_its_binary() {
    let f = Fixture::new();
    let (selected, grant) = f.register_and_grant().await;
    let mut replacement = selected.clone();
    replacement.id = Uuid::new_v4();
    replacement.properties.as_mut().unwrap()["source"] = json!("exec:/bin/echo");
    let replacement_pin = RegistryPin::from_entity(&replacement).unwrap();
    f.set_pin(
        &grant["id"],
        replacement.id,
        replacement_pin.definition_digest(),
    )
    .await;

    let decision = preflight_policy(&f.runtime, &f.token, "agent:pin", &selected)
        .await
        .unwrap();
    assert_eq!(tool_binary(&selected).unwrap(), "/bin/sh");
    assert_eq!(decision.source, "default");
    let decision = preflight_policy(&f.runtime, &f.token, "agent:pin", &replacement)
        .await
        .unwrap();
    assert_eq!(tool_binary(&replacement).unwrap(), "/bin/echo");
    assert_eq!(decision.source, "grant");
}
