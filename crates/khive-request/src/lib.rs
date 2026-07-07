//! `khive-request` — transport-agnostic DSL parser for verb-dispatch requests.

pub mod atomic;
mod conflict;
mod parser;
mod types;

pub use atomic::{check_atomic_admissible, AtomicRejection};
pub use conflict::write_keys_for_op_pub;
pub use parser::parse_request;
pub use types::{
    ArgValue, DslError, ExecutionMode, ParsedOp, ParsedRequest, MAX_OPS, RESERVED_ENVELOPE_ARGS,
};
