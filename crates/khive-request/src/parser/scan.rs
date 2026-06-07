//! Low-level scanner helpers: string scanning, `$prev` detection, char labels.

use serde_json::Value;

use crate::types::{ArgValue, DslError, ParsedOp};

/// Scan forward from an opening `"` to the matching close, handling `\\` escapes.
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

/// Human-readable label for a delimiter character in error messages.
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

/// Return `true` if the string value is a `$prev` reference written inside
/// JSON quotes. Matches `$prev`, `$prev.`, and `$prev[` prefixes.
pub(super) fn is_prev_ref_string(s: &str) -> bool {
    s == "$prev" || s.starts_with("$prev.") || s.starts_with("$prev[")
}

/// Recursively scan a JSON value for any string that is a `$prev` reference.
pub(crate) fn json_value_contains_prev_ref(v: &Value) -> bool {
    match v {
        Value::String(s) => is_prev_ref_string(s),
        Value::Array(arr) => arr.iter().any(json_value_contains_prev_ref),
        Value::Object(map) => map.values().any(json_value_contains_prev_ref),
        _ => false,
    }
}

/// Scan an op's args for any `PrevRef` and return a representative position
/// if found. Used to emit `PrevRefOutsideChain` for Single and Parallel modes.
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
