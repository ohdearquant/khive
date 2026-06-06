use std::collections::BTreeMap;
use std::fmt;

use serde_json::Value;

/// Hard cap on operations per request. Keeping batches bounded prevents
/// unbounded memory growth and ensures latency stays predictable.
pub const MAX_OPS: usize = 100;

/// Execution mode for a [`ParsedRequest`].
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
/// Most arguments are concrete JSON values. In chain ops, arguments may
/// reference the preceding op's result via `$prev` or `$prev.dotted.path`.
/// Substitution happens at dispatch time, not at parse time, because the
/// prior result isn't known until runtime.
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
    /// `$prev[0].name`. Bracket indices are parsed as `usize`.
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

/// Result of parsing a `request` input string.
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
