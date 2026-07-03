//! Script/identifier detection and splitting for code-aware tokenization.

/// Returns true if `text` looks like a code identifier (no whitespace, ≥1 ASCII letter, structural boundary present).
pub fn is_identifier(text: &str) -> bool {
    if text.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if !text.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }

    let chars: Vec<char> = text.chars().collect();
    let n = chars.len();

    // Single-char identifiers: just a letter, no boundary possible
    if n < 2 {
        return false;
    }

    // Check for explicit separator characters
    for &c in &chars {
        if matches!(c, '_' | '-' | '/' | '.' | ':') {
            return true;
        }
    }

    // Check for camelCase (lowercase followed by uppercase) or digit-letter boundaries
    for i in 0..n - 1 {
        let a = chars[i];
        let b = chars[i + 1];
        if (a.is_ascii_lowercase() && b.is_ascii_uppercase())
            || (a.is_ascii_alphabetic() && b.is_ascii_digit())
            || (a.is_ascii_digit() && b.is_ascii_alphabetic())
        {
            return true;
        }
    }

    false
}

/// Split `text` on separators (`_`, `-`, `.`, `/`, `::`) and camelCase/digit boundaries,
/// returning lowercase parts of at least `min_part_len` characters.
pub fn split_identifier(text: &str, min_part_len: usize) -> Vec<String> {
    // Collect chars, then split on separators / boundaries
    let chars: Vec<char> = text.chars().collect();
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();

    let n = chars.len();
    let mut i = 0;
    while i < n {
        let c = chars[i];

        // Skip separator characters
        if matches!(c, '_' | '-' | '/' | '.') {
            if !current.is_empty() {
                parts.push(current.clone());
                current.clear();
            }
            i += 1;
            continue;
        }

        // Handle '::' as a two-char separator
        if c == ':' && i + 1 < n && chars[i + 1] == ':' {
            if !current.is_empty() {
                parts.push(current.clone());
                current.clear();
            }
            i += 2;
            continue;
        }
        // Lone ':' — treat as separator too
        if c == ':' {
            if !current.is_empty() {
                parts.push(current.clone());
                current.clear();
            }
            i += 1;
            continue;
        }

        // Detect camelCase: lower→upper boundary
        if !current.is_empty() && c.is_ascii_uppercase() {
            let prev = chars[i - 1];
            if prev.is_ascii_lowercase() {
                // e.g. camelCase: split before 'C'
                parts.push(current.clone());
                current.clear();
            } else if prev.is_ascii_uppercase() && i + 1 < n && chars[i + 1].is_ascii_lowercase() {
                // e.g. "GPTModel": push accumulated "GP", keep "T" for new part
                parts.push(current.clone());
                current.clear();
            }
        }

        // Detect digit→letter and letter→digit transitions
        if !current.is_empty() {
            let prev = chars[i - 1];
            if (prev.is_ascii_digit() && c.is_ascii_alphabetic())
                || (prev.is_ascii_alphabetic() && c.is_ascii_digit())
            {
                parts.push(current.clone());
                current.clear();
            }
        }

        current.push(c);
        i += 1;
    }

    if !current.is_empty() {
        parts.push(current);
    }

    // Lowercase and filter by min_part_len (character count, not byte length)
    let min_part_len = min_part_len.max(1);
    parts
        .into_iter()
        .map(|p| p.to_lowercase())
        .filter(|p| p.chars().count() >= min_part_len)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- is_identifier ---

    #[test]
    fn camel_case_is_identifier() {
        assert!(is_identifier("camelCase"));
        assert!(is_identifier("myVariableName"));
    }

    #[test]
    fn snake_case_is_identifier() {
        assert!(is_identifier("snake_case"));
        assert!(is_identifier("MY_CONST"));
    }

    #[test]
    fn hyphenated_is_identifier() {
        assert!(is_identifier("kebab-case"));
        assert!(is_identifier("GPT-4"));
    }

    #[test]
    fn path_separators_are_identifier() {
        assert!(is_identifier("a/b"));
        assert!(is_identifier("pkg::Type"));
    }

    #[test]
    fn plain_word_is_not_identifier() {
        // "plain" has no boundary — single run of lowercase, no separator
        assert!(!is_identifier("plain"));
    }

    #[test]
    fn has_whitespace_is_not_identifier() {
        assert!(!is_identifier("hello world"));
        assert!(!is_identifier("foo bar"));
    }

    #[test]
    fn no_ascii_letter_not_identifier() {
        assert!(!is_identifier("123"));
        assert!(!is_identifier("_"));
    }

    // --- split_identifier ---

    #[test]
    fn camel_case_split() {
        assert_eq!(split_identifier("camelCase", 1), vec!["camel", "case"]);
    }

    #[test]
    fn snake_case_split() {
        assert_eq!(split_identifier("snake_case", 1), vec!["snake", "case"]);
    }

    #[test]
    fn gpt4_split() {
        assert_eq!(split_identifier("GPT-4", 1), vec!["gpt", "4"]);
    }

    #[test]
    fn lora_split() {
        assert_eq!(split_identifier("LoRA", 1), vec!["lo", "ra"]);
    }

    #[test]
    fn plain_single_part() {
        assert_eq!(split_identifier("plain", 1), vec!["plain"]);
    }

    #[test]
    fn bm25f_split() {
        // "BM25F": B+M (uppercase run), 2+5 (digits), F (letter)
        let parts = split_identifier("BM25F", 1);
        assert_eq!(parts, vec!["bm", "25", "f"]);
    }

    #[test]
    fn double_colon_separator() {
        assert_eq!(split_identifier("pkg::Type", 1), vec!["pkg", "type"]);
    }

    #[test]
    fn dot_separator() {
        assert_eq!(
            split_identifier("com.example.Foo", 1),
            vec!["com", "example", "foo"]
        );
    }

    #[test]
    fn slash_separator() {
        assert_eq!(split_identifier("a/b/c", 1), vec!["a", "b", "c"]);
    }

    #[test]
    fn min_part_len_filters_short() {
        // "GPT-4": parts ["gpt", "4"] — "4" has len 1, filtered when min=2
        let parts = split_identifier("GPT-4", 2);
        assert_eq!(parts, vec!["gpt"]);
    }

    #[test]
    fn split_identifier_filters_unicode_min_part_len_by_chars() {
        // "\u{4F60}" (你) is 1 char but 3 UTF-8 bytes; must be filtered by
        // char count, not byte length, when min_part_len == 2.
        let parts = split_identifier("foo_\u{4F60}", 2);
        assert_eq!(parts, vec!["foo"]);

        // A two-character non-ASCII part is kept at min_part_len == 2.
        let parts = split_identifier("foo_\u{4F60}\u{597D}", 2);
        assert_eq!(parts, vec!["foo", "\u{4F60}\u{597D}"]);
    }

    #[test]
    fn mixed_case_and_digits() {
        // "v2Api" -> ["v", "2", "api"] with min=1; min=2 drops "v" and "2"
        let parts = split_identifier("v2Api", 2);
        assert_eq!(parts, vec!["api"]);
    }

    #[test]
    fn all_uppercase_run_then_lower() {
        // "XMLParser" -> "XML" split before "P" (uppercase before lowercase)
        // => ["xml", "parser"]
        let parts = split_identifier("XMLParser", 1);
        assert_eq!(parts, vec!["xml", "parser"]);
    }
}
