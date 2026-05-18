//! `khive-request` — request-DSL parser, transport-agnostic.
//!
//! ## Scope
//!
//! Conceptually every transport into khive walks the same pipeline:
//!
//! ```text
//! request string  →  parse  →  ParsedRequest  →  dispatch (VerbRegistry)  →  result
//! ```
//!
//! This crate owns only the *parse* step. The AST it produces (`ParsedRequest`,
//! `ParsedOp`) is consumed by transports (MCP today; HTTP gateway, FFI, CLI
//! in future) which then dispatch through `khive-runtime`'s [`VerbRegistry`].
//!
//! Keeping the parser in its own crate frees us to grow the syntax — pipe
//! chains, `$prev` substitution, LNDL-style natural-language declarations,
//! bash-flavoured redirections — without touching the runtime layering.
//!
//! ## Today's syntax (v0.2 — ADR-020)
//!
//! - **Function-call form**: `tool_name(arg=value, arg=value)`
//! - **Function-call batch**: `[tool_name(...), tool_name(...)]`
//! - **JSON form**: `[{"tool":"...", "args": {...}}, ...]` (or a single object)
//!
//! Argument values are JSON literals — strings, numbers, booleans, `null`,
//! arrays, objects. Top-level operations inside `[...]` run in parallel by
//! convention (the parser preserves order; the transport drives concurrency).
//!
//! ## Planned (deferred to dedicated ADRs)
//!
//! - Pipe chains for sequential dependent ops (`v1(...) | v2(id=$prev.id)`).
//! - LNDL frontend — parses lact-block source and emits the same `ParsedRequest`.
//! - Bash-style redirection / substitution for ops that produce stream output.

use std::fmt;

use serde_json::{Map, Value};

/// Hard cap on operations per request. ADR-020 §Why-100.
pub const MAX_OPS: usize = 100;

/// A single parsed operation: tool name + named argument bag.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOp {
    pub tool: String,
    pub args: Map<String, Value>,
}

/// Result of parsing a `request` input string.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRequest {
    pub ops: Vec<ParsedOp>,
}

/// Parser error — surfaced as `invalid_params` at the MCP boundary.
#[derive(Debug, Clone, PartialEq)]
pub enum DslError {
    Empty,
    TooManyOps {
        count: usize,
        max: usize,
    },
    UnexpectedChar {
        pos: usize,
        found: char,
        expected: &'static str,
    },
    UnexpectedEof {
        expected: &'static str,
    },
    InvalidIdentifier {
        pos: usize,
    },
    DuplicateArg {
        name: String,
    },
    InvalidValue {
        pos: usize,
        error: String,
    },
    InvalidJson {
        error: String,
    },
    UnclosedString,
    UnclosedBracket {
        kind: char,
    },
}

impl fmt::Display for DslError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DslError::Empty => write!(f, "request is empty"),
            DslError::TooManyOps { count, max } => {
                write!(f, "batch has {count} ops; max is {max}")
            }
            DslError::UnexpectedChar {
                pos,
                found,
                expected,
            } => {
                write!(f, "at position {pos}: expected {expected}, found {found:?}")
            }
            DslError::UnexpectedEof { expected } => {
                write!(f, "unexpected end of input; expected {expected}")
            }
            DslError::InvalidIdentifier { pos } => {
                write!(
                    f,
                    "at position {pos}: invalid identifier (expected [A-Za-z_][A-Za-z0-9_]*)"
                )
            }
            DslError::DuplicateArg { name } => write!(f, "duplicate argument {name:?}"),
            DslError::InvalidValue { pos, error } => {
                write!(f, "at position {pos}: invalid value: {error}")
            }
            DslError::InvalidJson { error } => write!(f, "invalid JSON form: {error}"),
            DslError::UnclosedString => write!(f, "unterminated string literal"),
            DslError::UnclosedBracket { kind } => {
                write!(f, "unclosed bracket: {kind:?} has no matching close")
            }
        }
    }
}

