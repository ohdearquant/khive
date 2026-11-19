use super::*;

#[test]
fn atomic_preflight_registry_explicitly_disables_writer_ceiling() {
    if crate::test_process::run_in_child() {
        return;
    }
    std::env::set_var("KHIVE_SQLITE_WAL_CEILING_BYTES", "8192");
    let root = tempfile::tempdir().unwrap();
    let target = root.path().join("not-opened/database.db");
    let cfg = RuntimeConfig {
        db_path: Some(target.clone()),
        packs: vec!["kg".into()],
        wal_ceiling_bytes: 8192,
        wal_ceiling_configured_bytes: 8192,
        wal_ceiling_source: khive_runtime::WalCeilingSource::BackendField,
        ..RuntimeConfig::no_embeddings()
    };
    let (registry, runtime) = build_atomic_preflight_registry(&cfg, &KhiveConfig::default())
        .expect("ATOMIC_METADATA_CEILING_EXEMPTION");
    assert!(registry.has_verb("create"));
    assert!(!runtime.backend().is_file_backed());
    assert_eq!(
        runtime.config().wal_ceiling_source,
        khive_runtime::WalCeilingSource::BackendField
    );
    assert_eq!(runtime.config().wal_ceiling_bytes, 0);
    assert!(!target.parent().unwrap().exists());
    let file = KhiveRuntime::new(cfg)
        .err()
        .expect("target still governed by captured policy");
    assert!(matches!(
        file,
        RuntimeError::Storage(khive_storage::StorageError::Driver { source, .. }) if matches!(source.downcast_ref(), Some(khive_db::SqliteError::WalCapacityUnavailable { bytes: 8192, .. }))
    ));
}

use khive_types::Namespace;

include!("atomic_apply/test_runtime.rs");

#[cfg(feature = "pack-telemetry")]
#[test]
fn atomic_preparation_keeps_discovered_hooks_without_activating_unrequested_telemetry() {
    let runtime = KhiveRuntime::new(RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into()],
        brain_profile: None,
        ..RuntimeConfig::no_embeddings()
    })
    .expect("runtime");
    assert_eq!(runtime.config().telemetry.default_carrier, None);
    let names = atomic_preparation_pack_names(runtime.config());
    let discovered = PackRegistry::discovered_names();
    assert!(discovered.contains(&"telemetry"));
    for name in &discovered {
        assert_eq!(
            names.iter().any(|selected| selected == name),
            *name != "telemetry"
        );
    }
    let registry = full_registry(&runtime);
    assert!(registry.all_note_kinds().contains(&"task"));
    assert!(registry.all_note_kinds().contains(&"memory"));
    assert!(!registry.has_verb("telemetry.emit"));
    assert!(registry.has_verb("gtd.transition"));
    let (preflight, _runtime) =
        build_atomic_preflight_registry(runtime.config(), &KhiveConfig::default())
            .expect("unrequested telemetry needs no declaration");
    assert!(preflight.has_verb("create"));

    let mut configured = runtime.config().clone();
    configured.packs.push("telemetry".into());
    assert!(atomic_preparation_pack_names(&configured)
        .iter()
        .any(|name| name == "telemetry"));
    let error = build_atomic_preflight_registry(&configured, &KhiveConfig::default())
        .err()
        .expect("requested telemetry requires its declared default");
    assert!(format!("{error:#}").contains("telemetry.default_carrier"));
    configured.telemetry.default_carrier = Some(khive_runtime::TelemetryCarrier::Ephemeral);
    let (preflight, _runtime) =
        build_atomic_preflight_registry(&configured, &KhiveConfig::default())
            .expect("requested telemetry has its declared default");
    assert!(preflight.has_verb("telemetry.emit"));
}

#[cfg(feature = "pack-telemetry")]
#[test]
fn telemetry_atomic_exemption_has_no_vocabulary_hooks_or_admissible_verbs() {
    use khive_types::Pack;
    type Telemetry = khive_pack_telemetry::TelemetryPack;
    assert!(Telemetry::ENTITY_KINDS.is_empty());
    assert!(Telemetry::NOTE_KINDS.is_empty());
    assert!(Telemetry::EDGE_RULES.is_empty());
    assert!(Telemetry::ENTITY_TYPES.is_empty());
    // `pack_runtime_metadata!` returns these consts, so they belong to the exemption too.
    assert!(Telemetry::BRAIN_CONSUMER_KINDS.is_empty());
    assert!(Telemetry::NOTE_KIND_SPECS.is_empty());
    assert!(Telemetry::NOTE_EMBEDDING_POLICIES.is_empty());
    assert!(Telemetry::SCHEMA_PLAN.is_none());
    assert!(Telemetry::SCHEMA_COLUMN_ADDITIONS.is_empty());
    let implementation = include_str!("../../khive-pack-telemetry/src/pack.rs")
        .split_once("impl PackRuntime for TelemetryPack {")
        .expect("telemetry runtime implementation")
        .1;
    let implementation = implementation.split("\n}\n").next().unwrap();
    let mut methods = Vec::new();
    for line in implementation.lines() {
        let method = line
            .strip_prefix("    fn ")
            .or_else(|| line.strip_prefix("    async fn "))
            .or_else(|| line.strip_prefix("    khive_runtime::"));
        if let Some(method) = method {
            methods.push(method.split('(').next().unwrap());
        }
    }
    methods.sort_unstable();
    assert_eq!(
        methods,
        ["dispatch", "pack_runtime_metadata!", "validate_config"],
        "new runtime hooks require reconsidering the atomic telemetry exemption"
    );
    let cfg = RuntimeConfig {
        db_path: None,
        packs: vec!["kg".into(), "telemetry".into()],
        telemetry: khive_runtime::TelemetryConfig {
            default_carrier: Some(khive_runtime::TelemetryCarrier::Ephemeral),
            ..khive_runtime::TelemetryConfig::default()
        },
        ..RuntimeConfig::no_embeddings()
    };
    for handler in Telemetry::HANDLERS {
        let ops = vec![OpsFileEntry {
            tool: handler.name.into(),
            args: json!({}),
        }];
        assert!(
            !classify_atomic_preflight(&ops, &cfg, &KhiveConfig::default())
                .unwrap()
                .is_empty()
        );
    }
}

