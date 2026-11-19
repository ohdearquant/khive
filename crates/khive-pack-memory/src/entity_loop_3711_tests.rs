use super::*;
use std::cell::Cell;
thread_local! { static WORK: Cell<usize> = const { Cell::new(0) }; }
pub(super) fn body_materialized() {
    WORK.with(|work| work.set(work.get() + 1));
}
fn reset() {
    WORK.with(|work| work.set(0));
}
fn work() -> usize {
    WORK.with(Cell::get)
}
fn legacy_contains_at_word_boundary(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    if needle.chars().all(is_cjk_char) {
        return haystack.contains(needle);
    }
    let haystack_chars: Vec<char> = haystack.chars().collect();
    let needle_chars: Vec<char> = needle.chars().collect();
    let n = needle_chars.len();
    if n == 0 || haystack_chars.len() < n {
        return false;
    }
    for start in 0..=(haystack_chars.len() - n) {
        if haystack_chars[start..start + n] != needle_chars[..] {
            continue;
        }
        let before_ok = start == 0 || !haystack_chars[start - 1].is_alphanumeric();
        let after_idx = start + n;
        let after_ok =
            after_idx >= haystack_chars.len() || !haystack_chars[after_idx].is_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn ctx<'a>(body: &'a str, names: &'a [String]) -> CandidateContext<'a> {
    CandidateContext {
        memory_type: "episodic",
        age_days: 0.0,
        salience: 0.5,
        content: body,
        entity_names: names,
    }
}
#[test]
fn entity_body_cache_preserves_legacy_boundaries_and_empty_conditions() {
    let bodies = [
        "",
        "ALPHABET soup",
        "water scarcity",
        "École; beta, knowledge graph",
        "中华人民共和国",
        "é e\u{301} 東京 北京",
        "x_beta_y",
    ];
    let lists = [
        vec![],
        vec![""],
        vec!["beta"],
        vec!["car", "knowledge graph"],
        vec!["é", "e\u{301}", "école"],
        vec!["人民", "東京", "missing"],
        vec!["中x", "beta", "beta"],
    ];
    for body in bodies {
        for names in &lists {
            let names: Vec<String> = names.iter().map(|name| (*name).to_string()).collect();
            let lower = body.to_lowercase();
            let matched = names
                .iter()
                .any(|name| legacy_contains_at_word_boundary(&lower, name));
            let context = ctx(body, &names);
            assert_eq!(
                AdjustmentCondition::EntityMatch.matches(&context),
                !names.is_empty() && matched
            );
            assert_eq!(
                AdjustmentCondition::EntityMiss.matches(&context),
                !names.is_empty() && !matched
            );
        }
    }
}
#[test]
fn entity_body_cache_materializes_body_at_most_once_per_condition() {
    let body = "unchanged note body ".repeat(1024);
    for n in [1, 32, 256] {
        let names: Vec<String> = (0..n).map(|i| format!("missing{i}")).collect();
        let context = ctx(&body, &names);
        for condition in [
            AdjustmentCondition::EntityMatch,
            AdjustmentCondition::EntityMiss,
        ] {
            reset();
            assert_eq!(
                condition.matches(&context),
                matches!(condition, AdjustmentCondition::EntityMiss)
            );
            assert_eq!(work(), 1, "body characters are shared across all names");
        }
    }
    for names in [
        vec![],
        vec![String::new()],
        vec!["中文".to_string(), "東京".to_string()],
    ] {
        let context = ctx(&body, &names);
        reset();
        AdjustmentCondition::EntityMatch.matches(&context);
        assert_eq!(
            work(),
            0,
            "empty and all-CJK names retain their early branches"
        );
    }
}

#[test]
fn entity_body_cache_keeps_both_word_boundaries() {
    let names = vec!["beta".to_string()];
    for (body, matched) in [
        ("beta", true),
        (" beta.", true),
        ("x beta y", true),
        ("microbeta", false),
        ("betamax", false),
        ("ébeta", false),
        ("betaé", false),
        ("東京beta", false),
        ("beta東京", false),
        ("beta\u{301}", true),
    ] {
        let context = ctx(body, &names);
        assert_eq!(
            AdjustmentCondition::EntityMatch.matches(&context),
            matched,
            "match body: {body}"
        );
        assert_eq!(
            AdjustmentCondition::EntityMiss.matches(&context),
            !matched,
            "miss body: {body}"
        );
        assert_eq!(
            legacy_contains_at_word_boundary(&body.to_lowercase(), "beta"),
            matched,
            "parent oracle body: {body}"
        );
    }
}
