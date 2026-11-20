//! Public GTD string-refusal guards, checked before error projection.

use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{
    KhiveRuntime, Namespace, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder,
};
use khive_storage::{Note, SqlStatement, SqlValue};
use khive_types::ErrorKind;
use serde_json::{json, Value};

const ACTOR: &str = "gtd-string-refusal-fixture";
const STATUSES: [&str; 7] = [
    "inbox",
    "next",
    "waiting",
    "someday",
    "active",
    "done",
    "cancelled",
];

fn fixture() -> VerbRegistry {
    fixture_with_runtime().1
}

fn fixture_with_runtime() -> (KhiveRuntime, VerbRegistry) {
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
        packs: vec!["kg".into(), "gtd".into()],
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
    builder.register(GtdPack::new(runtime.clone()));
    let registry = builder.build().expect("KG and GTD registry");
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    (runtime, registry)
}

fn refused_strings() -> Vec<(String, String, bool)> {
    let quoted = "bad\"reference\\with\nnewline".to_string();
    vec![
        ("bogus".into(), "bogus".into(), false),
        (quoted.clone(), quoted, false),
        (
            format!("sk-proj-{}", "A".repeat(80)),
            "***MASKED***".into(),
            true,
        ),
    ]
}

async fn assign(registry: &VerbRegistry, args: Value) -> Value {
    let task = registry
        .dispatch("gtd.assign", args)
        .await
        .expect("healthy public assign");
    assert_eq!(task["kind"], "task");
    assert!(task["full_id"].as_str().is_some());
    task
}

async fn get(registry: &VerbRegistry, id: &Value) -> Value {
    registry
        .dispatch("get", json!({"id": id}))
        .await
        .expect("get actual stored task")
}

async fn stored_note(runtime: &KhiveRuntime, id: &Value) -> Note {
    let token = runtime.authorize(Namespace::local()).unwrap();
    runtime
        .notes(&token)
        .unwrap()
        .get_note(id.as_str().unwrap().parse().unwrap())
        .await
        .unwrap()
        .expect("actual stored task")
}

async fn task_domain_state(runtime: &KhiveRuntime) -> Vec<(String, String)> {
    let mut reader = runtime.sql().reader().await.unwrap();
    let mut result = Vec::new();
    for table in [
        "notes",
        "graph_edges",
        "fts_notes",
        "ann_write_log",
        "note_streams",
        "gtd_lifecycle_audit",
    ] {
        let rows = reader
            .query_all(SqlStatement {
                sql: format!("SELECT * FROM {table} ORDER BY rowid"),
                params: vec![],
                label: None,
            })
            .await
            .unwrap();
        result.push((table.to_owned(), format!("{rows:?}")));
    }
    result
}

// Default gtd.tasks lists only open tasks, so enumerate every canonical status.
// The tiny private fixture must fit a complete page for every status.
async fn task_state(registry: &VerbRegistry) -> Value {
    let mut census = Vec::new();
    let mut ids = Vec::new();
    for status in STATUSES {
        let page = registry
            .dispatch("gtd.tasks", json!({"status": status, "limit": 200}))
            .await
            .expect("complete task census page");
        let rows = page.as_array().expect("unclamped task census array");
        assert!(
            rows.len() < 200,
            "the private task census must not be truncated"
        );
        for row in rows {
            ids.push(
                row["full_id"]
                    .as_str()
                    .expect("census full task id")
                    .to_string(),
            );
        }
        census.push(json!({"status": status, "tasks": page}));
    }
    ids.sort();
    assert!(
        ids.windows(2).all(|pair| pair[0] != pair[1]),
        "one census row per task"
    );
    assert!(ids.len() >= 2, "refusal state must contain real tasks");
    let mut stored = Vec::new();
    for id in ids {
        stored.push(get(registry, &json!(id)).await);
    }
    json!({"census": census, "stored": stored})
}