/// Seed a live GTD task note directly (bypassing `gtd.assign`'s handler,
/// which lives one crate over) with the flat properties shape
/// `load_task`/`task_status` expect: `kind = "task"`,
/// `properties.status`.
async fn seed_task(runtime: &KhiveRuntime, token: &NamespaceToken, status: &str) -> Uuid {
    let mut note = khive_storage::note::Note::new("local", "task", "atomic-gtd-test-task");
    note.name = Some("atomic-gtd-test-task".to_string());
    note.properties = Some(json!({"status": status, "priority": "p2"}));
    let id = note.id;
    runtime
        .notes(token)
        .expect("notes store")
        .upsert_note(note)
        .await
        .expect("seed task");
    id
}

fn task_properties(note: &khive_storage::note::Note) -> &Value {
    note.properties
        .as_ref()
        .expect("task must carry properties")
}

fn full_registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    let pack_names = atomic_preparation_pack_names(runtime.config());
    PackRegistry::register_packs(&pack_names, runtime.clone(), &mut builder)
        .expect("register packs");
    builder.build().expect("registry")
}

async fn assert_task_update_then_lifecycle_rolls_back(lifecycle_tool: &str) {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");

    let (update, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({"id": task_id.to_string(), "content": "new mirrored body"}),
    )
    .await
    .expect("prepare generic task update");
    let lifecycle_args = match lifecycle_tool {
        "gtd.transition" => json!({"id": task_id.to_string(), "status": "next"}),
        "gtd.complete" => json!({"id": task_id.to_string(), "result": "shipped"}),
        other => panic!("unexpected lifecycle tool {other}"),
    };
    let (lifecycle, _) = prepare_one(&runtime, &token, &registry, lifecycle_tool, &lifecycle_args)
        .await
        .unwrap_or_else(|error| panic!("prepare {lifecycle_tool}: {error}"));

    // Both plans were decided from the same pre-unit task snapshot. The
    // generic update runs first and advances its revision; the lifecycle
    // plan must then fail its exact-snapshot guard. The atomic runner must
    // roll the first write back instead of committing stale lifecycle
    // properties over its newly mirrored description.
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![update, lifecycle],
    )
    .await
    .expect("atomic runner");
    assert!(
        matches!(
            &outcome,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
            }
        ),
        "update -> {lifecycle_tool} must fail closed and roll back; got: {outcome:?}"
    );

    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(persisted.updated_at, before.updated_at);
    assert_eq!(persisted.content, "atomic-gtd-test-task");
    assert_eq!(
        task_properties(&persisted)
            .get("status")
            .and_then(Value::as_str),
        Some("inbox")
    );
    assert!(task_properties(&persisted).get("description").is_none());
}

#[tokio::test]
async fn atomic_generic_task_update_synchronizes_content_and_description() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);

    let (plan, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({"id": task_id.to_string(), "content": "atomic body"}),
    )
    .await
    .expect("prepare atomic task content update");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit content update");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

    let after_content = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(after_content.content, "atomic body");
    assert_eq!(
        task_properties(&after_content)
            .get("description")
            .and_then(Value::as_str),
        Some("atomic body")
    );

    let (plan, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "properties": {"description": "atomic property body"},
        }),
    )
    .await
    .expect("prepare atomic task description update");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit description update");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

    let after_description = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(after_description.content, "atomic property body");
    assert_eq!(
        task_properties(&after_description)
            .get("description")
            .and_then(Value::as_str),
        Some("atomic property body")
    );

    let (plan, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({"id": task_id.to_string(), "content": null, "properties": null}),
    )
    .await
    .expect("prepare atomic null/no-op patch");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit null/no-op patch");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    let after_null = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(after_null.content, "atomic property body");
    assert_eq!(
        task_properties(&after_null)
            .get("description")
            .and_then(Value::as_str),
        Some("atomic property body")
    );

    let (plan, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "properties": {"description": null},
        }),
    )
    .await
    .expect("prepare atomic description clear");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit description clear");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    let after_clear = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get task")
        .expect("task exists");
    assert_eq!(after_clear.content, "atomic-gtd-test-task");
    assert!(task_properties(&after_clear)
        .get("description")
        .is_some_and(Value::is_null));
}

