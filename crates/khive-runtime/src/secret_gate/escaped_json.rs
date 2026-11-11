//! Decoded views of the escaped string literals inside a JSON document.
//!
//! The detectors read raw text, so a credential written with one JSON escape
//! (`AKIA...`, `https:\/\/...`) reads as something else until it is
//! decoded. Only strings that carry an escape are returned: an unescaped
//! string already reached the detectors byte for byte.

/// Return the decoded value of every string literal in `text` that contains a
/// backslash escape, when `text` as a whole parses as JSON. Anything else
/// yields nothing.
pub(super) fn escaped_strings(text: &str) -> Vec<String> {
    if !text.contains('\\') {
        return Vec::new();
    }
    let trimmed = text.trim_start();
    if !(trimmed.starts_with('{') || trimmed.starts_with('[') || trimmed.starts_with('"')) {
        return Vec::new();
    }
    if serde_json::from_str::<serde::de::IgnoredAny>(text).is_err() {
        return Vec::new();
    }
    let bytes = text.as_bytes();
    let mut decoded = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        let start = i;
        let mut escaped = false;
        i += 1;
        while i < bytes.len() && bytes[i] != b'"' {
            if bytes[i] == b'\\' {
                escaped = true;
                i += 2;
            } else {
                i += 1;
            }
        }
        let end = (i + 1).min(bytes.len());
        i = end;
        if escaped {
            if let Ok(value) = serde_json::from_str::<String>(&text[start..end]) {
                decoded.push(value);
            }
        }
    }
    decoded
}
