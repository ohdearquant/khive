//! Recursive-descent `Parser` for the verb-dispatch DSL function-call form.

use serde_json::Value;

use crate::types::{ArgValue, DslError, ParsedOp, NESTING_DEPTH_LIMIT};

use super::scan::{char_label, escape_literal_control_chars, scan_string_end};

/// Byte-slice cursor for the DSL input.
pub(crate) struct Parser<'a> {
    pub(crate) src: &'a [u8],
    pub(crate) pos: usize,
    /// Current container-nesting depth (`[`/`{` inside arg values). Guards
    /// `parse_array_arg`/`parse_object_arg` recursion against CWE-674, see
    /// [`NESTING_DEPTH_LIMIT`].
    depth: usize,
}

impl<'a> Parser<'a> {
    /// Create a new parser over the given source string.
    pub(crate) fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
            depth: 0,
        }
    }

    /// Enter one level of container nesting, rejecting once the depth limit
    /// is exceeded. Always pair with a `self.depth -= 1` after the nested
    /// parse completes (on both `Ok` and `Err`, so depth never leaks across
    /// sibling containers).
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

    /// Return true if the cursor is at the end of input.
    pub(crate) fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    /// Peek at the current byte as a char without advancing.
    pub(crate) fn peek(&self) -> Option<char> {
        self.src.get(self.pos).map(|b| *b as char)
    }

    /// Advance the cursor by `n` bytes.
    pub(crate) fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.src.len());
    }

    /// Skip ASCII whitespace.
    pub(crate) fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance(1);
            } else {
                break;
            }
        }
    }

    /// Expect a specific character, returning an error if not found.
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

    /// Parse a complete verb call: `verb(arg=val, ...)` or `pack.verb(...)`.
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

    fn parse_value(&mut self) -> Result<Value, DslError> {
        self.skip_ws();
        let start = self.pos;
        let end = self.scan_value_end()?;
        let slice = std::str::from_utf8(&self.src[start..end])
            .expect("ascii-or-utf8 maintained by scanner");
        let trimmed = slice.trim();
        // A quoted string literal may contain raw control bytes (newline, CR,
        // tab) verbatim in the DSL source; JSON proper forbids that, so
        // rewrite them to JSON escapes before handing the slice to
        // `serde_json`. Non-string values (numbers, bool, null) never
        // legitimately contain such bytes, so this only touches strings.
        let normalized;
        let json_src: &str = if trimmed.starts_with('"') {
            normalized = escape_literal_control_chars(trimmed);
            &normalized
        } else {
            trimmed
        };
        let value: Value = serde_json::from_str(json_src).map_err(|e| DslError::InvalidValue {
            pos: start,
            error: e.to_string(),
        })?;
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

/// Validate a quoted `$prev` path tail (everything after `$prev.` or after the
/// first `[N]` of a root `$prev[N]...` form) using the same bracket grammar as
/// unquoted refs: every `[...]` segment must be non-empty digits followed by
/// `]`, and a bare `]` outside a bracket is invalid. Malformed segments
/// anywhere in the path must keep the whole string a literal `ArgValue::Value`
/// rather than being promoted to `ArgValue::PrevRef`.
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
