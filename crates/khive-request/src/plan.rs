//! Syntax and catalog inspection without dispatch or admission.

use std::collections::BTreeMap;

use serde_json::{json, Map, Value};

use crate::parser::parse_request;
use crate::types::{ArgValue, ExecutionMode, MAX_OPS, MAX_OPS_INPUT_LEN, NESTING_DEPTH_LIMIT};

/// Parses a request and describes its stages using a verb-to-pack catalog.
///
/// Catalog membership does not grant permission to dispatch a verb. Previous
/// result references remain literal strings and are never resolved.
pub fn plan_request(ops: &str, catalog: &BTreeMap<String, String>) -> Value {
    let limits = json!({
        "max_ops": MAX_OPS,
        "max_depth": NESTING_DEPTH_LIMIT,
        "max_input_len": MAX_OPS_INPUT_LEN,
    });
    let request = match parse_request(ops) {
        Ok(request) => request,
        Err(error) => return json!({"parsed": false, "error": error.to_string(), "limits": limits}),
    };
    let mode = match request.mode {
        ExecutionMode::Single => "single",
        ExecutionMode::Parallel => "parallel",
        ExecutionMode::Chain => "chain",
    };
    let stages: Vec<Value> = request
        .ops
        .into_iter()
        .enumerate()
        .map(|(index, op)| {
            let mut prev_refs = Vec::new();
            let args: Map<String, Value> = op
                .args
                .into_iter()
                .map(|(name, arg)| (name, literal_arg(arg, &mut prev_refs)))
                .collect();
            let pack = catalog.get(&op.tool);
            json!({
                "index": index,
                "verb": op.tool,
                "pack": pack,
                "known": pack.is_some(),
                "args": args,
                "prev_refs": prev_refs,
            })
        })
        .collect();
    json!({
        "parsed": true,
        "mode": mode,
        "stage_count": stages.len(),
        "stages": stages,
        "limits": limits,
    })
}

fn literal_arg(arg: ArgValue, prev_refs: &mut Vec<String>) -> Value {
    match arg {
        ArgValue::Value(value) => value,
        ArgValue::PrevRef { path } => {
            let literal_path = path.replace(".[", "[");
            let literal = if path.is_empty() || path.starts_with('[') {
                format!("$prev{literal_path}")
            } else {
                format!("$prev.{literal_path}")
            };
            prev_refs.push(path);
            Value::String(literal)
        }
        ArgValue::Array(items) => Value::Array(
            items
                .into_iter()
                .map(|item| literal_arg(item, prev_refs))
                .collect(),
        ),
        ArgValue::Object(pairs) => Value::Object(
            pairs
                .into_iter()
                .map(|(name, value)| (name, literal_arg(value, prev_refs)))
                .collect(),
        ),
    }
}
