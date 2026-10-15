//! GitHub reference-grammar extraction (ADR-088 Amendment 1, ingest
//! enrichment riders): `Closes/Fixes/Resolves #N` and bare `#N` mentions in
//! commit messages and issue/PR bodies.
//!
//! Extraction never panics or errors -- fail-open per the amendment. A
//! malformed shape (`#54abc`, a `#` with no digits after it) is simply not
//! matched, not reported as an error.

/// The two reference kinds materialized as `annotates` edge metadata
/// (`ref_kind` on the edge -- see `crates/khive-pack-git/src/handlers.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Closes,
    Mentions,
}

impl RefKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            RefKind::Closes => "closes",
            RefKind::Mentions => "mentions",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefMention {
    pub number: u64,
    pub kind: RefKind,
}

/// GitHub's closing-keyword grammar (case-insensitive): `Closes`, `Fixes`,
/// `Resolves` and their inflections, immediately preceding `#N` (whitespace
/// and/or a colon allowed in between).
const CLOSING_KEYWORDS: &[&str] = &[
    "close", "closes", "closed", "fix", "fixes", "fixed", "resolve", "resolves", "resolved",
];

/// Extract every GitHub-style issue/PR reference from `text`.
///
/// A `#N` immediately preceded (skipping whitespace/`:`) by a closing
/// keyword is `RefKind::Closes`; every other `#N` is `RefKind::Mentions`. A
/// `#` not immediately followed by at least one digit, or a digit run
/// immediately followed by another alphanumeric character (e.g. `#54abc`,
/// not a clean reference), is skipped. May return duplicate `(number, kind)`
/// pairs for text with repeated references -- callers that materialize one
/// edge per referenced number should dedupe (see `dedupe_prefer_closes`).
pub fn extract_references(text: &str) -> Vec<RefMention> {
    let bytes = text.as_bytes();
    let mut mentions = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'#' {
            i += 1;
            continue;
        }
        let hash_idx = i;
        let Some((number, next)) = parse_number(text, hash_idx + 1) else {
            i += 1;
            continue;
        };
        if next < bytes.len() && bytes[next].is_ascii_alphanumeric() {
            // "#54abc" -- not a clean reference.
            i = next;
            continue;
        }
        let kind = if preceded_by_closing_keyword(text, hash_idx) {
            RefKind::Closes
        } else {
            RefKind::Mentions
        };
        mentions.push(RefMention { number, kind });
        i = next;
    }
    mentions
}

/// Collapse duplicate `number`s to a single mention, preferring `Closes`
/// over `Mentions` when both occur for the same number (a closing reference
/// is strictly more informative).
pub fn dedupe_prefer_closes(mentions: Vec<RefMention>) -> Vec<RefMention> {
    let mut by_number: std::collections::BTreeMap<u64, RefKind> = std::collections::BTreeMap::new();
    for m in mentions {
        by_number
            .entry(m.number)
            .and_modify(|k| {
                if m.kind == RefKind::Closes {
                    *k = RefKind::Closes;
                }
            })
            .or_insert(m.kind);
    }
    by_number
        .into_iter()
        .map(|(number, kind)| RefMention { number, kind })
        .collect()
}

fn parse_number(text: &str, start: usize) -> Option<(u64, usize)> {
    let bytes = text.as_bytes();
    let mut end = start;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == start {
        return None;
    }
    let n: u64 = text[start..end].parse().ok()?;
    Some((n, end))
}

/// `true` when the `#` at `hash_idx` is immediately preceded (allowing
/// trailing whitespace/`:` in between) by one of [`CLOSING_KEYWORDS`],
/// case-insensitively, with a non-alphanumeric (or start-of-string)
/// boundary before the keyword.
fn preceded_by_closing_keyword(text: &str, hash_idx: usize) -> bool {
    let before = &text[..hash_idx];
    let trimmed = before.trim_end_matches(|c: char| c.is_whitespace() || c == ':');
    for kw in CLOSING_KEYWORDS {
        let Some(start) = trimmed.len().checked_sub(kw.len()) else {
            continue;
        };
        let suffix = &trimmed.as_bytes()[start..];
        if suffix.iter().zip(kw.bytes()).all(|(left, right)| {
            observe_keyword_byte();
            left.eq_ignore_ascii_case(&right)
        }) {
            // A successful ASCII suffix match proves this is a UTF-8 boundary.
            let prev_char = trimmed[..start].chars().next_back();
            if prev_char
                .map(|c| !c.is_ascii_alphanumeric())
                .unwrap_or(true)
            {
                return true;
            }
        }
    }
    false
}

#[inline]
fn observe_keyword_byte() {
    #[cfg(test)]
    KEYWORD_BYTES.with(|count| count.set(count.get() + 1));
}

#[cfg(test)]
thread_local! {
    static KEYWORD_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Truncate `s` to at most `max_chars` characters (char-boundary safe, no
/// ellipsis -- the amendment specifies "truncated", not a marker suffix).
pub fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    s.chars().take(max_chars).collect()
}

#[cfg(test)]
#[path = "refs_tests.rs"]
mod tests;
