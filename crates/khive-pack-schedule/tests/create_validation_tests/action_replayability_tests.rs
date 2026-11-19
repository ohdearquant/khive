// ── Issue #461: schedule.schedule write-time replayability ──────────────────

#[tokio::test]
async fn schedule_schedule_rejects_bare_schedule_pack_action() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "remind(content=\"hello\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("not registered") || msg.contains("pack-prefixed"),
        "#461: bare unqualified schedule-pack verb must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_internal_subhandler_action() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                // `comm.ingest` is registered and introspectable, but its
                // HandlerDef is Visibility::Subhandler. Delaying execution
                // must not turn a public caller into an operator caller.
                "action": "comm.ingest()",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("internal subhandler") && msg.contains("comm.ingest"),
        "registered internal handlers must be rejected before intent is stored; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_chain_with_prev() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "stats() | create(kind=\"entity\", name=$prev.id)",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("chain") || msg.contains("$prev"),
        "#461: chained actions with $prev must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_missing_required_replay_args() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "schedule.remind(content=\"hello\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("missing") && msg.contains("at"),
        "#461: missing required replay arg `at` must be rejected; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_create_missing_kind_and_items() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("kind") && msg.contains("items"),
        "#461: create() missing both kind and items must be rejected at write time; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_accepts_create_with_kind() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("#461: create() with kind present must be accepted");

    assert_eq!(result["status"], "pending");
}

/// Regression: `create(kind="concept",
/// entity_kind="person", name="x")` is accepted by `schedule.schedule` before
/// this fix, yet the real `create` handler
/// (`khive-pack-kg/src/handlers/create.rs` via
/// `handlers::common::reconcile_specific`) deterministically rejects it —
/// `kind="concept"` and `entity_kind="person"` contradict. Schedule-time
/// validation must reject this the same way, not merely require presence of
/// `entity_kind`.
#[tokio::test]
async fn schedule_schedule_rejects_create_with_contradicting_kind_and_entity_kind() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", entity_kind=\"person\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("contradicts"),
        "#461/#462: a granular kind that contradicts entity_kind must be rejected at \
         schedule time, mirroring KG's reconcile_specific; got: {msg}"
    );

    // Confirm this really does mirror the live KG handler's own rejection.
    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({"kind": "concept", "entity_kind": "person", "name": "x"}),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("contradicts"),
        "sanity: the live KG create handler must also reject this contradiction; got: {kg_err}"
    );
}

/// Same contradiction, but inside a bulk `create(items=[...])` entry — the
/// bulk validator (`validate_create_bulk_items`) must apply the same
/// reconciliation per-entry that `khive-pack-kg`'s bulk create path applies.
#[tokio::test]
async fn schedule_schedule_rejects_create_bulk_item_with_contradicting_kind_and_entity_kind() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(items=[{\"kind\":\"concept\",\"entity_kind\":\"person\",\"name\":\"x\"}])",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("items[0]") && msg.contains("contradicts"),
        "#461/#462: a bulk items[] entry with a kind/entity_kind contradiction must be \
         rejected at schedule time; got: {msg}"
    );

    // Confirm this really does mirror the live KG bulk create handler's own rejection.
    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({
                "items": [{"kind": "concept", "entity_kind": "person", "name": "x"}]
            }),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("contradicts"),
        "sanity: the live KG bulk create handler must also reject this contradiction; \
         got: {kg_err}"
    );
}

/// An invalid legacy `entity_kind` value (unknown to both the base
/// `khive_types::EntityKind` parser and the registry's merged vocabulary)
/// must be rejected at schedule time, not merely accepted because
/// `entity_kind` was present.
#[tokio::test]
async fn schedule_schedule_rejects_create_with_invalid_entity_kind() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"entity\", entity_kind=\"not_a_real_kind\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("unknown entity_kind"),
        "an invalid legacy entity_kind must be rejected at schedule time; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_business_namespace_arg() {
    let (registry, _rt) = build_registry_with_brain();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "brain.bind(profile_id=\"p\", namespace=\"team-a\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("namespace"),
        "#461: scheduled action with a business `namespace` arg must be rejected \
         (replay always overwrites it with the firing event's routing namespace); got: {msg}"
    );
}

/// Issue #462: omitting `namespace` from the stored action does NOT make
/// `brain.bind` replayable. `dispatch_action` (`kkernel/src/pending_events.rs`)
/// unconditionally injects the firing event's routing namespace into every
/// op's args at trigger time, and the registry passes it straight through to
/// any handler that declares `namespace` as a param — so an *omitted*
/// `namespace` (which would otherwise default to `brain.bind`'s wildcard
/// `"*"`) is silently rebound to the event's routing namespace on replay.
/// This is exactly as unsafe as an explicitly-stored `namespace` arg, so
/// `schedule.schedule` must reject any handler whose schema declares
/// `namespace`, regardless of whether the stored args include the key.
#[tokio::test]
async fn schedule_schedule_rejects_verb_declaring_namespace_even_when_omitted() {
    let (registry, _rt) = build_registry_with_brain();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "brain.bind(profile_id=\"p\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("brain.bind") && msg.contains("namespace"),
        "#462: brain.bind must be rejected even without an explicit `namespace` arg — the \
         verb declares `namespace` as a business param, and replay would inject the event's \
         routing namespace regardless of what (if anything) was stored; got: {msg}"
    );
}

#[tokio::test]
async fn schedule_schedule_accepts_exact_replayable_single_action() {
    let (registry, _rt) = build_registry();

    let result = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "stats()",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("#461: exact registered zero-arg verb call must be accepted");

    assert_eq!(result["status"], "pending");
}
