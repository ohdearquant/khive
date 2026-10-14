//! JSON type names used in validation errors.

use serde_json::Value;

/// Return the JSON schema type of a value without exposing its contents.
///
/// Available with the `serde` feature.
pub fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(_) => "number",
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
#[path = "json_type_tests.rs"]
mod tests;
