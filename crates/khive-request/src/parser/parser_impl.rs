//! Recursive-descent `Parser` for the verb-dispatch DSL function-call form.

use serde_json::Value;

use crate::types::{ArgValue, DslError, ParsedOp, NESTING_DEPTH_LIMIT};

use super::scan::{char_label, normalize_quoted_string, scan_string_end, NormalizedQuotedString};

/// Byte-slice cursor for the DSL input.
pub(crate) struct Parser<'a> {
    pub(crate) src: &'a [u8],
    pub(crate) pos: usize,
    /// Current array/object depth, bounded by [`NESTING_DEPTH_LIMIT`].
    depth: usize,
}

impl<'a> Parser<'a> {
    /// Creates a cursor over `src`.
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    /// Enters a container; callers must decrement after success or failure.
    fn enter_container(&mut self) -> Result<(), DslError> {
        self.depth += 1;
        if self.depth > NESTING_DEPTH_LIMIT {
            let depth = self.depth;
            self.depth -= 1;
            return Err(DslError::NestingTooDeep {
                pos: self.pos,
                depth,
                max: NESTING_DEPTH_LIMIT,
            });
        }
        Ok(())
    }

    /// Returns whether the cursor is at end of input.
    pub(crate) fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Peeks at the current ASCII syntax byte.
    pub(crate) fn peek(&self) -> Option<char> {
        self.src.get(self.pos).map(|b| *b as char)
    }

