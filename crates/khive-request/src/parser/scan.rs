//! Low-level scanner helpers: string scanning, `$prev` detection, char labels.

use std::borrow::Cow;

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

/// A control byte (U+0000-U+001F) that survives verbatim into
/// [`NormalizedQuotedString::text`] — either a non-`\n`/`\r`/`\t` byte, which
/// is never rewritten, or a raw `\n`/`\r`/`\t` immediately following a
/// backslash, which [`normalize_quoted_string`] leaves untouched because it
/// is not eligible for the standalone rewrite (see that function's doc).
/// `normalized_pos` is its byte offset in `text`; `raw_pos` is its byte
/// offset in the original quoted span, for user-facing diagnostics.
pub(crate) struct ControlByteHit {
    pub(crate) normalized_pos: usize,
    pub(crate) raw_pos: usize,
    pub(crate) byte: u8,
}

/// Result of [`normalize_quoted_string`]: the text to hand to `serde_json`,
/// plus every literal control byte it still contains (so a subsequent parse
/// failure can be attributed without re-scanning the span).
pub(crate) struct NormalizedQuotedString<'a> {
    pub(crate) text: Cow<'a, str>,
    pub(crate) control_bytes: Vec<ControlByteHit>,
}

/// Rewrites raw literal newline, carriage return, and tab bytes inside a
/// double-quoted string literal into their JSON escape form, so a value
/// containing one of those three bytes verbatim (as opposed to a
/// `\n`/`\r`/`\t` escape sequence) still parses as valid JSON. Existing
/// backslash-escape pairs are copied through untouched: this walks the same
/// `\` + next-byte pairing [`scan_string_end`] uses, so an already-escaped
/// sequence is never reinterpreted — EXCEPT that a backslash directly
/// followed by a raw `\n`/`\r`/`\t` byte is not a valid two-byte JSON escape
/// (a valid escape is backslash plus an ASCII escape letter, never backslash
/// plus a literal control byte), so that byte is recorded as a
/// [`ControlByteHit`] even though it is copied through unrewritten; the
/// resulting `serde_json` failure is the "real control-char cause" for that
/// case (#491 round-2).
///
/// Per ADR-016, the standalone rewrite is limited to exactly those three
/// characters. Every other raw U+0000-U+001F control byte is left as-is and
/// falls through to `serde_json`, which rejects it as invalid JSON — the
/// same behavior as before this exception existed. When the span has no
/// control byte at all (the common case), `text` borrows `raw` directly —
/// no allocation.
pub(crate) fn normalize_quoted_string(raw: &str) -> NormalizedQuotedString<'_> {
    if !raw.bytes().any(|b| b < 0x20) {
        return NormalizedQuotedString {
            text: Cow::Borrowed(raw),
            control_bytes: Vec::new(),
        };
    }
    let mut out = String::with_capacity(raw.len() + 8);
    let mut control_bytes = Vec::new();
    let mut chars = raw.char_indices().peekable();
    while let Some((pos, c)) = chars.next() {
        if c == '\\' {
            out.push(c);
            if let Some(&(next_pos, next_c)) = chars.peek() {
                chars.next();
                if matches!(next_c, '\n' | '\r' | '\t') {
                    control_bytes.push(ControlByteHit {
                        normalized_pos: out.len(),
                        raw_pos: next_pos,
                        byte: next_c as u8,
                    });
                }
                out.push(next_c);
            }
            continue;
        }
        match c {
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                control_bytes.push(ControlByteHit {
                    normalized_pos: out.len(),
                    raw_pos: pos,
                    byte: c as u8,
                });
                out.push(c);
            }
            c => out.push(c),
        }
    }
    NormalizedQuotedString {
        text: Cow::Owned(out),
        control_bytes,
    }
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
