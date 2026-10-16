//! Matching a tag in current JSON string arrays and legacy stored text.

use alloc::{string::String, vec::Vec};

/// Match a case-sensitive, decoded element of a JSON string array.
///
/// Legacy values that are not a complete string array retain their raw,
/// case-sensitive substring semantics. In particular, a mixed-type array is
/// legacy text; decoding only its string elements would change its meaning.
pub fn tag_contains(tags: &str, marker: &str) -> bool {
    match serde_json::from_str::<Vec<String>>(tags) {
        Ok(tags) => tags.iter().any(|tag| tag == marker),
        Err(_) => tags.contains(marker),
    }
}

#[cfg(test)]
#[path = "tag_match_tests.rs"]
mod tests;
