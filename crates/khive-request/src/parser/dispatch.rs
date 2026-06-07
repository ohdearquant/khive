//! Top-level DSL dispatch: single op, batch, chain, and JSON form routing.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

use crate::types::{
    ArgValue, DslError, ExecutionMode, ParsedOp, ParsedRequest, MAX_OPS, RESERVED_ENVELOPE_ARGS,
};

use super::parser_impl::Parser;
use super::scan::{find_prev_ref_pos, json_value_contains_prev_ref};

/// Parse a request input string into a single op or a batch.
pub fn parse_request(input: &str) -> Result<ParsedRequest, DslError> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return Err(DslError::Empty);
    }

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

    if first == b'[' {
        return parse_fn_batch(trimmed);
    }

    let mut p = Parser::new(trimmed);
    let first_op = p.parse_op()?;
    p.skip_ws();

    if p.eof() {
        if let Some(pos) = find_prev_ref_pos(&first_op) {
            return Err(DslError::PrevRefOutsideChain { pos });
        }
        reject_reserved_args(&first_op)?;
        return Ok(ParsedRequest {
            ops: vec![first_op],
            mode: ExecutionMode::Single,
        });
    }

    if p.peek() == Some('|') {
        return parse_chain_tail(p, first_op);
    }

    Err(DslError::UnexpectedChar {
        pos: p.pos,
        found: p.peek().unwrap(),
        expected: "'|' or end of input",
    })
}

/// Parse the rest of a chain after the first op has been consumed.
fn parse_chain_tail(mut p: Parser<'_>, first_op: ParsedOp) -> Result<ParsedRequest, DslError> {
    reject_reserved_args(&first_op)?;
    let mut ops = vec![first_op];
    while p.peek() == Some('|') {
        if ops.len() >= MAX_OPS {
            return Err(DslError::TooManyOps {
                count: ops.len() + 1,
                max: MAX_OPS,
            });
        }
        p.advance(1);
        p.skip_ws();
        let op = p.parse_op()?;
        reject_reserved_args(&op)?;
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
        // JSON form: recursively scan for $prev references and reject.
        let mut args: BTreeMap<String, ArgValue> = BTreeMap::new();
        for (k, v) in args_map {
            if json_value_contains_prev_ref(&v) {
                return Err(DslError::PrevRefInJsonForm { arg_name: k });
            }
            args.insert(k, ArgValue::Value(v));
        }
        let op = ParsedOp { tool, args };
        reject_reserved_args(&op)?;
        ops.push(op);
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
    // PrevRef inside a parallel batch is invalid.
    for op in &ops {
        if let Some(pos) = find_prev_ref_pos(op) {
            return Err(DslError::PrevRefOutsideChain { pos });
        }
        reject_reserved_args(op)?;
    }
    Ok(ParsedRequest {
        ops,
        mode: ExecutionMode::Parallel,
    })
}

/// Reject reserved envelope-level args inside a verb's argument list.
pub(super) fn reject_reserved_args(op: &ParsedOp) -> Result<(), DslError> {
    for reserved in RESERVED_ENVELOPE_ARGS {
        if op.args.contains_key(*reserved) {
            return Err(DslError::ReservedEnvelopeArg {
                arg_name: (*reserved).to_owned(),
                verb: op.tool.clone(),
            });
        }
    }
    Ok(())
}
