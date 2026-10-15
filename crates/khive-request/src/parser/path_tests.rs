use super::*;
use serde_json::json;

/// A bracket index the DSL grammar accepts as syntax (the parser only
/// checks for ASCII digits, not usize range) but that overflows `usize`
/// at resolution time: `split_path` cannot parse it as an `Index`, so it
/// becomes `Malformed`, and `apply_path_segment` always misses on that
/// variant — parser acceptance (P108/P117 in `DSL_WIRE_CONTRACT.md`)
/// does not guarantee a resolution hit.
#[test]
fn oversized_bracket_index_is_malformed_and_always_misses() {
    let path = "[99999999999999999999]";
    let segments = split_path(path);
    assert_eq!(segments, vec![PathSegment::Malformed(path)]);

    let value = json!([1, 2, 3]);
    let seg = segments.into_iter().next().unwrap();
    assert_eq!(apply_path_segment(&value, seg), None);
}

#[test]
fn dotted_field_segments_resolve_by_key_or_miss() {
    let segments = split_path("a.b");
    assert_eq!(
        segments,
        vec![PathSegment::Field("a"), PathSegment::Field("b")]
    );

    let value = json!({"a": {"b": 42}});
    let mut cur = &value;
    for seg in segments {
        cur = apply_path_segment(cur, seg).expect("field must resolve");
    }
    assert_eq!(cur, &json!(42));

    let missing = split_path("missing");
    let seg = missing.into_iter().next().unwrap();
    assert_eq!(apply_path_segment(&value, seg), None);
}
