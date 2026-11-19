//! Public list refusals mask credentials without changing page or projection semantics.
use khive_pack_kg::KgPack;
use khive_pack_knowledge::KnowledgePack;
use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{
    runtime_error_value, DomainDisposition, KhiveRuntime, RuntimeConfig, RuntimeError,
    VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};
use uuid::Uuid;

const CONTENT: &str = "A private corpus fixture contains enough ordinary words to exercise real atom and domain writes while keeping pagination and error handling independent from embedding models.";
const ATOM_FIELDS: &str = "id, namespace, slug, name, content, tags, properties, status, source_uri, source_type, finalized, kind, created_at, updated_at";
const DOMAIN_FIELDS: &str =
    "id, namespace, slug, name, description, tags, members, kind, created_at, updated_at";

struct Fixture {
    runtime: KhiveRuntime,
    registry: VerbRegistry,
}

impl Fixture {
    async fn new() -> Self {
        let runtime = KhiveRuntime::new(RuntimeConfig {
            db_path: None,
            events_split: None,
            packs: vec!["kg".into(), "knowledge".into()],
            brain_profile: None,
            ..RuntimeConfig::no_embeddings()
        })
        .unwrap();
        assert!(!runtime.backend().is_file_backed());
        assert!(runtime.registered_embedding_model_names().is_empty());
        let mut builder = VerbRegistryBuilder::new();
        builder.register(KgPack::new(runtime.clone()));
        builder.register(KnowledgePack::new_with_index_role(runtime.clone(), false));
        let registry = builder.build().unwrap();
        registry.apply_schema_plans(runtime.backend());
        runtime.install_edge_rules(registry.all_edge_rules());
        for (verb, field, content_field, prefix) in [
            ("knowledge.upsert_atoms", "atoms", "content", "atom"),
            (
                "knowledge.upsert_domains",
                "domains",
                "description",
                "domain",
            ),
        ] {
            let rows: Vec<Value> = (0..2)
                .map(|index| {
                    let mut row = json!({
                        "slug": format!("list-{prefix}-{index}"),
                        "name": format!("List {prefix} {index}"),
                    });
                    row[content_field] = json!(CONTENT);
                    row
                })
                .collect();
            let mut params = json!({});
            params[field] = json!(rows);
            let response = registry.dispatch(verb, params).await.unwrap();
            assert_eq!(response["created"], 2, "{verb}: {response}");
        }
        Self { runtime, registry }
    }

    async fn list(&self, params: Value) -> Value {
        self.registry
            .dispatch("knowledge.list", params)
            .await
            .unwrap()
    }

    async fn snapshot(&self) -> Vec<Value> {
        let mut snapshot = Vec::new();
        for kind in ["atom", "domain"] {
            let response = self.list(json!({"type": kind, "limit": 20})).await;
            assert_eq!(response["total"], 2);
            assert_eq!(response["results"].as_array().unwrap().len(), 2);
            snapshot.push(response);
        }
        snapshot
    }

    async fn refuse(&self, params: Value) -> String {
        let before = self.snapshot().await;
        let acquisitions = self
            .runtime
            .backend()
            .pool()
            .reader_acquisition_snapshot()
            .acquisitions;
        let error = self
            .registry
            .dispatch("knowledge.list", params)
            .await
            .unwrap_err();
        assert_eq!(
            self.runtime
                .backend()
                .pool()
                .reader_acquisition_snapshot()
                .acquisitions,
            acquisitions,
            "parameter refusal must precede list reader acquisition"
        );
        assert_eq!(self.snapshot().await, before, "refusal changed stored rows");
        let RuntimeError::InvalidInput(message) = &error else {
            panic!("expected InvalidInput, got {error:?}");
        };
        let message = message.clone();
        let projection = runtime_error_value(error, DomainDisposition::Unknown);
        assert_eq!(projection["message"], format!("invalid input: {message}"));
        message
    }
}

fn credentials() -> [String; 2] {
    [
        format!("AKIA{}", "A1".repeat(8)),
        format!("ghp_{}", "xY7z".repeat(9)),
    ]
}

fn masked(message: &str, secret: &str, original: &str) {
    assert!(
        !message.contains(secret),
        "credential escaped knowledge.list refusal: {message}"
    );
    assert_eq!(
        message,
        mask_for_redaction_surface(RedactionSurface::GateProbe, original)
    );
}