#[tokio::test]
async fn atomic_task_update_rejects_title_clear_before_description_clear_can_write() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task before rejected update")
        .expect("task exists");

    let err = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "name": null,
            "properties": {"description": null},
        }),
    )
    .await
    .expect_err("a task title cannot be cleared under --atomic");
    assert!(
        err.to_string().contains("task title cannot be cleared"),
        "title-clear error must identify the task invariant; got: {err}"
    );

    let after = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task after rejected update")
        .expect("task exists");
    assert_eq!(after, before, "rejected preparation must not write");
}

#[tokio::test]
async fn atomic_task_update_rejects_lifecycle_properties_during_prepare() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task before rejected update")
        .expect("task exists");

    let err = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "properties": {"status": "done"},
        }),
    )
    .await
    .expect_err("atomic prepare must share lifecycle-owned property rejection");
    assert_eq!(
            err.to_string(),
            "invalid input: properties.status is lifecycle-owned and cannot be patched on a task; use gtd.transition for lifecycle changes or gtd.complete for terminal completion"
        );
    let after = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task after rejected update")
        .expect("task exists");
    assert_eq!(after, before, "rejected atomic prepare must not write");
}

async fn assert_issue_2675_atomic_derived_property_refused(field: &str, value: Value) {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "next").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task before refused preparation")
        .expect("task exists");

    let error = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "content": "body that must not be committed",
            "properties": {field: value, "priority": "p0"},
        }),
    )
    .await
    .expect_err("atomic preparation must refuse a derived task property");
    let message = error.to_string();
    assert!(
        message.contains(&format!("properties.{field}")),
        "refusal must name the supplied diagnostic field; got: {message}"
    );
    assert!(
        message.contains("properties.depends_on"),
        "refusal must direct callers to the writable dependency property; got: {message}"
    );
    let after = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task after refused preparation")
        .expect("task exists");
    assert_eq!(
            after, before,
            "refused preparation must preserve the entire task, including body, properties, timestamps and version"
        );
}

#[tokio::test]
async fn issue_2675_atomic_task_update_refuses_blocked_by() {
    assert_issue_2675_atomic_derived_property_refused(
        "blocked_by",
        json!(["11111111-1111-4111-8111-111111111111"]),
    )
    .await;
}

#[tokio::test]
async fn issue_2675_atomic_task_update_refuses_null_blocked_by() {
    assert_issue_2675_atomic_derived_property_refused("blocked_by", Value::Null).await;
}

#[tokio::test]
async fn issue_2675_atomic_task_update_refuses_dependency_state() {
    assert_issue_2675_atomic_derived_property_refused("dependency_state", json!("blocked")).await;
}

#[tokio::test]
async fn issue_2675_atomic_task_update_refuses_null_dependency_state() {
    assert_issue_2675_atomic_derived_property_refused("dependency_state", Value::Null).await;
}

#[tokio::test]
async fn issue_2675_atomic_task_update_refuses_actionable() {
    assert_issue_2675_atomic_derived_property_refused("actionable", json!(false)).await;
}

#[tokio::test]
async fn issue_2675_atomic_task_update_refuses_null_actionable() {
    assert_issue_2675_atomic_derived_property_refused("actionable", Value::Null).await;
}

#[tokio::test]
async fn issue_2675_atomic_task_update_applies_supported_properties() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "next").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task before supported preparation")
        .expect("task exists");

    let (plan, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "content": "supported atomic task body",
            "properties": {"planning_label": "reviewed", "priority": "p1"},
        }),
    )
    .await
    .expect("prepare legitimate task property update");
    let prepared = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task after supported preparation")
        .expect("task exists");
    assert_eq!(
        prepared, before,
        "preparation alone must not persist the patch"
    );

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit legitimate task property update");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    let after = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read committed task")
        .expect("task exists");
    assert_eq!(after.content, "supported atomic task body");
    assert_eq!(
        task_properties(&after)["description"],
        "supported atomic task body"
    );
    assert_eq!(task_properties(&after)["planning_label"], "reviewed");
    assert_eq!(task_properties(&after)["priority"], "p1");
    assert_eq!(task_properties(&after)["status"], "next");
    assert_ne!(
        after, before,
        "the positive control must actually persist its patch"
    );
}

#[tokio::test]
async fn atomic_task_update_checks_explicit_kind_before_running_task_hook() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);

    // The mirror fields deliberately conflict. If the task hook ran
    // before the explicit-kind check, its "must match" error would mask
    // canonical dispatch's expected NotFound result.
    let err = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "kind": "observation",
            "content": "one body",
            "properties": {"description": "another body"},
        }),
    )
    .await
    .expect_err("wrong explicit note kind must fail before task normalization");
    let message = err.to_string();
    assert!(message.contains("not found: note"), "got: {message}");
    assert!(
        !message.contains("must match"),
        "task hook must not run before kind mismatch rejection: {message}"
    );
}

#[tokio::test]
async fn atomic_task_update_guard_refuses_snapshot_changed_after_prepare() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let (plan, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({"id": task_id.to_string(), "content": "prepared body"}),
    )
    .await
    .expect("prepare task update");

    let mut concurrent = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    concurrent.content = "concurrent body".to_string();
    concurrent.properties =
        Some(json!({"status": "inbox", "priority": "p2", "description": "concurrent body"}));
    concurrent.updated_at = concurrent.updated_at.saturating_add(10);
    runtime
        .notes(&token)
        .expect("note store")
        .upsert_note(concurrent)
        .await
        .expect("concurrent write");

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("atomic runner");
    assert!(
        matches!(
            &outcome,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 0,
                failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
            }
        ),
        "stale prepared update must roll back; got: {outcome:?}"
    );
    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(persisted.content, "concurrent body");
    assert_eq!(
        task_properties(&persisted)
            .get("description")
            .and_then(Value::as_str),
        Some("concurrent body")
    );
}

