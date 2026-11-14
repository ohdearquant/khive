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

#[tokio::test]
async fn schedule_create_alias_type_refusals_mask_credentials_without_writes() {
    use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
    use khive_runtime::RuntimeError;
    use khive_storage::SqlStatement;
    use serde_json::{json, Value};

    async fn snapshot(runtime: &KhiveRuntime) -> Vec<Value> {
        let mut reader = runtime.sql().reader().await.unwrap();
        let mut tables = Vec::new();
        for table in ["entities", "notes", "graph_edges", "events"] {
            let mut rows: Vec<_> = reader
                .query_all(SqlStatement::new(format!("SELECT * FROM {table}"), vec![]))
                .await
                .unwrap()
                .into_iter()
                .map(|row| serde_json::to_value(row).unwrap())
                .collect();
            rows.sort_by_cached_key(Value::to_string);
            tables.push(json!({"table": table, "rows": rows}));
        }
        assert!(!tables[0]["rows"].as_array().unwrap().is_empty());
        assert!(!tables[1]["rows"].as_array().unwrap().is_empty());
        tables
    }

    let (registry, runtime) = build_registry();
    registry
        .dispatch(
            "create",
            json!({"kind": "concept", "name": "Retained replay baseline", "skip_dedup_check": true}),
        )
        .await
        .expect("valid entity baseline");
    let scheduled = registry
        .dispatch(
            "schedule.schedule",
            json!({
                "action": "create(kind=\"concept\", entity_kind=\"concept\", note_kind=null, name=\"Accepted replay\")",
                "at": "2099-06-01T10:00:00Z",
            }),
        )
        .await
        .expect("valid aliases still schedule an action");
    assert_eq!(scheduled["status"], "pending");
    let stored = registry
        .dispatch("get", json!({"id": scheduled["full_id"]}))
        .await
        .expect("actual persisted schedule baseline");
    assert_eq!(stored["kind"], "scheduled_event");
    assert_eq!(stored["properties"]["status"], "pending");
    assert_eq!(
        stored["content"],
        "create(kind=\"concept\", entity_kind=\"concept\", note_kind=null, name=\"Accepted replay\")"
    );
    let before = snapshot(&runtime).await;
    let secret = format!("sk-proj-{}", "A".repeat(80));

    for field in ["entity_kind", "note_kind"] {
        for (value, masked) in [
            (json!(17), false),
            (json!({"label": "ordinary"}), false),
            (json!(["ordinary"]), false),
            (json!({"credential": secret.clone()}), true),
            (json!([{"credential": secret.clone()}]), true),
        ] {
            let serialized = value.to_string();
            let shown = mask_for_redaction_surface(RedactionSurface::GateProbe, &serialized);
            if masked {
                assert!(serialized.contains(&secret));
                assert!(
                    !shown.contains(&secret),
                    "canonical masker recognizes the fixture"
                );
                assert!(shown.contains("***MASKED***"));
            } else {
                assert_eq!(shown, serialized, "ordinary JSON diagnostic is retained");
            }
            let action =
                format!("create(kind=\"concept\", {field}={serialized}, name=\"Refused replay\")");
            let error = registry
                .dispatch(
                    "schedule.schedule",
                    json!({"action": action, "at": "2099-06-01T10:00:00Z"}),
                )
                .await
                .expect_err("wrong alias type must refuse before scheduling");
            let RuntimeError::InvalidInput(message) = error else {
                panic!("wrong schedule refusal kind: {error:?}");
            };
            assert_eq!(
                message,
                format!("create: `{field}` must be a string or null; got {shown}"),
                "actual alias refusal before generic error projection"
            );
            assert!(!message.contains(&secret));
            if masked {
                assert!(message.contains("***MASKED***"));
            }
            assert_eq!(
                snapshot(&runtime).await,
                before,
                "refusal changed persisted rows"
            );
        }
    }
}
