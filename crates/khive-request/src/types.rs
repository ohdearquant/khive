use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

/// Maximum operations in a batch or chain.
pub const MAX_OPS: usize = 100;

/// Maximum raw `ops` byte length, checked before parsing.
///
/// See `crates/khive-request/docs/api/limits-and-errors.md` for the bulk-size rationale.
pub const MAX_OPS_INPUT_LEN: usize = 1024 * 1024;

/// Maximum array/object nesting in function and JSON forms.
///
/// See `crates/khive-request/docs/api/limits-and-errors.md` for stack-safety details.
pub const NESTING_DEPTH_LIMIT: usize = 64;

/// Names reserved at the request-envelope level; rejected if they appear inside verb args.
pub const RESERVED_ENVELOPE_ARGS: &[&str] = &["presentation", "presentation_per_op"];

/// Returns whether every array/object in `value` is at most `max_depth` deep.
///
/// The walk is iterative, so checking an untrusted handler result cannot itself
/// overflow the thread stack. Scalar roots have depth zero.
/// See `crates/khive-request/docs/api/limits-and-errors.md` for usage with `$prev`.
pub fn value_nesting_within_limit(value: &Value, max_depth: usize) -> bool {
    let mut stack: Vec<(&Value, usize)> = vec![(value, 0)];
    while let Some((v, depth)) = stack.pop() {
        match v {
            Value::Array(items) => {
                let next_depth = depth + 1;
                if next_depth > max_depth {
                    return false;
                }
                stack.extend(items.iter().map(|item| (item, next_depth)));
            }
            Value::Object(map) => {
                let next_depth = depth + 1;
                if next_depth > max_depth {
                    return false;
                }
                stack.extend(map.values().map(|item| (item, next_depth)));
            }
            _ => {}
        }
    }
    true
}

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

/// A concrete JSON argument or chain-time `$prev` expression.
///
/// See `crates/khive-request/docs/api/previous-result.md` for path semantics.
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

    /// Resolves this reference by borrowing from `prev_result`.
    ///
    /// Returns `None` for a non-reference, missing field, non-array index target,
    /// or out-of-range index.
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

    /// Materializes this argument, returning `None` if any nested reference misses.
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

/// One parsed tool name and its deterministically ordered named arguments.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedOp {
    pub tool: String,
    pub args: BTreeMap<String, ArgValue>,
}

/// Parsed operations in input order plus their execution mode.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedRequest {
    pub ops: Vec<ParsedOp>,
    pub mode: ExecutionMode,
}

/// Request syntax or preflight error surfaced as MCP `invalid_params`.
///
/// No operation has executed when this error is returned. Resource, syntax,
/// reference-placement, conflict, and envelope variants retain their relevant
/// count, position, field, or tool names.
/// See `crates/khive-request/docs/api/limits-and-errors.md` for the full taxonomy.
#[derive(Debug, Clone, PartialEq)]
pub enum DslError {
    Empty,
    TooManyOps {
        count: usize,
        max: usize,
    },
    /// Raw input exceeds [`MAX_OPS_INPUT_LEN`] before parsing begins.
    InputTooLarge {
        len: usize,
        max: usize,
    },
    /// Array/object nesting exceeds [`NESTING_DEPTH_LIMIT`].
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
    /// Function-form `$prev` appears outside chain mode.
    PrevRefOutsideChain {
        pos: usize,
    },
    /// `$prev` appears in JSON form, which cannot express chains.
    PrevRefInJsonForm {
        arg_name: String,
    },
    /// Mixing `,` and `|` at the top level.
    MixedSeparators,
    /// Empty batch `[]` — no ops provided.
    EmptyBatch,
    /// Tool name contains more than one namespace dot.
    UnsupportedVerbNesting {
        pos: usize,
    },
    /// Two parallel operations claim the same derived write key.
    WriteKeyConflict {
        /// Duplicated substrate-prefixed key.
        id: String,
        /// Name of the first op that claimed the key.
        first_op: String,
        /// Name of the second op that conflicts.
        second_op: String,
    },
    /// An envelope-only field appears inside a verb argument list.
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
