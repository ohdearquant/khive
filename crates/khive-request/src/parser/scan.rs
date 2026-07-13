//! Low-level scanner helpers: string scanning, `$prev` detection, char labels.

use serde_json::Value;

use crate::types::{ArgValue, DslError, ParsedOp, NESTING_DEPTH_LIMIT};

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

/// Rewrite raw control bytes (0x00-0x1F) inside a double-quoted string
/// literal into their JSON escape form, so a value containing a literal
/// newline, carriage return, or tab (as opposed to a `\n`/`\r`/`\t` escape
/// sequence) still parses as valid JSON. Existing backslash-escape pairs are
/// copied through untouched: this walks the same `\` + next-byte pairing
/// [`scan_string_end`] uses, so an already-escaped sequence is never
/// reinterpreted.
pub(crate) fn escape_literal_control_chars(s: &str) -> String {
    if !s.contains(|c: char| (c as u32) < 0x20) {
        return s.to_owned();
    }
    let mut out = String::with_capacity(s.len() + 8);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(next) = chars.next() {
                out.push(next);
            }
            continue;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
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
///
/// Only ever walks trees already bounded by [`check_json_nesting_depth`], but
/// carries its own depth counter defensively (cheap, and this is an easy
/// second recursion site to miss if the pre-pass invariant is ever changed).
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

/// Pre-pass O(n) scan for container-nesting depth (`[`/`{`) over raw JSON-form
/// input, bounding `serde_json::from_str::<Value>`'s otherwise-unbounded
/// native recursive descent (CWE-674). `serde_json` exposes no depth knob
/// for its untyped `Value` deserializer. Honors quoted strings via
/// [`scan_string_end`] so brackets inside string literals do not count.
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
