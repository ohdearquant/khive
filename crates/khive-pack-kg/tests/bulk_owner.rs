use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, KindHook, Namespace, NamespaceToken, PackRuntime, RuntimeError, VerbRegistry,
    VerbRegistryBuilder,
};
use khive_types::{
    EdgeEndpointRule, EntityTypeDef, HandlerDef, NoteKindSpec, Pack, PackColumnAddition,
    PackSchemaPlan,
};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Debug, Default)]
struct OwnerHook {
    prepared: Mutex<Vec<Value>>,
    created: Mutex<Vec<(Uuid, Value)>>,
}

#[async_trait]
impl KindHook for OwnerHook {
    async fn prepare_create(&self, _: &KhiveRuntime, args: &mut Value) -> Result<(), RuntimeError> {
        self.prepared.lock().unwrap().push(args.clone());
        let name = args["name"].as_str().unwrap().to_owned();
        if name == "refuse" {
            return Err(RuntimeError::InvalidInput("owner refused entity".into()));
        }
        let corrupt = args["properties"]["corrupt"].as_str().map(str::to_owned);
        args["name"] = json!(format!("normalized-{name}"));
        args["description"] = json!("normalized description");
        args["properties"] = json!({"owner": "checked"});
        args["tags"] = json!(["normalized"]);
        args["entity_type"] = json!("method");
        match corrupt.as_deref() {
            Some("name") => args["name"] = json!(3),
            Some("blank_name") => args["name"] = json!(" "),
            Some("description") => args["description"] = json!(false),
            Some("properties") => args["properties"] = json!([]),
            Some("tags") => args["tags"] = json!([3]),
            Some("entity_type") => args["entity_type"] = json!("not-a-known-type"),
            Some("entity_type_shape") => args["entity_type"] = json!(3),
            _ => {}
        }
        Ok(())
    }

    async fn after_create(
        &self,
        runtime: &KhiveRuntime,
        id: Uuid,
        args: &Value,
    ) -> Result<(), RuntimeError> {
        let token = runtime.authorize(Namespace::local()).unwrap();
        let stored = runtime.get_entity(&token, id).await.unwrap();
        assert_eq!(stored.name, args["name"].as_str().unwrap());
        self.created.lock().unwrap().push((id, args.clone()));
        if stored.name == "normalized-after-error" {
            return Err(RuntimeError::Internal("postcommit hook failed".into()));
        }
        Ok(())
    }
}

struct HookedKgPack {
    kg: KgPack,
    hook: Arc<OwnerHook>,
}

impl Pack for HookedKgPack {
    const NAME: &'static str = <KgPack as Pack>::NAME;
    const NOTE_KINDS: &'static [&'static str] = <KgPack as Pack>::NOTE_KINDS;
    const ENTITY_KINDS: &'static [&'static str] = <KgPack as Pack>::ENTITY_KINDS;
    const BRAIN_CONSUMER_KINDS: &'static [&'static str] = <KgPack as Pack>::BRAIN_CONSUMER_KINDS;
    const HANDLERS: &'static [HandlerDef] = <KgPack as Pack>::HANDLERS;
    const EDGE_RULES: &'static [EdgeEndpointRule] = <KgPack as Pack>::EDGE_RULES;
    const ENTITY_TYPES: &'static [EntityTypeDef] = <KgPack as Pack>::ENTITY_TYPES;
    const REQUIRES: &'static [&'static str] = <KgPack as Pack>::REQUIRES;
    const NOTE_KIND_SPECS: &'static [NoteKindSpec] = <KgPack as Pack>::NOTE_KIND_SPECS;
    const SCHEMA_PLAN: Option<PackSchemaPlan> = <KgPack as Pack>::SCHEMA_PLAN;
    const SCHEMA_COLUMN_ADDITIONS: &'static [PackColumnAddition] =
        <KgPack as Pack>::SCHEMA_COLUMN_ADDITIONS;
    const VALIDATION_RULES: &'static [&'static str] = <KgPack as Pack>::VALIDATION_RULES;
}