#[tokio::test]
async fn repeated_atomic_task_updates_fail_closed_without_projected_state() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let (first, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({"id": task_id.to_string(), "content": "first body"}),
    )
    .await
    .expect("prepare first update");
    let (second, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "update",
        &json!({
            "id": task_id.to_string(),
            "properties": {"description": "second body"},
        }),
    )
    .await
    .expect("prepare second update");

    // Both plans intentionally share the same pre-unit snapshot. The
    // first advances its revision; the second must then trip its guard,
    // rolling the whole unit back instead of overwriting the first write
    // with an independently prepared full-row image.
    let outcome =
        khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![first, second])
            .await
            .expect("atomic runner");
    assert!(
        matches!(
            &outcome,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
            }
        ),
        "repeated target must fail closed; got: {outcome:?}"
    );
    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(persisted.content, "atomic-gtd-test-task");
    assert!(task_properties(&persisted).get("description").is_none());
}

#[tokio::test]
async fn atomic_generic_update_then_transition_rolls_back_on_shared_snapshot() {
    assert_task_update_then_lifecycle_rolls_back("gtd.transition").await;
}

#[tokio::test]
async fn atomic_generic_update_then_complete_rolls_back_on_shared_snapshot() {
    assert_task_update_then_lifecycle_rolls_back("gtd.complete").await;
}

/// Every op is prepared from the pre-unit snapshot. A same-status
/// transition prepared second must therefore revalidate that snapshot
/// after an earlier real transition on the same task, not silently
/// commit without checking its now-stale hypothesis.
#[tokio::test]
async fn atomic_transition_then_stale_noop_rolls_back_whole_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");

    let (transition, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "gtd.transition",
        &json!({"id": task_id.to_string(), "status": "next"}),
    )
    .await
    .expect("prepare real transition");
    let (stale_noop, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "gtd.transition",
        &json!({"id": task_id.to_string(), "status": "inbox"}),
    )
    .await
    .expect("prepare same-status transition from the original snapshot");

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![transition, stale_noop],
    )
    .await
    .expect("atomic runner");
    assert!(
        matches!(
            &outcome,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                failure: AtomicOpFailure::GuardFailed {
                    statement_label: Some(label),
                    observed: 0,
                    ..
                },
            } if label == "gtd_atomic_noop_assertion"
        ),
        "the stale no-op assertion must fail at op 1; got: {outcome:?}"
    );

    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists after rollback");
    assert_eq!(persisted.updated_at, before.updated_at);
    assert_eq!(task_properties(&persisted)["status"], "inbox");
}

#[tokio::test]
async fn atomic_version_only_change_then_stale_noop_rolls_back_whole_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .unwrap()
        .get_note(task_id)
        .await
        .unwrap()
        .unwrap();
    let (stale_noop, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "gtd.transition",
        &json!({"id": task_id.to_string(), "status": "inbox"}),
    )
    .await
    .unwrap();
    // Equal-value DML preserves the timestamp and status, but its trigger
    // advances version. Only the revision predicate catches this change.
    let equal_update = AtomicOpPlan::GtdTransition(GtdTransitionPlan::new(
        task_id,
        vec![PlanStatement {
            statement: SqlStatement {
                sql: "UPDATE notes SET content=content WHERE id=?1".into(),
                params: vec![SqlValue::Text(task_id.to_string())],
                label: Some("equal-value-note-update".into()),
            },
            guard: Some(AffectedRowGuard::exactly(1)),
        }],
        false,
        PostCommitEffect::None,
    ));
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![equal_update, stale_noop],
    )
    .await
    .unwrap();
    assert!(
        matches!(
            outcome,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                failure: AtomicOpFailure::GuardFailed { observed: 0, .. },
            }
        ),
        "stale version must invalidate the no-op: {outcome:?}"
    );
    let after = runtime
        .notes(&token)
        .unwrap()
        .get_note(task_id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(after, before);
}

/// A deletion earlier in the unit must likewise invalidate a no-op that
/// was prepared while the task still existed. The no-op's assertion is
/// what forces the delete to roll back as part of the whole unit.
#[tokio::test]
async fn atomic_delete_then_stale_noop_rolls_back_whole_unit() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let registry = full_registry(&runtime);
    let before = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");

    let (delete, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "delete",
        &json!({"id": task_id.to_string(), "hard": true}),
    )
    .await
    .expect("prepare hard delete");
    let (stale_noop, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "gtd.transition",
        &json!({"id": task_id.to_string(), "status": "inbox"}),
    )
    .await
    .expect("prepare same-status transition before delete applies");

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![delete, stale_noop],
    )
    .await
    .expect("atomic runner");
    assert!(
        matches!(
            &outcome,
            AtomicRunOutcome::RolledBack {
                failed_op_index: 1,
                failure: AtomicOpFailure::GuardFailed {
                    statement_label: Some(label),
                    observed: 0,
                    ..
                },
            } if label == "gtd_atomic_noop_assertion"
        ),
        "the delete must invalidate the no-op assertion at op 1; got: {outcome:?}"
    );

    let persisted = runtime
        .notes(&token)
        .expect("note store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("hard delete must roll back with the unit");
    assert_eq!(persisted, before);
}