impl std::error::Error for DslError {}

/// Parse a request input string, returning either a single op or a batch.
pub fn parse_request(input: &str) -> Result<ParsedRequest, DslError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DslError::Empty);
    }

    // JSON form: `[{...}, ...]` or `{...}`.
    let first = trimmed.as_bytes()[0];
    let looks_like_json =
        first == b'{' || (first == b'[' && trimmed.as_bytes().get(1).is_some_and(|b| *b == b'{'));
    if looks_like_json {
        return parse_json_form(trimmed);
    }

    // Function-call batch.
    if first == b'[' {
        return parse_fn_batch(trimmed);
    }

    // Single op.
    let mut p = Parser::new(trimmed);
    let op = p.parse_op()?;
    p.skip_ws();
    if !p.eof() {
        return Err(DslError::UnexpectedChar {
            pos: p.pos,
            found: p.peek().unwrap(),
            expected: "end of input",
        });
    }
    Ok(ParsedRequest { ops: vec![op] })
}

fn parse_json_form(input: &str) -> Result<ParsedRequest, DslError> {
    let v: Value = serde_json::from_str(input).map_err(|e| DslError::InvalidJson {
        error: e.to_string(),
    })?;
    let arr: Vec<Value> = match v {
        Value::Array(arr) => arr,
        Value::Object(_) => vec![v],
        other => {
            return Err(DslError::InvalidJson {
                error: format!("expected object or array of objects, got {other}"),
            })
        }
    };
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
        let args = match args {
            Value::Object(m) => m,
            other => {
                return Err(DslError::InvalidJson {
                    error: format!("\"args\" must be an object, got {other}"),
                })
            }
        };
        ops.push(ParsedOp { tool, args });
    }
    Ok(ParsedRequest { ops })
}

