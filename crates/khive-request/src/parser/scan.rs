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
/// [`NormalizedQuotedString::text`] — either a raw control byte outside a
/// backslash pair, which is never rewritten, or a raw control byte
/// immediately following a backslash, which [`normalize_quoted_string`]
/// leaves untouched because backslash plus a literal control byte is never
/// a valid two-byte JSON escape (see that function's doc). `normalized_pos`
/// is its byte offset in `text`; `raw_pos` is its byte offset in the
/// original quoted span, for user-facing diagnostics. `preceded_by_backslash`
/// records which of the two cases this is: `serde_json` reports a broken
/// `\<ctrl>` pair as an invalid-escape failure at the control byte's own
/// offset, while a genuinely standalone control byte is reported as a
/// control-character failure at that same kind of offset — and a malformed
/// `\u` escape whose short hex run is padded out by a *later*, unrelated
/// standalone control byte also fails as an invalid escape at that byte's
/// offset. Offset alone cannot tell the legitimate backslash-pair case
/// apart from that last, spurious one; `preceded_by_backslash` is the signal
/// the caller uses to do so. A control byte that lands inside a `\u`
/// escape's own 4-hex-digit slot run is never recorded at all, even when
/// the byte immediately preceding it happens to be a backslash — see
/// [`normalize_quoted_string`] for why that adjacency is not real.
pub(crate) struct ControlByteHit {
    pub(crate) normalized_pos: usize,
    pub(crate) raw_pos: usize,
    pub(crate) byte: u8,
    pub(crate) preceded_by_backslash: bool,
}

/// Result of [`normalize_quoted_string`]: the text to hand to `serde_json`,
/// plus the first literal control byte it still contains, if any. A
/// subsequent `serde_json` parse failure always occurs at the *earliest*
/// invalid byte, so the first hit in scan order is the only one whose
/// offset can ever match — retaining just that one hit (instead of every
/// occurrence) bounds diagnostic metadata to O(1) regardless of how many
/// control bytes a malformed span contains, while still letting a parse
/// failure be attributed without re-scanning the span.
pub(crate) struct NormalizedQuotedString<'a> {
    pub(crate) text: Cow<'a, str>,
    pub(crate) first_control_byte: Option<ControlByteHit>,
}