/// ADR-099 B3: atomic `gtd.transition`
/// must persist a caller-supplied `note` as `properties.transition_note`
/// — parity with `khive-pack-gtd::handlers::handle_transition`
/// (handlers.rs:1028), which the pre-fix atomic prepare silently
/// dropped (it never read the `note` arg at all).
#[tokio::test]
async fn atomic_gtd_transition_persists_transition_note() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;

    let plan = prepare_gtd_transition(
        &runtime,
        &token,
        &json!({"id": task_id.to_string(), "status": "next", "note": "handed off to reviewer"}),
    )
    .await
    .expect("prepare transition");

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit ok");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

    let note = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must still exist");
    let props = task_properties(&note);
    assert_eq!(props.get("status").and_then(|v| v.as_str()), Some("next"));
    assert_eq!(
        props.get("transition_note").and_then(|v| v.as_str()),
        Some("handed off to reviewer"),
        "transition_note must be persisted into properties: {props:?}"
    );
}

/// ADR-099 B3: a secret in the
/// `gtd.transition` `note` arg must be REJECTED at prepare, before any
/// DB write — parity with `handle_transition`'s pre-write secret_gate
/// check (handlers.rs:988).
#[tokio::test]
async fn atomic_gtd_transition_rejects_secret_in_note_before_any_write() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "inbox").await;

    let err = prepare_gtd_transition(
        &runtime,
        &token,
        &json!({
            "id": task_id.to_string(),
            "status": "next",
            "note": "leaked key AKIAFAKEKEY1234567890",
        }),
    )
    .await
    .expect_err("a secret in the transition note must be rejected at prepare");
    assert!(
        err.to_string().contains("write blocked"),
        "expected a secret_gate rejection, got: {err}"
    );

    // No write must have happened: status is still "inbox".
    let note = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must still exist");
    assert_eq!(
        task_properties(&note)
            .get("status")
            .and_then(|v| v.as_str()),
        Some("inbox"),
        "rejected prepare must not have mutated the task"
    );
}

/// ADR-099 B3: a secret in the
/// `gtd.complete` `result` arg must be REJECTED at prepare, before any
/// DB write — parity with `handle_complete`'s pre-write secret_gate
/// check (handlers.rs:803); a clean result persists normally
/// (handlers.rs:832 parity).
#[tokio::test]
async fn atomic_gtd_complete_rejects_secret_in_result_and_persists_clean_result() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    // (a) secret in `result` rejected before any write.
    // Use the default inbox state to preserve direct-complete parity with
    // the accepted lifecycle table (inbox -> done is legal).
    let task_id = seed_task(&runtime, &token, "inbox").await;
    let err = prepare_gtd_complete(
        &runtime,
        &token,
        &json!({
            "id": task_id.to_string(),
            "result": "shipped using AKIAFAKEKEY1234567890",
        }),
    )
    .await
    .expect_err("a secret in the complete result must be rejected at prepare");
    assert!(
        err.to_string().contains("write blocked"),
        "expected a secret_gate rejection, got: {err}"
    );
    let note = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must still exist");
    assert_eq!(
        task_properties(&note)
            .get("status")
            .and_then(|v| v.as_str()),
        Some("inbox"),
        "rejected prepare must not have mutated the task"
    );

    // (b) a clean result persists.
    let plan = prepare_gtd_complete(
        &runtime,
        &token,
        &json!({"id": task_id.to_string(), "result": "shipped clean"}),
    )
    .await
    .expect("prepare complete");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit ok");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

    let note = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must still exist");
    let props = task_properties(&note);
    assert_eq!(props.get("status").and_then(|v| v.as_str()), Some("done"));
    assert_eq!(
        props.get("result").and_then(|v| v.as_str()),
        Some("shipped clean")
    );
}

/// GAP-3 (ADR-099 B3): atomic `gtd.transition(status="finished")`
/// on an active task must SUCCEED with the alias normalized to "done"
/// — parity with the `normalize_status`/`is_valid_status` gate in
/// `handle_transition` (handlers.rs:980-987). The pre-fix atomic
/// prepare ran `can_transition` on the raw unnormalized string, which
/// rejects "finished" outright (it is not itself a lifecycle state
/// name).
#[tokio::test]
async fn atomic_gtd_transition_normalizes_status_alias() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "active").await;

    let plan = prepare_gtd_transition(
        &runtime,
        &token,
        &json!({"id": task_id.to_string(), "status": "finished"}),
    )
    .await
    .expect("prepare transition with aliased status must succeed");

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit ok");
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));

    let note = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must still exist");
    assert_eq!(
        task_properties(&note)
            .get("status")
            .and_then(|v| v.as_str()),
        Some("done"),
        "the \"finished\" alias must normalize to \"done\", parity with canonical"
    );
}

async fn seed_dependency_pair(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    blocker_status: &str,
) -> (Uuid, Uuid) {
    let store = runtime.notes(token).unwrap();
    let blocker_id = seed_task(runtime, token, blocker_status).await;
    let task_id = seed_task(runtime, token, "next").await;
    let mut task = store.get_note(task_id).await.unwrap().unwrap();
    task.properties.as_mut().unwrap()["depends_on"] = json!([blocker_id.to_string()]);
    store.upsert_note(task).await.unwrap();
    (blocker_id, task_id)
}

