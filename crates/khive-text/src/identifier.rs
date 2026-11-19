//! Script/identifier detection and splitting for code-aware tokenization.

/// Return whether text has an ASCII letter and an identifier boundary but no whitespace.
///
/// See `crates/khive-text/docs/api/identifier-and-script.md`.
pub fn is_identifier(text: &str) -> bool {
    if text.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    if !text.chars().any(|c| c.is_ascii_alphabetic()) {
        return false;
    }

    let mut chars = text.chars();
    let mut previous: Option<char> = None;

    while let Some(current) = chars.next() {
        if matches!(current, '_' | '-' | '/' | '.' | ':') {
            return true;
        }

        if let Some(prev) = previous {
            let mut lookahead = chars.clone();
            let next = lookahead.next();
            let after_next = lookahead.next();
            if (prev.is_ascii_lowercase() && current.is_ascii_uppercase())
                || is_acronym_boundary(prev, current, next, after_next)
                || (prev.is_ascii_alphabetic() && current.is_ascii_digit())
                || (prev.is_ascii_digit() && current.is_ascii_alphabetic())
            {
                return true;
            }
        }
        previous = Some(current);
    }

    false
}

fn is_acronym_boundary(
    previous: char,
    current: char,
    next: Option<char>,
    after_next: Option<char>,
) -> bool {
    // An uppercase pair followed by a lowercase letter starts a word (XMLParser, XMLToJSON),
    // except a terminal plural `s` (APIs, IDs, URLs). An ASCII lowercase letter after the
    // `s` makes it part of the next word instead (DBUsers, APIUsage).
    previous.is_ascii_uppercase()
        && current.is_ascii_uppercase()
        && next.is_some_and(|next| {
            next.is_ascii_lowercase()
                && (next != 's' || after_next.is_some_and(|after_s| after_s.is_ascii_lowercase()))
        })
}

/// Split `text` on separators (`_`, `-`, `.`, `/`, `::`) and camelCase/digit boundaries,
/// returning lowercase parts of at least `min_part_len` characters.
///
/// See `crates/khive-text/docs/api/identifier-and-script.md`.
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
            } else if is_acronym_boundary(
                prev,
                c,
                chars.get(i + 1).copied(),
                chars.get(i + 2).copied(),
            ) {
                // e.g. "XMLParser": push "XML", start the next part with "P".
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
#[path = "identifier/tests.rs"]
mod tests;
