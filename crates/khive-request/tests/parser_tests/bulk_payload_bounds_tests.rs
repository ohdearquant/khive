/// Build a JSON-form `create` op carrying `count` bulk `items`, each with a
/// `description` of `desc_len` bytes, mirrors the ADR-038 1000-item bulk
/// contract (`crates/khive-pack-kg/src/handler_defs.rs` `items` param).
fn bulk_create_payload(count: usize, desc_len: usize) -> String {
    let items: Vec<serde_json::Value> = (0..count)
        .map(|i| {
            json!({
                "kind": "concept",
                "name": format!("item-{i}"),
                "description": "d".repeat(desc_len),
            })
        })
        .collect();
    let payload = json!([{ "tool": "create", "args": { "kind": "concept", "items": items } }]);
    payload.to_string()
}

#[test]
fn bulk_create_1000_items_at_220_byte_descriptions_parses() {
    // ADR-038 promises up to 1000 items per bulk `create`; 220-byte
    // descriptions land the realistic payload around 276 KB, comfortably
    // under MAX_OPS_INPUT_LEN (1 MiB) but well over the old 256 KiB cap this
    // finding raised the limit to fix.
    let input = bulk_create_payload(1000, 220);
    assert!(
        input.len() > 256 * 1024 && input.len() < MAX_OPS_INPUT_LEN,
        "expected a realistic ~276 KB bulk payload, got {} bytes",
        input.len()
    );
    let parsed = parse_request(&input)
        .unwrap_or_else(|e| panic!("realistic 1000-item bulk create must parse: {e}"));
    assert_eq!(parsed.ops.len(), 1);
    assert_eq!(parsed.ops[0].tool, "create");
}

#[test]
fn oversized_bulk_create_payload_rejected_before_parsing() {
    // Same bulk-create shape, but padded well past MAX_OPS_INPUT_LEN, the
    // cap must still reject pathological input even in the documented-contract
    // shape it was raised to accommodate.
    let input = bulk_create_payload(1000, 2000);
    assert!(
        input.len() > MAX_OPS_INPUT_LEN,
        "test payload must exceed the cap, got {} bytes",
        input.len()
    );
    let err = parse_request(&input).unwrap_err();
    assert!(
        matches!(
            &err,
            DslError::InputTooLarge { len, max }
            if *len > MAX_OPS_INPUT_LEN && *max == MAX_OPS_INPUT_LEN
        ),
        "expected InputTooLarge, got {err:?}"
    );
}

#[test]
fn realistic_nested_payload_well_under_limit_still_parses() {
    // Regression guard (test plan item 5): a `kg.propose`-shaped nested
    // changeset payload, several levels deep, must keep parsing under the
    // new depth guard exactly as it did before this change.
    let r = req(
        r#"propose(title="t", description="d", changeset=[{"op":"create","fields":{"tags":["a","b"],"props":{"nested":{"deep":[1,2,3]}}}}])"#,
    );
    assert_eq!(r.mode, ExecutionMode::Single);
}

// ── ArgValue::find_prev_failure — $prev substitution error diagnosis ───────
//
// These exercise `find_prev_failure` directly against hand-built `ArgValue`s
// rather than through `parse_request`, because the DSL parser (both the
// unquoted `$prev[..]` grammar above and `quoted_prev_path_is_valid` for the
// quoted-string form) already rejects malformed bracket syntax before an
// `ArgValue::PrevRef` is ever constructed — that parser-level rejection is
// covered in `khive-mcp`'s integration suite
// (`test_prev_malformed_index_rejected_at_parse_time`). The substituter's own
// `PrevFailure::Unsupported` defense is otherwise unreachable through the
// public DSL surface today, so it is verified here, directly, as
// defense-in-depth against a future caller that builds an `ArgValue` some
// other way (e.g. a second parser front-end).

use khive_request::PrevFailure;
use serde_json::Value as JsonValue;