#[async_trait]
impl PackRuntime for HookedKgPack {
    fn name(&self) -> &str {
        KgPack::NAME
    }
    fn note_kinds(&self) -> &'static [&'static str] {
        KgPack::NOTE_KINDS
    }
    fn entity_kinds(&self) -> &'static [&'static str] {
        KgPack::ENTITY_KINDS
    }
    fn handlers(&self) -> &'static [HandlerDef] {
        KgPack::HANDLERS
    }
    fn edge_rules(&self) -> &'static [EdgeEndpointRule] {
        KgPack::EDGE_RULES
    }
    fn kind_hook(&self, kind: &str) -> Option<Arc<dyn KindHook>> {
        (kind == "concept").then(|| self.hook.clone() as Arc<dyn KindHook>)
    }
    async fn dispatch(
        &self,
        verb: &str,
        args: Value,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Value, RuntimeError> {
        self.kg.dispatch(verb, args, registry, token).await
    }
}

fn fixture() -> (KhiveRuntime, VerbRegistry, Arc<OwnerHook>) {
    let runtime = KhiveRuntime::memory().unwrap();
    let hook = Arc::new(OwnerHook::default());
    let mut builder = VerbRegistryBuilder::new();
    builder.register(HookedKgPack {
        kg: KgPack::new(runtime.clone()),
        hook: hook.clone(),
    });
    // No runtime entity-update hook map is installed: creation must use the
    // actual registry supplied to this dispatch, including custom pack hooks.
    (runtime, builder.build().unwrap(), hook)
}