async fn invalid_input(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    raw: &str,
    secret: bool,
    expected: String,
    before: &Value,
) {
    let error = registry
        .dispatch(verb, args)
        .await
        .expect_err("public string guard must refuse");
    let RuntimeError::InvalidInput(message) = error.refusal_source() else {
        panic!("{verb} must reach the intended InvalidInput guard, got {error:?}");
    };
    if secret {
        assert!(
            !message.contains(raw),
            "actual raw InvalidInput exposed the synthetic credential before projection"
        );
        assert!(
            message.contains("***MASKED***"),
            "the intended refusal must contain the mask"
        );
        assert!(
            !error.to_string().contains(raw),
            "raw error Display exposed the synthetic credential"
        );
    }
    assert_eq!(
        message, &expected,
        "{verb} must preserve exact field/type/Debug wording"
    );
    assert_eq!(
        &task_state(registry).await,
        before,
        "{verb} refusal changed stored task state"
    );
}

#[tokio::test]
async fn dependency_update_refusal_masks_invalid_uuid_strings_without_task_mutation() {
    let registry = fixture();
    let blocker = assign(&registry, json!({"title": "dependency blocker"})).await;
    let target = assign(&registry, json!({"title": "dependency update target"})).await;
    registry
        .dispatch(
            "update",
            json!({
                "id": target["full_id"], "properties": {"depends_on": [blocker["full_id"]]}
            }),
        )
        .await
        .expect("healthy dependency property update reaches the task hook");
    assert_eq!(
        get(&registry, &target["full_id"]).await["properties"]["depends_on"],
        json!([blocker["full_id"]])
    );
    let before = task_state(&registry).await;
    for (raw, shown, secret) in refused_strings() {
        invalid_input(
            &registry,
            "update",
            json!({
                "id": target["full_id"], "properties": {"depends_on": [raw.clone()]}
            }),
            &raw,
            secret,
            format!("task properties.depends_on entry {shown:?} must be a full UUID"),
            &before,
        )
        .await;
    }
}

