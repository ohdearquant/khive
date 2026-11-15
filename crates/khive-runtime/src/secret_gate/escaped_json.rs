//! Decoded views of the escaped JSON string literals inside a field.
//!
//! The detectors read raw text, so a credential written with one JSON escape
//! (`AKIA...`, `https:\/\/...`) reads as something else until it is
//! decoded. Only strings that carry an escape are returned: an unescaped
//! string already reached the detectors byte for byte.

/// Return the decoded value of every JSON string literal in `text` that
/// contains a backslash escape, whether `text` is a whole JSON document, a
/// fragment inside prose, or prose with quoted runs.
///
/// An opening quote may follow a stray prose backslash. Escaped quotes inside
/// a candidate remain content, but an invalid fragment lets the next quote
/// start a new candidate. Fragment validation and complete candidates each
/// cover disjoint runs, keeping total work linear. Every escape must still be
/// valid JSON; paths like `C:\Users` do not decode.
pub(super) fn escaped_strings(text: &str) -> Vec<String> {
    if !text.contains('\\') {
        return Vec::new();
    }
    let Some(mut start) = text.find('"') else {
        return Vec::new();
    };
    let mut previous_quote = start;
    let mut decoded = Vec::new();
    let mut backslashes = 0usize;
    for (i, &b) in text.as_bytes().iter().enumerate().skip(start + 1) {
        if b == b'"' {
            if backslashes.is_multiple_of(2) {
                let literal = &text[start..=i];
                if literal.contains('\\') {
                    if let Ok(value) = serde_json::from_str::<String>(literal) {
                        decoded.push(value);
                    }
                }
                start = i;
            } else {
                // An escaped quote completes an escape, so no JSON escape can
                // straddle this boundary. A synthetic closer validates just
                // this fragment without repeatedly parsing its earlier prefix.
                let fragment = format!("{}\"", &text[previous_quote..=i]);
                if serde_json::from_str::<String>(&fragment).is_err() {
                    start = i;
                }
            }
            previous_quote = i;
        }
        backslashes = if b == b'\\' { backslashes + 1 } else { 0 };
    }
    decoded
}
