use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::types::{ArgValue, DslError, ExecutionMode, ParsedOp, ParsedRequest, MAX_OPS};

/// Parse a request input string, returning either a single op or a batch.
pub fn parse_request(input: &str) -> Result<ParsedRequest, DslError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DslError::Empty);
    }

    // JSON form: `[{...}, ...]` or `{...}`. After `[`, JSON whitespace is legal
    // before the first element — common when pretty-printers emit `[ {...} ]`.
    let first = trimmed.as_bytes()[0];
    let looks_like_json = first == b'{'
        || (first == b'['
            && trimmed
                .as_bytes()
                .iter()
                .skip(1)
                .find(|b| !matches!(b, b' ' | b'\t' | b'\n' | b'\r'))
                .is_some_and(|b| *b == b'{'));
    if looks_like_json {
        return parse_json_form(trimmed);
    }

    // Function-call batch `[...]` — parallel.
    if first == b'[' {
        return parse_fn_batch(trimmed);
    }

    // Chain or single: starts with an identifier.
    // Parse the first op, then check for `|` to detect chain mode.
    let mut p = Parser::new(trimmed);
    let first_op = p.parse_op()?;
    p.skip_ws();

    if p.eof() {
        // Single op — no separator follows.
        // PrevRef in a single op is always invalid.
        if let Some(pos) = find_prev_ref_pos(&first_op) {
            return Err(DslError::PrevRefOutsideChain { pos });
        }
        return Ok(ParsedRequest {
            ops: vec![first_op],
            mode: ExecutionMode::Single,
        });
    }

    if p.peek() == Some('|') {
        // Chain mode: `op1 | op2 | ...`
        return parse_chain_tail(p, first_op);
    }

    // Unexpected trailing content after a single op.
    Err(DslError::UnexpectedChar {
        pos: p.pos,
        found: p.peek().unwrap(),
        expected: "'|' or end of input",
    })
}

/// Parse the rest of a chain after the first op has been consumed.
///
/// Called when we've seen `first_op` followed by `|`. Parses one or more
/// `| op` segments and returns a `Chain` request.
fn parse_chain_tail(mut p: Parser<'_>, first_op: ParsedOp) -> Result<ParsedRequest, DslError> {
    let mut ops = vec![first_op];
    while p.peek() == Some('|') {
        if ops.len() >= MAX_OPS {
            return Err(DslError::TooManyOps {
                count: ops.len() + 1,
                max: MAX_OPS,
            });
        }
        p.advance(1); // consume '|'
        p.skip_ws();
        let op = p.parse_op()?;
        ops.push(op);
        p.skip_ws();
    }
    if !p.eof() {
        if p.peek() == Some(',') {
            return Err(DslError::MixedSeparators);
        }
        return Err(DslError::UnexpectedChar {
            pos: p.pos,
            found: p.peek().unwrap(),
            expected: "'|' or end of input",
        });
    }
    Ok(ParsedRequest {
        ops,
        mode: ExecutionMode::Chain,
    })
}

fn parse_json_form(input: &str) -> Result<ParsedRequest, DslError> {
    let v: Value = serde_json::from_str(input).map_err(|e| DslError::InvalidJson {
        error: e.to_string(),
    })?;
    let (arr, is_single) = match v {
        Value::Array(arr) => (arr, false),
        Value::Object(_) => (vec![v], true),
        other => {
            return Err(DslError::InvalidJson {
                error: format!("expected object or array of objects, got {other}"),
            })
        }
    };
    if arr.is_empty() && !is_single {
        return Err(DslError::EmptyBatch);
    }
    if arr.len() > MAX_OPS {
        return Err(DslError::TooManyOps {
            count: arr.len(),
            max: MAX_OPS,
        });
    }
    let mut ops = Vec::with_capacity(arr.len());
    for entry in arr {
        let obj = entry.as_object().ok_or_else(|| DslError::InvalidJson {
            error: "each batch entry must be an object".into(),
        })?;
        let tool = obj
            .get("tool")
            .and_then(Value::as_str)
            .ok_or_else(|| DslError::InvalidJson {
                error: "each entry needs a \"tool\" string".into(),
            })?
            .to_owned();
        let args = obj
            .get("args")
            .cloned()
            .unwrap_or_else(|| Value::Object(Map::new()));
        let args_map = match args {
            Value::Object(m) => m,
            other => {
                return Err(DslError::InvalidJson {
                    error: format!("\"args\" must be an object, got {other}"),
                })
            }
        };
        // JSON form does not support $prev references — all args are Values.
        // Recursively scan every arg value (including nested arrays/objects) for
        // any string matching $prev, $prev.*, or $prev[* and reject with a typed
        // PrevRefInJsonForm error.
        let mut args: BTreeMap<String, ArgValue> = BTreeMap::new();
        for (k, v) in args_map {
            if json_value_contains_prev_ref(&v) {
                return Err(DslError::PrevRefInJsonForm { arg_name: k });
            }
            args.insert(k, ArgValue::Value(v));
        }
        ops.push(ParsedOp { tool, args });
    }
    let mode = if is_single {
        ExecutionMode::Single
    } else {
        ExecutionMode::Parallel
    };
    Ok(ParsedRequest { ops, mode })
}

