//! `$prev` path resolution: segment splitting and JSON value traversal.

use serde_json::Value;

/// One object-field or array-index segment in a `$prev` path.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
    /// Bracket syntax that is not a valid non-negative-integer index, e.g.
    /// `[abc]` or `[-1]`. Always a lookup miss (see `apply_path_segment`);
    /// kept distinct from `Field` so callers building error messages can
    /// tell "no such field" apart from "this isn't a supported path form".
    Malformed(&'a str),
}

/// Splits a `$prev` path into field and index segments.
pub(crate) fn split_path(path: &str) -> Vec<PathSegment<'_>> {
    let mut segments = Vec::new();
    let mut remaining = path;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix('[') {
            if let Some(close) = rest.find(']') {
                let index_str = &rest[..close];
                if let Ok(idx) = index_str.parse::<usize>() {
                    segments.push(PathSegment::Index(idx));
                    remaining = &rest[close + 1..];
                    remaining = remaining.strip_prefix('.').unwrap_or(remaining);
                    continue;
                }
            }
            // Preserve malformed quoted paths as a lookup miss, never a partial match.
            segments.push(PathSegment::Malformed(remaining));
            break;
        }
        let end = remaining.find(['.', '[']).unwrap_or(remaining.len());
        let field = &remaining[..end];
        if !field.is_empty() {
            segments.push(PathSegment::Field(field));
        }
        remaining = &remaining[end..];
        remaining = remaining.strip_prefix('.').unwrap_or(remaining);
    }
    segments
}

/// Applies one field lookup or array index, returning `None` on mismatch.
pub(crate) fn apply_path_segment<'a>(cur: &'a Value, seg: PathSegment<'_>) -> Option<&'a Value> {
    match seg {
        PathSegment::Field(key) => cur.get(key),
        PathSegment::Index(idx) => cur.as_array()?.get(idx),
        PathSegment::Malformed(_) => None,
    }
}

#[cfg(test)]
mod tests {
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
}
