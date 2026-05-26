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
//! ## Today's syntax (ADR-016)
//!
//! - **Single op**: `tool_name(arg=value, arg=value)` — `ExecutionMode::Single`
//! - **Parallel batch**: `[tool_name(...), tool_name(...)]` — `ExecutionMode::Parallel`
//! - **Sequential chain**: `op1(...) | op2(id=$prev.id)` — `ExecutionMode::Chain`
//! - **JSON form**: `[{"tool":"...", "args": {...}}, ...]` (or a single object)
//!
//! Argument values are JSON literals — strings, numbers, booleans, `null`,
//! arrays, objects. Chain-only: `$prev` and `$prev.field.path` references resolve
//! at dispatch time against the preceding op's result.

use std::collections::BTreeMap;
use std::fmt;

use serde_json::{Map, Value};

/// Hard cap on operations per request. ADR-016 §Why-100.
pub const MAX_OPS: usize = 100;

/// Execution mode for a [`ParsedRequest`] (ADR-016).
///
/// - `Single`: one operation, no batching.
/// - `Parallel`: operations separated by `,` inside `[...]`; run concurrently,
///   results in input order.
/// - `Chain`: operations separated by `|`; run sequentially, each op may
///   reference the prior op's result via `$prev` / `$prev.field.path`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionMode {
    /// One operation, no batching or chaining.
    Single,
    /// `[op1(...), op2(...)]` — parallel, best-effort, independent results.
    Parallel,
    /// `op1(...) | op2(id=$prev.id)` — sequential, abort-on-failure.
    Chain,
}

/// An argument value in a [`ParsedOp`].
///
/// Most arguments are concrete JSON values. In chain ops (ADR-016 §Chain
/// semantics), arguments may reference the preceding op's result via `$prev`
/// or `$prev.dotted.path`. Substitution happens at dispatch time, not at parse
/// time, because the prior result isn't known until runtime.
///
/// `$prev` references may also appear inside array or object literals, which is
/// why `Array` and `Object` variants exist alongside the flat `PrevRef` variant.
/// The dispatcher resolves these recursively before calling the verb handler.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    /// A concrete JSON value (no `$prev` references anywhere inside).
    Value(Value),
    /// A `$prev` or `$prev.field.path` reference — chain mode only.
    ///
    /// `path` is the dot-separated field path after `$prev`. Empty string means
    /// the whole prior result (`$prev` with no field selector).
    PrevRef { path: String },
    /// An array literal whose elements may themselves be `ArgValue`s (including
    /// nested `PrevRef` or further `Array`/`Object`).  Used when at least one
    /// element contains a `$prev` reference; pure-JSON arrays are still
    /// represented as `Value(Value::Array(_))` for efficiency.
    Array(Vec<ArgValue>),
    /// An object literal whose values may themselves be `ArgValue`s.  Used when
    /// at least one value contains a `$prev` reference; pure-JSON objects are
    /// still represented as `Value(Value::Object(_))`.
    Object(Vec<(String, ArgValue)>),
}

impl ArgValue {
    /// Returns the contained [`Value`] if this is `ArgValue::Value`.
    pub fn as_value(&self) -> Option<&Value> {
        match self {
            ArgValue::Value(v) => Some(v),
            ArgValue::PrevRef { .. } | ArgValue::Array(_) | ArgValue::Object(_) => None,
        }
    }

    /// Returns `true` if this is a `$prev` reference.
    pub fn is_prev_ref(&self) -> bool {
        matches!(self, ArgValue::PrevRef { .. })
    }

    /// Resolve a `$prev` reference against a preceding op's result.
    ///
    /// Returns the extracted field value, or `None` if the path doesn't
    /// exist in `prev_result`. Non-`PrevRef` variants return `None`.
    pub fn resolve_prev<'a>(&self, prev_result: &'a Value) -> Option<&'a Value> {
        let ArgValue::PrevRef { path } = self else {
            return None;
        };
        if path.is_empty() {
            return Some(prev_result);
        }
        let mut cur = prev_result;
        for segment in path.split('.') {
            cur = cur.get(segment)?;
        }
        Some(cur)
    }

    /// Recursively resolve all `$prev` references within this value.
    ///
    /// - `PrevRef`: resolved via `resolve_prev`.
    /// - `Array`/`Object`: each element/value is resolved recursively.
    /// - `Value`: returned as-is (no `$prev` anywhere inside).
    ///
    /// Returns `None` if any `$prev` path is missing from `prev_result`.
    pub fn resolve_all<'a>(&'a self, prev_result: &'a Value) -> Option<Value> {
        match self {
            ArgValue::Value(v) => Some(v.clone()),
            ArgValue::PrevRef { .. } => self.resolve_prev(prev_result).cloned(),
            ArgValue::Array(elements) => {
                let mut out = Vec::with_capacity(elements.len());
                for el in elements {
                    out.push(el.resolve_all(prev_result)?);
                }
                Some(Value::Array(out))
            }
            ArgValue::Object(pairs) => {
                let mut map = serde_json::Map::with_capacity(pairs.len());
                for (key, val) in pairs {
                    map.insert(key.clone(), val.resolve_all(prev_result)?);
                }
                Some(Value::Object(map))
            }
        }
    }
}