fn parse_fn_batch(input: &str) -> Result<ParsedRequest, DslError> {
    let mut p = Parser::new(input);
    p.expect_char('[')?;
    p.skip_ws();
    let mut ops = Vec::new();
    if p.peek() == Some(']') {
        p.advance(1);
        return Ok(ParsedRequest { ops });
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
    Ok(ParsedRequest { ops })
}

// ── recursive-descent parser ────────────────────────────────────────────────

struct Parser<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn new(src: &'a str) -> Self {
        Self {
            src: src.as_bytes(),
            pos: 0,
        }
    }

    fn eof(&self) -> bool {
        self.pos >= self.src.len()
    }

    fn peek(&self) -> Option<char> {
        self.src.get(self.pos).map(|b| *b as char)
    }

    fn advance(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.src.len());
    }

    fn skip_ws(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_ascii_whitespace() {
                self.advance(1);
            } else {
                break;
            }
        }
    }

    fn expect_char(&mut self, want: char) -> Result<(), DslError> {
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

    fn parse_op(&mut self) -> Result<ParsedOp, DslError> {
        let tool = self.parse_identifier()?;
        self.expect_char('(')?;
        self.skip_ws();
        let mut args: Map<String, Value> = Map::new();
        if self.peek() == Some(')') {
            self.advance(1);
            return Ok(ParsedOp { tool, args });
        }
        loop {
            let name = self.parse_identifier()?;
            self.expect_char('=')?;
            self.skip_ws();
            let value = self.parse_value()?;
            if args.contains_key(&name) {
                return Err(DslError::DuplicateArg { name });
            }
            args.insert(name, value);
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

fn scan_string_end(src: &[u8], start: usize) -> Result<usize, DslError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ops(s: &str) -> Vec<ParsedOp> {
        parse_request(s)
            .unwrap_or_else(|e| panic!("parse({s:?}) failed: {e}"))
            .ops
    }

    #[test]
    fn single_op_no_args() {
        let v = ops("next()");
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tool, "next");
        assert!(v[0].args.is_empty());
    }

    #[test]
    fn single_op_with_string_arg() {
        let v = ops(r#"assign(title="ship release")"#);
        assert_eq!(v[0].tool, "assign");
        assert_eq!(v[0].args["title"], json!("ship release"));
    }

    #[test]
    fn single_op_with_multiple_typed_args() {
        let v = ops(
            r#"create(kind="entity", entity_kind="concept", name="LoRA", weight=0.9, active=true)"#,
        );
        assert_eq!(v[0].tool, "create");
        assert_eq!(v[0].args["kind"], json!("entity"));
        assert_eq!(v[0].args["weight"], json!(0.9));
        assert_eq!(v[0].args["active"], json!(true));
    }

    #[test]
    fn batch_three_ops() {
        let v = ops(
            r#"[create(kind="entity", name="A"), create(kind="entity", name="B"), link(source_id="x", target_id="y", relation="extends")]"#,
        );
        assert_eq!(v.len(), 3);
        assert_eq!(v[0].tool, "create");
        assert_eq!(v[2].tool, "link");
        assert_eq!(v[2].args["relation"], json!("extends"));
    }

    #[test]
    fn empty_batch_is_legal() {
        let v = ops("[]");
        assert!(v.is_empty());
    }

    #[test]
    fn nested_array_and_object_values() {
        let v = ops(r#"assign(title="x", tags=["a","b"], properties={"k":"v","n":1})"#);
        assert_eq!(v[0].args["tags"], json!(["a", "b"]));
        assert_eq!(v[0].args["properties"], json!({"k": "v", "n": 1}));
    }

    #[test]
    fn string_with_comma_and_paren_inside() {
        let v = ops(r#"assign(title="hello, world (now)")"#);
        assert_eq!(v[0].args["title"], json!("hello, world (now)"));
    }

    #[test]
    fn string_with_escaped_quote() {
        let v = ops(r#"assign(title="he said \"hi\"")"#);
        assert_eq!(v[0].args["title"], json!("he said \"hi\""));
    }

    #[test]
    fn null_and_negative_number() {
        let v = ops(r#"update(id="x", description=null, weight=-0.5)"#);
        assert_eq!(v[0].args["description"], json!(null));
        assert_eq!(v[0].args["weight"], json!(-0.5));
    }

    #[test]
    fn json_form_batch_parses() {
        let v = ops(r#"[{"tool":"next","args":{}}, {"tool":"complete","args":{"id":"abc"}}]"#);
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].tool, "complete");
        assert_eq!(v[1].args["id"], json!("abc"));
    }

    #[test]
    fn json_form_single_object_is_treated_as_one_op() {
        let v = ops(r#"{"tool":"next","args":{}}"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tool, "next");
    }

    #[test]
    fn duplicate_arg_rejected() {
        let err = parse_request(r#"assign(title="a", title="b")"#).unwrap_err();
        assert!(matches!(err, DslError::DuplicateArg { ref name } if name == "title"));
    }

    #[test]
    fn unknown_token_after_op_rejected() {
        let err = parse_request(r#"next() garbage"#).unwrap_err();
        assert!(matches!(err, DslError::UnexpectedChar { .. }));
    }

    #[test]
    fn unclosed_paren_rejected() {
        let err = parse_request(r#"assign(title="a""#).unwrap_err();
        // The string is closed; the args list isn't.
        assert!(matches!(err, DslError::UnexpectedEof { .. }));
    }

    #[test]
    fn unterminated_string_rejected() {
        let err = parse_request(r#"assign(title="oops)"#).unwrap_err();
        assert!(matches!(err, DslError::UnclosedString));
    }

    #[test]
    fn too_many_ops_rejected() {
        let one = r#"next(),"#;
        let mut s = String::from("[");
        for _ in 0..MAX_OPS + 1 {
            s.push_str(one);
        }
        s.push_str("next()]");
        let err = parse_request(&s).unwrap_err();
        assert!(matches!(err, DslError::TooManyOps { .. }));
    }

    #[test]
    fn empty_request_rejected() {
        let err = parse_request("   ").unwrap_err();
        assert!(matches!(err, DslError::Empty));
    }
}