fn projection_message(kind: &str, field: &str) -> String {
    let fields = if kind == "atom" {
        ATOM_FIELDS
    } else {
        DOMAIN_FIELDS
    };
    format!("knowledge.list: unknown {kind} projection field {field:?}; expected one of: {fields}")
}

fn cursor_message(raw: &str) -> String {
    let error = Uuid::parse_str(raw).unwrap_err();
    format!("knowledge.list: `after` must be a full UUID returned as `next_after`; prefixes are unsafe for keyset pagination; got {raw:?}: {error}")
}

#[tokio::test]
async fn list_type_refusals_mask_credentials() {
    let f = Fixture::new().await;
    for key in ["type", "kind"] {
        let message = f.refuse(json!({key: "memo"})).await;
        assert_eq!(message, "unknown type \"memo\"; valid: atom | domain");
        for secret in credentials() {
            let message = f.refuse(json!({key: secret.clone()})).await;
            masked(
                &message,
                &secret,
                &format!("unknown type {secret:?}; valid: atom | domain"),
            );
        }
    }
}

#[tokio::test]
async fn list_projection_refusals_mask_credentials() {
    let f = Fixture::new().await;
    for kind in ["atom", "domain"] {
        let ordinary = f
            .refuse(json!({"type": kind, "fields": ["id", "memo"]}))
            .await;
        assert_eq!(ordinary, projection_message(kind, "memo"));
        for secret in credentials() {
            let message = f
                .refuse(json!({"type": kind, "fields": ["id", secret.clone(), "memo"]}))
                .await;
            masked(&message, &secret, &projection_message(kind, &secret));
        }
    }
}

#[tokio::test]
async fn list_cursor_refusals_mask_credentials() {
    let f = Fixture::new().await;
    for kind in ["atom", "domain"] {
        let ordinary = f.refuse(json!({"type": kind, "after": "not-a-uuid"})).await;
        assert_eq!(ordinary, cursor_message("not-a-uuid"));
        for secret in credentials() {
            let message = f
                .refuse(json!({"type": kind, "after": secret.clone()}))
                .await;
            masked(&message, &secret, &cursor_message(&secret));
        }
    }
}

#[tokio::test]
async fn list_refusal_precedence_and_status_shape_are_unchanged() {
    let f = Fixture::new().await;
    for kind in ["atom", "domain"] {
        assert_eq!(
            f.refuse(json!({"type": "memo", "after": "bad", "offset": 0, "fields": ["memo"]}))
                .await,
            "unknown type \"memo\"; valid: atom | domain"
        );
        assert_eq!(
            f.refuse(json!({"type": kind, "after": "bad", "offset": 0, "fields": ["memo"]}))
                .await,
            "knowledge.list: `after` and `offset` are mutually exclusive"
        );
        assert_eq!(
            f.refuse(json!({"type": kind, "after": "bad", "fields": ["memo"]}))
                .await,
            projection_message(kind, "memo")
        );
        assert_eq!(
            f.refuse(json!({"type": kind, "after": "bad", "fields": []}))
                .await,
            "knowledge.list: `fields` must be non-empty"
        );
    }
    assert_eq!(
        f.refuse(json!({"type": "atom", "status": 42})).await,
        "status must be a string or an array of strings"
    );
}

#[tokio::test]
async fn list_projection_and_cursor_accept_populated_atom_and_domain_pages() {
    let f = Fixture::new().await;
    let before = f.snapshot().await;
    for kind in ["atom", "domain"] {
        let ordinary = f.list(json!({"type": kind})).await;
        assert_eq!(ordinary, f.list(json!({"kind": kind})).await);
        let first = f
            .list(json!({"type": kind, "after": "", "limit": 1, "fields": ["id", "slug"]}))
            .await;
        let cursor = first["next_after"]
            .as_str()
            .expect("another populated page");
        Uuid::parse_str(cursor).unwrap();
        let second = f
            .list(json!({"kind": kind, "after": cursor, "limit": 1, "fields": ["id", "slug"]}))
            .await;
        for page in [&first, &second] {
            assert_eq!(page["order"], "created_at_asc_id_asc");
            assert!(page.get("total").is_none());
            let rows = page["results"].as_array().unwrap();
            assert_eq!(rows.len(), 1);
            let object = rows[0].as_object().unwrap();
            assert_eq!(object.len(), 2);
            assert!(object.contains_key("id") && object.contains_key("slug"));
        }
        assert_ne!(first["results"][0]["id"], second["results"][0]["id"]);
        assert!(second["next_after"].is_null());
    }
    assert_eq!(f.snapshot().await, before);
}