    /// Advances by at most `n` bytes.
    pub(crate) fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.src.len());
    }

    /// Skips ASCII whitespace.
    pub(crate) fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance(1);
            } else {
                break;
            }
        }
    }

    /// Consumes `want` or returns a positioned delimiter error.
    pub(crate) fn expect_char(&mut self, want: char) -> Result<(), DslError> {
        self.skip_ws();
        match self.peek() {
            Some(c) if c == want => {
                self.advance(1);
                Ok(())
            }
            Some(c) => Err(DslError::UnexpectedChar {
                pos: self.pos,
                found: c,
                expected: char_label(want),
            }),
            None => Err(DslError::UnexpectedEof {
                expected: char_label(want),
            }),
        }
    }

    fn parse_identifier(&mut self) -> Result<String, DslError> {
        self.skip_ws();
        let start = self.pos;
        match self.peek() {
            Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
            _ => return Err(DslError::InvalidIdentifier { pos: self.pos }),
        }
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || c == '_' {
                self.advance(1);
            } else {
                break;
            }
        }
        Ok(std::str::from_utf8(&self.src[start..self.pos])
            .expect("ascii-only chunk")
            .to_owned())
    }

    /// Parses one `verb(...)` or `pack.verb(...)` call.
    pub(crate) fn parse_op(&mut self) -> Result<ParsedOp, DslError> {
        let mut tool = self.parse_identifier()?;
        if self.peek() == Some('.') {
            self.advance(1);
            let sub = self.parse_identifier()?;
            tool = format!("{tool}.{sub}");
            if self.peek() == Some('.') {
                return Err(DslError::UnsupportedVerbNesting { pos: self.pos });
            }
        }
        self.expect_char('(')?;
        self.skip_ws();
        let mut args: std::collections::BTreeMap<String, ArgValue> =
            std::collections::BTreeMap::new();
        if self.peek() == Some(')') {
            self.advance(1);
            return Ok(ParsedOp { tool, args });
        }
        loop {
            let name = self.parse_identifier()?;
            self.expect_char('=')?;
            self.skip_ws();
            let arg_val = self.parse_arg_value_with_hint(Some(&name))?;
            if args.contains_key(&name) {
                return Err(DslError::DuplicateArg { name });
            }
            args.insert(name, arg_val);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.advance(1);
                    self.skip_ws();
                }
                Some(')') => {
                    self.advance(1);
                    return Ok(ParsedOp { tool, args });
                }
                Some(c) => {
                    return Err(DslError::UnexpectedChar {
                        pos: self.pos,
                        found: c,
                        expected: "',' or ')'",
                    });
                }
                None => return Err(DslError::UnexpectedEof { expected: "')'" }),
            }
        }
    }

    fn parse_arg_value(&mut self) -> Result<ArgValue, DslError> {
        self.parse_arg_value_with_hint(None)
    }

    /// Same as [`Self::parse_arg_value`], but `hint` names the argument or
    /// object key this value is being assigned to (when known), so a
    /// bareword-value failure can show the exact corrected call instead of
    /// just the corrected literal.
    fn parse_arg_value_with_hint(&mut self, hint: Option<&str>) -> Result<ArgValue, DslError> {
        self.skip_ws();
        if self.peek() == Some('$') {
            return self.parse_prev_ref();
        }
        if self.peek() == Some('[') {
            return self.parse_array_arg();
        }
        if self.peek() == Some('{') {
            return self.parse_object_arg();
        }
        let v = self.parse_value(hint)?;
        if let Value::String(s) = &v {
            if let Some(prev_ref) = Self::string_as_prev_ref(s) {
                return Ok(prev_ref);
            }
        }
        Ok(ArgValue::Value(v))
    }

    fn parse_array_arg(&mut self) -> Result<ArgValue, DslError> {
        self.enter_container()?;
        let result = self.parse_array_arg_body();
        self.depth -= 1;
        result
    }

    fn parse_array_arg_body(&mut self) -> Result<ArgValue, DslError> {
        self.advance(1); // consume '['
        self.skip_ws();
        let mut elements: Vec<ArgValue> = Vec::new();
        if self.peek() == Some(']') {
            self.advance(1);
            return Ok(ArgValue::Value(Value::Array(vec![])));
        }
        loop {
            self.skip_ws();
            let elem = self.parse_arg_value()?;
            elements.push(elem);
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.advance(1);
                }
                Some(']') => {
                    self.advance(1);
                    break;
                }
                Some(c) => {
                    return Err(DslError::UnexpectedChar {
                        pos: self.pos,
                        found: c,
                        expected: "',' or ']'",
                    });
                }
                None => return Err(DslError::UnexpectedEof { expected: "']'" }),
            }
        }
        let has_dynamic = elements.iter().any(|e| !matches!(e, ArgValue::Value(_)));
        if has_dynamic {
            Ok(ArgValue::Array(elements))
        } else {
            let vals: Vec<Value> = elements
                .into_iter()
                .map(|e| match e {
                    ArgValue::Value(v) => v,
                    _ => unreachable!(),
                })
                .collect();
            Ok(ArgValue::Value(Value::Array(vals)))
        }
    }

    fn parse_object_arg(&mut self) -> Result<ArgValue, DslError> {
        self.enter_container()?;
        let result = self.parse_object_arg_body();
        self.depth -= 1;
        result
    }

    fn parse_object_arg_body(&mut self) -> Result<ArgValue, DslError> {
        self.advance(1); // consume '{'
        self.skip_ws();
        let mut pairs: Vec<(String, ArgValue)> = Vec::new();
        if self.peek() == Some('}') {
            self.advance(1);
            return Ok(ArgValue::Value(Value::Object(serde_json::Map::new())));
        }
        loop {
            self.skip_ws();
            let key = match self.peek() {
                Some('"') => {
                    let start = self.pos;
                    let end = scan_string_end(self.src, start)?;
                    let raw = std::str::from_utf8(&self.src[start..end]).expect("utf8 key literal");
                    let s = decode_quoted_json_key(raw, start)?;
                    self.pos = end;
                    s
                }
                Some(c) => {
                    return Err(DslError::UnexpectedChar {
                        pos: self.pos,
                        found: c,
                        expected: "quoted string key",
                    });
                }
                None => {
                    return Err(DslError::UnexpectedEof {
                        expected: "object key",
                    })
                }
            };
            self.skip_ws();
            self.expect_char(':')?;
            self.skip_ws();
            let val = self.parse_arg_value_with_hint(Some(&key))?;
            pairs.push((key, val));
            self.skip_ws();
            match self.peek() {
                Some(',') => {
                    self.advance(1);
                }
                Some('}') => {
                    self.advance(1);
                    break;
                }
                Some(c) => {
                    return Err(DslError::UnexpectedChar {
                        pos: self.pos,
                        found: c,
                        expected: "',' or '}'",
                    });
                }
                None => return Err(DslError::UnexpectedEof { expected: "'}'" }),
            }
        }
        let has_dynamic = pairs.iter().any(|(_, v)| !matches!(v, ArgValue::Value(_)));
        if has_dynamic {
            Ok(ArgValue::Object(pairs))
        } else {
            let mut map = serde_json::Map::with_capacity(pairs.len());
            for (k, v) in pairs {
                match v {
                    ArgValue::Value(val) => {
                        map.insert(k, val);
                    }
                    _ => unreachable!(),
                }
            }
            Ok(ArgValue::Value(Value::Object(map)))
        }
    }

    fn parse_prev_ref(&mut self) -> Result<ArgValue, DslError> {
        let start = self.pos;
        self.advance(1); // consume '$'
        let ident = self
            .parse_identifier()
            .map_err(|_| DslError::InvalidValue {
                pos: start,
                error: "expected '$prev' — '$' must be followed by 'prev'".into(),
            })?;
        if ident != "prev" {
            return Err(DslError::InvalidValue {
                pos: start,
                error: format!("expected '$prev', found '${}'", ident),
            });
        }
        let mut path = String::new();
        loop {
            match self.peek() {
                Some('.') => {
                    self.advance(1);
                    let segment = self.parse_identifier()?;
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(&segment);
                }
                Some('[') => {
                    self.advance(1); // consume '['
                    let idx_start = self.pos;
                    let mut idx_str = String::new();
                    while let Some(c) = self.peek() {
                        if c.is_ascii_digit() {
                            idx_str.push(c);
                            self.advance(1);
                        } else {
                            break;
                        }
                    }
                    if idx_str.is_empty() {
                        return Err(DslError::InvalidValue {
                            pos: idx_start,
                            error: "expected non-negative integer inside '[...]'".into(),
                        });
                    }
                    match self.peek() {
                        Some(']') => self.advance(1),
                        Some(c) => {
                            return Err(DslError::UnexpectedChar {
                                pos: self.pos,
                                found: c,
                                expected: "']'",
                            });
                        }
                        None => {
                            return Err(DslError::UnexpectedEof { expected: "']'" });
                        }
                    }
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push('[');
                    path.push_str(&idx_str);
                    path.push(']');
                }
                _ => break,
            }
        }
        Ok(ArgValue::PrevRef { path })
    }

    /// `hint` names the argument or object key this value is assigned to
    /// (when the caller knows it), used only to show a fully corrected call
    /// when the failure is a bareword value (see [`bareword_hint_message`]).
    fn parse_value(&mut self, hint: Option<&str>) -> Result<Value, DslError> {
        self.skip_ws();
        let start = self.pos;
        let end = self.scan_value_end()?;
        let slice = std::str::from_utf8(&self.src[start..end])
            .expect("ascii-or-utf8 maintained by scanner");
        let trimmed = slice.trim();
        // A quoted string literal may contain raw control bytes (newline, CR,
        // tab) verbatim in the DSL source; JSON proper forbids that, so
        // `decode_quoted_json_string` rewrites them to JSON escapes before
        // handing the slice to `serde_json`. Non-string values (numbers,
        // bool, null) never legitimately contain such bytes, so this only
        // touches strings.
        let value = if trimmed.starts_with('"') {
            Value::String(decode_quoted_json_string(trimmed, start)?)
        } else {
            serde_json::from_str(trimmed).map_err(|e| {
                let error = if is_bareword_identifier(trimmed) {
                    bareword_hint_message(trimmed, hint)
                } else {
                    // `e.to_string()` carries its own "at line L column C",
                    // always relative to `trimmed` (this one value's own
                    // slice, always line 1) rather than the DSL input as a
                    // whole — pairing it verbatim with the `at position
                    // {start}` this crate reports for the same failure would
                    // state two different, disagreeing locations for one
                    // problem. Keep the descriptive prefix, drop the
                    // contradicting position clause.
                    strip_serde_position(&e.to_string())
                };
                DslError::InvalidValue { pos: start, error }
            })?
        };
        self.pos = end;
        Ok(value)
    }

    fn string_as_prev_ref(s: &str) -> Option<ArgValue> {
        if let Some(rest) = s.strip_prefix('\\') {
            if rest == "$prev" || rest.starts_with("$prev.") || rest.starts_with("$prev[") {
                return Some(ArgValue::Value(Value::String(rest.to_owned())));
            }
        }
        if s == "$prev" {
            return Some(ArgValue::PrevRef {
                path: String::new(),
            });
        }
        if let Some(rest) = s.strip_prefix("$prev.") {
            if !rest.is_empty() && quoted_prev_path_is_valid(rest) {
                return Some(ArgValue::PrevRef {
                    path: rest.to_owned(),
                });
            }
            return None;
        }
        if let Some(after_bracket) = s.strip_prefix("$prev[") {
            if let Some(close) = after_bracket.find(']') {
                let index_str = &after_bracket[..close];
                if !index_str.is_empty() && index_str.chars().all(|c| c.is_ascii_digit()) {
                    let tail = &after_bracket[close + 1..];
                    if quoted_prev_path_is_valid(tail) {
                        let path = format!("[{index_str}]{tail}");
                        return Some(ArgValue::PrevRef { path });
                    }
                }
            }
            return None;
        }
        None
    }

    fn scan_value_end(&self) -> Result<usize, DslError> {
        let mut i = self.pos;
        let mut depth_paren: i32 = 0;
        let mut depth_brack: i32 = 0;
        let mut depth_brace: i32 = 0;
        while i < self.src.len() {
            let c = self.src[i] as char;
            match c {
                '"' => {
                    i = scan_string_end(self.src, i)?;
                    continue;
                }
                '[' => depth_brack += 1,
                ']' => {
                    if depth_brack == 0 {
                        if depth_paren == 0 && depth_brace == 0 {
                            return Ok(i);
                        }
                        return Ok(i);
                    }
                    depth_brack -= 1;
                }
                '{' => depth_brace += 1,
                '}' => {
                    if depth_brace == 0 {
                        if depth_paren == 0 && depth_brack == 0 {
                            return Ok(i);
                        }
                        return Err(DslError::UnclosedBracket { kind: '{' });
                    }
                    depth_brace -= 1;
                }
                '(' => depth_paren += 1,
                ')' => {
                    if depth_paren == 0 && depth_brack == 0 && depth_brace == 0 {
                        return Ok(i);
                    }
                    if depth_paren == 0 {
                        return Err(DslError::UnclosedBracket { kind: '(' });
                    }
                    depth_paren -= 1;
                }
                ',' if depth_paren == 0 && depth_brack == 0 && depth_brace == 0 => {
                    return Ok(i);
                }
                _ => {}
            }
            i += 1;
        }
        if depth_brack > 0 {
            return Err(DslError::UnclosedBracket { kind: '[' });
        }
        if depth_brace > 0 {
            return Err(DslError::UnclosedBracket { kind: '{' });
        }
        Ok(i)
    }
}

