use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_runtime::{KhiveRuntime, RuntimeConfig, RuntimeError, VerbRegistry, VerbRegistryBuilder};
use serde_json::{json, Value};

const STATUS_HINT: &str = "inbox, next, waiting, someday, active, done, cancelled";
const STATUS_ALIASES: &str = " (aliases: in_progress, todo, blocked, later, finished)";

fn fixture() -> VerbRegistry {
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
        packs: vec!["kg".into(), "gtd".into()],
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
    builder.with_actor_id(Some("gtd-refusal-fixture".into()));
    builder.with_default_namespace("local");
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    let registry = builder.build().unwrap();
    registry.apply_schema_plans(runtime.backend());
    runtime.install_edge_rules(registry.all_edge_rules());
    registry
}

fn refused_values() -> Vec<(String, &'static str)> {
    vec![
        ("bogus".into(), "bogus"),
        (format!("sk-proj-{}", "A".repeat(80)), "***MASKED***"),
    ]
}

async fn assert_refusal(
    registry: &VerbRegistry,
    verb: &str,
    args: Value,
    raw: &str,
    expected: String,
) {
    let error = registry.dispatch(verb, args).await.unwrap_err();
    let RuntimeError::InvalidInput(message) = error else {
        panic!("{verb} changed its error kind: {error:?}");
    };
    assert_eq!(message, expected, "{verb}");
    if raw != "bogus" {
        assert!(
            !message.contains(raw),
            "{verb} leaked the refused credential"
        );
        assert!(message.contains("***MASKED***"));
    }
}

async fn task(registry: &VerbRegistry) -> Value {
    registry
        .dispatch("gtd.assign", json!({"title":"refusal target"}))
        .await
        .unwrap()["full_id"]
        .clone()
}

#[tokio::test]
async fn assign_refused_status_and_priority_are_masked_without_changing_plain_diagnostics() {
    let f = fixture();
    for (raw, shown) in refused_values() {
        for parameter in ["status", "priority"] {
            let mut args = json!({"title":"must not be created"});
            args[parameter] = json!(raw);
            let expected = if parameter == "status" {
                format!("invalid status {shown:?} — valid: {STATUS_HINT}{STATUS_ALIASES}")
            } else {
                format!("invalid priority {shown:?} — valid: p0, p1, p2, p3")
            };
            assert_refusal(&f, "gtd.assign", args, &raw, expected).await;
        }
    }
    assert_eq!(f.dispatch("gtd.tasks", json!({})).await.unwrap(), json!([]));
    let created = f
        .dispatch(
            "gtd.assign",
            json!({"title":"accepted aliases", "status":"in_progress", "priority":"P1"}),
        )
        .await
        .unwrap();
    assert_eq!(created["status"], "active");
    assert_eq!(created["priority"], "p1");
}

#[tokio::test]
async fn transition_refused_status_is_masked_and_done_spellings_still_work() {
    let f = fixture();
    let id = task(&f).await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f,
            "gtd.transition",
            json!({"id":id, "status":raw}),
            &raw,
            format!("invalid status {shown:?} — valid: {STATUS_HINT}{STATUS_ALIASES}"),
        )
        .await;
    }
    assert_eq!(
        f.dispatch("gtd.tasks", json!({"status":"inbox"}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for status in ["done", "completed"] {
        let fresh = task(&f).await;
        let transitioned = f
            .dispatch("gtd.transition", json!({"id":fresh, "status":status}))
            .await
            .unwrap();
        assert_eq!(transitioned["to"], "done");
    }
}

#[tokio::test]
async fn tasks_refused_filters_are_masked_and_canonical_aliases_still_filter() {
    let f = fixture();
    let id = task(&f).await;
    for (raw, shown) in refused_values() {
        for parameter in ["status", "priority"] {
            let mut args = json!({});
            args[parameter] = json!(raw);
            let expected = if parameter == "status" {
                format!("invalid status {shown:?} — valid: {STATUS_HINT}")
            } else {
                format!("invalid priority {shown:?} — valid: p0, p1, p2, p3")
            };
            assert_refusal(&f, "gtd.tasks", args, &raw, expected).await;
        }
    }
    let inbox = f
        .dispatch("gtd.tasks", json!({"status":"todo", "priority":"p2"}))
        .await
        .unwrap();
    assert_eq!(inbox.as_array().unwrap().len(), 1);
    assert_eq!(inbox[0]["full_id"], id);
    f.dispatch("gtd.transition", json!({"id":id, "status":"done"}))
        .await
        .unwrap();
    let completed = f
        .dispatch("gtd.tasks", json!({"status":"completed"}))
        .await
        .unwrap();
    assert_eq!(completed.as_array().unwrap().len(), 1);
    assert_eq!(completed[0]["full_id"], id);
}

#[tokio::test]
async fn complete_refused_status_is_masked_and_terminal_targets_still_work() {
    let f = fixture();
    let id = task(&f).await;
    for (raw, shown) in refused_values() {
        assert_refusal(
            &f,
            "gtd.complete",
            json!({"id":id, "status":raw}),
            &raw,
            format!("complete: status must be \"done\" or \"cancelled\"; got {shown:?}"),
        )
        .await;
    }
    assert_eq!(
        f.dispatch("gtd.tasks", json!({"status":"inbox"}))
            .await
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
        1
    );
    for status in ["done", "cancelled"] {
        let fresh = task(&f).await;
        let completed = f
            .dispatch("gtd.complete", json!({"id":fresh, "status":status}))
            .await
            .unwrap();
        assert_eq!(completed["to"], status);
    }
}

#[tokio::test]
async fn shared_task_create_uses_the_same_masked_refusals() {
    let f = fixture();
    for (raw, shown) in refused_values() {
        for parameter in ["status", "priority"] {
            let mut args = json!({"kind":"task", "name":"refusal target", "content":"must not be created", "properties":{}});
            args["properties"][parameter] = json!(raw);
            let expected = if parameter == "status" {
                format!("invalid status {shown:?} — valid: {STATUS_HINT}{STATUS_ALIASES}")
            } else {
                format!("invalid priority {shown:?} — valid: p0, p1, p2, p3")
            };
            assert_refusal(&f, "create", args, &raw, expected).await;
        }
    }
    assert_eq!(f.dispatch("gtd.tasks", json!({})).await.unwrap(), json!([]));
}
