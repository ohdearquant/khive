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
#[path = "canonical_json_tests.rs"]
mod tests;
