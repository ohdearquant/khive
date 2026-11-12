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
/// A candidate literal is the run between two adjacent unescaped quotes, so
/// every byte is read at most twice and the candidates are fewer than the
/// quotes. A run decodes only when every escape in it is a valid JSON escape;
/// prose quotes and paths like `C:\Users` do not, and keep today's verdict.
pub(super) fn escaped_strings(text: &str) -> Vec<String> {
    if !text.contains('\\') {
        return Vec::new();
    }
    unescaped_quotes(text.as_bytes())
        .windows(2)
        .filter_map(|pair| {
            let literal = &text[pair[0]..=pair[1]];
            if !literal.contains('\\') {
                return None;
            }
            serde_json::from_str::<String>(literal).ok()
        })
        .collect()
}

/// Byte offsets of the `"` characters not preceded by an odd run of backslashes.
fn unescaped_quotes(bytes: &[u8]) -> Vec<usize> {
    let mut quotes = Vec::new();
    let mut backslashes = 0usize;
    for (i, &b) in bytes.iter().enumerate() {
        if b == b'"' && backslashes.is_multiple_of(2) {
            quotes.push(i);
        }
        backslashes = if b == b'\\' { backslashes + 1 } else { 0 };
    }
    quotes
}