fn parse_fn_batch(input: &str) -> Result<ParsedRequest, DslError> {
    let mut p = Parser::new(input);
    p.expect_char('[')?;
    p.skip_ws();
    let mut ops = Vec::new();
    if p.peek() == Some(']') {
        p.advance(1);
        return Err(DslError::EmptyBatch);
    }
    loop {
        if ops.len() >= MAX_OPS {
            return Err(DslError::TooManyOps {
                count: ops.len() + 1,
                max: MAX_OPS,
            });
        }
        let op = p.parse_op()?;
        ops.push(op);
        p.skip_ws();
        match p.peek() {
            Some(',') => {
                p.advance(1);
                p.skip_ws();
            }
            Some(']') => {
                p.advance(1);
                break;
            }
            Some('|') => return Err(DslError::MixedSeparators),
            Some(c) => {
                return Err(DslError::UnexpectedChar {
                    pos: p.pos,
                    found: c,
                    expected: "',' or ']'",
                });
            }
            None => return Err(DslError::UnexpectedEof { expected: "']'" }),
        }
    }
    p.skip_ws();
    if !p.eof() {
        return Err(DslError::UnexpectedChar {
            pos: p.pos,
            found: p.peek().unwrap(),
            expected: "end of input",
        });
    }
    // PrevRef inside a function-call parallel batch is invalid.
    for op in &ops {
        if let Some(pos) = find_prev_ref_pos(op) {
            return Err(DslError::PrevRefOutsideChain { pos });
        }
    }
    Ok(ParsedRequest {
        ops,
        mode: ExecutionMode::Parallel,
    })
}

// ── recursive-descent parser ────────────────────────────────────────────────

