// ── ADR-078: OutputFormat tests ───────────────────────────────────────────

/// (a) verbose json preserves full shape (no field dropped, no transformation).
#[test]
fn format_verbose_json_preserves_full_shape() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "properties": {"k": "v"},
        "title": "test"
    });
    let rendered = render_format(v.clone(), OutputFormat::Json, PresentationMode::Verbose);
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    // full_id must not be dropped in json mode.
    assert!(
        parsed.get("full_id").is_some(),
        "json mode must keep full_id"
    );
    // namespace must NOT be elided in json mode.
    assert_eq!(
        parsed.get("namespace").and_then(Value::as_str),
        Some("local")
    );
    // properties must NOT be deduped in json mode.
    assert!(parsed.get("properties").is_some());
}

/// (a-agent-json) Agent JSON applies the Machine-scope redundancy
/// reduction: `namespace`/duplicate-`properties` elision, but `full_id`
/// is retained — it is the caller's chaining handle, not a view drop
/// (ADR-078 Amendment 3, corrected).
#[test]
fn format_agent_json_drops_redundancy() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "title": "test",
        "status": "next",
        "properties": {"status": "next", "additive": true}
    });
    let rendered = render_format(v, OutputFormat::Json, PresentationMode::Agent);
    let parsed: Value = serde_json::from_str(&rendered).unwrap();
    assert!(
        parsed.get("full_id").is_some(),
        "Agent JSON must keep full_id: it is the machine contract's chaining handle"
    );
    assert!(parsed.get("namespace").is_none());
    assert_eq!(parsed["status"], json!("next"));
    assert!(parsed["properties"].get("status").is_none());
    assert_eq!(parsed["properties"]["additive"], json!(true));
}

/// Agent JSON (Machine scope) keeps `full_id` while dropping
/// `namespace="local"` and duplicate `properties`; Auto (View scope) on
/// the same record still drops all three — the view scope is unchanged
/// by Amendment 3's correction.
#[test]
fn agent_json_keeps_full_id_while_auto_still_drops_it() {
    let v = json!({
        "full_id": "a1b2c3d4-e5f6-7890-abcd-ef1234567890",
        "namespace": "local",
        "title": "test",
        "properties": {"title": "test", "additive": true}
    });

    let json_rendered = render_format(v.clone(), OutputFormat::Json, PresentationMode::Agent);
    let json_parsed: Value = serde_json::from_str(&json_rendered).unwrap();
    assert!(
        json_parsed.get("full_id").is_some(),
        "Agent JSON must keep full_id"
    );
    assert!(
        json_parsed.get("namespace").is_none(),
        "Agent JSON must elide namespace=\"local\""
    );
    assert!(
        json_parsed["properties"].get("title").is_none(),
        "Agent JSON must dedupe a duplicate properties entry"
    );
    assert_eq!(json_parsed["properties"]["additive"], json!(true));

    let auto_rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        !auto_rendered.contains("full_id"),
        "Auto (view scope) must still drop full_id: {auto_rendered}"
    );
    assert!(
        !auto_rendered.contains("\"namespace\""),
        "Auto must still elide namespace=\"local\": {auto_rendered}"
    );
}
