use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

/// Hard cap on operations per request.
pub const MAX_OPS: usize = 100;

/// Hard cap on the byte length of a raw `ops` input string, checked before any
/// parsing begins. A cheap O(1) first line of defense against pathological
/// input; does not by itself bound container-nesting depth (see
/// [`NESTING_DEPTH_LIMIT`]), since a compact payload can still pack tens of
/// thousands of nesting levels into a few KiB.
pub const MAX_OPS_INPUT_LEN: usize = 256 * 1024;

/// Hard cap on container-nesting depth (`[`/`{`) tracked through the DSL
/// parser, the JSON-form pre-pass scan, and (by construction, since
/// `ArgValue::Array` and `ArgValue::Object` are only ever built inside the
/// depth-guarded parser functions) any `$prev` reference nested inside
/// array/object literals.
///
/// 64 gives generous headroom over real khive `ops` payloads (observed at 2-4
/// levels of nesting) while remaining far too shallow for a native recursive
/// descent to threaten the thread stack (CWE-674).
pub const NESTING_DEPTH_LIMIT: usize = 64;

/// Names reserved at the request-envelope level; rejected if they appear inside verb args.
pub const RESERVED_ENVELOPE_ARGS: &[&str] = &["presentation", "presentation_per_op"];

/// Execution mode: `Single` (one op), `Parallel` (`[...]`), or `Chain` (`op | op`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecutionMode {
    /// One operation, no batching or chaining.
    Single,
    /// `[op1(...), op2(...)]` — parallel, best-effort, independent results.
    Parallel,
    /// `op1(...) | op2(id=$prev.id)` — sequential, abort-on-failure.
    Chain,
}

/// An argument value in a [`ParsedOp`]: concrete JSON, a `$prev` path ref, or a nested container.
#[derive(Debug, Clone, PartialEq)]
pub enum ArgValue {
    /// A concrete JSON value (no `$prev` references anywhere inside).
    Value(Value),
    /// A `$prev` or `$prev.field.path` back-reference (chain mode only); empty `path` = whole result.
    PrevRef { path: String },
    /// Array literal with at least one `$prev` element; pure-JSON arrays fold to `Value(Array)`.
    Array(Vec<ArgValue>),
    /// Object literal with at least one `$prev` value; pure-JSON objects fold to `Value(Object)`.
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

    /// Resolve a `$prev` reference against a preceding op's result, returning `None` on miss.
    pub fn resolve_prev<'a>(&self, prev_result: &'a Value) -> Option<&'a Value> {
        let ArgValue::PrevRef { path } = self else {
            return None;
        };
        if path.is_empty() {
            return Some(prev_result);
        }
        let mut cur = prev_result;
        for segment in crate::parser::split_path(path) {
            cur = crate::parser::apply_path_segment(cur, segment)?;
        }
        Some(cur)
    }

    /// Recursively resolve all `$prev` refs within this value; returns `None` if any path is absent.
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

/// A single parsed operation: tool name plus named argument bag.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOp {
    pub tool: String,
    pub args: BTreeMap<String, ArgValue>,
}

/// Result of parsing a `request` input: a list of ops and their execution mode.
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
    /// Raw `ops` input exceeds [`MAX_OPS_INPUT_LEN`], rejected before any parsing begins.
    InputTooLarge {
        len: usize,
        max: usize,
    },
    /// Container nesting (`[`/`{`) exceeds [`NESTING_DEPTH_LIMIT`]. Covers the
    /// function-call array/object parser, the JSON-form pre-pass scan, and any
    /// `$prev` reference nested inside array/object literals (bounded at the
    /// same construction sites).
    NestingTooDeep {
        pos: usize,
        depth: usize,
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
    /// `$prev` reference used outside a chain context — emitted for Single-op
    /// and Parallel-batch forms, and for JSON form.
    ///
    /// `$prev` references are only meaningful in chain (`|`) mode. If they
    /// appear in a non-chain context the parser rejects the request here so
    /// downstream consumers get a typed error rather than a runtime string.
    PrevRefOutsideChain {
        pos: usize,
    },
    /// `$prev` found in JSON-form input — JSON form does not support chains.
    ///
    /// JSON form (`[{"tool":"...","args":{...}},...]`) always runs in parallel.
    /// To use `$prev` substitution, use the function-call DSL with the `|`
    /// chain operator: `verb1(...) | verb2(id=$prev.id)`.
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
    /// Two or more ops in a parallel batch write to the same UUID.
    ///
    /// Write-key conflict detection is a preflight check applied after parsing.
    /// Write ops are: `update`, `delete`, `merge`, `link`. When two ops share
    /// the same `id` (or `into_id` / `from_id` for `merge`,
    /// `source_id`/`target_id` for `link`) the batch is rejected before any op
    /// is dispatched.
    WriteKeyConflict {
        /// The duplicated UUID.
        id: String,
        /// Name of the first op that claimed the key.
        first_op: String,
        /// Name of the second op that conflicts.
        second_op: String,
    },
    /// `presentation` and `presentation_per_op` are envelope-only fields and
    /// must not appear inside individual verb argument lists.
    ReservedEnvelopeArg {
        arg_name: String,
        verb: String,
    },
}

impl fmt::Display for DslError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DslError::Empty => write!(f, "request is empty"),
            DslError::TooManyOps { count, max } => {
                write!(f, "batch has {count} ops; max is {max}")
            }
            DslError::InputTooLarge { len, max } => {
                write!(f, "ops input is {len} bytes; max is {max} bytes")
            }
            DslError::NestingTooDeep { pos, depth, max } => {
                write!(
                    f,
                    "at position {pos}: container nesting depth {depth} exceeds max {max}"
                )
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
            DslError::ReservedEnvelopeArg { arg_name, verb } => {
                write!(
                    f,
                    "argument {arg_name:?} in verb {verb:?} is reserved for the request \
                     envelope; pass it at the envelope level, not inside verb args"
                )
            }
        }
    }
}

impl std::error::Error for DslError {}