/// Maps a `serde_json::Error`'s 1-indexed `(line, column)` to a 0-indexed
/// byte offset into `text` — the exact string `serde_json` was parsing —
/// pointing AT the offending byte itself. `line`/`column` count raw bytes
/// (including any literal control byte a [`NormalizedQuotedString`] left
/// unrewritten inside a would-be escape pair, which is the failure case a
/// `line > 1` report handles: consuming that raw `\n` itself advances the
/// tracker to `(line + 1, column 0)`, so `column` can legitimately be `0`
/// — it is not an error sentinel). A `line > 1` failure is resolved by
/// walking `text` for the `line - 1`-th `\n` byte at index `i`; the
/// offending byte sits at `i + column` in both the "it's the newline
/// itself" case (`column == 0`) and the "N more bytes were consumed on the
/// new line first" case. Returns `None` only if `text` has fewer `\n` bytes
/// than the error claims (never observed from `serde_json` in practice) or
/// `line <= 1` with `column == 0` (no valid 0-indexed byte before column 1
/// on the first line), in which case the caller falls back to the plain
/// serde message.
fn serde_error_byte_offset(e: &serde_json::Error, text: &str) -> Option<usize> {
    let line = e.line();
    let column = e.column();
    if line <= 1 {
        return column.checked_sub(1);
    }
    let mut newlines_seen = 0usize;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            newlines_seen += 1;
            if newlines_seen == line - 1 {
                return Some(i + column);
            }
        }
    }
    None
}