async fn dependency_audit_count(runtime: &KhiveRuntime, task_id: Uuid) -> i64 {
    ensure_audit_schema(runtime).await;
    let mut reader = runtime.sql().reader().await.unwrap();
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT COUNT(*) AS count FROM gtd_lifecycle_audit WHERE note_id = ?1".into(),
            params: vec![SqlValue::Text(task_id.to_string())],
            label: None,
        })
        .await
        .unwrap();
    let Some(SqlValue::Integer(count)) = rows[0].get("count") else {
        panic!("audit count must be an integer");
    };
    *count
}

#[tokio::test]
async fn dependency_completion_atomic_prepare_refuses_before_writes() {
    for verb in ["gtd.complete", "gtd.transition"] {
        let runtime = scratch_runtime();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let registry = full_registry(&runtime);
        let store = runtime.notes(&token).unwrap();
        let (blocker_id, task_id) = seed_dependency_pair(&runtime, &token, "next").await;
        let before = store.get_note(task_id).await.unwrap().unwrap();
        assert_eq!(dependency_audit_count(&runtime, task_id).await, 0);
        let error = prepare_one(
            &runtime,
            &token,
            &registry,
            verb,
            &json!({"id": task_id.to_string(), "status": "done"}),
        )
        .await
        .expect_err("blocked preparation");
        let message = error.to_string();
        assert!(message.contains(&blocker_id.to_string()));
        assert!(message.contains("ignore_dependencies=true"));
        let Some(khive_runtime::RuntimeError::Khive(error)) =
            error.downcast_ref::<khive_runtime::RuntimeError>()
        else {
            panic!("structured dependency error must survive atomic prepare: {error:?}");
        };
        assert_eq!(error.kind(), khive_types::ErrorKind::Conflict);
        let details = error.details().unwrap();
        assert_eq!(details.get("reason"), Some("dependency_blocked"));
        assert_eq!(
            details.get("dependency_ids"),
            Some(blocker_id.to_string().as_str())
        );
        assert_eq!(store.get_note(task_id).await.unwrap().unwrap(), before);
        assert_eq!(dependency_audit_count(&runtime, task_id).await, 0);
    }
}

#[tokio::test]
async fn dependency_completion_atomic_override_is_audited() {
    for verb in ["gtd.complete", "gtd.transition"] {
        let runtime = scratch_runtime();
        let token = runtime.authorize(Namespace::local()).unwrap();
        let registry = full_registry(&runtime);
        let store = runtime.notes(&token).unwrap();
        let (_, task_id) = seed_dependency_pair(&runtime, &token, "next").await;
        let mut args =
            json!({"id": task_id.to_string(), "status": "done", "ignore_dependencies": "true"});
        assert!(prepare_one(&runtime, &token, &registry, verb, &args)
            .await
            .is_err());
        args["ignore_dependencies"] = json!(true);
        let (plan, _) = prepare_one(&runtime, &token, &registry, verb, &args)
            .await
            .unwrap();
        let outcome =
            khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                .await
                .unwrap();
        let AtomicRunOutcome::Committed { post_commit } = outcome else {
            panic!("expected committed override, got {outcome:?}");
        };
        let audit = apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
        assert_eq!(audit.get(&task_id), Some(&true));
        assert_eq!(dependency_audit_count(&runtime, task_id).await, 1);
        let mut reader = runtime.sql().reader().await.unwrap();
        let rows = reader
            .query_all(SqlStatement {
                sql: "SELECT from_state, to_state FROM gtd_lifecycle_audit WHERE note_id = ?1"
                    .into(),
                params: vec![SqlValue::Text(task_id.to_string())],
                label: None,
            })
            .await
            .unwrap();
        assert!(matches!(
            rows[0].get("from_state"),
            Some(SqlValue::Text(value)) if value == "next"
        ));
        assert!(matches!(
            rows[0].get("to_state"),
            Some(SqlValue::Text(value)) if value == "done"
        ));
        drop(reader);
        let after = store.get_note(task_id).await.unwrap().unwrap();
        assert_eq!(task_properties(&after)["status"], "done");
    }
}

#[tokio::test]
async fn dependency_completion_atomic_cancellation_and_ready_controls() {
    for verb in ["gtd.complete", "gtd.transition"] {
        for (blocker_status, target) in [("next", "cancelled"), ("done", "done")] {
            let runtime = scratch_runtime();
            let token = runtime.authorize(Namespace::local()).unwrap();
            let registry = full_registry(&runtime);
            let (_, task_id) = seed_dependency_pair(&runtime, &token, blocker_status).await;
            let (plan, _) = prepare_one(
                &runtime,
                &token,
                &registry,
                verb,
                &json!({"id": task_id.to_string(), "status": target}),
            )
            .await
            .unwrap();
            let outcome =
                khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
                    .await
                    .unwrap();
            assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
            let after = runtime
                .notes(&token)
                .unwrap()
                .get_note(task_id)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(task_properties(&after)["status"], target);
        }
    }
}