/// Rewrites raw literal newline, carriage return, and tab bytes inside a
/// double-quoted string literal into their JSON escape form, so a value
/// containing one of those three bytes verbatim (as opposed to a
/// `\n`/`\r`/`\t` escape sequence) still parses as valid JSON. Existing
/// backslash-escape pairs are copied through untouched: this walks the same
/// `\` + next-byte pairing [`scan_string_end`] uses, so an already-escaped
/// sequence is never reinterpreted — EXCEPT that a backslash directly
/// followed by any raw U+0000-U+001F control byte is not a valid two-byte
/// JSON escape (a valid escape is backslash plus an ASCII escape letter,
/// never backslash plus a literal control byte), so that byte is recorded
/// as [`NormalizedQuotedString::first_control_byte`] (when no earlier hit
/// was already recorded) even though it is copied through unrewritten; the
/// resulting `serde_json` failure is the "real control-char cause" for that
/// case (#491 round-2).
///
/// Per ADR-016, the standalone rewrite is limited to exactly those three
/// characters. Every other raw U+0000-U+001F control byte is left as-is and
/// falls through to `serde_json`, which rejects it as invalid JSON — the
/// same behavior as before this exception existed. When the span has no
/// control byte at all (the common case), `text` borrows `raw` directly —
/// no allocation.
///
/// A `\u` escape is handled separately from the general backslash-pair walk:
/// once the `u` is seen, the following 4 characters are copied through
/// verbatim as a single unit, exactly mirroring how `serde_json`'s
/// `decode_hex_escape` consumes its 4-byte candidate-digit slice — as raw
/// bytes, never reinterpreted as introducing a new escape, even when one of
/// them is itself a backslash. Without this, a short `\u` run (e.g. 2 hex
/// digits followed by a backslash and then a raw control byte) would have
/// its 3rd slot — the backslash — re-enter the general branch above and get
/// misread as a fresh, genuine `\<ctrl>` pair, even though `serde_json` never
/// treats it as one; the resulting hit would carry `preceded_by_backslash:
/// true` for a failure that is actually a malformed unicode escape, not a
/// broken control-byte pair. Bytes inside the 4-slot window are therefore
/// never recorded as a [`ControlByteHit`], matching the fact that
/// `decode_hex_escape` never reports a control-character-style failure for
/// them — only ever `ErrorCode::InvalidEscape`, the same code (and `Display`
/// text) a genuine broken pair produces, which is why scan-time origin, not
/// the error text, has to be the source of truth.
///
/// A completed `\u` escape whose value is a high surrogate (U+D800-U+DBFF)
/// must be followed by a `\u` low-surrogate escape; `serde_json` consumes the
/// next `\` as that continuation and fails on the malformed pair when a `\u`
/// does not follow. The control byte it lands on there belongs to that
/// surrogate failure, not to a fresh broken `\<ctrl>` pair, so it is likewise
/// not recorded as a [`ControlByteHit`]: recording it would attach the
/// double-escape teaching to a malformed-surrogate error it does not explain.
pub(crate) fn normalize_quoted_string(raw: &str) -> NormalizedQuotedString<'_> {
    if !raw.bytes().any(|b| b < 0x20) {
        return NormalizedQuotedString {
            text: Cow::Borrowed(raw),
            first_control_byte: None,
        };
    }
    let mut out = String::with_capacity(raw.len() + 8);
    let mut first_control_byte = None;
    let mut chars = raw.char_indices().peekable();
    let mut hex_slots_remaining = 0u32;
    let mut hex_acc = 0u32;
    let mut hex_valid = true;
    let mut after_high_surrogate = false;
    while let Some((pos, c)) = chars.next() {
        if hex_slots_remaining > 0 {
            hex_slots_remaining -= 1;
            match c.to_digit(16) {
                Some(d) => hex_acc = (hex_acc << 4) | d,
                None => hex_valid = false,
            }
            out.push(c);
            if hex_slots_remaining == 0 {
                after_high_surrogate = hex_valid && (0xD800..=0xDBFF).contains(&hex_acc);
            }
            continue;
        }
        // True for exactly the one position after a completed high-surrogate
        // `\u` escape; consumed here so it gates only the immediate surrogate
        // continuation, never anything later.
        let prev_was_high_surrogate = after_high_surrogate;
        after_high_surrogate = false;
        if c == '\\' {
            out.push(c);
            if let Some(&(next_pos, next_c)) = chars.peek() {
                chars.next();
                if next_c == 'u' {
                    out.push(next_c);
                    hex_slots_remaining = 4;
                    hex_acc = 0;
                    hex_valid = true;
                    continue;
                }
                if (next_c as u32) < 0x20
                    && !prev_was_high_surrogate
                    && first_control_byte.is_none()
                {
                    first_control_byte = Some(ControlByteHit {
                        normalized_pos: out.len(),
                        raw_pos: next_pos,
                        byte: next_c as u8,
                        preceded_by_backslash: true,
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
                if first_control_byte.is_none() {
                    first_control_byte = Some(ControlByteHit {
                        normalized_pos: out.len(),
                        raw_pos: pos,
                        byte: c as u8,
                        preceded_by_backslash: false,
                    });
                }
                out.push(c);
            }
            c => out.push(c),
        }
    }
    NormalizedQuotedString {
        text: Cow::Owned(out),
        first_control_byte,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn many_control_bytes_retain_only_the_first_hit() {
        let raw = "\u{0}".repeat(4096);
        let normalized = normalize_quoted_string(&raw);
        let hit = normalized
            .first_control_byte
            .expect("first control byte must be recorded");
        assert_eq!(
            hit.raw_pos, 0,
            "must record the earliest hit, not a later one"
        );
        assert_eq!(hit.byte, 0);
    }
}
