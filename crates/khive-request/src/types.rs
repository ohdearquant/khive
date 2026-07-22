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

    /// Finds the first `$prev` reference within this argument that fails to
    /// resolve against `prev_result`, and explains why. `None` means every
    /// reference resolves (consistent with `resolve_all(prev_result).is_some()`)
    /// or this argument holds no `$prev` reference at all.
    ///
    /// Walks the same segments as `resolve_prev`/`resolve_all` but never
    /// changes what resolves — it exists purely to build an actionable error
    /// message for a `resolve_all` miss.
    pub fn find_prev_failure(&self, prev_result: &Value) -> Option<PrevFailure> {
        self.find_prev_failure_at("", prev_result)
    }

    fn find_prev_failure_at(&self, arg_path: &str, prev_result: &Value) -> Option<PrevFailure> {
        match self {
            ArgValue::Value(_) => None,
            ArgValue::PrevRef { path } => {
                // Empty path = whole-value reference; always resolves. Its
                // downstream shape (bare $prev onto a map/array) is a
                // separate, already-handled concern (UE4-H1), not a lookup miss.
                if path.is_empty() {
                    return None;
                }
                let mut cur = prev_result;
                let mut resolved_prefix = String::new();
                for seg in crate::parser::split_path(path) {
                    match seg {
                        crate::parser::PathSegment::Field(key) => match cur {
                            Value::Object(map) => match map.get(key) {
                                Some(v) => {
                                    cur = v;
                                    if !resolved_prefix.is_empty() {
                                        resolved_prefix.push('.');
                                    }
                                    resolved_prefix.push_str(key);
                                }
                                None => {
                                    let mut available: Vec<String> = map.keys().cloned().collect();
                                    available.sort_unstable();
                                    return Some(PrevFailure::NotFound {
                                        arg_path: arg_path.to_string(),
                                        prev_path: format!("$prev.{path}"),
                                        resolved_prefix: prev_path_prefix(&resolved_prefix),
                                        missing: key.to_string(),
                                        available,
                                    });
                                }
                            },
                            other => {
                                return Some(PrevFailure::WrongType {
                                    arg_path: arg_path.to_string(),
                                    prev_path: format!("$prev.{path}"),
                                    resolved_prefix: prev_path_prefix(&resolved_prefix),
                                    segment: key.to_string(),
                                    expected: "object",
                                    found: json_type_name(other),
                                });
                            }
                        },
                        crate::parser::PathSegment::Index(idx) => match cur {
                            Value::Array(arr) => match arr.get(idx) {
                                Some(v) => {
                                    cur = v;
                                    resolved_prefix.push_str(&format!("[{idx}]"));
                                }
                                None => {
                                    return Some(PrevFailure::NotFound {
                                        arg_path: arg_path.to_string(),
                                        prev_path: format!("$prev.{path}"),
                                        resolved_prefix: prev_path_prefix(&resolved_prefix),
                                        missing: format!("[{idx}]"),
                                        available: vec![format!(
                                            "array has {} element(s)",
                                            arr.len()
                                        )],
                                    });
                                }
                            },
                            other => {
                                return Some(PrevFailure::WrongType {
                                    arg_path: arg_path.to_string(),
                                    prev_path: format!("$prev.{path}"),
                                    resolved_prefix: prev_path_prefix(&resolved_prefix),
                                    segment: format!("[{idx}]"),
                                    expected: "array",
                                    found: json_type_name(other),
                                });
                            }
                        },
                        crate::parser::PathSegment::Malformed(raw) => {
                            return Some(PrevFailure::Unsupported {
                                arg_path: arg_path.to_string(),
                                prev_path: format!("$prev.{path}"),
                                segment: raw.to_string(),
                            });
                        }
                    }
                }
                None
            }
            ArgValue::Array(elements) => elements.iter().enumerate().find_map(|(i, el)| {
                el.find_prev_failure_at(&format!("{arg_path}[{i}]"), prev_result)
            }),
            ArgValue::Object(pairs) => pairs.iter().find_map(|(k, v)| {
                let sub = if arg_path.is_empty() {
                    k.clone()
                } else {
                    format!("{arg_path}.{k}")
                };
                v.find_prev_failure_at(&sub, prev_result)
            }),
        }
    }
}