#[tokio::test]
async fn dependency_completion_atomic_guard_uses_preparation_time_readiness() {
    let runtime = scratch_runtime();
    let token = runtime.authorize(Namespace::local()).unwrap();
    let registry = full_registry(&runtime);
    let (blocker_id, task_id) = seed_dependency_pair(&runtime, &token, "done").await;
    let (delete, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "delete",
        &json!({"id": blocker_id.to_string(), "hard": true}),
    )
    .await
    .unwrap();
    let (complete, _) = prepare_one(
        &runtime,
        &token,
        &registry,
        "gtd.complete",
        &json!({"id": task_id.to_string()}),
    )
    .await
    .unwrap();

    // Readiness is an admission check; commit revalidates only the task snapshot.
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(
        runtime.sql().as_ref(),
        vec![delete, complete],
    )
    .await
    .unwrap();
    assert!(matches!(outcome, AtomicRunOutcome::Committed { .. }));
    let store = runtime.notes(&token).unwrap();
    assert!(store.get_note(blocker_id).await.unwrap().is_none());
    let after = store.get_note(task_id).await.unwrap().unwrap();
    assert_eq!(task_properties(&after)["status"], "done");
}

/// GAP-6 (ADR-099 B3): an idempotent atomic `gtd.transition` (current ==
/// target after `normalize_status`) must perform no persisted mutation.
/// Its guarded no-effect statement only revalidates the snapshot. This
/// matches canonical for both note-omitting and note-bearing requests. The
/// pre-fix atomic prepare only special-cased `current != target` inside
/// its `can_transition` guard, so a current==target call fell through
/// to an unconditional `UPDATE` that bumped `updated_at` for nothing.
#[tokio::test]
async fn atomic_gtd_transition_idempotent_noop_performs_no_mutation() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "next").await;

    let before = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must exist");
    let updated_at_before = before.updated_at;

    let plan = prepare_gtd_transition(
        &runtime,
        &token,
        &json!({"id": task_id.to_string(), "status": "next"}),
    )
    .await
    .expect("prepare idempotent transition must succeed (no-op, not an error)");
    let AtomicOpPlan::GtdTransition(noop_plan) = &plan else {
        panic!("expected gtd transition plan")
    };
    assert!(noop_plan.is_idempotent_noop());
    assert_eq!(noop_plan.statements().len(), 1);

    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit ok");
    let post_commit = match outcome {
        AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("idempotent no-op must still succeed as Committed, got {other:?}"),
    };
    assert!(
            post_commit.as_slice().is_empty(),
            "an idempotent no-op transition must produce no post-commit effect (no audit row \
             either — canonical no-note no-ops never reach their own audit helper): {post_commit:?}"
        );

    let after = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("get_note")
        .expect("task must still exist");
    assert_eq!(
        after.updated_at, updated_at_before,
        "an idempotent transition must not change updated_at"
    );
}

#[tokio::test]
async fn atomic_same_status_transition_with_note_is_refused_at_prepare() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "next").await;
    let before = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    let args = json!({
        "id": task_id.to_string(),
        "status": "next",
        "note": "canonical-only note event",
    });

    // The refusal lives in the shared decide step, so the prepared-plan path
    // inherits it and the unit never reaches the runner. This test used to
    // assert that the plan committed while persisting nothing, which is the
    // behaviour the refusal replaces: nothing persisted is right, reporting
    // it as a success was not.
    let err = prepare_gtd_transition(&runtime, &token, &args)
        .await
        .expect_err("a note on a same-status transition must be refused at prepare");
    assert!(
        err.to_string().contains("already in status"),
        "the refusal must name the condition: {err}"
    );

    let after = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(after.updated_at, before.updated_at);
    assert_eq!(
        after.version, before.version,
        "a refused prepare must not mutate the note revision"
    );
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap()
    );
    assert!(task_properties(&after).get("transition_note").is_none());
}

/// The bare form still commits as a mutation-free assertion, and the result
/// this path renders must carry the same shape as the pack handler's: the
/// explanation under `reason`, never under the caller's `note` key, and
/// `note_recorded` present rather than inferred from a missing field.
#[tokio::test]
async fn atomic_same_status_transition_without_note_renders_reason_and_note_recorded() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    let task_id = seed_task(&runtime, &token, "next").await;
    let before = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    let args = json!({"id": task_id.to_string(), "status": "next"});

    let plan = prepare_gtd_transition(&runtime, &token, &args)
        .await
        .expect("a bare same-status prepare still succeeds");
    let outcome =
        khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan.clone()])
            .await
            .expect("commit guarded no-op assertion");
    let post_commit = match outcome {
        AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected committed no-op, got {other:?}"),
    };
    assert!(post_commit.as_slice().is_empty());

    let audit_outcomes = HashMap::new();
    let result = build_op_result(
        &runtime,
        &token,
        "gtd.transition",
        &args,
        &args,
        &plan,
        &audit_outcomes,
    )
    .await
    .expect("render no-op result");
    assert_eq!(result["transitioned"], false);
    assert_eq!(result["note_recorded"], false);
    assert_eq!(result["reason"], "already in target status");
    assert!(
        result.get("note").is_none(),
        "the explanation belongs under `reason`; `note` is the caller's key: {result:?}"
    );
    assert!(result.get("audit_persisted").is_none());

    let after = runtime
        .notes(&token)
        .expect("notes store")
        .get_note(task_id)
        .await
        .expect("read task")
        .expect("task exists");
    assert_eq!(
        serde_json::to_value(&after).unwrap(),
        serde_json::to_value(&before).unwrap(),
        "the assertion must leave the row untouched"
    );
}

