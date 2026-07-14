//! Low-level scanner helpers: string scanning, `$prev` detection, char labels.

use serde_json::Value;

use crate::types::{ArgValue, DslError, ParsedOp, NESTING_DEPTH_LIMIT};

/// Scans a JSON string through its closing quote while honoring escapes.
pub(crate) fn scan_string_end(src: &[u8], start: usize) -> Result<usize, DslError> {
    let mut i = start + 1;
    while i < src.len() {
        match src[i] as char {
            '\\' => {
                i += 2; // skip escape pair
                continue;
            }
            '"' => return Ok(i + 1),
            _ => i += 1,
        }
    }
    Err(DslError::UnclosedString)
}

/// Returns a stable delimiter label for diagnostics.
pub(crate) fn char_label(c: char) -> &'static str {
    match c {
        '(' => "'('",
        ')' => "')'",
        '[' => "'['",
        ']' => "']'",
        '=' => "'='",
        ',' => "','",
        _ => "expected char",
    }
}

/// Detects a quoted `$prev`, `$prev.`, or `$prev[` reference boundary.
pub(super) fn is_prev_ref_string(s: &str) -> bool {
    s == "$prev" || s.starts_with("$prev.") || s.starts_with("$prev[")
}

/// Detects a `$prev` string in a depth-bounded JSON tree.
pub(crate) fn json_value_contains_prev_ref(v: &Value) -> bool {
    json_value_contains_prev_ref_at(v, 0)
}

fn json_value_contains_prev_ref_at(v: &Value, depth: usize) -> bool {
    if depth > NESTING_DEPTH_LIMIT {
        return true;
    }
    match v {
        Value::String(s) => is_prev_ref_string(s),
        Value::Array(arr) => arr
            .iter()
            .any(|e| json_value_contains_prev_ref_at(e, depth + 1)),
        Value::Object(map) => map
            .values()
            .any(|e| json_value_contains_prev_ref_at(e, depth + 1)),
        _ => false,
    }
}

/// Bounds JSON container depth before `serde_json::Value` native recursion (CWE-674).
/// Quoted brackets do not count.
pub(crate) fn check_json_nesting_depth(input: &str) -> Result<(), DslError> {
    let bytes = input.as_bytes();
    let mut i = 0;
    let mut depth: usize = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => {
                i = scan_string_end(bytes, i)?;
                continue;
            }
            b'[' | b'{' => {
                depth += 1;
                if depth > NESTING_DEPTH_LIMIT {
                    return Err(DslError::NestingTooDeep {
                        pos: i,
                        depth,
                        max: NESTING_DEPTH_LIMIT,
                    });
                }
            }
            b']' | b'}' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Finds a representative `$prev` position for non-chain diagnostics.
pub(crate) fn find_prev_ref_pos(op: &ParsedOp) -> Option<usize> {
    for av in op.args.values() {
        if arg_value_has_prev_ref(av) {
            return Some(0);
        }
    }
    None
}

fn arg_value_has_prev_ref(av: &ArgValue) -> bool {
    match av {
        ArgValue::PrevRef { .. } => true,
        ArgValue::Array(els) => els.iter().any(arg_value_has_prev_ref),
        ArgValue::Object(pairs) => pairs.iter().any(|(_, v)| arg_value_has_prev_ref(v)),
        ArgValue::Value(_) => false,
    }
}