/// Enriches a `serde_json` string-decode failure with the ADR-016 escape
/// grammar. Enrichment is
/// gated on the failure being AT the recorded [`ControlByteHit`]: the serde
/// error's `(line, column)` is mapped to a byte offset in `normalized.text`
/// via [`serde_error_byte_offset`], then compared against
/// `normalized.first_control_byte` (collected during the same pass that
/// built `text` — no re-scan of the span). A failure whose offset lands
/// elsewhere (e.g. an invalid `\q` escape, even with an unrelated control
/// byte later in the span) falls through to the plain serde message
/// unchanged, so a control byte is never misattributed as the cause of a
/// different failure.
///
/// Offset alone is not sufficient when the hit is *not*
/// `preceded_by_backslash`: a malformed `\u` escape (e.g. `\u123` where the
/// 4th hex-digit slot is consumed by a following, unrelated raw control
/// byte) fails with the same "invalid escape" kind and can land at that
/// byte's exact offset too — indistinguishable from a genuine standalone
/// control-character failure by offset alone. Only a hit whose recorded
/// origin was a broken `\<ctrl>` pair (`preceded_by_backslash`) is known —
/// by construction, not by re-deriving it from the error text — to be an
/// actual invalid-escape failure over that byte; a non-backslash hit is only
/// enriched when the failure is the plain control-character kind serde
/// reports for a standalone raw control byte, checked via the error's
/// `Display` text since `ErrorCode` itself is private and `classify()` only
/// returns the coarse `Category::Syntax` shared by every one of these kinds.
fn describe_quoted_string_parse_error(
    e: &serde_json::Error,
    normalized: &NormalizedQuotedString<'_>,
) -> String {
    let base = strip_serde_position(&e.to_string());
    let Some(offset) = serde_error_byte_offset(e, normalized.text.as_ref()) else {
        return base;
    };
    let Some(hit) = normalized
        .first_control_byte
        .as_ref()
        .filter(|h| h.normalized_pos == offset)
    else {
        return base;
    };
    if !hit.preceded_by_backslash && !base.starts_with("control character") {
        return base;
    }
    // `raw_pos` counts from the span's opening quote; report the index relative
    // to the value itself (the saturating guard covers the unreachable pos-0 hit).
    let idx = hit.raw_pos.saturating_sub(1);
    let c = hit.byte as char;
    format!(
        "{base} — byte {idx} of the value is {c:?} (U+{:04X}). DSL string escapes follow JSON: \
         \\n, \\t, \\\", \\\\ (raw newline/CR/tab are also accepted literally; other control \
         bytes must be escaped).",
        c as u32
    )
}