/// GAP-5 (ADR-099 B3): a committed atomic `gtd.transition` AND a
/// committed atomic `gtd.complete` must each write a
/// `gtd_lifecycle_audit` row — parity with `handle_transition`/
/// `handle_complete`'s best-effort `ensure_audit_schema` +
/// `write_audit_record_with_status` calls. The
/// pre-fix atomic prepare wrote no audit row at all.
#[tokio::test]
async fn atomic_gtd_transition_and_complete_write_lifecycle_audit_rows() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");

    // (a) transition inbox -> next, with a transition note.
    let transition_task = seed_task(&runtime, &token, "inbox").await;
    let plan = prepare_gtd_transition(
        &runtime,
        &token,
        &json!({"id": transition_task.to_string(), "status": "next", "note": "audit me"}),
    )
    .await
    .expect("prepare transition");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit ok");
    let post_commit = match outcome {
        AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    let transition_audit =
        apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
    assert_eq!(transition_audit.get(&transition_task), Some(&true));

    // (b) complete next -> done.
    let complete_task = seed_task(&runtime, &token, "next").await;
    let plan = prepare_gtd_complete(
        &runtime,
        &token,
        &json!({"id": complete_task.to_string(), "result": "shipped"}),
    )
    .await
    .expect("prepare complete");
    let outcome = khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan])
        .await
        .expect("commit ok");
    let post_commit = match outcome {
        AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected Committed, got {other:?}"),
    };
    let complete_audit =
        apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
    assert_eq!(complete_audit.get(&complete_task), Some(&true));

    let mut reader = runtime.sql().reader().await.expect("reader");
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT note_id, from_state, to_state, namespace FROM gtd_lifecycle_audit \
                      ORDER BY at ASC"
                .to_string(),
            params: vec![],
            label: Some("test-gtd-audit-rows".to_string()),
        })
        .await
        .expect("query gtd_lifecycle_audit");
    assert_eq!(
        rows.len(),
        2,
        "both the transition and the complete must each write exactly one audit row: {rows:?}"
    );

    let transition_task_str = transition_task.to_string();
    let complete_task_str = complete_task.to_string();

    let transition_row_present = rows.iter().any(|r| {
        matches!(r.get("note_id"), Some(SqlValue::Text(id)) if id == &transition_task_str)
            && matches!(r.get("from_state"), Some(SqlValue::Text(s)) if s == "inbox")
            && matches!(r.get("to_state"), Some(SqlValue::Text(s)) if s == "next")
            && matches!(r.get("namespace"), Some(SqlValue::Text(ns)) if ns == "local")
    });
    assert!(
        transition_row_present,
        "expected an audit row for the transition op: {rows:?}"
    );

    let complete_row_present = rows.iter().any(|r| {
        matches!(r.get("note_id"), Some(SqlValue::Text(id)) if id == &complete_task_str)
            && matches!(r.get("from_state"), Some(SqlValue::Text(s)) if s == "next")
            && matches!(r.get("to_state"), Some(SqlValue::Text(s)) if s == "done")
            && matches!(r.get("namespace"), Some(SqlValue::Text(ns)) if ns == "local")
    });
    assert!(
        complete_row_present,
        "expected an audit row for the complete op: {rows:?}"
    );
}

#[tokio::test]
async fn atomic_complete_result_reports_failed_audit_append() {
    let runtime = scratch_runtime();
    let token = runtime
        .authorize(Namespace::parse("local").expect("ns"))
        .expect("authorize");
    ensure_audit_schema(&runtime).await;
    {
        let mut writer = runtime.sql().writer().await.expect("writer");
        writer
            .execute_script(
                "CREATE TRIGGER reject_atomic_gtd_audit_insert \
                     BEFORE INSERT ON gtd_lifecycle_audit \
                     BEGIN SELECT RAISE(FAIL, 'forced atomic audit failure'); END;"
                    .to_string(),
            )
            .await
            .expect("failure-injection trigger");
    }

    let task_id = seed_task(&runtime, &token, "next").await;
    let args = json!({"id": task_id.to_string(), "result": "shipped"});
    let plan = prepare_gtd_complete(&runtime, &token, &args)
        .await
        .expect("prepare complete");
    let outcome =
        khive_runtime::atomic_runner::run_atomic_unit(runtime.sql().as_ref(), vec![plan.clone()])
            .await
            .expect("commit domain write");
    let post_commit = match outcome {
        AtomicRunOutcome::Committed { post_commit } => post_commit,
        other => panic!("expected committed completion, got {other:?}"),
    };
    let audit_outcomes =
        apply_gtd_audit_post_commit_effects(&runtime, post_commit.as_slice()).await;
    assert_eq!(audit_outcomes.get(&task_id), Some(&false));

    let result = build_op_result(
        &runtime,
        &token,
        "gtd.complete",
        &args,
        &args,
        &plan,
        &audit_outcomes,
    )
    .await
    .expect("render committed completion");
    assert_eq!(result["completed"], true);
    assert_eq!(result["audit_persisted"], false);
}

include!("atomic_project_origin_tests.rs");
include!("atomic_apply/parameter_alias_tests.rs");
