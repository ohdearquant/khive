//! Integration tests for the `khive-request` DSL parser.
//!
//! All tests exercise the public API (`parse_request`, public types). Tests that
//! require access to `pub(crate)` helpers (`check_write_key_conflicts`) live in
//! `src/conflict_tests.rs`, registered by `src/conflict.rs` under `#[cfg(test)] mod tests`.

use khive_request::{
    parse_request as parse_request_unplanned, parse_typed_json_batch, plan_request, ArgValue,
    DslError, ExecutionMode, TypedJsonOp, MAX_OPS, MAX_OPS_INPUT_LEN, NESTING_DEPTH_LIMIT,
    RESERVED_ENVELOPE_ARGS,
};
use serde_json::json;

/// Standalone default; including this corpus as a module lets a transport test
/// supply its own crate-root callback over the same inputs.
pub fn plan_for_parser_corpus(input: &str) -> serde_json::Value {
    plan_request(input, &std::collections::BTreeMap::new())
}

fn parse_request(input: &str) -> Result<khive_request::ParsedRequest, DslError> {
    let parsed = parse_request_unplanned(input);
    if let Err(error) = &parsed {
        let plan = crate::plan_for_parser_corpus(input);
        assert_eq!(plan["parsed"], false);
        assert_eq!(plan["error"], error.to_string());
        assert!(plan.get("stages").is_none());
    }
    parsed
}

fn req(s: &str) -> khive_request::ParsedRequest {
    parse_request(s).unwrap_or_else(|e| panic!("parse({s:?}) failed: {e}"))
}

fn ops(s: &str) -> Vec<khive_request::ParsedOp> {
    req(s).ops
}

/// Extract the concrete `Value` from an `ArgValue::Value`, panicking on dynamic variants.
fn val(arg: &ArgValue) -> &serde_json::Value {
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

include!("parser_tests/basic_calls_tests.rs");

include!("parser_tests/string_values_tests.rs");

include!("parser_tests/control_characters_tests.rs");

include!("parser_tests/value_diagnostics_tests.rs");

include!("parser_tests/escape_diagnostics_tests.rs");

include!("parser_tests/batch_and_syntax_tests.rs");

include!("parser_tests/identifiers_tests.rs");

include!("parser_tests/chains_tests.rs");

include!("parser_tests/nested_prev_values_tests.rs");

include!("parser_tests/quoted_prev_values_tests.rs");

include!("parser_tests/parallel_units_tests.rs");

include!("parser_tests/prev_array_indices_tests.rs");

include!("parser_tests/prev_literal_and_malformed_indices_tests.rs");

include!("parser_tests/envelope_arguments_tests.rs");

include!("parser_tests/nesting_limits_tests.rs");

include!("parser_tests/input_bounds_tests.rs");

include!("parser_tests/bulk_payload_bounds_tests.rs");

include!("parser_tests/prev_failure_diagnostics_tests.rs");
