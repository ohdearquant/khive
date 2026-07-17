//! `$prev` path resolution: segment splitting and JSON value traversal.

use serde_json::Value;

/// One object-field or array-index segment in a `$prev` path.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
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
            segments.push(PathSegment::Field(remaining));
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
    }
}
