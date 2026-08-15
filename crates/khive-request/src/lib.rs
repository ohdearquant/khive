//! `khive-request` — transport-agnostic DSL parser for verb-dispatch requests.

pub mod atomic;
mod conflict;
mod parser;
mod types;

pub use atomic::{check_atomic_admissible, AtomicRejection};
pub use conflict::write_keys_for_op_pub;
pub use parser::{parse_request, parse_typed_json_batch};
pub use types::{
    value_nesting_within_limit, ArgValue, DslError, ExecutionMode, ParsedOp, ParsedRequest,
    PrevFailure, TypedJsonOp, MAX_OPS, MAX_OPS_INPUT_LEN, NESTING_DEPTH_LIMIT,
    RESERVED_ENVELOPE_ARGS,
};
