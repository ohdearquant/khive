//! Reading the arguments of a forms verb.

use khive_runtime::RuntimeError;
use serde_json::{Map, Value};

use crate::store::{invalid, scalar};

pub(crate) type Args = Map<String, Value>;

pub(crate) fn args(params: Value) -> Result<Args, RuntimeError> {
    match params {
        Value::Object(map) => Ok(map),
        Value::Null => Ok(Map::new()),
        _ => Err(invalid("the arguments must be an object")),
    }
}

pub(crate) fn opt_text(args: &Args, key: &str) -> Result<Option<String>, RuntimeError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => match scalar(value) {
            Some(text) if !text.trim().is_empty() => Ok(Some(text.trim().to_owned())),
            _ => Err(invalid(format!("{key} must be a non-empty string"))),
        },
    }
}

pub(crate) fn text(args: &Args, key: &str) -> Result<String, RuntimeError> {
    opt_text(args, key)?.ok_or_else(|| invalid(format!("{key} is required")))
}

pub(crate) fn object<'a>(args: &'a Args, key: &str) -> Result<Option<&'a Args>, RuntimeError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(_) => Err(invalid(format!("{key} must be an object"))),
    }
}

pub(crate) fn list<'a>(args: &'a Args, key: &str) -> Result<&'a [Value], RuntimeError> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(&[]),
        Some(Value::Array(items)) => Ok(items),
        Some(_) => Err(invalid(format!("{key} must be a list"))),
    }
}

pub(crate) fn string_list(args: &Args, key: &str) -> Result<Vec<String>, RuntimeError> {
    list(args, key)?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .ok_or_else(|| invalid(format!("each entry of {key} must be a non-empty string")))
        })
        .collect()
}

pub(crate) fn entry<'a>(value: &'a Value, what: &str) -> Result<&'a Args, RuntimeError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("each {what} must be an object")))
}

pub(crate) fn one_of(value: String, key: &str, allowed: &[&str]) -> Result<String, RuntimeError> {
    if allowed.contains(&value.as_str()) {
        Ok(value)
    } else {
        Err(invalid(format!(
            "{key} must be one of {}; got \"{value}\"",
            allowed.join(", ")
        )))
    }
}

/// A printed number such as a page or an order; a string of digits is accepted.
pub(crate) fn number(args: &Args, key: &str) -> Result<Value, RuntimeError> {
    match args.get(key) {
        Some(Value::Number(n)) => Ok(Value::Number(n.clone())),
        Some(Value::String(s)) => s
            .trim()
            .parse::<i64>()
            .map(Value::from)
            .map_err(|_| invalid(format!("{key} must be a number; got \"{s}\""))),
        None | Some(Value::Null) => Err(invalid(format!("{key} is required"))),
        Some(_) => Err(invalid(format!("{key} must be a number"))),
    }
}