/// Decodes a quoted-string DSL literal (`raw`, the exact quoted span
/// including its surrounding `"`) into its `String` value, normalizing raw
/// literal newline/CR/tab bytes to JSON escapes first (ADR-016) and
/// enriching any remaining decode failure via
/// [`describe_quoted_string_parse_error`]. Used by quoted string values
/// (`parse_value`) only — the literal-newline/CR/tab carve-out is scoped to
/// argument values, not object keys (see [`decode_quoted_json_key`]).
fn decode_quoted_json_string(raw: &str, pos: usize) -> Result<String, DslError> {
    let normalized = normalize_quoted_string(raw);
    serde_json::from_str(normalized.text.as_ref()).map_err(|e| DslError::InvalidValue {
        pos,
        error: describe_quoted_string_parse_error(&e, &normalized),
    })
}

/// Decodes a quoted-string DSL literal that is an object KEY (`raw`, the
/// exact quoted span including its surrounding `"`) into its `String` value
/// using strict `serde_json` semantics. Unlike [`decode_quoted_json_string`],
/// no raw control byte is tolerated here: per ADR-016 / PR #957, the
/// literal-newline/CR/tab carve-out applies to quoted argument VALUES only —
/// a key containing one of those bytes must use its JSON escape form
/// (`\n`, `\r`, `\t`) like any other control byte.
fn decode_quoted_json_key(raw: &str, pos: usize) -> Result<String, DslError> {
    serde_json::from_str(raw).map_err(|e| DslError::InvalidValue {
        pos,
        error: strip_serde_position(&e.to_string()),
    })
}

