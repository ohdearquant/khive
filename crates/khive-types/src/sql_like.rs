//! Literal text for SQL `LIKE` patterns using a backslash escape character.

use alloc::string::String;

/// Escape `%`, `_`, and `\` so `input` matches literally under `LIKE ... ESCAPE '\'`.
/// Callers can add their own wildcard prefix or suffix after escaping.
pub fn escape_like_literal(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for c in input.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
#[path = "sql_like_tests.rs"]
mod tests;
