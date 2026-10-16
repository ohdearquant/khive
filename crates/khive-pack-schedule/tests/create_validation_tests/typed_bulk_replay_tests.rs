/// Same cross-kind rejection, but inside a bulk `create(items=[...])` entry
/// — the bulk validator must apply the same `entity_type` validation
/// per-entry that the real KG bulk create handler applies.
#[tokio::test]
async fn schedule_schedule_rejects_create_bulk_item_with_cross_kind_entity_type() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(items=[{\"kind\":\"concept\",\"entity_type\":\"paper\",\"name\":\"x\"}])",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("items[0]") && msg.contains("entity_type"),
        "bulk items[] entity_type must be validated per-entry the same way \
         the real KG bulk create handler does; got: {msg}"
    );

    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({
                "items": [{"kind": "concept", "entity_type": "paper", "name": "x"}]
            }),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("entity_type"),
        "sanity: the live KG bulk create handler must also reject this cross-kind \
         entity_type; got: {kg_err}"
    );
}

#[tokio::test]
async fn schedule_schedule_accepts_create_bulk_item_with_valid_entity_type() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(items=[{\"kind\":\"document\",\"entity_type\":\"paper\",\"name\":\"x\"}])",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("bulk entity_type=\"paper\" under kind=\"document\" is valid");

    registry
        .dispatch(
            "create",
            serde_json::json!({
                "items": [{
                    "kind": "document", "entity_type": "paper", "name": "kg-bulk-doc-paper"
                }]
            }),
        )
        .await
        .expect("sanity: the live KG bulk create handler must accept this pairing too");
}

/// `entity_type` validation short-circuits on the *kind name itself*: the
/// real KG `validate_entity_type` (`khive-pack-kg/src/handlers/common.rs`)
/// parses the kind into the base 8-variant `khive_types::EntityKind` before
/// ever consulting the subtype registry, and `resource` (ADR-048, pack-owned,
/// declared only in `khive-pack-kg::vocab::EntityKind`) has no variant in
/// that base enum — so ANY non-null `entity_type` under `kind="resource"` is
/// rejected with "unknown entity kind", regardless of the entity_type value.
/// Verified live via `kkernel exec` against a scratch DB. `schedule.schedule`
/// must reproduce this exact short-circuit rather than being "more lenient"
/// than the real handler. That would be the GAP-1 false-accept failure mode.
#[tokio::test]
async fn schedule_schedule_rejects_entity_type_under_resource_kind_singleton() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"resource\", entity_type=\"atom_type\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("unknown entity kind"),
        "resource-kind entities can never carry an entity_type (base EntityKind has no \
         Resource variant); got: {msg}"
    );

    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({"kind": "resource", "entity_type": "atom_type", "name": "x"}),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("unknown entity kind"),
        "sanity: the live KG create handler must reject this the same way; got: {kg_err}"
    );
}

#[tokio::test]
async fn schedule_schedule_rejects_entity_type_under_resource_kind_bulk() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(items=[{\"kind\":\"resource\",\"entity_type\":\"atom_type\",\"name\":\"x\"}])",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("items[0]") && msg.contains("unknown entity kind"),
        "resource-kind entities can never carry an entity_type, in bulk items[] either; \
         got: {msg}"
    );

    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({
                "items": [{"kind": "resource", "entity_type": "atom_type", "name": "x"}]
            }),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("unknown entity kind"),
        "sanity: the live KG bulk create handler must reject this the same way; got: {kg_err}"
    );
}

/// Self-updating parity check (mitigates the exact drift that produced this
/// gap): iterate every real `khive_types::EntityKind` and every
/// subtype the LIVE `khive-pack-kg::EntityTypeRegistry` actually has
/// registered (via the dev-dependency), and assert `schedule.schedule`'s
/// replay validation (`validate_entity_type_for_replay` in `handlers.rs`,
/// which resolves subtypes through the shared `khive_types::EntityTypeRegistry`)
/// accepts every one of them. If `BUILTIN_DEFS` ever gains a new subtype that
/// the shared registry doesn't resolve, this test starts failing in CI
/// instead of silently reproducing a false-rejection bug like the one
/// found before.
#[tokio::test]
async fn schedule_accepts_every_registered_entity_subtype() {
    use khive_pack_kg::EntityTypeRegistry;
    use khive_types::EntityKind;

    let (registry, _rt) = build_registry();
    let live = EntityTypeRegistry::global();

    for kind in EntityKind::ALL {
        let valid_types = live.valid_types_for(kind);
        if valid_types == "(none registered)" {
            continue;
        }
        for type_name in valid_types.split(" | ") {
            let action = format!(
                "create(kind=\"{}\", entity_type=\"{type_name}\", name=\"parity\")",
                kind.name()
            );
            registry
                .dispatch(
                    "schedule.schedule",
                    serde_json::json!({ "action": action, "at": "2099-06-01T10:00:00Z" }),
                )
                .await
                .unwrap_or_else(|e| {
                    panic!(
                        "parity gap: the live EntityTypeRegistry accepts entity_type=\
                         {type_name:?} for kind={:?}, but schedule.schedule's replay \
                         validation rejected it — validate_entity_type_for_replay in \
                         handlers.rs is out of sync with khive_types::EntityTypeRegistry \
                         for this entry: {e}",
                        kind.name()
                    )
                });
        }
    }
}