pub(crate) struct Parser<'a> {
    src: &'a [u8],
    pub(crate) pos: usize,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    pub(crate) fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    pub(crate) fn peek(&self) -> Option<char> {
        self.src.get(self.pos).map(|b| *b as char)
    }

    pub(crate) fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.src.len());
    }

    pub(crate) fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance(1);
            } else {
                break;
            }
        }
    }

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

    pub(crate) fn parse_op(&mut self) -> Result<ParsedOp, DslError> {
        let mut tool = self.parse_identifier()?;
        // One-level dotted verbs: brain.state, recall.candidates
        if self.peek() == Some('.') {
            self.advance(1);
            let sub = self.parse_identifier()?;
            tool = format!("{tool}.{sub}");
            // Only one level of dotting is supported. A second '.' is a clear
            // error — emit UnsupportedVerbNesting instead of the misleading
            // "expected '|' or end of input, found '.'" message.
            if self.peek() == Some('.') {
                return Err(DslError::UnsupportedVerbNesting { pos: self.pos });
            }
        }
        self.expect_char('(')?;
        self.skip_ws();
        let mut args: BTreeMap<String, ArgValue> = BTreeMap::new();
        if self.peek() == Some(')') {
            self.advance(1);
            return Ok(ParsedOp { tool, args });
        }
        loop {
            let name = self.parse_identifier()?;
            self.expect_char('=')?;
            self.skip_ws();
            let arg_val = self.parse_arg_value()?;
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

    /// Parse an argument value — either a `$prev` reference, an array/object
    /// literal (which may contain `$prev` refs), or a plain JSON literal.
    ///
    /// A quoted string like `"$prev.id"` is treated identically to the
    /// unquoted token `$prev.id`. Both resolve to `ArgValue::PrevRef { path: "id" }`.
    /// To pass the literal string `$prev.id` as a value, escape the leading `$`
    /// in the JSON string: `"\\$prev.id"` deserializes to `\$prev.id`, which is
    /// stripped to `$prev.id` and returned as a concrete `ArgValue::Value`.
    fn parse_arg_value(&mut self) -> Result<ArgValue, DslError> {
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
        let v = self.parse_value()?;
        // Promote quoted "$prev[.path]" strings to PrevRef.
        if let Value::String(s) = &v {
            if let Some(prev_ref) = Self::string_as_prev_ref(s) {
                return Ok(prev_ref);
            }
        }
        Ok(ArgValue::Value(v))
    }

    /// Parse an array argument: `[elem, elem, ...]` where each element may be
    /// a `$prev` reference, a nested array/object, or a plain JSON literal.
    ///
    /// When no element contains a `$prev` reference, the result is folded back
    /// into an `ArgValue::Value(Array(_))` so pure-JSON callers see no change.
    fn parse_array_arg(&mut self) -> Result<ArgValue, DslError> {
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
        // If no element is a PrevRef/Array/Object, fold to Value for efficiency.
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

    /// Parse an object argument: `{"key": value, ...}`.
    ///
    /// Keys must be quoted strings (JSON-style). Values may contain `$prev` refs.
    /// Pure-JSON objects without any `$prev` fold back into `ArgValue::Value`.
    fn parse_object_arg(&mut self) -> Result<ArgValue, DslError> {
        self.advance(1); // consume '{'
        self.skip_ws();
        let mut pairs: Vec<(String, ArgValue)> = Vec::new();
        if self.peek() == Some('}') {
            self.advance(1);
            return Ok(ArgValue::Value(Value::Object(serde_json::Map::new())));
        }
        loop {
            self.skip_ws();
            // Key must be a quoted string — parse it directly (not via parse_value,
            // which uses scan_value_end and would greedily consume `:value`).
            let key = match self.peek() {
                Some('"') => {
                    let start = self.pos;
                    let end = scan_string_end(self.src, start)?;
                    let raw = std::str::from_utf8(&self.src[start..end]).expect("utf8 key literal");
                    let s: String =
                        serde_json::from_str(raw).map_err(|e| DslError::InvalidValue {
                            pos: start,
                            error: e.to_string(),
                        })?;
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
            let val = self.parse_arg_value()?;
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
        // Fold pure-JSON objects back to Value.
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

    /// Parse a `$prev` or `$prev.field.path` reference.
    ///
    /// Grammar: `$prev` optionally followed by a path composed of:
    /// - dot-separated identifiers: `.field`
    /// - bracket array indices:     `[N]`
    ///
    /// Examples: `$prev`, `$prev.id`, `$prev.items[0].id`, `$prev[0].name`
    fn parse_prev_ref(&mut self) -> Result<ArgValue, DslError> {
        let start = self.pos;
        // Consume `$`
        self.advance(1);
        // Must be followed by `prev`
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
        // Optional path — dot-segments and/or bracket indices.
        let mut path = String::new();
        loop {
            match self.peek() {
                Some('.') => {
                    self.advance(1); // consume '.'
                    let segment = self.parse_identifier()?;
                    if !path.is_empty() {
                        path.push('.');
                    }
                    path.push_str(&segment);
                }
                Some('[') => {
                    // Array index: `[N]`
                    self.advance(1); // consume '['
                    let idx_start = self.pos;
                    // Read digits
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
                    // Encode index as `[N]` so split_path can parse it back.
                    path.push('[');
                    path.push_str(&idx_str);
                    path.push(']');
                }
                _ => break,
            }
        }
        Ok(ArgValue::PrevRef { path })
    }

    fn parse_value(&mut self) -> Result<Value, DslError> {
        self.skip_ws();
        let start = self.pos;
        let end = self.scan_value_end()?;
        let slice = std::str::from_utf8(&self.src[start..end])
            .expect("ascii-or-utf8 maintained by scanner");
        let value: Value =
            serde_json::from_str(slice.trim()).map_err(|e| DslError::InvalidValue {
                pos: start,
                error: e.to_string(),
            })?;
        self.pos = end;
        Ok(value)
    }

    /// Check whether a parsed string value is a `$prev` reference written
    /// inside quotes. Returns `Some(PrevRef)` if so, or `Some(Value(...))` if
    /// the string is an escaped literal, or `None` if neither.
    ///
    /// ## Escape semantics
    ///
    /// A string like `"$prev.id"` deserializes to the Rust string `$prev.id`
    /// and is promoted to `ArgValue::PrevRef { path: "id" }`.
    ///
    /// To pass the **literal** string `$prev.id` as a value, write `"\\$prev.id"`
    /// in the DSL source. That deserializes to `\$prev.id` (one leading backslash).
    /// This function strips the leading `\` and returns
    /// `ArgValue::Value(json!("$prev.id"))`.
    ///
    /// `$prevish.id` does NOT match (prefix boundary is `.` or `[` only).
    ///
    /// ## Bracket-index validation
    ///
    /// Quoted `$prev[...]` strings are routed through the same bracket-body
    /// validator as unquoted refs: only non-negative integers are accepted inside
    /// `[...]`. Malformed brackets return `None` (treated as a literal).
    fn string_as_prev_ref(s: &str) -> Option<ArgValue> {
        // Escape: `\$prev...` → strip the leading backslash, return literal.
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
        // "$prev.field..."
        if let Some(rest) = s.strip_prefix("$prev.") {
            if !rest.is_empty() {
                return Some(ArgValue::PrevRef {
                    path: rest.to_owned(),
                });
            }
        }
        // "$prev[N]..." — validate bracket body before promoting.
        if let Some(after_bracket) = s.strip_prefix("$prev[") {
            // after_bracket is everything after "[", e.g. "0].id" or "-1].id"
            if let Some(close) = after_bracket.find(']') {
                let index_str = &after_bracket[..close];
                // Only non-negative integers are valid.
                if !index_str.is_empty() && index_str.chars().all(|c| c.is_ascii_digit()) {
                    let tail = &after_bracket[close + 1..]; // e.g. ".id" after "]"
                    let path = format!("[{index_str}]{tail}");
                    return Some(ArgValue::PrevRef { path });
                }
            }
            // Malformed bracket — treat as invalid literal.
            return None;
        }
        None
    }

    /// Walk forward through the input to find the end of a JSON value, respecting
    /// nested brackets / braces and string literals. The returned index is one
    /// past the last byte of the value (exclusive).
    fn scan_value_end(&self) -> Result<usize, DslError> {
        let mut i = self.pos;
        let mut depth_paren: i32 = 0; // `(` from the surrounding op
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
                        // we never opened a paren here; this terminates the value.
                        return Ok(i);
                    }
                    depth_brack -= 1;
                }
                '{' => depth_brace += 1,
                '}' => {
                    if depth_brace == 0 {
                        // Closing brace outside any open brace — terminates the
                        // current value (e.g. a string value inside an object literal
                        // parsed by parse_object_arg).
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
                ',' => {
                    if depth_paren == 0 && depth_brack == 0 && depth_brace == 0 {
                        return Ok(i);
                    }
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

/// Return `true` if the string value is a `$prev` reference written inside JSON
/// quotes. Used to detect `"$prev.id"` literals in JSON-form input.
///
/// Matches exactly `$prev`, strings starting with `$prev.`, or strings starting
/// with `$prev[` (bracket-index form). Does NOT match `$prevish.id` — the prefix
/// boundary is `.` or `[` only.
fn is_prev_ref_string(s: &str) -> bool {
    s == "$prev" || s.starts_with("$prev.") || s.starts_with("$prev[")
}

/// Recursively scan a JSON value for any string that is a `$prev` reference.
///
/// This covers nested arrays and objects, so `{"ids": ["$prev.id"]}` and
/// `{"nested": {"id": "$prev[0].id"}}` are both detected.
fn json_value_contains_prev_ref(v: &Value) -> bool {
    match v {
        Value::String(s) => is_prev_ref_string(s),
        Value::Array(arr) => arr.iter().any(json_value_contains_prev_ref),
        Value::Object(map) => map.values().any(json_value_contains_prev_ref),
        _ => false,
    }
}

/// A single segment in a `$prev` path — either a field name or an array index.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
}

/// Split a dotted path that may contain bracket array indices into segments.
///
/// `"items[0].id"` → `[Field("items"), Index(0), Field("id")]`
/// `"[2].name"` → `[Index(2), Field("name")]`
/// `"plain.path"` → `[Field("plain"), Field("path")]`
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

/// Scan an op's args for any `PrevRef` (or `Array`/`Object` containing one) and
/// return a representative position (0) if any is found. Used to emit
/// `PrevRefOutsideChain` at parse time for Single and Parallel modes.
fn find_prev_ref_pos(op: &ParsedOp) -> Option<usize> {
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

fn char_label(c: char) -> &'static str {
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
