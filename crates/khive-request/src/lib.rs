//! `khive-request` — transport-agnostic request-DSL parser.
//!
//! Parses the function-call DSL string into a [`ParsedRequest`] / [`ParsedOp`] AST
//! that transports dispatch through `khive_runtime::VerbRegistry`. Supports
//! single ops, parallel batches `[...]`, sequential chains `op1 | op2($prev)`,
//! and raw JSON form.
//!
//! See `docs/protocol.md` for the full DSL grammar, JSON form, `$prev` path
//! semantics, escape rules, and write-key conflict detection contract.

mod conflict;
mod parser;
mod types;

pub use conflict::write_keys_for_op_pub;
pub use parser::parse_request;
pub use types::{
    ArgValue, DslError, ExecutionMode, ParsedOp, ParsedRequest, MAX_OPS, RESERVED_ENVELOPE_ARGS,
};