/// A single parsed operation: tool name + named argument bag.
///
/// Arguments may be concrete [`ArgValue::Value`]s or `$prev` references
/// ([`ArgValue::PrevRef`]) that the dispatcher resolves against the prior op's
/// result (chain mode only).
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOp {
    pub tool: String,
    pub args: BTreeMap<String, ArgValue>,
}

/// Result of parsing a `request` input string (ADR-016).
///
/// The `mode` field tells the dispatcher how to execute the operations:
/// - `Single`: dispatch the one op, wrap in a single-element envelope.
/// - `Parallel`: dispatch all ops concurrently via `join_all`, collect in order.
/// - `Chain`: dispatch ops sequentially; substitute `$prev` references between
///   ops; abort remaining ops when any op or substitution fails.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRequest {
    pub ops: Vec<ParsedOp>,
    pub mode: ExecutionMode,
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
    /// `$prev` reference used outside a chain context.
    PrevRefOutsideChain {
        pos: usize,
    },
    /// Mixing `,` and `|` at the top level.
    MixedSeparators,
    /// Empty batch `[]` — no ops provided.
    EmptyBatch,
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
            DslError::PrevRefOutsideChain { pos } => {
                write!(
                    f,
                    "at position {pos}: $prev reference is only valid in chain (|) mode"
                )
            }
            DslError::MixedSeparators => {
                write!(
                    f,
                    "cannot mix ',' (parallel) and '|' (chain) separators at the top level"
                )
            }
            DslError::EmptyBatch => {
                write!(f, "empty batch not allowed; provide at least one op")
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
        let args: BTreeMap<String, ArgValue> = args_map
            .into_iter()
            .map(|(k, v)| (k, ArgValue::Value(v)))
            .collect();
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
    Ok(ParsedRequest {
        ops,
        mode: ExecutionMode::Parallel,
    })
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
        let mut tool = self.parse_identifier()?;
        // One-level dotted verbs: brain.state, recall.candidates
        if self.peek() == Some('.') {
            self.advance(1);
            let sub = self.parse_identifier()?;
            tool = format!("{tool}.{sub}");
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
    /// Grammar: `$prev` optionally followed by `.identifier(.identifier)*`
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
        // Optional dot-path
        let mut path = String::new();
        while self.peek() == Some('.') {
            self.advance(1); // consume '.'
            let segment = self.parse_identifier()?;
            if !path.is_empty() {
                path.push('.');
            }
            path.push_str(&segment);
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

    fn req(s: &str) -> ParsedRequest {
        parse_request(s).unwrap_or_else(|e| panic!("parse({s:?}) failed: {e}"))
    }

    fn ops(s: &str) -> Vec<ParsedOp> {
        req(s).ops
    }

    /// Extract the concrete `Value` from an `ArgValue::Value`, panicking on dynamic variants.
    fn val(arg: &ArgValue) -> &Value {
        match arg {
            ArgValue::Value(v) => v,
            ArgValue::PrevRef { path } => {
                panic!("expected Value, got PrevRef {{ path: {path:?} }}")
            }
            ArgValue::Array(els) => {
                panic!("expected Value, got Array with {} elements", els.len())
            }
            ArgValue::Object(pairs) => {
                panic!("expected Value, got Object with {} keys", pairs.len())
            }
        }
    }

    #[test]
    fn single_op_no_args() {
        let r = req("gtd.next()");
        assert_eq!(r.mode, ExecutionMode::Single);
        assert_eq!(r.ops.len(), 1);
        assert_eq!(r.ops[0].tool, "gtd.next");
        assert!(r.ops[0].args.is_empty());
    }

    #[test]
    fn single_op_with_string_arg() {
        let v = ops(r#"gtd.assign(title="ship release")"#);
        assert_eq!(v[0].tool, "gtd.assign");
        assert_eq!(val(&v[0].args["title"]), &json!("ship release"));
    }

    #[test]
    fn single_op_with_multiple_typed_args() {
        let v = ops(
            r#"create(kind="entity", entity_kind="concept", name="LoRA", weight=0.9, active=true)"#,
        );
        assert_eq!(v[0].tool, "create");
        assert_eq!(val(&v[0].args["kind"]), &json!("entity"));
        assert_eq!(val(&v[0].args["weight"]), &json!(0.9));
        assert_eq!(val(&v[0].args["active"]), &json!(true));
    }

    #[test]
    fn batch_three_ops() {
        let r = req(
            r#"[create(kind="entity", name="A"), create(kind="entity", name="B"), link(source_id="x", target_id="y", relation="extends")]"#,
        );
        assert_eq!(r.mode, ExecutionMode::Parallel);
        assert_eq!(r.ops.len(), 3);
        assert_eq!(r.ops[0].tool, "create");
        assert_eq!(r.ops[2].tool, "link");
        assert_eq!(val(&r.ops[2].args["relation"]), &json!("extends"));
    }

    #[test]
    fn empty_batch_rejected() {
        // UE4-H2: empty batch must be rejected with EmptyBatch error.
        let err = parse_request("[]").unwrap_err();
        assert!(
            matches!(err, DslError::EmptyBatch),
            "expected EmptyBatch, got {err:?}"
        );
        // JSON form empty array is also rejected.
        let err2 = parse_request("[]").unwrap_err();
        assert!(matches!(err2, DslError::EmptyBatch));
    }

    #[test]
    fn nested_array_and_object_values() {
        let v = ops(r#"gtd.assign(title="x", tags=["a","b"], properties={"k":"v","n":1})"#);
        assert_eq!(val(&v[0].args["tags"]), &json!(["a", "b"]));
        assert_eq!(val(&v[0].args["properties"]), &json!({"k": "v", "n": 1}));
    }

    #[test]
    fn string_with_comma_and_paren_inside() {
        let v = ops(r#"gtd.assign(title="hello, world (now)")"#);
        assert_eq!(val(&v[0].args["title"]), &json!("hello, world (now)"));
    }

    #[test]
    fn string_with_escaped_quote() {
        let v = ops(r#"gtd.assign(title="he said \"hi\"")"#);
        assert_eq!(val(&v[0].args["title"]), &json!("he said \"hi\""));
    }

    #[test]
    fn null_and_negative_number() {
        let v = ops(r#"update(id="x", description=null, weight=-0.5)"#);
        assert_eq!(val(&v[0].args["description"]), &json!(null));
        assert_eq!(val(&v[0].args["weight"]), &json!(-0.5));
    }

    #[test]
    fn json_form_batch_parses() {
        let r =
            req(r#"[{"tool":"gtd.next","args":{}}, {"tool":"gtd.complete","args":{"id":"abc"}}]"#);
        assert_eq!(r.mode, ExecutionMode::Parallel);
        assert_eq!(r.ops.len(), 2);
        assert_eq!(r.ops[1].tool, "gtd.complete");
        assert_eq!(val(&r.ops[1].args["id"]), &json!("abc"));
    }

    #[test]
    fn json_form_with_leading_whitespace_inside_array_parses() {
        // Pretty-printers commonly emit `[ {...} ]` with spaces or newlines after `[`.
        // The whitespace is legal JSON, so the parser must route this to the JSON
        // path rather than the function-call batch parser.
        let v = ops(r#"[  {"tool":"gtd.next","args":{}} ]"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tool, "gtd.next");

        let v = ops("[\n  {\"tool\":\"gtd.next\",\"args\":{}},\n  {\"tool\":\"gtd.complete\",\"args\":{\"id\":\"x\"}}\n]");
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].tool, "gtd.complete");
    }

    #[test]
    fn json_form_single_object_is_treated_as_one_op() {
        let r = req(r#"{"tool":"gtd.next","args":{}}"#);
        assert_eq!(r.mode, ExecutionMode::Single);
        assert_eq!(r.ops.len(), 1);
        assert_eq!(r.ops[0].tool, "gtd.next");
    }

    #[test]
    fn duplicate_arg_rejected() {
        let err = parse_request(r#"gtd.assign(title="a", title="b")"#).unwrap_err();
        assert!(matches!(err, DslError::DuplicateArg { ref name } if name == "title"));
    }

    #[test]
    fn unknown_token_after_op_rejected() {
        let err = parse_request(r#"gtd.next() garbage"#).unwrap_err();
        assert!(matches!(err, DslError::UnexpectedChar { .. }));
    }

    #[test]
    fn unclosed_paren_rejected() {
        let err = parse_request(r#"gtd.assign(title="a""#).unwrap_err();
        // The string is closed; the args list isn't.
        assert!(matches!(err, DslError::UnexpectedEof { .. }));
    }

    #[test]
    fn unterminated_string_rejected() {
        let err = parse_request(r#"gtd.assign(title="oops)"#).unwrap_err();
        assert!(matches!(err, DslError::UnclosedString));
    }

    #[test]
    fn too_many_ops_rejected() {
        let one = r#"gtd.next(),"#;
        let mut s = String::from("[");
        for _ in 0..MAX_OPS + 1 {
            s.push_str(one);
        }
        s.push_str("gtd.next()]");
        let err = parse_request(&s).unwrap_err();
        assert!(matches!(err, DslError::TooManyOps { .. }));
    }

    #[test]
    fn empty_request_rejected() {
        let err = parse_request("   ").unwrap_err();
        assert!(matches!(err, DslError::Empty));
    }

    // ── Required prompt examples ───────────────────────────────────────────────

    #[test]
    fn recall_with_query_arg() {
        let v = ops(r#"memory.recall(query="test")"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tool, "memory.recall");
        assert_eq!(val(&v[0].args["query"]), &json!("test"));
    }

    #[test]
    fn search_with_query_and_limit() {
        let v = ops(r#"search(query="test", limit=5)"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tool, "search");
        assert_eq!(val(&v[0].args["query"]), &json!("test"));
        assert_eq!(val(&v[0].args["limit"]), &json!(5));
    }

    #[test]
    fn parallel_recall_and_inbox() {
        let r = req(r#"[memory.recall(query="x"), comm.inbox()]"#);
        assert_eq!(r.mode, ExecutionMode::Parallel);
        assert_eq!(r.ops.len(), 2);
        assert_eq!(r.ops[0].tool, "memory.recall");
        assert_eq!(val(&r.ops[0].args["query"]), &json!("x"));
        assert_eq!(r.ops[1].tool, "comm.inbox");
        assert!(r.ops[1].args.is_empty());
    }

    // ── JSON form edge cases ───────────────────────────────────────────────────

    #[test]
    fn json_missing_args_defaults_to_empty_map() {
        let v = ops(r#"{"tool":"comm.inbox"}"#);
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].tool, "comm.inbox");
        assert!(v[0].args.is_empty());
    }

    #[test]
    fn json_args_as_array_rejected() {
        let err = parse_request(r#"{"tool":"x","args":[]}"#).unwrap_err();
        assert!(matches!(err, DslError::InvalidJson { .. }));
    }

    // ── Identifier grammar ────────────────────────────────────────────────────

    #[test]
    fn dotted_tool_name_parsed() {
        let v = ops("brain.state()");
        assert_eq!(v[0].tool, "brain.state");
        assert!(v[0].args.is_empty());
    }

    #[test]
    fn dotted_tool_with_args() {
        let v = ops(r#"memory.recall_candidates(query="test", limit=5)"#);
        assert_eq!(v[0].tool, "memory.recall_candidates");
        assert_eq!(val(&v[0].args["query"]), &json!("test"));
        assert_eq!(val(&v[0].args["limit"]), &json!(5));
    }

    #[test]
    fn dotted_tool_in_batch() {
        let v = ops(r#"[brain.state(), memory.recall_fuse(query="x")]"#);
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].tool, "brain.state");
        assert_eq!(v[1].tool, "memory.recall_fuse");
    }

    #[test]
    fn leading_underscore_identifier_is_valid() {
        let v = ops("_internal()");
        assert_eq!(v[0].tool, "_internal");
        assert!(v[0].args.is_empty());
    }

    #[test]
    fn identifier_starting_with_digit_rejected() {
        let err = parse_request("1bad()").unwrap_err();
        assert!(matches!(err, DslError::InvalidIdentifier { pos: 0 }));
    }

    // ── Argument value edge cases ─────────────────────────────────────────────

    #[test]
    fn boolean_false_as_arg_value() {
        let v = ops("flag(active=false)");
        assert_eq!(val(&v[0].args["active"]), &json!(false));
    }

    #[test]
    fn unicode_string_arg_preserved() {
        let v = ops(r#"gtd.assign(title="café")"#);
        assert_eq!(val(&v[0].args["title"]), &json!("café"));
    }

    // ── Chain mode (ADR-016) ──────────────────────────────────────────────────

    #[test]
    fn chain_two_ops_with_prev_ref() {
        let r = req(
            r#"create(kind="entity", entity_kind="concept", name="A") | link(source_id=$prev.id, target_id="abc", relation="extends")"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        assert_eq!(r.ops.len(), 2);
        assert_eq!(r.ops[0].tool, "create");
        assert_eq!(r.ops[1].tool, "link");
        // The second op's source_id should be a PrevRef
        assert_eq!(
            r.ops[1].args["source_id"],
            ArgValue::PrevRef { path: "id".into() }
        );
        // target_id is a concrete value
        assert_eq!(val(&r.ops[1].args["target_id"]), &json!("abc"));
    }

    #[test]
    fn chain_three_ops_mode() {
        let r = req(
            r#"create(kind="entity", name="A") | link(source_id=$prev.id, target_id="b", relation="extends") | update(id=$prev.id, description="desc")"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        assert_eq!(r.ops.len(), 3);
        assert_eq!(r.ops[2].args["id"], ArgValue::PrevRef { path: "id".into() });
    }

    #[test]
    fn chain_prev_no_field_selector() {
        // $prev alone (no dot path) refers to the whole prior result.
        let r = req(r#"gtd.next() | update(id=$prev)"#);
        assert_eq!(r.mode, ExecutionMode::Chain);
        assert_eq!(r.ops[1].args["id"], ArgValue::PrevRef { path: "".into() });
    }

    #[test]
    fn chain_prev_deep_path() {
        let r = req(
            r#"create(kind="entity", name="A") | link(source_id=$prev.result.id, target_id="b", relation="extends")"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        assert_eq!(
            r.ops[1].args["source_id"],
            ArgValue::PrevRef {
                path: "result.id".into()
            }
        );
    }

    #[test]
    fn single_op_mode() {
        let r = req("gtd.next()");
        assert_eq!(r.mode, ExecutionMode::Single);
    }

    #[test]
    fn chain_too_many_ops_rejected() {
        let mut s = String::from("gtd.next()");
        for _ in 0..MAX_OPS {
            s.push_str(" | gtd.next()");
        }
        let err = parse_request(&s).unwrap_err();
        assert!(matches!(err, DslError::TooManyOps { .. }));
    }

    // ── ArgValue helpers ──────────────────────────────────────────────────────

    #[test]
    fn arg_value_resolve_prev_simple() {
        let prev = json!({"id": "abc-123", "name": "A"});
        let r = ArgValue::PrevRef { path: "id".into() };
        assert_eq!(r.resolve_prev(&prev), Some(&json!("abc-123")));
    }

    #[test]
    fn arg_value_resolve_prev_empty_path() {
        let prev = json!({"id": "x"});
        let r = ArgValue::PrevRef { path: "".into() };
        assert_eq!(r.resolve_prev(&prev), Some(&prev));
    }

    #[test]
    fn arg_value_resolve_prev_nested_path() {
        let prev = json!({"result": {"id": "nested-id"}});
        let r = ArgValue::PrevRef {
            path: "result.id".into(),
        };
        assert_eq!(r.resolve_prev(&prev), Some(&json!("nested-id")));
    }

    #[test]
    fn arg_value_resolve_prev_missing_field_returns_none() {
        let prev = json!({"id": "x"});
        let r = ArgValue::PrevRef {
            path: "nonexistent".into(),
        };
        assert_eq!(r.resolve_prev(&prev), None);
    }

    #[test]
    fn arg_value_value_returns_none_for_resolve_prev() {
        let r = ArgValue::Value(json!("hello"));
        assert_eq!(r.resolve_prev(&json!({})), None);
    }

    // ── G-C1: $prev inside array / object literals (regression) ──────────────

    #[test]
    fn chain_prev_in_single_element_array() {
        // `gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.full_id])`
        let r = req(
            r#"gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.full_id])"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        assert_eq!(r.ops.len(), 2);
        match &r.ops[1].args["depends_on"] {
            ArgValue::Array(els) => {
                assert_eq!(els.len(), 1);
                assert_eq!(
                    els[0],
                    ArgValue::PrevRef {
                        path: "full_id".into()
                    }
                );
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn chain_prev_in_mixed_array() {
        // `[$prev.id, "literal-uuid"]` — first element is PrevRef, second is literal.
        let r = req(
            r#"gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.id, "literal-uuid"])"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        match &r.ops[1].args["depends_on"] {
            ArgValue::Array(els) => {
                assert_eq!(els.len(), 2);
                assert_eq!(els[0], ArgValue::PrevRef { path: "id".into() });
                assert_eq!(els[1], ArgValue::Value(json!("literal-uuid")));
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn chain_prev_multiple_in_array() {
        // `depends_on=[$prev.field.deep, $prev.other]`
        let r = req(
            r#"gtd.assign(title="root") | gtd.assign(title="dep", depends_on=[$prev.field.deep, $prev.other])"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        match &r.ops[1].args["depends_on"] {
            ArgValue::Array(els) => {
                assert_eq!(els.len(), 2);
                assert_eq!(
                    els[0],
                    ArgValue::PrevRef {
                        path: "field.deep".into()
                    }
                );
                assert_eq!(
                    els[1],
                    ArgValue::PrevRef {
                        path: "other".into()
                    }
                );
            }
            other => panic!("expected Array, got {other:?}"),
        }
    }

    #[test]
    fn chain_prev_inside_object_inside_array() {
        // `properties={"refs":[$prev.id]}` — nested: object containing array containing PrevRef
        let r = req(
            r#"gtd.assign(title="root") | gtd.assign(title="dep", properties={"refs": [$prev.id]})"#,
        );
        assert_eq!(r.mode, ExecutionMode::Chain);
        match &r.ops[1].args["properties"] {
            ArgValue::Object(pairs) => {
                assert_eq!(pairs.len(), 1);
                assert_eq!(pairs[0].0, "refs");
                match &pairs[0].1 {
                    ArgValue::Array(els) => {
                        assert_eq!(els.len(), 1);
                        assert_eq!(els[0], ArgValue::PrevRef { path: "id".into() });
                    }
                    other => panic!("expected inner Array, got {other:?}"),
                }
            }
            other => panic!("expected Object, got {other:?}"),
        }
    }

    #[test]
    fn pure_json_array_folds_to_value() {
        // An array with no $prev refs should still produce ArgValue::Value(Array(...))
        let v = ops(r#"gtd.assign(title="x", depends_on=["a", "b"])"#);
        assert_eq!(val(&v[0].args["depends_on"]), &json!(["a", "b"]));
    }

    #[test]
    fn pure_json_object_folds_to_value() {
        // An object with no $prev refs should still produce ArgValue::Value(Object(...))
        let v = ops(r#"gtd.assign(title="x", properties={"k": "v"})"#);
        assert_eq!(val(&v[0].args["properties"]), &json!({"k": "v"}));
    }

    #[test]
    fn resolve_all_on_array_with_prev_ref() {
        let prev = json!({"full_id": "abc-def-123"});
        let arr = ArgValue::Array(vec![ArgValue::PrevRef {
            path: "full_id".into(),
        }]);
        assert_eq!(arr.resolve_all(&prev), Some(json!(["abc-def-123"])));
    }

    #[test]
    fn resolve_all_on_mixed_array() {
        let prev = json!({"id": "x"});
        let arr = ArgValue::Array(vec![
            ArgValue::PrevRef { path: "id".into() },
            ArgValue::Value(json!("literal")),
        ]);
        assert_eq!(arr.resolve_all(&prev), Some(json!(["x", "literal"])));
    }

    #[test]
    fn resolve_all_on_nested_object() {
        let prev = json!({"id": "obj-id"});
        let obj = ArgValue::Object(vec![(
            "refs".into(),
            ArgValue::Array(vec![ArgValue::PrevRef { path: "id".into() }]),
        )]);
        assert_eq!(obj.resolve_all(&prev), Some(json!({"refs": ["obj-id"]})));
    }

    #[test]
    fn resolve_all_missing_path_returns_none() {
        let prev = json!({"id": "x"});
        let arr = ArgValue::Array(vec![ArgValue::PrevRef {
            path: "missing".into(),
        }]);
        assert_eq!(arr.resolve_all(&prev), None);
    }
}
