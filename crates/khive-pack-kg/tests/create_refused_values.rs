//! Singleton create discriminants refuse wrong types without echoing credentials.

use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use serde_json::{json, Value};

const ACTOR: &str = "kg-create-refusal-fixture";
const MASK: &str = "***MASKED***";
const LIMIT: usize = 200;

fn fixture() -> VerbRegistry {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        default_namespace: Namespace::local(),
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
        packs: vec!["kg".into()],
        actor_id: Some(ACTOR.into()),
        brain_profile: None,
        brain: Default::default(),
        blob: Default::default(),
        mounts: Vec::new(),
        events_split: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("private in-memory runtime");
    assert!(runtime.backend().pool().canonical_path().is_none());
    assert!(runtime.backend_data_dir().is_none());
    assert!(runtime.backend_ann_root().is_none());
    assert!(runtime.default_embedder_name().is_empty());
    let mut builder = VerbRegistryBuilder::new();
    builder.with_actor_id(Some(ACTOR.into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    let registry = builder.build().expect("KG registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn entity_args(name: &str) -> Value {
    json!({
        "kind": "concept",
        "name": name,
        "description": "This ordinary fixture describes a stable concept used to verify that public create refuses malformed discriminants before any domain mutation while preserving existing records and their complete stored fields.",
        "skip_dedup_check": true
    })
}

fn note_args(content: &str) -> Value {
    json!({"kind": "note", "content": content})
}

async fn create_and_get(registry: &VerbRegistry, args: Value, expected_kind: &str) {
    let name = args.get("name").cloned();
    let content = args.get("content").cloned();
    let created = registry
        .dispatch("create", args)
        .await
        .expect("healthy public create");
    let id = created["id"].as_str().expect("created id");
    let stored = registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("actual created record readback");
    assert_eq!(stored["kind"], expected_kind);
    if let Some(name) = name {
        assert_eq!(stored["name"], name);
    }
    if let Some(content) = content {
        assert_eq!(stored["content"], content);
    }
}

#[derive(Debug, PartialEq)]
struct DomainSnapshot {
    entities: Vec<Value>,
    notes: Vec<Value>,
    records: Vec<Value>,
}

async fn rows(registry: &VerbRegistry, kind: &str) -> Vec<Value> {
    let response = registry
        .dispatch("list", json!({"kind": kind, "limit": LIMIT}))
        .await
        .expect("public domain census");
    let mut rows = response["items"].as_array().expect("list items").clone();
    assert!(rows.len() < LIMIT, "fixture census must be complete");
    rows.sort_by_key(|row| row["id"].as_str().expect("listed id").to_owned());
    for pair in rows.windows(2) {
        assert_ne!(pair[0]["id"], pair[1]["id"], "duplicate census id");
    }
    rows
}

async fn snapshot(registry: &VerbRegistry) -> DomainSnapshot {
    let entities = rows(registry, "entity").await;
    let notes = rows(registry, "note").await;
    assert!(!entities.is_empty(), "entity positive control missing");
    assert!(!notes.is_empty(), "note positive control missing");
    let mut records = Vec::new();
    for row in entities.iter().chain(notes.iter()) {
        let id = row["id"].as_str().expect("listed id");
        records.push(
            registry
                .dispatch("get", json!({"id": id}))
                .await
                .expect("full stored record"),
        );
    }
    DomainSnapshot {
        entities,
        notes,
        records,
    }
}

fn wrong_values(secret: &str) -> Vec<Value> {
    vec![
        json!(false),
        json!(17),
        json!({"benign": "quote\"slash\\line\n"}),
        json!([secret]),
        json!({"nested": {"credential": secret}}),
    ]
}

fn assert_invalid_input(error: RuntimeError, expected: &str, secret: Option<&str>) {
    let RuntimeError::InvalidInput(message) = error else {
        panic!("expected raw create InvalidInput, got {error:?}");
    };
    if let Some(secret) = secret {
        assert!(
            !message.contains(secret),
            "raw create InvalidInput exposed the synthetic credential before projection"
        );
        assert!(
            message.contains(MASK),
            "raw create refusal omitted mask marker"
        );
    }
    assert_eq!(message, expected, "original create diagnostic shape");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_kind_refusals_mask_values_without_entity_or_note_writes() {
    let registry = fixture();
    create_and_get(&registry, entity_args("Required kind baseline"), "concept").await;
    create_and_get(
        &registry,
        note_args("Required kind baseline note."),
        "observation",
    )
    .await;
    let before = snapshot(&registry).await;
    let mut missing = entity_args("Missing kind must not create");
    missing.as_object_mut().unwrap().remove("kind");
    assert_invalid_input(
        registry
            .dispatch("create", missing)
            .await
            .expect_err("missing kind"),
        "create requires 'kind'",
        None,
    );
    assert_eq!(snapshot(&registry).await, before);

    let secret = format!("sk-proj-{}", "A".repeat(80));
    for value in std::iter::once(Value::Null).chain(wrong_values(&secret)) {
        let shown = value.to_string();
        let hidden = shown.contains(&secret).then_some(secret.as_str());
        let expected = format!(
            "create: `kind` must be a string; got {}",
            shown.replace(&secret, MASK)
        );
        let mut args = entity_args("Wrong kind must not create");
        args["kind"] = value;
        assert_invalid_input(
            registry
                .dispatch("create", args)
                .await
                .expect_err("wrong kind type"),
            &expected,
            hidden,
        );
        assert_eq!(snapshot(&registry).await, before);
    }

    let first = json!({"first": secret});
    let mut args = entity_args("Required kind must precede aliases");
    args["kind"] = first.clone();
    args["entity_kind"] = json!(["later entity alias"]);
    args["note_kind"] = json!(["later note alias"]);
    let expected = format!(
        "create: `kind` must be a string; got {}",
        first.to_string().replace(&secret, MASK)
    );
    assert_invalid_input(
        registry
            .dispatch("create", args)
            .await
            .expect_err("required guard first"),
        &expected,
        Some(&secret),
    );
    assert_eq!(snapshot(&registry).await, before);
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_kind_alias_refusals_mask_values_without_entity_or_note_writes() {
    let registry = fixture();
    create_and_get(&registry, entity_args("Absent aliases baseline"), "concept").await;
    create_and_get(
        &registry,
        note_args("Absent aliases baseline note."),
        "observation",
    )
    .await;
    let mut entity = entity_args("Null aliases baseline");
    entity["entity_kind"] = Value::Null;
    entity["note_kind"] = Value::Null;
    create_and_get(&registry, entity, "concept").await;
    let mut note = note_args("Null aliases baseline note.");
    note["entity_kind"] = Value::Null;
    note["note_kind"] = Value::Null;
    create_and_get(&registry, note, "observation").await;
    let before = snapshot(&registry).await;
    let secret = format!("sk-proj-{}", "A".repeat(80));

    // Each alias is checked even when the selected substrate does not use it.
    for entity in [true, false] {
        for field in ["entity_kind", "note_kind"] {
            for value in wrong_values(&secret) {
                let shown = value.to_string();
                let hidden = shown.contains(&secret).then_some(secret.as_str());
                let expected = format!(
                    "create: `{field}` must be a string or null; got {}",
                    shown.replace(&secret, MASK)
                );
                let mut args = if entity {
                    entity_args("Wrong alias must not create")
                } else {
                    note_args("Wrong alias must not create.")
                };
                args[field] = value;
                assert_invalid_input(
                    registry
                        .dispatch("create", args)
                        .await
                        .expect_err("wrong alias type"),
                    &expected,
                    hidden,
                );
                assert_eq!(snapshot(&registry).await, before);
            }
            for empty in ["", " \t\n"] {
                let mut args = if entity {
                    entity_args("Empty alias must not create")
                } else {
                    note_args("Empty alias must not create.")
                };
                args[field] = json!(empty);
                assert_invalid_input(
                    registry
                        .dispatch("create", args)
                        .await
                        .expect_err("empty alias"),
                    &format!("create: `{field}` must not be empty"),
                    None,
                );
                assert_eq!(snapshot(&registry).await, before);
            }
        }
    }

    let first = json!({"first": secret});
    let mut args = entity_args("Entity alias must precede note alias");
    args["entity_kind"] = first.clone();
    args["note_kind"] = json!(["later note alias"]);
    let expected = format!(
        "create: `entity_kind` must be a string or null; got {}",
        first.to_string().replace(&secret, MASK)
    );
    assert_invalid_input(
        registry
            .dispatch("create", args)
            .await
            .expect_err("entity alias first"),
        &expected,
        Some(&secret),
    );
    assert_eq!(snapshot(&registry).await, before);
}

// Same synthetic detector value as the existing sk-proj fixture; split source
// literals avoid presenting a credential-shaped literal to repository scanners.
const BULK_SECRET: &str = concat!(
    "sk-proj-",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
    "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
);

fn bulk_items() -> Value {
    json!([
        {"kind": "note", "content": "Bulk refusal first healthy observation."},
        {"kind": "note", "content": "Bulk refusal second healthy observation."}
    ])
}

async fn bulk_refusal_case(field: &str, expected_type: &str) {
    // Real unpaused Tokio time. One outer budget covers fixture startup await,
    // every dispatch and every full snapshot; timeout is failure, never a kill.
    tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let registry = tokio::task::spawn_blocking(fixture)
            .await
            .expect("isolated public registry fixture startup");
        create_and_get(
            &registry,
            entity_args("Bulk refusal baseline concept"),
            "concept",
        )
        .await;
        create_and_get(
            &registry,
            note_args("Bulk refusal baseline observation."),
            "observation",
        )
        .await;
        let seeded = snapshot(&registry).await;

        // Positive control through the SAME public verb and actual bulk arm.
        // Its two durable notes must move the observer used by every refusal.
        let healthy = registry
            .dispatch(
                "create",
                json!({
                    "kind": "note", "items": bulk_items(), "atomic": true, "verbose": false
                }),
            )
            .await
            .expect("healthy public bulk create must reach commit");
        assert_eq!(healthy["attempted"], 2);
        assert_eq!(healthy["created"], 2);
        assert_eq!(healthy["skipped"], 0);
        assert_eq!(healthy["failed"], 0);
        let results = healthy["results"].as_array().expect("actual bulk results");
        assert_eq!(results.len(), 2);
        for (index, row) in results.iter().enumerate() {
            assert_eq!(row["index"], index);
            assert_eq!(row["ok"], true);
            let id = row["result"]["id"].as_str().expect("actual bulk note id");
            let stored = registry
                .dispatch("get", json!({"id": id}))
                .await
                .expect("actual bulk note readback");
            assert_eq!(stored["kind"], "observation");
            assert_eq!(stored["content"], bulk_items()[index]["content"]);
        }
        let before = snapshot(&registry).await;
        assert_eq!(before.entities, seeded.entities);
        assert_eq!(
            before.notes.len(),
            seeded.notes.len() + 2,
            "healthy bulk write must move the refusal observer"
        );
        assert_eq!(before.records.len(), seeded.records.len() + 2);
        for record in &seeded.records {
            assert!(
                before.records.contains(record),
                "bulk positive control preserves seeded records"
            );
        }

        let mut values = vec![
            Value::Null,
            json!(17),
            json!("benign quote\"slash\\line\n"),
            json!({"benign": "quote\"slash\\line\n"}),
            json!(BULK_SECRET),
            json!({"nested": {"credential": BULK_SECRET}}),
        ];
        if field == "items" {
            // Arrays are valid for items; a boolean is not.
            values.push(json!(false));
        } else {
            // Booleans are valid for atomic/verbose; an array is not.
            values.push(json!([BULK_SECRET]));
        }
        for value in values {
            let shown = value.to_string();
            let hidden = shown.contains(BULK_SECRET).then_some(BULK_SECRET);
            let expected = format!(
                "create: `{field}` must be {expected_type}; got {}",
                shown.replace(BULK_SECRET, MASK)
            );
            let mut args = json!({
                "kind": "note", "items": bulk_items(), "atomic": true, "verbose": false
            });
            // Later malformed options also pin the existing parse order. The
            // earlier exact field diagnostic, not a schema/gate error, must win.
            if field == "items" {
                args["atomic"] = json!({"later": BULK_SECRET});
                args["verbose"] = json!([BULK_SECRET]);
            } else if field == "atomic" {
                args["verbose"] = json!([BULK_SECRET]);
            }
            args[field] = value;
            let error = registry
                .dispatch("create", args)
                .await
                .expect_err("wrong bulk option type must refuse before item preparation");
            assert_invalid_input(error, &expected, hidden);
            assert_eq!(
                snapshot(&registry).await,
                before,
                "bulk option refusal must preserve seeded entity/note populations and full records"
            );
        }
    })
    .await
    .expect("bulk refusal fixture exceeded its unpaused wall-clock budget");
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_bulk_items_refusal_masks_before_any_domain_write() {
    bulk_refusal_case("items", "an array").await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_bulk_atomic_refusal_masks_before_any_domain_write() {
    bulk_refusal_case("atomic", "a boolean").await;
}

#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn create_bulk_verbose_refusal_masks_before_any_domain_write() {
    bulk_refusal_case("verbose", "a boolean").await;
}
