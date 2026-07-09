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
    let lower = trimmed.to_ascii_lowercase();
    for kw in CLOSING_KEYWORDS {
        if let Some(boundary_idx) = lower.len().checked_sub(kw.len()) {
            if &lower[boundary_idx..] == *kw {
                let prev_char = lower[..boundary_idx].chars().next_back();
                if prev_char
                    .map(|c| !c.is_ascii_alphanumeric())
                    .unwrap_or(true)
                {
                    return true;
                }
            }
        }
    }
    false
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
mod tests {
    use super::*;

    fn numbers_of(kind: RefKind, mentions: &[RefMention]) -> Vec<u64> {
        mentions
            .iter()
            .filter(|m| m.kind == kind)
            .map(|m| m.number)
            .collect()
    }

    #[test]
    fn closes_keyword_variants_detected() {
        for kw in [
            "Closes",
            "closes",
            "Close",
            "Fixes",
            "fix",
            "Resolved",
            "resolves:",
        ] {
            let text = format!("{kw} #42");
            let mentions = extract_references(&text);
            assert_eq!(
                mentions,
                vec![RefMention {
                    number: 42,
                    kind: RefKind::Closes
                }],
                "{text:?}"
            );
        }
    }

    #[test]
    fn bare_hash_number_is_mention() {
        let mentions = extract_references("see #7 for context");
        assert_eq!(
            mentions,
            vec![RefMention {
                number: 7,
                kind: RefKind::Mentions
            }]
        );
    }

    #[test]
    fn multiple_references_in_one_text() {
        let mentions = extract_references("Fixes #12, Resolves #34, see also #56");
        assert_eq!(numbers_of(RefKind::Closes, &mentions), vec![12, 34]);
        assert_eq!(numbers_of(RefKind::Mentions, &mentions), vec![56]);
    }

    #[test]
    fn no_false_positive_on_number_immediately_followed_by_letters() {
        let mentions = extract_references("see #54abc for the hex code, not an issue");
        assert!(mentions.is_empty(), "{mentions:?}");
    }

    #[test]
    fn no_false_positive_on_bare_hash_with_no_digits() {
        let mentions = extract_references("a #hashtag not a number, and a lone # too");
        assert!(mentions.is_empty(), "{mentions:?}");
    }

    #[test]
    fn double_hash_still_extracts_the_digit_run() {
        let mentions = extract_references("##54");
        assert_eq!(
            mentions,
            vec![RefMention {
                number: 54,
                kind: RefKind::Mentions
            }]
        );
    }

    #[test]
    fn dedupe_prefers_closes_over_mentions_for_same_number() {
        let mentions = vec![
            RefMention {
                number: 9,
                kind: RefKind::Mentions,
            },
            RefMention {
                number: 9,
                kind: RefKind::Closes,
            },
        ];
        let deduped = dedupe_prefer_closes(mentions);
        assert_eq!(
            deduped,
            vec![RefMention {
                number: 9,
                kind: RefKind::Closes
            }]
        );
    }

    #[test]
    fn truncate_chars_leaves_short_strings_untouched() {
        assert_eq!(truncate_chars("short", 120), "short");
    }

    #[test]
    fn truncate_chars_cuts_long_strings_at_char_boundary() {
        let long = "a".repeat(200);
        let truncated = truncate_chars(&long, 120);
        assert_eq!(truncated.chars().count(), 120);
    }
}