#[tokio::test]
async fn owner_normalizes_raw_entity_type_for_singleton_and_bulk() {
    for atomic in [true, false] {
        let (_, registry, hook) = fixture();
        let singleton = registry
            .dispatch(
                "create",
                json!({
                    "kind": "concept", "name": "singleton",
                    "entity_type": "owner-shorthand", "skip_dedup_check": true,
                }),
            )
            .await
            .expect("the owner normalizes the raw entity_type before validation");
        let bulk = registry
            .dispatch(
                "create",
                json!({
                    "atomic": atomic, "verbose": true,
                    "items": [{
                        "kind": "concept", "name": "bulk",
                        "entity_type": "owner-shorthand",
                    }],
                }),
            )
            .await
            .expect("bulk must use the same preparation order as singleton creation");
        assert_eq!(bulk["created"], 1);
        assert_eq!(bulk["failed"], 0);
        for id in [&singleton["id"], &bulk["entities"][0]["id"]] {
            let stored = registry.dispatch("get", json!({"id": id})).await.unwrap();
            assert_eq!(stored["entity_type"], "function");
        }
        assert_eq!(singleton["entity_type_normalized"]["requested"], "method");
        assert_eq!(singleton["entity_type_normalized"]["stored"], "function");
        assert_eq!(bulk["entity_type_normalized"][0]["requested"], "method");
        assert_eq!(bulk["entity_type_normalized"][0]["stored"], "function");
        assert_eq!(bulk["entity_type_normalized"][0]["index"], 0);
        let prepared = hook.prepared.lock().unwrap();
        assert_eq!(prepared.len(), 2);
        assert!(prepared
            .iter()
            .all(|args| args["entity_type"] == "owner-shorthand"));
        assert_eq!(hook.created.lock().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn invalid_entity_type_after_preparation_fails_for_singleton_and_bulk() {
    for atomic in [true, false] {
        for owner_installed in [true, false] {
            let (runtime, hooked_registry, hook) = fixture();
            let registry = if owner_installed {
                hooked_registry
            } else {
                let mut builder = VerbRegistryBuilder::new();
                builder.register(KgPack::new(runtime.clone()));
                builder.build().unwrap()
            };
            // Must fail: an installed owner emits an unknown type, or no owner
            // is available to normalize the caller's unknown type.
            let fields = json!({
                "kind": "concept", "name": "invalid",
                "entity_type": "owner-shorthand",
                "properties": {"corrupt": "entity_type"},
            });
            registry
                .dispatch("create", fields.clone())
                .await
                .expect_err("entity_type must be valid after preparation");
            let bulk = registry
                .dispatch("create", json!({"atomic": atomic, "items": [fields]}))
                .await;
            if atomic {
                let error = bulk.expect_err("an invalid prepared type rejects the batch");
                assert!(error.to_string().contains("items[0]"));
            } else {
                let bulk = bulk.expect("a preparation error is an indexed item failure");
                assert_eq!(bulk["created"], 0);
                assert_eq!(bulk["failed"], 1);
                assert_eq!(bulk["errors"][0]["index"], 0);
                assert!(bulk["errors"][0]["error"]
                    .as_str()
                    .unwrap()
                    .contains("items[0]"));
            }
            let listed = registry
                .dispatch("list", json!({"kind": "entity"}))
                .await
                .unwrap();
            assert!(listed["items"].as_array().unwrap().is_empty());
            assert!(hook.created.lock().unwrap().is_empty());
            let token = runtime.authorize(Namespace::local()).unwrap();
            assert_eq!(
                runtime
                    .text(&token)
                    .unwrap()
                    .count(khive_storage::TextFilter::default())
                    .await
                    .unwrap(),
                0
            );
        }
    }
}

#[tokio::test]
async fn bulk_owner_normalization_and_postcommit_effects_match_written_entities() {
    for atomic in [true, false] {
        let (_, registry, hook) = fixture();
        let response = registry
            .dispatch(
                "create",
                json!({
                    "atomic": atomic, "verbose": true,
                    "items": [
                        {"kind": "concept", "name": "first", "description": "original", "tags": ["original"]},
                        {"kind": "entity", "entity_kind": "concept", "name": "after-error"},
                    ],
                }),
            )
            .await
            .expect("postcommit hook failures must not report a committed item as failed");
        assert_eq!(response["created"], 2);
        assert_eq!(response["failed"], 0);
        let prepared = hook.prepared.lock().unwrap().clone();
        assert_eq!(prepared.len(), 2);
        let created = hook.created.lock().unwrap().clone();
        assert_eq!(created.len(), 2);
        for (index, name) in ["normalized-first", "normalized-after-error"]
            .into_iter()
            .enumerate()
        {
            assert_eq!(prepared[index]["kind"], "entity");
            assert_eq!(prepared[index]["entity_kind"], "concept");
            assert_eq!(prepared[index]["namespace"], "local");
            let entity = registry
                .dispatch("get", json!({"id": response["entities"][index]["id"]}))
                .await
                .unwrap();
            assert_eq!(entity["name"], name);
            assert_eq!(entity["description"], "normalized description");
            assert_eq!(entity["properties"], json!({"owner": "checked"}));
            assert_eq!(entity["tags"], json!(["normalized"]));
            assert_eq!(entity["entity_type"], "function");
            assert_eq!(created[index].0.to_string(), entity["id"].as_str().unwrap());
            assert_eq!(created[index].1["name"], name);
            assert_eq!(response["entity_type_normalized"][index]["index"], index);
            assert_eq!(
                response["entity_type_normalized"][index]["requested"],
                "method"
            );
            assert_eq!(
                response["entity_type_normalized"][index]["stored"],
                "function"
            );
        }
    }
}

#[tokio::test]
async fn bulk_owner_refusal_and_invalid_normalization_have_no_postcommit_effects() {
    for atomic in [true, false] {
        for corrupt in [
            "refuse",
            "name",
            "blank_name",
            "description",
            "properties",
            "tags",
            "entity_type",
            "entity_type_shape",
        ] {
            let (runtime, registry, hook) = fixture();
            let response = registry
                .dispatch(
                    "create",
                    json!({
                        "atomic": atomic,
                        "items": [
                            {"kind": "concept", "name": "valid"},
                            {"kind": "concept", "name": corrupt, "properties": {"corrupt": corrupt}},
                        ],
                    }),
                )
                .await;
            let expected = if atomic {
                response.expect_err("invalid owner output rejects the atomic batch");
                0
            } else {
                let response = response.expect("owner errors remain per-item failures");
                assert_eq!(response["attempted"], 2);
                assert_eq!(response["created"], 1);
                assert_eq!(response["failed"], 1);
                assert_eq!(response["errors"][0]["index"], 1);
                assert_eq!(
                    response["entity_type_normalized"].as_array().unwrap().len(),
                    1
                );
                assert_eq!(response["entity_type_normalized"][0]["index"], 0);
                1
            };
            assert_eq!(hook.created.lock().unwrap().len(), expected);
            let listed = registry
                .dispatch("list", json!({"kind": "entity"}))
                .await
                .unwrap();
            assert_eq!(listed["items"].as_array().unwrap().len(), expected);
            let token = runtime.authorize(Namespace::local()).unwrap();
            assert_eq!(
                runtime
                    .text(&token)
                    .unwrap()
                    .count(khive_storage::TextFilter::default())
                    .await
                    .unwrap(),
                expected as u64
            );
        }
    }
}
