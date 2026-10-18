// ── entity_kind / kind alias replay-parity ───────────
//
// `classify_create_kind` had no alias resolution at all — neither the base
// `khive_types::EntityKind` aliases (e.g. "paper" -> document) nor the
// pack-local `resource`-kind aliases (e.g. "atom" -> resource, ADR-048) — so
// `schedule.schedule` wrongly rejected legitimate KG-accepted kind spellings.
// `canonical_entity_kind_for_replay` (the legacy `entity_kind=` path) had the
// same gap for the pack-local alias set specifically.

/// (alias, expected canonical kind) pairs spanning both the base 8
/// `khive_types::EntityKind` aliases and the pack-local `resource`-kind
/// aliases (ADR-048, `khive-pack-kg/src/vocab.rs`).
const ENTITY_KIND_ALIAS_CASES: &[(&str, &str)] = &[
    ("doc", "document"),
    ("paper", "document"),
    ("data", "dataset"),
    ("benchmark", "dataset"),
    ("repo", "project"),
    ("crate", "project"),
    ("library", "project"),
    ("lib", "project"),
    ("author", "person"),
    ("researcher", "person"),
    ("organization", "org"),
    ("organisation", "org"),
    ("lab", "org"),
    ("company", "org"),
    ("art", "artifact"),
    ("svc", "service"),
    ("atom", "resource"),
    ("runbook", "resource"),
    ("template", "resource"),
    ("prompt", "resource"),
    ("skill", "resource"),
    ("tool", "resource"),
];

/// Every alias the real KG `create` handler accepts as a top-level `kind`
/// must also be accepted by `schedule.schedule`, matched against a direct
/// `create` dispatch on the same live registry as the oracle.
#[tokio::test]
async fn schedule_schedule_accepts_every_kg_entity_kind_alias_singleton() {
    let (registry, _rt) = build_registry();

    for &(alias, canonical) in ENTITY_KIND_ALIAS_CASES {
        let action = format!("create(kind=\"{alias}\", name=\"x-{alias}\")");
        registry
            .dispatch(
                "schedule.schedule",
                serde_json::json!({ "action": action, "at": "2099-06-01T10:00:00Z" }),
            )
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "alias {alias:?} (-> {canonical}) must be accepted by \
                     schedule.schedule; got: {e}"
                )
            });

        // Sanity: the live KG create handler must also accept this alias.
        registry
            .dispatch(
                "create",
                serde_json::json!({"kind": alias, "name": format!("kg-{alias}")}),
            )
            .await
            .unwrap_or_else(|e| {
                panic!("sanity: the live KG create handler must accept kind={alias:?}; got: {e}")
            });
    }
}

/// Same alias matrix, but inside a bulk `create(items=[...])` entry.
#[tokio::test]
async fn schedule_schedule_accepts_every_kg_entity_kind_alias_bulk() {
    let (registry, _rt) = build_registry();

    for &(alias, canonical) in ENTITY_KIND_ALIAS_CASES {
        let action = format!("create(items=[{{\"kind\":\"{alias}\",\"name\":\"x-{alias}\"}}])");
        registry
            .dispatch(
                "schedule.schedule",
                serde_json::json!({ "action": action, "at": "2099-06-01T10:00:00Z" }),
            )
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "bulk alias {alias:?} (-> {canonical}) must be accepted by \
                     schedule.schedule; got: {e}"
                )
            });

        // Sanity: the live KG bulk create handler must also accept this alias.
        registry
            .dispatch(
                "create",
                serde_json::json!({
                    "items": [{"kind": alias, "name": format!("kg-bulk-{alias}")}]
                }),
            )
            .await
            .unwrap_or_else(|e| {
                panic!(
                    "sanity: the live KG bulk create handler must accept kind={alias:?}; \
                     got: {e}"
                )
            });
    }
}

#[tokio::test]
async fn schedule_schedule_rejects_create_bulk_over_1000_entries() {
    let (registry, _rt) = build_registry();

    let entries: Vec<String> = (0..1001)
        .map(|i| format!("{{\"kind\":\"org\",\"name\":\"x{i}\"}}"))
        .collect();
    let action = format!("create(items=[{}])", entries.join(","));

    let err = registry
        .dispatch(
            "schedule.schedule",
            serde_json::json!({
                "action": action,
                "at": "2099-06-01T10:00:00Z"
            }),
        )
        .await
        .unwrap_err();
    assert!(
        err.to_string()
            .contains("bulk create limited to 1000 entries per request"),
        "schedule must mirror KG's 1000-entry bulk cap at write time; got: {err}"
    );

    let items: Vec<serde_json::Value> = (0..1001)
        .map(|i| serde_json::json!({"kind": "org", "name": format!("x{i}")}))
        .collect();
    let kg_err = registry
        .dispatch("create", serde_json::json!({ "items": items }))
        .await
        .unwrap_err();
    assert!(
        kg_err
            .to_string()
            .contains("bulk create limited to 1000 entries per request"),
        "sanity: the live KG bulk create handler must reject 1001 entries; got: {kg_err}"
    );
}
