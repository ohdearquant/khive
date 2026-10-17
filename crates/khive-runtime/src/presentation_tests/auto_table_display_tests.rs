/// (b1) homogeneous record array → markdown table with header + separator + rows.
#[test]
fn format_auto_homogeneous_array_renders_markdown_table() {
    let v = json!([
        {"id": "abc", "title": "First"},
        {"id": "def", "title": "Second"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(rendered.starts_with('|'), "must start with |");
    assert!(
        rendered.contains("| id |") || rendered.contains("| id"),
        "must have id column"
    );
    assert!(rendered.contains("title"), "must have title column");
    assert!(rendered.contains("|---|"), "must have separator row");
    assert!(rendered.contains("abc"), "must have first row data");
    assert!(rendered.contains("Second"), "must have second row data");
}

/// (b2) single record → compact JSON, lossless (kv-block renderer removed).
#[test]
fn format_auto_single_record_renders_compact_json() {
    let v = json!({"id": "abc", "title": "Hello World"});
    let rendered = render_format(v.clone(), OutputFormat::Auto, PresentationMode::Agent);
    let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
    assert_eq!(parsed, v, "single record must round-trip losslessly");
    assert!(
        !rendered.starts_with('|'),
        "single record must not be a markdown table"
    );
}

/// (b2-lossless) a single record with a large payload field (a compose
/// briefing's `markdown`) arrives whole — the former kv-block renderer
/// truncated it.
#[test]
fn format_auto_single_record_large_payload_survives_whole() {
    let briefing = "line one\nline two\n".repeat(500); // ~9KB, newlines included
    let v = json!({"id": "abc", "markdown": briefing.clone()});
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    let parsed: Value = serde_json::from_str(&rendered).expect("must be valid JSON");
    assert_eq!(
        parsed.get("markdown").and_then(Value::as_str),
        Some(briefing.as_str()),
        "large payload field must survive untruncated"
    );
}

/// Envelope siblings survive table rendering: a `query`-style page whose
/// `has_more` is dropped reads as complete — fail-open.
#[test]
fn format_auto_table_preserves_sibling_scalars() {
    let v = json!({
        "results": [
            {"id": "abc", "title": "First"},
            {"id": "def", "title": "Second"}
        ],
        "has_more": true,
        "offset": 20,
        "page_size": 2,
        "truncated": false
    });
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(rendered.contains("| id"), "records must render as a table");
    assert!(
        rendered.contains("has_more: true"),
        "has_more sibling must survive: {rendered}"
    );
    assert!(
        rendered.contains("offset: 20"),
        "offset sibling must survive"
    );
    assert!(
        rendered.contains("page_size: 2"),
        "page_size sibling must survive"
    );
    assert!(
        rendered.contains("truncated: false"),
        "truncated sibling must survive"
    );
}

/// Nested table cells render a constant elision marker, never
/// truncated pseudo-JSON that reads as data while missing fields.
#[test]
fn format_auto_nested_cell_renders_elision_marker() {
    let big_nested: Value = json!({"k": "v".repeat(300), "other": {"deep": true}});
    let v = json!([
        {"id": "abc", "properties": big_nested},
        {"id": "def", "properties": {"x": 1}}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains("{…}"),
        "nested object cell must render the elision marker: {rendered}"
    );
    assert!(
        !rendered.contains("..."),
        "no truncated pseudo-JSON in nested cells"
    );
}

/// Arrays of scalars (tags) still render as compact JSON in cells.
#[test]
fn format_auto_scalar_array_cell_renders_compact_json() {
    let v = json!([
        {"id": "abc", "tags": ["lesson", "khive"]},
        {"id": "def", "tags": ["adr"]}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains(r#"["lesson","khive"]"#),
        "scalar array cell must be compact JSON: {rendered}"
    );
}

/// Identity/decision columns are exempt from truncation: a truncated
/// title cannot be selected on, a truncated signature is unusable.
#[test]
fn format_auto_identity_columns_not_truncated() {
    let long_title = "T".repeat(300);
    let long_note = "N".repeat(300);
    let v = json!([
        {"id": "abc", "title": long_title.clone(), "note": long_note.clone()},
        {"id": "def", "title": "short", "note": "short"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Agent);
    assert!(
        rendered.contains(&long_title),
        "title column must not be truncated"
    );
    assert!(
        !rendered.contains(&long_note),
        "non-exempt column must still truncate"
    );
}

/// Verbose presentation disables cell truncation entirely (ADR-078 §3a).
#[test]
fn format_auto_verbose_disables_truncation() {
    let long_note = "N".repeat(300);
    let v = json!([
        {"id": "abc", "note": long_note.clone()},
        {"id": "def", "note": "short"}
    ]);
    let rendered = render_format(v, OutputFormat::Auto, PresentationMode::Verbose);
    assert!(
        rendered.contains(&long_note),
        "verbose must render full cell content"
    );
}

/// The pre-pass hoists named scalar payload fields (`trigger_at`, `due`,
/// `status`) out of `properties` so they can surface as table columns —
/// a scheduled event carries them only inside `properties`.
#[test]
fn table_path_hoists_payload_scalars_to_columns() {
    // The hoist is table-path only (ADR-078 Amendment 2): two scheduled
    // events whose trigger_at/status live inside `properties` must gain
    // top-level columns in the rendered table.
    let record = |n: u32| {
        json!({
            "id": format!("evt-{n}"),
            "properties": {
                "trigger_at": "2026-09-01T14:00:00-04:00",
                "status": "pending",
                "dispatch_receipt": {"state": "succeeded"}
            }
        })
    };
    let out = render_format(
        json!([record(1), record(2)]),
        OutputFormat::Auto,
        PresentationMode::Agent,
    );
    let header = out.lines().next().expect("table header");
    assert!(
        header.contains("trigger_at") && header.contains("status"),
        "hoisted scalars must appear as table columns, got header: {header}"
    );
    assert!(
        out.contains("2026-09-01T14:00:00-04:00") && out.contains("pending"),
        "hoisted values must render in cells:\n{out}"
    );
}
