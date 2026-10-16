// ── schedule.agenda Agent response preserves properties.trigger_at ────────────

/// Schedule agenda in default Agent mode must not compact `trigger_at` inside
/// `properties` — the full ISO-8601 string must round-trip verbatim.
#[tokio::test]
async fn schedule_agenda_agent_preserves_properties_trigger_at_verbatim() -> anyhow::Result<()> {
    let client = connect_schedule().await?;
    let trigger_at = "2099-01-01T00:00:00Z";

    ok_one(
        &client,
        &format!(r#"schedule.remind(content="agent trigger_at fidelity", at="{trigger_at}")"#),
    )
    .await?;

    // Default Agent mode (no `presentation` key).
    let result = call(&client, "request", json!({"ops": "schedule.agenda()"})).await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "schedule.agenda must succeed: {first}"
    );

    let events = first["result"]["events"].as_array().expect("events array");
    assert!(!events.is_empty(), "agenda must have at least one event");
    let actual = events[0]["properties"]["trigger_at"].as_str().unwrap_or("");
    assert_eq!(
        actual, trigger_at,
        "trigger_at inside properties must be preserved verbatim in Agent mode"
    );
    assert_ne!(
        actual, "2099-01-01T00:00",
        "trigger_at must not be truncated to minute granularity"
    );
    Ok(())
}

// ── #871: schedule.remind create-response preserves top-level trigger_at ─────
//
// These two tests deliberately cover `schedule.remind` only; `schedule` (the
// verb-dispatch scheduling sibling) shares the same create-response shape and
// the same presentation-layer fix, but an equivalent end-to-end case for it
// is out of scope here to keep this regression narrow (#871).

/// `schedule.remind`'s create response returns `trigger_at` as a top-level
/// convenience field (sibling to `id`/`full_id`), not nested under
/// `"properties"`. In default Agent mode this must round-trip verbatim,
/// offset intact — the humanize layer previously discarded the offset (and
/// seconds) by minute-truncating or relativizing this top-level field, even
/// though the underlying offset-to-UTC conversion used for the relative-time
/// comparison itself was already correct (#871).
#[tokio::test]
async fn schedule_remind_agent_preserves_top_level_trigger_at_with_offset() -> anyhow::Result<()> {
    let client = connect_schedule().await?;
    // Far enough in the future (relative to "now") to land outside the 24h
    // relative-render window, so pre-fix this would have been silently
    // minute-truncated (dropping the seconds and the "-04:00" offset)
    // instead of round-tripping the exact ISO string.
    let trigger_at = "2099-06-15T19:00:00-04:00";

    let result = call(
        &client,
        "request",
        json!({"ops": format!(
            r#"schedule.remind(content="agent create-response trigger_at fidelity", at="{trigger_at}")"#
        )}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let first = &body["results"][0];
    assert_eq!(
        first["ok"],
        json!(true),
        "schedule.remind must succeed: {first}"
    );

    let actual = first["result"]["trigger_at"].as_str().unwrap_or("");
    assert_eq!(
        actual, trigger_at,
        "top-level trigger_at in the create response must be preserved verbatim, offset intact, in Agent mode"
    );
    Ok(())
}

/// A `Z`-suffixed UTC `at` input must likewise round-trip unchanged through
/// the create-response humanize layer (#871 regression coverage). Bare
/// offset-less input (no `Z`/`±HH:MM` suffix) is not a case here: it is
/// rejected upstream as non-RFC-3339 by `schedule.remind`'s own validation
/// (`crates/khive-pack-schedule/src/handlers.rs` `validate_at`), so it never
/// reaches this presentation-layer fix.
#[tokio::test]
async fn schedule_remind_agent_preserves_top_level_trigger_at_utc() -> anyhow::Result<()> {
    let client = connect_schedule().await?;

    let utc = "2099-06-15T23:00:00Z";
    let result = call(
        &client,
        "request",
        json!({"ops": format!(
            r#"schedule.remind(content="agent create-response trigger_at utc", at="{utc}")"#
        )}),
    )
    .await?;
    let body: Value = serde_json::from_str(&first_text(&result))?;
    let actual = body["results"][0]["result"]["trigger_at"]
        .as_str()
        .unwrap_or("");
    assert_eq!(actual, utc, "UTC trigger_at must round-trip unchanged");

    Ok(())
}