/// Renders the `$prev`-relative path successfully traversed before a failure,
/// for messages like "`$prev.user` is a string, not an object".
fn prev_path_prefix(resolved_prefix: &str) -> String {
    if resolved_prefix.is_empty() {
        "$prev".to_string()
    } else {
        format!("$prev.{resolved_prefix}")
    }
}

fn json_type_name(v: &Value) -> &'static str {
    match v {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// Explains why a `$prev` path failed to resolve, for error-message
/// construction. Distinguishes three mistakes that a single generic
/// "not found" message conflates:
///
/// - the field/index simply isn't there (`NotFound`);
/// - a segment expects an object or array to index into, but the prior
///   result holds a scalar at that point (`WrongType`);
/// - the bracket syntax itself isn't a supported index form (`Unsupported`).
#[derive(Debug, Clone, PartialEq)]
pub enum PrevFailure {
    /// A path segment names a field or index that does not exist on an
    /// otherwise correctly-typed container.
    NotFound {
        /// Path to the offending `$prev` reference within the verb argument
        /// it appears in (empty when the argument itself is the reference).
        arg_path: String,
        /// The full `$prev` path that was attempted, e.g. `$prev.user.id`.
        prev_path: String,
        /// The `$prev`-relative path successfully traversed before the miss.
        resolved_prefix: String,
        /// The field name or `[index]` that could not be found.
        missing: String,
        /// Sibling field names (object) or a length note (array) at the
        /// point of failure.
        available: Vec<String>,
    },
    /// A path segment expects an object (for a field) or an array (for an
    /// index) to continue into, but the prior result holds a different JSON
    /// type at that point.
    WrongType {
        arg_path: String,
        prev_path: String,
        /// The `$prev`-relative path successfully traversed before the type
        /// mismatch — this is what actually holds `found`.
        resolved_prefix: String,
        /// The field name or `[index]` that could not be applied.
        segment: String,
        expected: &'static str,
        found: &'static str,
    },
    /// Bracket syntax that is not a valid non-negative-integer index.
    Unsupported {
        arg_path: String,
        prev_path: String,
        /// The unsupported segment as written, e.g. `[bad]`.
        segment: String,
    },
}

impl fmt::Display for PrevFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fn arg_ref(arg_path: &str) -> String {
            if arg_path.is_empty() {
                String::new()
            } else {
                format!(" (at {arg_path})")
            }
        }
        match self {
            PrevFailure::NotFound {
                arg_path,
                prev_path,
                resolved_prefix,
                missing,
                available,
            } => {
                write!(
                    f,
                    "{prev_path}{arg}: {resolved_prefix} has no {missing:?}. \
                     Available top-level fields: [{}]",
                    available.join(", "),
                    arg = arg_ref(arg_path),
                )
            }
            PrevFailure::WrongType {
                arg_path,
                prev_path,
                resolved_prefix,
                segment,
                expected,
                found,
            } => {
                write!(
                    f,
                    "{prev_path}{arg}: {resolved_prefix} is a {found}, not an {expected}, so \
                     {segment:?} cannot be applied to it",
                    arg = arg_ref(arg_path),
                )
            }
            PrevFailure::Unsupported {
                arg_path,
                prev_path,
                segment,
            } => {
                write!(
                    f,
                    "{prev_path}{arg}: {segment:?} is not a supported $prev path segment; \
                     array indices must be a plain non-negative integer, e.g. $prev.items[0]",
                    arg = arg_ref(arg_path),
                )
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
    /// A batch element list has a `,` immediately before its closing `]`,
    /// e.g. `[a(),]` — the slot between the comma and the bracket looks
    /// like a missing element, not a second `,` mistake.
    TrailingComma {
        pos: usize,
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
            DslError::TrailingComma { pos } => {
                write!(
                    f,
                    "at position {pos}: trailing comma before ']' — a batch cannot end with \
                     an empty element; remove the comma or add another op after it"
                )
            }
        }
    }
}

impl std::error::Error for DslError {}
