//! Compact, recursively key-sorted JSON for stable digests.

use alloc::collections::BTreeMap;
use alloc::vec::Vec;

use serde_json::Value;

/// Serialize an already-parsed JSON value to compact UTF-8 bytes.
///
/// Object keys are sorted recursively by Rust string ordering, including objects
/// inside arrays. Array order and scalar values are preserved. There is no
/// insignificant whitespace; string escaping and number formatting are those of
/// `serde_json`. This is not RFC 8785 numeric or Unicode canonicalization, and it
/// does not preserve the source text's whitespace, escapes, or numeric spelling.
///
/// The input boundary is [`Value`], not raw JSON. Duplicate object keys have
/// already been resolved by the parser or value builder and cannot be detected
/// here. A `serde_json::Number` cannot represent NaN or infinity; a prior
/// conversion that replaced such a value with null is likewise not detectable.
/// Validate those source-level constraints before constructing the input when
/// they matter to the caller.
///
/// Available with the `serde` feature. The implementation uses `alloc` and has
/// no `std` API requirements.
pub fn canonical_json_bytes(value: &Value) -> Result<Vec<u8>, serde_json::Error> {
    serde_json::to_vec(&canonical(value))
}

// Promoted from the mounted-tool catalog so existing catalog pins retain their bytes.
fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map
                .iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect();
            Value::Object(sorted.into_iter().collect())
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn canonical_json_matches_catalog_definition_bytes() {
        let definition: Value = serde_json::from_str(
            r#"{"name":"A","description":"one","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}},"outputSchema":{"type":"object"}}"#,
        )
        .unwrap();
        assert_eq!(
            canonical_json_bytes(&definition).unwrap(),
            br#"{"description":"one","inputSchema":{"properties":{"x":{"type":"string"}},"type":"object"},"name":"A","outputSchema":{"type":"object"}}"#,
        );
    }

    #[test]
    fn canonical_json_ignores_object_order_but_preserves_values_and_array_order() {
        let left: Value = serde_json::from_str(
            r#"{"z":[{"y":2,"x":1},true,null],"a":{"second":"two","first":"one"}}"#,
        )
        .unwrap();
        let reordered: Value = serde_json::from_str(
            r#"{"a":{"first":"one","second":"two"},"z":[{"x":1,"y":2},true,null]}"#,
        )
        .unwrap();
        let bytes = canonical_json_bytes(&left).unwrap();
        assert_eq!(bytes, canonical_json_bytes(&reordered).unwrap());
        assert_eq!(
            bytes,
            br#"{"a":{"first":"one","second":"two"},"z":[{"x":1,"y":2},true,null]}"#,
        );

        let mut changed = left.clone();
        changed["z"][0]["x"] = json!(3);
        assert_ne!(bytes, canonical_json_bytes(&changed).unwrap());
        let mut changed = left.clone();
        changed["a"]["first"] = json!("ONE");
        assert_ne!(bytes, canonical_json_bytes(&changed).unwrap());
        let mut changed = left;
        changed["z"].as_array_mut().unwrap().swap(1, 2);
        assert_ne!(bytes, canonical_json_bytes(&changed).unwrap());
    }

    #[test]
    fn canonical_json_parse_serialize_reparse_serialize_is_byte_stable() {
        for source in [
            r#" { "b": [3, {"z": false, "a": null}], "a": "line\nquote\"slash\\" } "#,
            r#"{"unicode":"\u00e9\u2603","\u00e9":1,"a":2}"#,
            r#"[-9223372036854775808,18446744073709551615,-0.0,1.25,1e20,1e-20]"#,
            r#"[]"#,
            r#"{}"#,
            r#"true"#,
            r#"null"#,
            r#""a scalar""#,
        ] {
            let parsed: Value = serde_json::from_str(source).unwrap();
            let first = canonical_json_bytes(&parsed).unwrap();
            let reparsed: Value = serde_json::from_slice(&first).unwrap();
            assert_eq!(first, canonical_json_bytes(&reparsed).unwrap(), "{source}");
        }
    }

    #[test]
    fn canonical_json_starts_at_the_parsed_value_boundary() {
        let parsed: Value = serde_json::from_str(r#"{"key":1,"key":2}"#).unwrap();
        assert_eq!(canonical_json_bytes(&parsed).unwrap(), br#"{"key":2}"#);
        for number in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(serde_json::Number::from_f64(number).is_none());
        }
    }
}
