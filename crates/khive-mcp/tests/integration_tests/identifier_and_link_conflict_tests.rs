// ── P-C1: full_id is never shortened in Agent mode ───────────────────────────

/// `get` is AlwaysVerbose (ADR-045 §6) — returns full 36-char UUIDs even
/// in default (Agent) mode.  The response is now flat (P-H2): `{kind, id, ...}`
/// rather than the old wrapped `{kind, data: {...}}` shape.
#[tokio::test]
async fn get_returns_flat_shape_with_full_uuid_in_default_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    // ok_one uses presentation=verbose, so this gives us the full UUID.
    let created = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="FlatGetEntity")"#,
    )
    .await?;
    let full_id = created["id"].as_str().unwrap().to_string();
    assert_eq!(full_id.len(), 36, "verbose create must have full UUID");

    // Fetch via `get` WITHOUT specifying presentation — default is Agent mode.
    // `get` is AlwaysVerbose so it returns the full UUID regardless.
    let result = call(
        &client,
        "request",
        json!({"ops": format!(r#"get(id="{full_id}")"#)}),
        // Deliberately no `presentation` key — defaults to Agent.
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "get must succeed: {first}");

    // P-H2: `get` now returns a flat object — `kind` is at the top level
    // (the entity_kind, e.g. "concept"), NOT nested as `result.data.kind`.
    // There is no `data` wrapper.
    let entity = &first["result"];
    assert_eq!(
        entity["kind"], "concept",
        "get flat response must have top-level kind=concept (entity_kind); got {entity}"
    );
    assert!(
        entity.get("data").is_none(),
        "get must NOT wrap in {{data: ...}}; got {entity}"
    );
    // `get` is AlwaysVerbose: full 36-char UUID in `id` even in Agent mode.
    let returned_id = entity["id"].as_str().unwrap_or("");
    assert_eq!(
        returned_id.len(),
        36,
        "get (AlwaysVerbose) must return full 36-char UUID in id; got {returned_id:?}"
    );
    assert_eq!(
        returned_id, full_id,
        "returned id must match the created entity's full UUID"
    );
    Ok(())
}

/// ADR-045 §6 C2: `link` is `AlwaysVerbose` — edge IDs needed for follow-up.
///
/// At scale, two edges can share the same 8-char prefix (birthday collision ~65K
/// edges), so shortening the returned edge ID in agent mode violates ADR-045 §6
/// "Edge IDs needed for follow-up." `link` must return full 36-char UUIDs in
/// all modes including agent.
#[tokio::test]
async fn link_is_always_verbose_returns_full_uuids_in_agent_mode() -> anyhow::Result<()> {
    let client = connect().await?;

    // Create two entities via ok_one (verbose) to get full UUIDs for linking.
    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LinkVerboseA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="LinkVerboseB")"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();
    assert_eq!(a_id.len(), 36);
    assert_eq!(b_id.len(), 36);

    // Call `link` in default Agent mode (no presentation key).
    // AlwaysVerbose policy: source_id/target_id must be full 36-char UUIDs
    // even in agent mode (ADR-045 §6 C2 fix).
    let result = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends")"#
            )
            // No `presentation` key — defaults to Agent, but AlwaysVerbose overrides.
        }),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(first["ok"], true, "link must succeed: {first}");

    let edge = &first["result"];
    let src = edge["source_id"].as_str().unwrap_or("");
    let tgt = edge["target_id"].as_str().unwrap_or("");
    assert_eq!(
        src.len(),
        36,
        "link source_id must be full 36-char UUID in Agent mode (AlwaysVerbose); got {src:?}"
    );
    assert_eq!(
        tgt.len(),
        36,
        "link target_id must be full 36-char UUID in Agent mode (AlwaysVerbose); got {tgt:?}"
    );
    // The edge's own id must also be full UUID in agent mode.
    let edge_id = edge["id"].as_str().unwrap_or("");
    assert_eq!(
        edge_id.len(),
        // Edge IDs are LinkId which may serialize as full UUID; accept 36-char.
        // The AlwaysVerbose policy ensures no shortening occurs.
        36,
        "link edge id must be full UUID in Agent mode (AlwaysVerbose); got {edge_id:?}"
    );

    // Verify: explicit presentation=verbose also returns full 36-char UUIDs.
    let result_verbose = call(
        &client,
        "request",
        json!({
            "ops": format!(
                r#"link(source_id="{a_id}", target_id="{b_id}", relation="variant_of")"#
            ),
            "presentation": "verbose"
        }),
    )
    .await?;
    let body_v: Value = serde_json::from_str(&first_text(&result_verbose))?;
    let first_v = &body_v["results"][0];
    assert_eq!(first_v["ok"], true, "verbose link must succeed: {first_v}");
    let edge_v = &first_v["result"];
    assert_eq!(
        edge_v["source_id"].as_str().unwrap_or("").len(),
        36,
        "link source_id must be 36-char in verbose mode"
    );
    Ok(())
}

