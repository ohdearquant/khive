// ── entity_type replay-parity validation ─────────────
//
// `entity_type` was never validated in the replay mirror at all: the
// singleton path (`validate_conditional_requirements`) checked `name` but
// never touched `entity_type`; the bulk path (`ScheduleBulkCreateEntryCheck`)
// parsed `entity_type` but never validated it. The real KG `create` handler
// validates `entity_type` in both paths
// (`khive-pack-kg/src/handlers/create.rs:137-140` bulk, `:310` singleton)
// via `khive-pack-kg/src/entity_type_registry.rs`'s cross-kind rejection.

/// `entity_type="paper"` is a `Document` subtype
/// (`khive-pack-kg/src/entity_type_registry.rs` `BUILTIN_DEFS`), not `Concept`
/// — the real KG `create` handler deterministically rejects
/// `create(kind="concept", entity_type="paper", ...)`. `schedule.schedule`
/// must reject the same action at write time, not silently accept it and
/// fail hours later at replay.
#[tokio::test]
async fn schedule_schedule_rejects_create_with_cross_kind_entity_type_singleton() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", entity_type=\"paper\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();

    let msg = err.to_string();
    assert!(
        msg.contains("entity_type") && msg.contains("paper"),
        "entity_type=\"paper\" is a Document subtype, not Concept; \
         schedule.schedule must reject it the same way KG create does; got: {msg}"
    );

    // Confirm this really does mirror the live KG handler's own rejection.
    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({"kind": "concept", "entity_type": "paper", "name": "x"}),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("entity_type"),
        "sanity: the live KG create handler must also reject this cross-kind entity_type; \
         got: {kg_err}"
    );
}

#[tokio::test]
async fn schedule_schedule_accepts_create_with_valid_entity_type_singleton() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"document\", entity_type=\"paper\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("entity_type=\"paper\" is a valid Document subtype");

    // Sanity: the live KG create handler must also accept this pairing.
    registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "document", "entity_type": "paper", "name": "kg-doc-paper"
            }),
        )
        .await
        .expect("sanity: the live KG create handler must accept this pairing too");
}

/// PR #925: the tests above only ever exercise the builtin
/// `paper` subtype, so they would stay green even if schedule silently
/// reverted to the builtin-only `EntityTypeRegistry::global()` instead of
/// the composed `EntityTypeRegistry::with_extra(registry.all_entity_types())`
/// registry. With a real type-declaring pack (git, `adr`) loaded alongside
/// schedule, `schedule.schedule` must accept the pack-declared subtype in a
/// singleton `create`, and the live KG `create` handler must accept it too.
#[tokio::test]
async fn schedule_schedule_accepts_pack_declared_entity_type_singleton_with_git_loaded() {
    let (registry, _rt) = build_registry_with_git();

    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"document\", entity_type=\"adr\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule must accept the git-pack-declared adr Document subtype");

    // Sanity: the live KG create handler must also accept this pairing.
    registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "document", "entity_type": "adr", "name": "kg-doc-adr"
            }),
        )
        .await
        .expect("sanity: the live KG create handler must accept adr with GitPack loaded");
}

/// Same pack-declared-type acceptance, but inside a bulk `create(items=[...])`
/// entry (PR #925).
#[tokio::test]
async fn schedule_schedule_accepts_pack_declared_entity_type_bulk_with_git_loaded() {
    let (registry, _rt) = build_registry_with_git();

    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(items=[{\"kind\":\"document\",\"entity_type\":\"adr\",\"name\":\"x\"}])",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("schedule bulk create must accept the git-pack-declared adr Document subtype");
}

/// Retains the no-declaring-pack rejection case alongside the new
/// acceptance coverage above (PR #925): without a pack that
/// declares `adr`, schedule must reject it exactly like the live KG handler
/// does when GitPack isn't loaded.
#[tokio::test]
async fn schedule_schedule_rejects_pack_declared_entity_type_without_declaring_pack_loaded() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"document\", entity_type=\"adr\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("adr"),
        "error must name the rejected value: {err}"
    );
}

/// `entity_type` resolution must follow aliases, not just exact canonical
/// names — "algo" is an accepted alias for the canonical Concept subtype
/// "algorithm" (`khive-pack-kg/src/entity_type_registry.rs` `BUILTIN_DEFS`).
#[tokio::test]
async fn schedule_schedule_accepts_entity_type_alias_singleton() {
    let (registry, _rt) = build_registry();

    registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", entity_type=\"algo\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .expect("entity_type alias \"algo\" must resolve to \"algorithm\"");

    registry
        .dispatch(
            "create",
            serde_json::json!({"kind": "concept", "entity_type": "algo", "name": "kg-algo"}),
        )
        .await
        .expect("sanity: the live KG create handler must accept the algo alias too");
}

#[tokio::test]
async fn schedule_schedule_rejects_completely_unknown_entity_type() {
    let (registry, _rt) = build_registry();

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": "create(kind=\"concept\", entity_type=\"totally_made_up_xyz\", name=\"x\")",
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string().contains("unknown entity_type"),
        "a completely unregistered entity_type must be rejected; got: {err}"
    );

    let kg_err = registry
        .dispatch(
            "create",
            serde_json::json!({
                "kind": "concept", "entity_type": "totally_made_up_xyz", "name": "x"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        kg_err.to_string().contains("unknown entity_type"),
        "sanity: the live KG create handler must also reject this; got: {kg_err}"
    );
}
