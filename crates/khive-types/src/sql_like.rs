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
mod tests {
    use super::escape_like_literal;

    #[test]
    fn escapes_only_like_wildcards_and_the_escape_character() {
        for (input, expected) in [
            ("", ""),
            ("plain text", "plain text"),
            ("%", "\\%"),
            ("_", "\\_"),
            ("\\", "\\\\"),
            ("name%_\\tail", "name\\%\\_\\\\tail"),
            ("中文_é%\\", "中文\\_é\\%\\\\"),
            ("quote' and \"double\"", "quote' and \"double\""),
        ] {
            assert_eq!(escape_like_literal(input), expected, "input={input:?}");
        }
    }
}