// ── #469 regression: bulk/symmetric link write-key conflict preflight ────────

/// #469: a bulk `link(links=[...])` op and a singleton `link` op that target
/// the same natural edge key must both be rejected with per-op conflict
/// errors by MCP preflight (`khive_request::write_keys_for_op_pub`) before
/// either dispatches, instead of racing through storage where SQLite's
/// `ON CONFLICT DO UPDATE` would let the last write silently win.
#[tokio::test]
async fn parallel_link_bulk_conflict_is_rejected_before_storage_race() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="BulkConflictA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="BulkConflictB")"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    let ops = format!(
        r#"[link(links=[{{"source_id":"{a_id}","target_id":"{b_id}","relation":"extends","weight":0.1}}]), link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.9)]"#
    );
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;

    assert_eq!(body["summary"]["total"], json!(2));
    assert_eq!(
        body["summary"]["failed"],
        json!(2),
        "both conflicting link ops must fail preflight: {body}"
    );
    for i in 0..2 {
        let entry = &body["results"][i];
        assert_eq!(entry["ok"], json!(false), "op #{i} must fail: {entry}");
        let err = entry["error"]["message"].as_str().unwrap_or("");
        assert!(
            err.contains("conflict"),
            "op #{i} error must mention conflict: {entry}"
        );
    }

    // No edge should have been written — neither op reached storage.
    let get_result = call(
        &client,
        "request",
        json!({
            "ops": format!(r#"list(kind="entity_edge", source_id="{a_id}", target_id="{b_id}")"#),
            "presentation": "verbose"
        }),
    )
    .await;
    if let Ok(get_result) = get_result {
        if let Ok(get_body) = serde_json::from_str::<Value>(&first_text(&get_result)) {
            if get_body["results"][0]["ok"] == json!(true) {
                let items = get_body["results"][0]["result"]["items"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                assert!(
                    items.is_empty(),
                    "no edge should have been written from conflicting preflight-rejected ops: {items:?}"
                );
            }
        }
    }
    Ok(())
}

/// #469: reversed singleton symmetric links (`link(A,B,competes_with)` and
/// `link(B,A,competes_with)`) must canonicalize to the same natural edge key
/// and be rejected as conflicting, matching storage's own endpoint-order
/// canonicalization for symmetric relations.
#[tokio::test]
async fn parallel_reversed_symmetric_link_conflict_is_rejected() -> anyhow::Result<()> {
    let client = connect().await?;

    let a = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="SymConflictA")"#,
    )
    .await?;
    let b = ok_one(
        &client,
        r#"create(kind="entity", entity_kind="concept", name="SymConflictB")"#,
    )
    .await?;
    let a_id = a["id"].as_str().unwrap().to_string();
    let b_id = b["id"].as_str().unwrap().to_string();

    let ops = format!(
        r#"[link(source_id="{a_id}", target_id="{b_id}", relation="competes_with"), link(source_id="{b_id}", target_id="{a_id}", relation="competes_with")]"#
    );
    let result = call(
        &client,
        "request",
        json!({"ops": ops, "presentation": "verbose"}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;

    assert_eq!(body["summary"]["total"], json!(2));
    assert_eq!(
        body["summary"]["failed"],
        json!(2),
        "reversed symmetric link ops must both fail preflight: {body}"
    );
    for i in 0..2 {
        let entry = &body["results"][i];
        assert_eq!(entry["ok"], json!(false), "op #{i} must fail: {entry}");
        let err = entry["error"]["message"].as_str().unwrap_or("");
        assert!(
            err.contains("conflict"),
            "op #{i} error must mention conflict: {entry}"
        );
    }
    Ok(())
}
