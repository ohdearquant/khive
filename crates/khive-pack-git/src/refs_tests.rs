use super::*;

#[test]
fn thousands_of_mixed_references_preserve_extraction_and_deduped_links() {
    let mut text = String::new();
    let mut expected_mentions = Vec::new();
    let keywords = [
        "cLoSe", "CLOSES", "Closed", "fIx", "FIXES", "Fixed", "rEsOlVe", "RESOLVES", "Resolved",
    ];
    let separators = [" ", ":", ":\u{2003}"];

    for group in (0..1024u64).rev() {
        let first = group * 4 + 1;
        let second = first + 1;
        let mention_only = first + 2;
        let double_hash = first + 3;
        let malformed = first + 1_000_000;
        let keyword = keywords[group as usize % keywords.len()];
        let separator = separators[group as usize % separators.len()];
        text.push_str(&format!(
            "修复说明 — {keyword}{separator}#{first}; see #{second}; again #{first}; \
                 FiXeS #{second}; see #{mention_only}; prefixFIXES #{mention_only}; \
                 ##{double_hash}; —x#{mention_only}; malformed #{malformed}abc \
                 #hashtag #18446744073709551616 #\n"
        ));
        expected_mentions.extend([
            RefMention {
                number: first,
                kind: RefKind::Closes,
            },
            RefMention {
                number: second,
                kind: RefKind::Mentions,
            },
            RefMention {
                number: first,
                kind: RefKind::Mentions,
            },
            RefMention {
                number: second,
                kind: RefKind::Closes,
            },
            RefMention {
                number: mention_only,
                kind: RefKind::Mentions,
            },
            RefMention {
                number: mention_only,
                kind: RefKind::Mentions,
            },
            RefMention {
                number: double_hash,
                kind: RefKind::Mentions,
            },
            RefMention {
                number: mention_only,
                kind: RefKind::Mentions,
            },
        ]);
    }

    let mut expected_links = Vec::new();
    for group in 0..1024u64 {
        for (offset, kind) in [
            RefKind::Closes,
            RefKind::Closes,
            RefKind::Mentions,
            RefKind::Mentions,
        ]
        .into_iter()
        .enumerate()
        {
            expected_links.push(RefMention {
                number: group * 4 + offset as u64 + 1,
                kind,
            });
        }
    }

    let mentions = extract_references(&text);
    assert_eq!(mentions.len(), 8192);
    assert_eq!(mentions, expected_mentions);
    let links = dedupe_prefer_closes(mentions);
    assert_eq!(links.len(), 4096);
    assert_eq!(links, expected_links);
}

#[test]
fn closing_keyword_work_ignores_unrelated_prefix_length() {
    for length in [64, 4096, 65536] {
        let text = format!("{} See #13; Fixes :\u{2003}#17", "界".repeat(length));
        KEYWORD_BYTES.with(|count| count.set(0));
        let references = extract_references(&text);
        assert_eq!(
            references,
            vec![
                RefMention {
                    number: 13,
                    kind: RefKind::Mentions
                },
                RefMention {
                    number: 17,
                    kind: RefKind::Closes
                },
            ]
        );
        let compared = KEYWORD_BYTES.with(std::cell::Cell::get);
        let per_reference: usize = CLOSING_KEYWORDS.iter().map(|word| word.len()).sum();
        assert!(compared <= 2 * per_reference, "actual byte work {compared}");
        assert!(compared > 0, "production comparator must be observed");
    }
}

#[test]
fn closing_keyword_suffix_matches_original_ascii_and_unicode_boundaries() {
    for prefix in ["", "a", "界", "—", "é", "\u{2003}"] {
        for word in CLOSING_KEYWORDS {
            for separator in ["", ":", "\t:\u{2003}"] {
                let text = format!("{prefix}{}{separator}#41", word.to_ascii_uppercase());
                let hash = text.find('#').unwrap();
                let trimmed =
                    text[..hash].trim_end_matches(|c: char| c.is_whitespace() || c == ':');
                let lower = trimmed.to_ascii_lowercase();
                let original = CLOSING_KEYWORDS.iter().any(|word| {
                    lower.strip_suffix(*word).is_some_and(|rest| {
                        rest.chars()
                            .next_back()
                            .is_none_or(|c| !c.is_ascii_alphanumeric())
                    })
                });
                assert_eq!(
                    preceded_by_closing_keyword(&text, hash),
                    original,
                    "{text:?}"
                );
            }
        }
    }
}

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
fn reference_scanning_handles_multibyte_boundaries_across_keyword_lengths() {
    // Each prefix is one byte longer than a distinct closing-keyword length
    // (3, 5, 6, 7, or 8), so subtracting that length lands inside `—`.
    for (text, number) in [
        ("—x#1", 1),
        ("—xxx#2", 2),
        ("—xxxx#3", 3),
        ("—xxxxx#4", 4),
        ("—xxxxxx#5", 5),
    ] {
        assert_eq!(
            extract_references(text),
            vec![RefMention {
                number,
                kind: RefKind::Mentions,
            }],
            "{text:?}"
        );
    }
}

#[test]
fn multibyte_body_preserves_closing_and_mention_classification() {
    assert_eq!(
        extract_references("修复说明 — Fixes #41; 中文说明 #42"),
        vec![
            RefMention {
                number: 41,
                kind: RefKind::Closes,
            },
            RefMention {
                number: 42,
                kind: RefKind::Mentions,
            },
        ]
    );
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