#[tokio::test]
async fn assign_key_conflict_masks_message_and_details_without_task_mutation() {
    let (runtime, registry) = fixture_with_runtime();
    assign(&registry, json!({"title": "unrelated retained task"})).await;
    for (index, (raw, shown, secret)) in refused_strings().into_iter().enumerate() {
        let args = json!({"title": format!("keyed original task {index}"), "idempotency_key": raw.clone()});
        let first = if secret {
            let first = assign(
                &registry,
                json!({"title": format!("keyed original task {index}")}),
            )
            .await;
            let mut expected = stored_note(&runtime, &first["full_id"]).await;
            assert_eq!(expected.key, None);
            // Legacy rows remain readable; application writes must not seed a credential key.
            let changed = runtime
                .sql()
                .writer()
                .await
                .unwrap()
                .execute(SqlStatement {
                    sql: "UPDATE notes SET key = ?1 WHERE id = ?2 AND namespace = 'local' AND kind = 'task' AND key IS NULL".into(),
                    params: vec![
                        SqlValue::Text(raw.clone()),
                        SqlValue::Text(expected.id.to_string()),
                    ],
                    label: None,
                })
                .await
                .unwrap();
            assert_eq!(changed, 1, "seed exactly one historical task key");
            expected.key = Some(raw.clone());
            expected.version += 1;
            assert_eq!(stored_note(&runtime, &first["full_id"]).await, expected);
            first
        } else {
            assign(&registry, args.clone()).await
        };
        let stored_before = stored_note(&runtime, &first["full_id"]).await;
        let domain_before = task_domain_state(&runtime).await;
        let replay = assign(&registry, args).await;
        assert_eq!(replay["full_id"], first["full_id"]);
        assert_eq!(replay["replayed"], true);
        assert_eq!(
            stored_note(&runtime, &first["full_id"]).await,
            stored_before,
            "replay preserves every stored task field, including its legacy key"
        );
        assert_eq!(task_domain_state(&runtime).await, domain_before);
        let before = task_state(&registry).await;
        let error = registry
            .dispatch(
                "gtd.assign",
                json!({
                    "title": format!("different keyed task {index}"), "idempotency_key": raw.clone()
                }),
            )
            .await
            .expect_err("different content under the real stored key must conflict");
        let RuntimeError::Khive(conflict) = error.refusal_source() else {
            panic!("the stored-key route must retain its typed Conflict, got {error:?}");
        };
        assert_eq!(conflict.kind(), ErrorKind::Conflict);
        // Serialize the actual structured error before any runtime projection.
        let envelope = serde_json::to_value(conflict).expect("serialize raw typed Conflict");
        if secret {
            assert!(
                !envelope.to_string().contains(&raw),
                "actual raw Conflict envelope exposed the synthetic credential before projection"
            );
            assert!(
                !error.to_string().contains(&raw),
                "raw Conflict Display exposed the synthetic credential"
            );
        }
        let id = first["full_id"]
            .as_str()
            .expect("stored keyed task full id");
        let expected = format!("conflict: idempotency_key_conflict: key {shown:?} already exists; stored content differs (existing task {id})");
        assert_eq!(conflict.message(), expected);
        let details = conflict.details().expect("Conflict details retained");
        assert_eq!(details.get("reason"), Some("idempotency_key_conflict"));
        assert_eq!(details.get("key"), Some(shown.as_str()));
        assert_eq!(details.get("existing_id"), Some(id));
        assert_eq!(envelope["message"], json!(expected));
        assert_eq!(
            envelope["details"],
            json!({
                "reason": "idempotency_key_conflict", "key": shown, "existing_id": id
            })
        );
        assert_eq!(
            task_state(&registry).await,
            before,
            "conflict must preserve the real keyed note and every other task"
        );
        assert_eq!(
            stored_note(&runtime, &first["full_id"]).await,
            stored_before
        );
        assert_eq!(task_domain_state(&runtime).await, domain_before);
    }
}

#[tokio::test]
async fn assign_new_credential_key_refuses_without_task_or_index_mutation() {
    let (runtime, registry) = fixture_with_runtime();
    let blocker = assign(&registry, json!({"title": "retained blocker"})).await;
    assign(
        &registry,
        json!({"title": "retained dependent", "depends_on": [blocker["full_id"]]}),
    )
    .await;
    let edges = runtime
        .sql()
        .reader()
        .await
        .unwrap()
        .query_all(SqlStatement {
            sql: "SELECT * FROM graph_edges".into(),
            params: vec![],
            label: None,
        })
        .await
        .unwrap();
    assert_eq!(edges.len(), 1, "retain a real task dependency edge");
    let before = task_state(&registry).await;
    let domain_before = task_domain_state(&runtime).await;
    let raw = format!("sk-proj-{}", "A".repeat(80));
    let result = registry
        .dispatch(
            "gtd.assign",
            json!({"title": "new credential-key task", "idempotency_key": raw.clone(),
                "depends_on": [blocker["full_id"]]}),
        )
        .await;
    assert!(
        matches!(&result, Err(error)
            if matches!(error.refusal_source(), RuntimeError::SecretDetected(matched)
                if matched.detector == "openai-api-key"
                    && matched.trigger.is_none()
                    && matched.location.as_deref() == Some("note.key"))),
        "a new credential key must reach the typed note.key write refusal: {result:?}"
    );
    let error = result.unwrap_err();
    assert!(!error.to_string().contains(&raw));
    assert!(!format!("{error:?}").contains(&raw));
    assert_eq!(task_state(&registry).await, before);
    assert_eq!(
        task_domain_state(&runtime).await,
        domain_before,
        "key refusal must preserve complete task, edge, index, stream and lifecycle rows"
    );
}

