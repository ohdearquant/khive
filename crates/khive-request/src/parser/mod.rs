//! Recursive-descent parser for the verb-dispatch DSL (ADR-016).

mod path;
mod scan;

use std::collections::BTreeMap;

use serde_json::{Map, Value};

pub(crate) use path::{apply_path_segment, split_path};
pub(crate) use scan::scan_string_end;

use scan::{char_label, find_prev_ref_pos, json_value_contains_prev_ref};

use crate::types::{
    ArgValue, DslError, ExecutionMode, ParsedOp, ParsedRequest, MAX_OPS, RESERVED_ENVELOPE_ARGS,
};

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
        reject_reserved_args(&first_op)?;
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
fn parse_chain_tail(mut p: Parser<'_>, first_op: ParsedOp) -> Result<ParsedRequest, DslError> {
    reject_reserved_args(&first_op)?;
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
        reject_reserved_args(&op)?;
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
        let op = ParsedOp { tool, args };
        reject_reserved_args(&op)?;
        ops.push(op);
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
        reject_reserved_args(op)?;
    }
    Ok(ParsedRequest {
        ops,
        mode: ExecutionMode::Parallel,
    })
}

/// Reject reserved envelope-level args inside a verb's argument list.
fn reject_reserved_args(op: &ParsedOp) -> Result<(), DslError> {
    for reserved in RESERVED_ENVELOPE_ARGS {
        if op.args.contains_key(*reserved) {
            return Err(DslError::ReservedEnvelopeArg {
                arg_name: (*reserved).to_owned(),
                verb: op.tool.clone(),
            });
        }
    }
    Ok(())
}

// -- recursive-descent parser -------------------------------------------------

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
    /// A quoted `"$prev.id"` is promoted identically to unquoted `$prev.id`.
    /// See `docs/protocol.md` for escape semantics.
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
            // Key must be a quoted string.
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
    /// Grammar: `$prev` optionally followed by dot-segments (`.field`)
    /// and/or bracket indices (`[N]`).
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
    /// inside quotes. Returns `Some(PrevRef)` if so, `Some(Value(...))` for
    /// escaped literals (`"\\$prev.id"` -> literal `$prev.id`), or `None`.
    ///
    /// `$prevish.id` does NOT match (prefix boundary is `.` or `[` only).
    /// Malformed bracket indices return `None` (treated as literal).
    /// See `docs/protocol.md` for escape and bracket-index semantics.
    fn string_as_prev_ref(s: &str) -> Option<ArgValue> {
        // Escape: `\$prev...` -> strip the leading backslash, return literal.
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

    /// Walk forward through the input to find the end of a JSON value,
    /// respecting nested brackets/braces and string literals.
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
