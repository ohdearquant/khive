//! `$prev` path resolution: segment splitting and JSON value traversal.

use serde_json::Value;

/// A single segment in a `$prev` path — either a field name or an array index.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
}

/// Split a dotted path with optional bracket array indices into `PathSegment`s.
pub(crate) fn split_path(path: &str) -> Vec<PathSegment<'_>> {
    let mut segments = Vec::new();
    let mut remaining = path;
    while !remaining.is_empty() {
        if let Some(rest) = remaining.strip_prefix('[') {
            // Array index: `[N]...`
            if let Some(close) = rest.find(']') {
                let index_str = &rest[..close];
                if let Ok(idx) = index_str.parse::<usize>() {
                    segments.push(PathSegment::Index(idx));
                    remaining = &rest[close + 1..];
                    // Strip leading '.' before next segment, if any.
                    remaining = remaining.strip_prefix('.').unwrap_or(remaining);
                    continue;
                }
            }
            // Malformed index — treat whole remainder as field (will fail lookup).
            segments.push(PathSegment::Field(remaining));
            break;
        }
        // Field name — up to next '.' or '['.
        let end = remaining.find(['.', '[']).unwrap_or(remaining.len());
        let field = &remaining[..end];
        if !field.is_empty() {
            segments.push(PathSegment::Field(field));
        }
        remaining = &remaining[end..];
        // Strip leading '.' separator.
        remaining = remaining.strip_prefix('.').unwrap_or(remaining);
    }
    segments
}

/// Apply one path segment to a JSON value — field lookup or array index.
pub(crate) fn apply_path_segment<'a>(cur: &'a Value, seg: PathSegment<'_>) -> Option<&'a Value> {
    match seg {
        PathSegment::Field(key) => cur.get(key),
        PathSegment::Index(idx) => cur.as_array()?.get(idx),
    }
}
