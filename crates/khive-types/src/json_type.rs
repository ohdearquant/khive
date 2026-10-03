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
mod tests {
    use super::json_type_name;
    use serde_json::json;

    #[test]
    fn json_type_names_cover_all_six_variants() {
        for (value, expected) in [
            (json!(null), "null"),
            (json!(true), "boolean"),
            (json!(7), "number"),
            (json!(-1.25), "number"),
            (json!("text"), "string"),
            (json!([]), "array"),
            (json!({}), "object"),
        ] {
            assert_eq!(json_type_name(&value), expected);
        }
    }
}