#[tokio::test]
async fn assign_dependency_refusal_masks_invalid_reference_strings_without_task_mutation() {
    let registry = fixture();
    let blocker = assign(&registry, json!({"title": "reference blocker"})).await;
    let full = assign(
        &registry,
        json!({
            "title": "full reference positive", "depends_on": [blocker["full_id"]]
        }),
    )
    .await;
    let prefix = assign(
        &registry,
        json!({
            "title": "prefix reference positive", "depends_on": [blocker["id"]]
        }),
    )
    .await;
    for task in [&full, &prefix] {
        assert_eq!(
            get(&registry, &task["full_id"]).await["properties"]["depends_on"],
            json!([blocker["full_id"]])
        );
    }
    let before = task_state(&registry).await;
    for (raw, shown, secret) in refused_strings() {
        invalid_input(
            &registry,
            "gtd.assign",
            json!({
                "title": "must not be assigned", "depends_on": [raw.clone()]
            }),
            &raw,
            secret,
            format!("invalid UUID (expected full UUID or 8+ hex prefix): {shown:?}"),
            &before,
        )
        .await;
    }
}

#[tokio::test]
async fn lifecycle_refusal_masks_invalid_reference_strings_without_task_mutation() {
    let registry = fixture();
    let target = assign(&registry, json!({"title": "lifecycle target"})).await;
    assign(&registry, json!({"title": "lifecycle retained control"})).await;
    let before = task_state(&registry).await;
    for (raw, shown, secret) in refused_strings() {
        invalid_input(
            &registry,
            "gtd.complete",
            json!({"id": raw.clone(), "status": "done"}),
            &raw,
            secret,
            format!("invalid UUID (expected full UUID or 8+ hex prefix): {shown:?}"),
            &before,
        )
        .await;
    }
    let advanced = registry
        .dispatch(
            "gtd.transition",
            json!({
                "id": target["full_id"], "status": "next"
            }),
        )
        .await
        .expect("healthy full-id lifecycle transition");
    assert_eq!(advanced["full_id"], target["full_id"]);
    assert_eq!(advanced["to"], "next");
    let completed = registry
        .dispatch(
            "gtd.complete",
            json!({
                "id": target["id"], "status": "done"
            }),
        )
        .await
        .expect("healthy short-prefix lifecycle completion");
    assert_eq!(completed["full_id"], target["full_id"]);
    assert_eq!(completed["to"], "done");
    assert_eq!(
        get(&registry, &target["full_id"]).await["properties"]["status"],
        "done"
    );
}

#[tokio::test]
async fn assign_context_refusal_masks_invalid_uuid_strings_without_task_mutation() {
    let registry = fixture();
    assign(&registry, json!({"title": "context retained control"})).await;
    let anchor = registry.dispatch("create", json!({
        "kind": "entity", "entity_kind": "concept", "name": "string refusal context anchor",
        "description": "This isolated concept provides a real primary namespace anchor for task context validation while keeping every credential shaped refusal outside persistent task records and preserving ordinary assignment behavior."
    })).await.expect("healthy actual context entity");
    let healthy = assign(
        &registry,
        json!({
            "title": "context positive", "context_entity_id": anchor["id"]
        }),
    )
    .await;
    assert_eq!(
        get(&registry, &healthy["full_id"]).await["properties"]["context_entity_id"],
        anchor["id"]
    );
    let before = task_state(&registry).await;
    let mut refused = refused_strings();
    refused.push(("deadbeef".into(), "deadbeef".into(), false));
    for (raw, shown, secret) in refused {
        invalid_input(
            &registry,
            "gtd.assign",
            json!({
                "title": "must not get a context", "context_entity_id": raw.clone()
            }),
            &raw,
            secret,
            format!(
                "context_entity_id must be a full UUID because a short prefix would require a \
             primary-namespace resolution and this field stores an explicit stable entity \
             reference; got {shown:?}"
            ),
            &before,
        )
        .await;
    }
}