/// Returns whether `s` is a bare identifier (`[A-Za-z_][A-Za-z0-9_]*`) —
/// the shape of the most common invalid-value mistake: a string value typed
/// without its surrounding `"..."`. Only called after `serde_json` has
/// already rejected `s` as a value, so this never misclassifies a valid
/// JSON literal (`true`, `false`, `null`, a number) as a bareword.
fn is_bareword_identifier(s: &str) -> bool {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Builds the message for a bareword-shaped invalid value. `hint`, when the
/// caller knows the argument name or object key this value belongs to,
/// lets the message show the exact corrected call rather than just the
/// corrected literal — a caller who wrote `id=abc` sees `id="abc"` and can
/// copy it directly.
fn bareword_hint_message(bareword: &str, hint: Option<&str>) -> String {
    match hint {
        Some(name) => format!(
            "{bareword:?} is a bareword, not a valid value; string values must be \
             double-quoted — did you mean {name}={bareword:?}?"
        ),
        None => format!(
            "{bareword:?} is a bareword, not a valid value; string values must be \
             double-quoted — did you mean {bareword:?}?"
        ),
    }
}

/// Strips a `serde_json` `Display` message's trailing `at line L column C`
/// clause. That clause is always relative to the isolated substring
/// `serde_json` parsed (one value's slice, or a quoted string's decoded
/// body) rather than the overall DSL input, so it disagrees with the
/// `at position {pos}` this crate reports for the same failure whenever the
/// value does not start at byte 0. The descriptive prefix (`expected
/// value`, `trailing characters`, `control character ... found while
/// parsing a string`, ...) stays meaningful on its own and is kept.
fn strip_serde_position(msg: &str) -> String {
    msg.split(" at line ").next().unwrap_or(msg).to_owned()
}

/// Validates quoted-reference brackets before promoting a string to `PrevRef`.
/// A malformed segment keeps the entire value literal.
fn quoted_prev_path_is_valid(path: &str) -> bool {
    let bytes = path.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'[' => {
                i += 1;
                let start = i;
                while i < bytes.len() && bytes[i].is_ascii_digit() {
                    i += 1;
                }
                if i == start || i >= bytes.len() || bytes[i] != b']' {
                    return false;
                }
                i += 1;
            }
            b']' => return false,
            _ => i += 1,
        }
    }
    true
}
