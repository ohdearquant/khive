// FILE SIZE JUSTIFICATION: All production code is a single-pass recursive-descent
// parser whose phases (lexer helpers, JSON-form, function-call batch, chain, Parser
// struct, and path-resolution utilities) share deeply-coupled private functions
// (scan_value_end, scan_string_end, split_path, apply_path_segment, find_prev_ref_pos).
// Splitting into submodules would require making all of those helpers pub(crate),
// increasing API surface and breaking the encapsulation invariant that no partial
// parse state escapes the parser. The 1 273-line production section is one cohesive
// unit; an integration test file (tests/parser.rs) holds the public-boundary tests.

//! `khive-request` — transport-agnostic request-DSL parser (ADR-016).
//!
//! Parses the function-call DSL string into a [`ParsedRequest`] / [`ParsedOp`] AST
//! that transports dispatch through [`khive_runtime::VerbRegistry`]. Supports
//! single ops, parallel batches `[...]`, sequential chains `op1 | op2($prev)`,
//! and raw JSON form.
//!
//! See `docs/protocol.md` for the full DSL grammar, JSON form, `$prev` path
//! semantics, escape rules, and write-key conflict detection contract.

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
    ///
    /// Path segments may include array indices: `$prev.items[0].id` or
    /// `$prev[0].name`. Bracket indices are parsed as `usize` (ue-dsl-chain H1).
    pub fn resolve_prev<'a>(&self, prev_result: &'a Value) -> Option<&'a Value> {
        let ArgValue::PrevRef { path } = self else {
            return None;
        };
        if path.is_empty() {
            return Some(prev_result);
        }
        let mut cur = prev_result;
        for segment in split_path(path) {
            cur = apply_path_segment(cur, segment)?;
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
    /// `$prev` reference used outside a chain context (emitted by the parser for
    /// Single-op and Parallel-batch forms, and for JSON form).
    ///
    /// # Policy
    ///
    /// `$prev` references are only meaningful in chain (`|`) mode. If they appear
    /// in a non-chain context the parser rejects the request here so downstream
    /// consumers that pattern-match on `DslError` get a typed error rather than
    /// a runtime string.
    PrevRefOutsideChain {
        pos: usize,
    },
    /// `$prev` found in JSON-form input — JSON form does not support chains.
    ///
    /// JSON form (`[{"tool":"...","args":{...}},...]`) always runs in parallel.
    /// To use `$prev` substitution, use the function-call DSL with the `|` chain
    /// operator: `verb1(...) | verb2(id=$prev.id)`.
    PrevRefInJsonForm {
        arg_name: String,
    },
    /// Mixing `,` and `|` at the top level.
    MixedSeparators,
    /// Empty batch `[]` — no ops provided.
    EmptyBatch,
    /// Dotted verb name with more than one level (e.g. `a.b.c`). Only
    /// single-level dotted names are supported (`a.b`).
    UnsupportedVerbNesting {
        pos: usize,
    },
    /// Two or more ops in a parallel batch write to the same UUID (ADR-038).
    ///
    /// Write-key conflict detection is a preflight check applied after parsing.
    /// Write ops are: `update`, `delete`, `merge`, `link`. When two ops share the
    /// same `id` (or `into_id` / `from_id` for `merge`, `source_id`/`target_id`
    /// for `link`) the batch is rejected before any op is dispatched.
    WriteKeyConflict {
        /// The duplicated UUID.
        id: String,
        /// Name of the first op that claimed the key.
        first_op: String,
        /// Name of the second op that conflicts.
        second_op: String,
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
            DslError::PrevRefOutsideChain { pos } => {
                write!(
                    f,
                    "at position {pos}: $prev reference is only valid in chain (|) mode; \
                     use function-call form with '|' to chain ops"
                )
            }
            DslError::PrevRefInJsonForm { arg_name } => {
                write!(
                    f,
                    "argument {arg_name:?}: $prev substitution requires the function-call DSL \
                     with the chain (|) operator; JSON form does not support chains. \
                     Use: verb1(...) | verb2({arg_name}=$prev.id)"
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
            DslError::UnsupportedVerbNesting { pos } => {
                write!(
                    f,
                    "at position {pos}: only single-level dotted verb names are supported \
                     (e.g. brain.state); use a shorter name or register a pack alias"
                )
            }
            DslError::WriteKeyConflict {
                id,
                first_op,
                second_op,
            } => {
                write!(
                    f,
                    "write-key conflict: id {id:?} is targeted by both {first_op:?} and \
                     {second_op:?} in the same batch; split into separate requests"
                )
            }
        }
    }
}

impl std::error::Error for DslError {}

/// Check a parsed batch for write-key conflicts (ADR-038 preflight).
///
/// Write operations (`update`, `delete`, `merge`, `link`) target specific UUIDs.
/// If two ops in the same parallel (or single-op) batch write to the same UUID,
/// the batch is rejected before any op is dispatched.
///
/// Chain mode is excluded: sequential ops intentionally build on prior results
/// and the runtime resolves `$prev` references between them.
///
/// The checked keys per verb:
/// - `update` / `delete`: `id`
/// - `merge`:             `into_id`, `from_id`
/// - `link`:              `source_id`, `target_id`
///
/// Only concrete `ArgValue::Value(String)` arguments are checked; `$prev`
/// references are skipped because their target is not known until dispatch time.
///
/// # Not the ADR-038 transport contract
///
/// This function returns a single batch-level [`DslError::WriteKeyConflict`] for
/// the first conflict found. It is a parse-time preflight helper, not the
/// per-op envelope described in ADR-038 §envelope. The MCP server builds per-op
/// envelopes using [`write_keys_for_op_pub`] directly. Do not expose this
/// function as an MCP-level conflict API or downstream callers may violate the
/// per-op envelope invariant.
#[cfg(test)]
pub(crate) fn check_write_key_conflicts(req: &ParsedRequest) -> Result<(), DslError> {
    // Chain mode is sequentially ordered; skip conflict detection.
    if req.mode == ExecutionMode::Chain {
        return Ok(());
    }
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    for op in &req.ops {
        let keys = write_keys_for_op(op);
        for key in keys {
            if let Some(first) = seen.get(&key) {
                return Err(DslError::WriteKeyConflict {
                    id: key,
                    first_op: first.clone(),
                    second_op: op.tool.clone(),
                });
            }
            seen.insert(key, op.tool.clone());
        }
    }
    Ok(())
}

/// Extract write-conflict keys from a single op for preflight conflict detection (ADR-038).
///
/// Keys are substrate-prefixed to avoid false positives between different substrates:
/// - Entity write ops (`update`, `delete`, `merge`): `entity:<uuid>`
/// - Edge write ops (`link`): `edge-natural:<source_id>:<target_id>:<relation>`
///   — `link` creates an edge record, NOT an entity write, so source/target entity
///   IDs must NOT be used as entity-level keys.
///
/// Only concrete `ArgValue::Value(String)` args are checked; `$prev` refs are skipped.
///
/// # Why `create` is excluded
///
/// `create` generates its UUID server-side and the ID is not statically known at parse
/// time, so there is no key to conflict on.  The existing DB-level serialization handles
/// concurrent creates safely (unique constraint on the generated UUID).
///
/// This function is `pub` so the MCP server can call it directly without the full
/// `check_write_key_conflicts` batch-level function.
pub fn write_keys_for_op_pub(op: &ParsedOp) -> Vec<String> {
    let mut keys = Vec::new();
    match op.tool.as_str() {
        "update" | "delete" => {
            if let Some(ArgValue::Value(Value::String(s))) = op.args.get("id") {
                keys.push(format!("entity:{s}"));
            }
        }
        "merge" => {
            for name in &["into_id", "from_id"] {
                if let Some(ArgValue::Value(Value::String(s))) = op.args.get(*name) {
                    keys.push(format!("entity:{s}"));
                }
            }
        }
        "link" => {
            // `link` writes an edge, not an entity.  Use a natural-key format so
            // update(id="X") + link(source_id="X", ...) do NOT conflict (they target
            // different substrates).
            let src = op.args.get("source_id");
            let tgt = op.args.get("target_id");
            let rel = op.args.get("relation");
            if let (
                Some(ArgValue::Value(Value::String(s))),
                Some(ArgValue::Value(Value::String(t))),
                Some(ArgValue::Value(Value::String(r))),
            ) = (src, tgt, rel)
            {
                keys.push(format!("edge-natural:{s}:{t}:{r}"));
            }
        }
        _ => {}
    }
    keys
}

/// Extract the write-key UUIDs from a single op (internal helper, kept for tests).
#[cfg(test)]
fn write_keys_for_op(op: &ParsedOp) -> Vec<String> {
    write_keys_for_op_pub(op)
}

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
        // PrevRef in a single op is always invalid (adr-dsl-packs H1).
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
        // PrevRefInJsonForm error (ue-dsl-chain C1, fix: recursive scan).
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
    // PrevRef inside a function-call parallel batch is invalid (adr-dsl-packs H1).
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
            // Only one level of dotting is supported. A second '.' is a clear
            // error (adr-dsl-packs H2) — emit UnsupportedVerbNesting instead of
            // the misleading "expected '|' or end of input, found '.'" message.
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
    /// CC-3: a quoted string like `"$prev.id"` is treated identically to the
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
        // CC-3: promote quoted "$prev[.path]" strings to PrevRef.
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
    /// (ue-dsl-chain H1: minimal array-index support added).
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
    /// inside quotes (CC-3). Returns `Some(PrevRef)` if so, or
    /// `Some(Value(...))` if the string is an escaped literal, or `None` if
    /// neither (i.e. an ordinary string the caller should store as-is).
    ///
    /// ## Escape semantics (High-2 fix)
    ///
    /// A string like `"$prev.id"` deserializes to the Rust string `$prev.id`
    /// and is promoted to `ArgValue::PrevRef { path: "id" }`.
    ///
    /// To pass the **literal** string `$prev.id` as a value, write `"\\$prev.id"`
    /// in the DSL source. That deserializes to `\$prev.id` (one leading backslash).
    /// This function strips the leading `\` and returns
    /// `ArgValue::Value(json!("$prev.id"))` — so the handler receives the clean
    /// string without the escape marker.
    ///
    /// `$prevish.id` does NOT match (prefix boundary is `.` or `[` only).
    ///
    /// ## Bracket-index validation (Medium-1 fix)
    ///
    /// Quoted `$prev[...]` strings are routed through the same bracket-body
    /// validator as unquoted refs: only non-negative integers are accepted inside
    /// `[...]`. Malformed brackets (negative index, non-numeric, unclosed) return
    /// `None` (treated as a literal, consistent with the caller treating unknown
    /// forms as values).
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
        // "$prev[N]..." — validate bracket body before promoting (Medium-1 fix).
        if let Some(after_bracket) = s.strip_prefix("$prev[") {
            // after_bracket is everything after "[", e.g. "0].id" or "-1].id"
            if let Some(close) = after_bracket.find(']') {
                let index_str = &after_bracket[..close];
                // Only non-negative integers are valid.
                if !index_str.is_empty() && index_str.chars().all(|c| c.is_ascii_digit()) {
                    let tail = &after_bracket[close + 1..]; // "].id" → ".id" after close
                                                            // path encodes as "[N]..." (used by split_path)
                    let path = format!("[{index_str}]{tail}");
                    return Some(ArgValue::PrevRef { path });
                }
            }
            // Malformed bracket (missing ']', negative, non-numeric) — treat as invalid.
            // Return None so the caller stores it as a literal Value.
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
/// quotes. Used to detect `"$prev.id"` literals in JSON-form input (ue-dsl-chain C1).
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
/// `{"nested": {"id": "$prev[0].id"}}` are both detected (fix for High-1).
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
enum PathSegment<'a> {
    Field(&'a str),
    Index(usize),
}

/// Split a dotted path that may contain bracket array indices into segments.
///
/// `"items[0].id"` → `[Field("items"), Index(0), Field("id")]`
/// `"[2].name"` → `[Index(2), Field("name")]`
/// `"plain.path"` → `[Field("plain"), Field("path")]`
fn split_path(path: &str) -> Vec<PathSegment<'_>> {
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
fn apply_path_segment<'a>(cur: &'a Value, seg: PathSegment<'_>) -> Option<&'a Value> {
    match seg {
        PathSegment::Field(key) => cur.get(key),
        PathSegment::Index(idx) => cur.as_array()?.get(idx),
    }
}

/// Scan an op's args for any `PrevRef` (or `Array`/`Object` containing one) and
/// return a representative position (0 — we don't track source positions per-arg
/// at this stage) if any is found. Used to emit `PrevRefOutsideChain` at parse
/// time for Single and Parallel modes (adr-dsl-packs H1).
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

// INLINE TEST JUSTIFICATION: Only write-key conflict tests remain inline because
// they exercise check_write_key_conflicts, which is pub(crate) and not reachable
// from the integration test crate in tests/. All public-API tests live in
// tests/parser.rs.
#[cfg(test)]
mod tests {
    use super::*;

    // ── ADR-038: write-key conflict detection ─────────────────────────────────
    // These tests call check_write_key_conflicts (pub(crate)) and therefore must
    // remain inline — integration tests in tests/ cannot access pub(crate) items.

    #[test]
    fn no_conflict_on_non_write_ops() {
        // search + list ops share no write keys; must pass.
        let r =
            parse_request(r#"[list(kind="entity"), search(kind="entity", query="x")]"#).unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn update_and_delete_same_id_conflict() {
        // Two ops targeting the same UUID should be rejected.
        // Keys are substrate-prefixed: entity:<uuid>.
        let r =
            parse_request(r#"[update(id="abc-123", name="new"), delete(id="abc-123")]"#).unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, first_op, second_op }
                if id == "entity:abc-123" && first_op == "update" && second_op == "delete"),
            "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
        );
    }

    #[test]
    fn two_updates_same_id_conflict() {
        let r = parse_request(
            r#"[update(id="uuid-1", name="a"), update(id="uuid-1", description="b")]"#,
        )
        .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:uuid-1"),
            "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
        );
    }

    #[test]
    fn merge_from_id_conflicts_with_delete() {
        // merge's from_id overlaps a delete's id — both are entity writes.
        let r =
            parse_request(r#"[merge(into_id="new-id", from_id="old-id"), delete(id="old-id")]"#)
                .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. } if id == "entity:old-id"),
            "expected WriteKeyConflict with entity-prefixed key, got {err:?}"
        );
    }

    #[test]
    fn different_ids_no_conflict() {
        // Each op has a distinct UUID; no conflict.
        let r = parse_request(
            r#"[update(id="id-1", name="a"), delete(id="id-2"), update(id="id-3", name="c")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn chain_mode_skips_conflict_detection() {
        // Chain ops run sequentially; write-key preflight is skipped.
        let r = parse_request(r#"update(id="same-id", name="a") | delete(id="same-id")"#).unwrap();
        assert_eq!(r.mode, ExecutionMode::Chain);
        // Must not return an error even though the same id appears in both ops.
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn link_source_id_does_not_conflict_with_entity_update() {
        // update(id="X") + link(source_id="X", ...) must NOT conflict — `link` writes an
        // edge record, not the entity at "X".  Substrate-prefixed keys distinguish them:
        // entity:X vs edge-natural:X:Y:rel.
        let r = parse_request(
            r#"[update(id="node-1", name="x"), link(source_id="node-1", target_id="node-2", relation="extends")]"#,
        )
        .unwrap();
        check_write_key_conflicts(&r).unwrap();
    }

    #[test]
    fn two_links_same_natural_key_conflict() {
        // Two link ops targeting the same (source, target, relation) triple conflict
        // because they would produce duplicate edges.
        let r = parse_request(
            r#"[link(source_id="a", target_id="b", relation="extends"), link(source_id="a", target_id="b", relation="extends")]"#,
        )
        .unwrap();
        let err = check_write_key_conflicts(&r).unwrap_err();
        assert!(
            matches!(&err, DslError::WriteKeyConflict { id, .. }
                if id == "edge-natural:a:b:extends"),
            "expected WriteKeyConflict on edge-natural key, got {err:?}"
        );
    }

    #[test]
    fn single_write_op_no_conflict() {
        let r = parse_request(r#"delete(id="solo-id")"#).unwrap();
        assert_eq!(r.mode, ExecutionMode::Single);
        check_write_key_conflicts(&r).unwrap();
    }
}
