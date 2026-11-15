use super::*;

#[test]
#[allow(deprecated)]
fn is_meaningful_query_rejects_empty() {
    assert!(!is_meaningful_query(""));
    assert!(!is_meaningful_query("   "));
}

#[test]
#[allow(deprecated)]
fn is_meaningful_query_rejects_symbols_only() {
    assert!(!is_meaningful_query("!@#$%"));
    assert!(!is_meaningful_query("..."));
}

#[test]
#[allow(deprecated)]
fn is_meaningful_query_rejects_single_latin_char() {
    assert!(!is_meaningful_query("a"));
    assert!(!is_meaningful_query("Z"));
}

#[test]
#[allow(deprecated)]
fn is_meaningful_query_rejects_repeated_gibberish() {
    assert!(!is_meaningful_query("aaaa bbbb cccc"));
}

#[test]
#[allow(deprecated)]
fn is_meaningful_query_accepts_normal_queries() {
    assert!(is_meaningful_query("what is the capital of France"));
    assert!(is_meaningful_query("rust async runtime"));
    assert!(is_meaningful_query("hello"));
}

#[test]
fn contains_cjk_detects_chinese() {
    // Purely CJK — well above 15% threshold.
    assert!(contains_cjk("你好世界"));
    // Mixed string: "世界" = 2 CJK out of 6 chars = 33%, above threshold.
    assert!(contains_cjk("世界 hi"));
    // Mostly-Latin with 2 CJK chars out of 15 total = 13% → below threshold.
    assert!(!contains_cjk("hello 世界 world"));
}

#[test]
fn contains_cjk_ignores_latin() {
    assert!(!contains_cjk("hello world"));
    assert!(!contains_cjk(""));
}

#[test]
fn is_cjk_dominant_query_accepts_pure_cjk() {
    assert!(is_cjk_dominant_query("你好世界"));
    assert!(is_cjk_dominant_query("重做嵌入索引之前必须做什么准备？"));
    // A single character is dominant: nothing outnumbers it.
    assert!(is_cjk_dominant_query("你"));
    // Hiragana, Katakana, and Hangul count as CJK.
    assert!(is_cjk_dominant_query("これはペンです"));
    assert!(is_cjk_dominant_query("안녕하세요"));
}

#[test]
fn is_cjk_dominant_query_rejects_pure_english() {
    assert!(!is_cjk_dominant_query("hello world"));
    assert!(!is_cjk_dominant_query(
        "what must be prepared before rebuilding the embedding index"
    ));
    assert!(!is_cjk_dominant_query("café naïve"));
    // Other scripts are letters that outnumber a short CJK run.
    assert!(!is_cjk_dominant_query("привет мир 你好"));
}

#[test]
fn is_cjk_dominant_query_mixed_follows_the_letter_majority() {
    // 7 CJK characters against 11 Latin letters.
    assert!(!is_cjk_dominant_query("KHIVE 的 recall 为什么返回空"));
    // 2 CJK characters against 10 Latin letters.
    assert!(!is_cjk_dominant_query("hello 世界 world"));
    // 7 CJK characters against 5 Latin letters.
    assert!(is_cjk_dominant_query("重建嵌入索引 khive 的"));
    // 4 CJK characters against 2 Latin letters.
    assert!(is_cjk_dominant_query("世界你好 hi"));
}

#[test]
fn is_cjk_dominant_query_ignores_digits_whitespace_and_punctuation() {
    assert!(!is_cjk_dominant_query(""));
    assert!(!is_cjk_dominant_query("   "));
    assert!(!is_cjk_dominant_query("2026-10-13 12345"));
    assert!(!is_cjk_dominant_query("。。。，，，！？"));
    assert!(!is_cjk_dominant_query("hello 12345 !!!"));
    // Digits and punctuation do not offset CJK: 2 CJK characters against no other letter.
    assert!(is_cjk_dominant_query("你好 2026 12345 ,.;"));
    assert!(is_cjk_dominant_query("（你好）"));
}

#[test]
fn is_cjk_dominant_query_tie_is_not_dominant() {
    // 2 CJK characters against 2 Latin letters: a tie, and strictly more CJK is required.
    assert!(!is_cjk_dominant_query("世界 hi"));
    assert!(!is_cjk_dominant_query("你 a"));
    // One more CJK character than Latin letters tips it.
    assert!(is_cjk_dominant_query("世界 h"));
    assert!(is_cjk_dominant_query("你好 a"));
    for n in 1..=6usize {
        let tie = format!("{} {}", "字".repeat(n), "a".repeat(n));
        let one_more = format!("{} {}", "字".repeat(n + 1), "a".repeat(n));
        assert!(!is_cjk_dominant_query(&tie), "tie at n={n}: {tie:?}");
        assert!(
            is_cjk_dominant_query(&one_more),
            "n+1 CJK at n={n}: {one_more:?}"
        );
    }
}

#[test]
fn is_cjk_dominant_query_is_stricter_than_contains_cjk() {
    // 2 of 5 characters are CJK: above the 15% share, but not more than the other letters.
    assert!(contains_cjk("世界 hi"));
    assert!(!is_cjk_dominant_query("世界 hi"));
}

#[test]
fn scoring_config_old_multilingual_routing_key_still_disables_cjk_bypass() {
    let cfg: ScoringConfig =
        serde_json::from_value(serde_json::json!({ "enable_multilingual_routing": false }))
            .expect("old key must still deserialize via the alias");
    assert!(
        !cfg.enable_cjk_fts_bypass,
        "a config that disabled the old enable_multilingual_routing key must still \
             disable the CJK FTS bypass under its new name"
    );
}

#[test]
fn scoring_config_default_enables_cjk_bypass() {
    assert!(ScoringConfig::default().enable_cjk_fts_bypass);
}

#[test]
fn normalize_min_score_fraction_passthrough() {
    let v = normalize_min_score(0.5).unwrap();
    assert!((v - 0.5f32).abs() < 1e-6);
}

#[test]
fn normalize_min_score_percent_form() {
    let v = normalize_min_score(50.0).unwrap();
    assert!((v - 0.5f32).abs() < 1e-6);
}

#[test]
fn normalize_min_score_rejects_out_of_range() {
    assert!(normalize_min_score(200.0).is_err());
    assert!(normalize_min_score(-1.0).is_err());
    assert!(normalize_min_score(f64::NAN).is_err());
}

#[test]
fn calculate_score_returns_unit_interval() {
    let config = ScoringConfig::default();
    let score = calculate_score(
        &ScoreInput {
            salience: 0.9,
            memory_type_str: "episodic",
            content: "test content",
            created_at_millis: 0,
            decay_factor: 0.01,
            now_millis: 1000,
            relevance_score: 0.8,
            entity_names: &[],
        },
        &config,
    );
    assert!((0.0..=1.0).contains(&score), "score {score} out of [0,1]");
}

#[test]
fn calculate_score_floors_nonfinite_adjustment_chain() {
    let episodic = AdjustmentCondition::MemoryType {
        kind: "episodic".to_string(),
    };
    let config = ScoringConfig {
        adjustments: vec![
            ScoreAdjustment {
                condition: episodic.clone(),
                operation: AdjustmentOp::Add { value: 3.0e38 },
            },
            ScoreAdjustment {
                condition: episodic.clone(),
                operation: AdjustmentOp::Add { value: 3.0e38 },
            },
            ScoreAdjustment {
                condition: episodic,
                operation: AdjustmentOp::Multiply { factor: 0.0 },
            },
        ],
        ..ScoringConfig::default()
    };
    let score = calculate_score(
        &ScoreInput {
            salience: 0.8,
            memory_type_str: "episodic",
            content: "overflow adjustment chain",
            created_at_millis: 0,
            decay_factor: 0.01,
            now_millis: 1_000,
            relevance_score: 0.8,
            entity_names: &[],
        },
        &config,
    );
    assert_eq!(score, 0.0, "non-finite composite must rank at the floor");
}

include!("scoring_tests/positive_candidate_ranking_tests.rs");

#[test]
fn dos_caps_enforce_limits() {
    let mut config = ScoringConfig {
        max_recall_candidates: 9999,
        default_token_budget: 99999,
        default_recall_limit: 9999,
        ..ScoringConfig::default()
    };
    config.apply_dos_caps();
    assert_eq!(config.max_recall_candidates, MAX_RECALL_CANDIDATES);
    assert_eq!(config.default_token_budget, MAX_TOKEN_BUDGET);
    assert_eq!(config.default_recall_limit, MAX_RECALL_LIMIT);
}

#[test]
fn normalize_rrf_scores_preserves_ordering() {
    let config = ScoringConfig::default();
    let input = vec![
        (Uuid::new_v4(), 0.030f32),
        (Uuid::new_v4(), 0.020f32),
        (Uuid::new_v4(), 0.015f32),
    ];
    let ids: Vec<Uuid> = input.iter().map(|(id, _)| *id).collect();
    let output = normalize_rrf_scores(input, &config);
    let s0 = output[&ids[0]];
    let s1 = output[&ids[1]];
    let s2 = output[&ids[2]];
    assert!(s0 > s1 && s1 > s2, "ordering must be preserved");
    assert!(s0 <= 1.0 && s2 >= 0.0, "scores must be in [0,1]");
}

// ── extract_entity_candidates ─────────────────────────────────────────────

#[test]
fn extract_entity_candidates_capitalized_tokens_only() {
    // Only capitalized tokens qualify; lowercase content words
    // ("sibling", "college") are excluded even though they're not
    // stopwords.
    let out = extract_entity_candidates("Alex sibling college Acme Beta Gamma");
    assert_eq!(out, vec!["alex", "acme", "beta", "gamma"]);
}

#[test]
fn extract_entity_candidates_all_lowercase_query_extracts_nothing() {
    // DESIGN RULING: the lowercase fallback was removed after review —
    // it degenerated EntityMatch into a second lexical-overlap reward on
    // top of retrieval-stage relevance. An all-lowercase query (however
    // topical) must extract zero candidates.
    let out = extract_entity_candidates("alex sibling college acme beta gamma");
    assert!(out.is_empty());
}

#[test]
fn extract_entity_candidates_strips_stopwords() {
    let out = extract_entity_candidates("what is the capital of France");
    // "what", "is", "the", "of" are stopwords; "capital" is lowercase (excluded);
    // "France" is capitalized and not a stopword, so it remains.
    assert_eq!(out, vec!["france"]);
}

#[test]
fn extract_entity_candidates_strips_punctuation() {
    let out = extract_entity_candidates("Alex's sibling, Acme!");
    assert_eq!(out, vec!["alex's", "acme"]);
}

#[test]
fn extract_entity_candidates_caps_at_max_auto_entity_names() {
    let query = "Alpha Bravo Charlie Delta Echo Foxtrot Golf Hotel India Juliet";
    let out = extract_entity_candidates(query);
    assert_eq!(out.len(), MAX_AUTO_ENTITY_NAMES);
    assert_eq!(
        out,
        vec!["alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel"]
    );
}

#[test]
fn extract_entity_candidates_dedupes_case_insensitively() {
    let out = extract_entity_candidates("Acme ACME college");
    // capitalization signal present (Acme, ACME) → only capitalized tokens
    // qualify; "college" is dropped; Acme and ACME collapse to one entry.
    assert_eq!(out, vec!["acme"]);
}

#[test]
fn extract_entity_candidates_empty_query_returns_empty() {
    assert!(extract_entity_candidates("").is_empty());
    assert!(extract_entity_candidates("   ").is_empty());
}

#[test]
fn extract_entity_candidates_all_stopwords_returns_empty() {
    assert!(extract_entity_candidates("is the a of").is_empty());
}

#[test]
fn entity_lookup_candidates_is_length_and_position_fair_for_long_cjk_run() {
    let run: String = (0..65)
        .map(|offset| char::from_u32(0x4e00 + offset).expect("valid CJK character"))
        .collect();
    let candidates = entity_lookup_candidates(&run);

    assert_eq!(candidates.len(), MAX_ENTITY_LOOKUP_CANDIDATES);
    for len in MIN_CJK_LOOKUP_CHARS..=MAX_CJK_LOOKUP_CHARS {
        let expected_quota = MAX_ENTITY_LOOKUP_CANDIDATES
            / (MAX_CJK_LOOKUP_CHARS - MIN_CJK_LOOKUP_CHARS + 1)
            + usize::from(len == MIN_CJK_LOOKUP_CHARS);
        assert_eq!(
            candidates
                .iter()
                .filter(|candidate| candidate.chars().count() == len)
                .count(),
            expected_quota,
            "candidate set must reserve the exact quota for length {len}"
        );

        let chars: Vec<char> = run.chars().collect();
        let has_late_candidate = (chars.len() - 10..=chars.len() - len).any(|start| {
            let expected: String = chars[start..start + len].iter().collect();
            candidates.contains(&expected)
        });
        assert!(
            has_late_candidate,
            "length {len} must include a candidate starting in the final 10 positions"
        );
    }
}

#[test]
fn entity_lookup_candidates_retains_final_bigram_independent_of_ascii_case() {
    let lowercase = "one two three four five six seven eight nine ten eleven twelve thirteen fourteen fifteen sixteen final entity";
    let title_case = "One Two Three Four Five Six Seven Eight Nine Ten Eleven Twelve Thirteen Fourteen Fifteen Sixteen Final Entity";

    let lowercase_candidates = entity_lookup_candidates(lowercase);
    let title_case_candidates = entity_lookup_candidates(title_case);
    let stored_name_ci = "Final Entity".to_ascii_lowercase();

    assert_eq!(title_case_candidates, lowercase_candidates);
    assert!(lowercase_candidates.contains(&stored_name_ci));
    assert!(title_case_candidates.contains(&stored_name_ci));
    assert!(!title_case_candidates.contains(&"Final Entity".to_string()));
}

// ── EntityMatch word-boundary matching ──────────────────────────────────────

fn entity_match_ctx<'a>(content: &'a str, entity_names: &'a [String]) -> CandidateContext<'a> {
    CandidateContext {
        memory_type: "episodic",
        age_days: 0.0,
        salience: 0.5,
        content,
        entity_names,
    }
}

#[test]
fn contains_at_word_boundary_rejects_substring_inside_another_word() {
    assert!(!contains_at_word_boundary("alphabet soup", "beta"));
    assert!(!contains_at_word_boundary("buy a betamax player", "beta"));
    assert!(!contains_at_word_boundary(
        "water scarcity is rising",
        "car"
    ));
}

#[test]
fn contains_at_word_boundary_accepts_the_word_itself() {
    assert!(contains_at_word_boundary("beta release notes", "beta"));
    assert!(contains_at_word_boundary("drove the car home", "car"));
    // start/end of string edges (no adjacent char to fail the boundary check).
    assert!(contains_at_word_boundary("beta", "beta"));
    assert!(contains_at_word_boundary("notes beta", "beta"));
}

#[test]
fn contains_at_word_boundary_accepts_multi_word_phrase() {
    // Internal spaces in a multi-word candidate are themselves
    // non-alphanumeric, so the phrase's own boundaries (not each word's)
    // are what the check anchors on.
    assert!(contains_at_word_boundary(
        "the knowledge graph shows this",
        "knowledge graph"
    ));
    assert!(!contains_at_word_boundary(
        "prior knowledge, graphical view",
        "knowledge graph"
    ));
}

#[test]
fn entity_match_condition_rejects_substring_false_positives() {
    let entity_names = vec!["beta".to_string()];
    let ctx = entity_match_ctx("alphabet soup for dinner", &entity_names);
    assert!(
        !AdjustmentCondition::EntityMatch.matches(&ctx),
        "\"beta\" must not match inside \"alphabet\""
    );

    let entity_names = vec!["car".to_string()];
    let ctx = entity_match_ctx("water scarcity is rising", &entity_names);
    assert!(
        !AdjustmentCondition::EntityMatch.matches(&ctx),
        "\"car\" must not match inside \"scarcity\""
    );
}

#[test]
fn entity_match_condition_matches_multi_word_explicit_name() {
    let entity_names = vec!["knowledge graph".to_string()];
    let ctx = entity_match_ctx("notes on the knowledge graph design", &entity_names);
    assert!(
        AdjustmentCondition::EntityMatch.matches(&ctx),
        "explicit multi-word entity name must still match on its own boundaries"
    );
}

#[test]
fn entity_match_condition_still_matches_real_word_occurrence() {
    let entity_names = vec!["beta".to_string()];
    let ctx = entity_match_ctx("the beta release ships tomorrow", &entity_names);
    assert!(
        AdjustmentCondition::EntityMatch.matches(&ctx),
        "boundary anchoring must not break matching the actual word"
    );
}

#[test]
fn entity_match_condition_matches_contiguous_cjk_with_cjk_on_both_sides() {
    let entity_names = vec!["北京大学".to_string()];
    let ctx = entity_match_ctx("我在北京大学学习", &entity_names);
    assert!(AdjustmentCondition::EntityMatch.matches(&ctx));
}

#[test]
fn entity_match_condition_keeps_alphabetic_word_boundaries() {
    let entity_names = vec!["rust".to_string()];
    let ctx = entity_match_ctx("trust requires evidence", &entity_names);
    assert!(!AdjustmentCondition::EntityMatch.matches(&ctx));
}

// ── ADR-104 §2 (Stage B): entity_posterior_term ────────────────────────────

#[test]
fn entity_posterior_term_neutral_when_no_posterior() {
    assert_eq!(entity_posterior_term(None, ENTITY_POSTERIOR_WEIGHT), 1.0);
}

#[test]
fn entity_posterior_term_neutral_at_uninformative_prior_mean() {
    // Beta(1,1) mean = 0.5 — the uninformative prior itself, distinct
    // from `None`, must also be an identity multiplier.
    let term = entity_posterior_term(Some(0.5), ENTITY_POSTERIOR_WEIGHT);
    assert!((term - 1.0).abs() < 1e-6, "got {term}");
}

#[test]
fn entity_posterior_term_reaches_low_clamp_bound_at_mean_zero() {
    let term = entity_posterior_term(Some(0.0), ENTITY_POSTERIOR_WEIGHT);
    assert!(
        (term - ENTITY_POSTERIOR_CLAMP_MIN).abs() < 1e-6,
        "w_ent=0.3 at mean=0.0 must land exactly on the 0.85 clamp bound, got {term}"
    );
}

#[test]
fn entity_posterior_term_reaches_high_clamp_bound_at_mean_one() {
    let term = entity_posterior_term(Some(1.0), ENTITY_POSTERIOR_WEIGHT);
    assert!(
        (term - ENTITY_POSTERIOR_CLAMP_MAX).abs() < 1e-6,
        "w_ent=0.3 at mean=1.0 must land exactly on the 1.15 clamp bound, got {term}"
    );
}

#[test]
fn entity_posterior_term_never_exceeds_clamp_bounds_for_out_of_range_input() {
    // A Beta posterior mean is mathematically bounded to [0, 1], but the
    // clamp must hold defensively regardless — the term is never allowed
    // to move a score by more than +-15% no matter what value reaches it.
    let low = entity_posterior_term(Some(-5.0), ENTITY_POSTERIOR_WEIGHT);
    let high = entity_posterior_term(Some(5.0), ENTITY_POSTERIOR_WEIGHT);
    assert_eq!(low, ENTITY_POSTERIOR_CLAMP_MIN);
    assert_eq!(high, ENTITY_POSTERIOR_CLAMP_MAX);
}

#[test]
fn entity_posterior_term_moves_monotonically_with_mean() {
    let low = entity_posterior_term(Some(0.2), ENTITY_POSTERIOR_WEIGHT);
    let mid = entity_posterior_term(Some(0.5), ENTITY_POSTERIOR_WEIGHT);
    let high = entity_posterior_term(Some(0.8), ENTITY_POSTERIOR_WEIGHT);
    assert!(low < mid, "low={low} mid={mid}");
    assert!(mid < high, "mid={mid} high={high}");
}

#[test]
fn meaningful_query_policies_keep_distinct_admission_rules() {
    let cases = [
        ("aaaa bbbb cccc", true, false),
        ("12345", true, false),
        ("e", false, false),
        ("", false, false),
        (" \t\n", false, false),
        ("...", false, false),
        ("😀", false, false),
        ("5", true, false),
        ("１", true, false),
        ("11111", false, false),
        ("é", true, false),
        ("Ω", true, false),
        ("ｅ", true, false),
        ("你", true, true),
        ("你你", false, true),
        ("你好世界", true, true),
        ("你好你好你好", true, false),
        ("BM25", true, true),
        ("a1", true, false),
        ("a!", true, false),
        ("aa", false, true),
        ("a a", false, true),
        ("a\u{00A0}a", false, true),
        ("aaa", false, false),
        ("aaab", true, true),
        ("aaaab", true, true),
        ("aaaaab", false, false),
        ("aaaaaaaa b", false, true),
        ("aaaaaaaab", false, false),
        ("aaaaAAAA", true, false),
        ("aaaa1111", true, false),
        ("ab!!!!!!!!!!", false, true),
        ("e\u{0301}", true, false),
        ("ééé", false, false),
        ("e\u{0301}e\u{0301}e\u{0301}", true, false),
        ("the and", true, true),
        ("hello", true, true),
    ];

    for (query, text_expected, recall_expected) in cases {
        assert_eq!(
            khive_text::is_meaningful_text_query(query),
            text_expected,
            "text policy for {query:?}"
        );
        assert_eq!(
            is_meaningful_recall_query(query),
            recall_expected,
            "recall policy for {query:?}"
        );
    }
}

fn scalar_boost_input() -> ScoreInput<'static> {
    ScoreInput {
        salience: 1.0,
        memory_type_str: "episodic",
        content: "scalar boost fixture",
        created_at_millis: 0,
        decay_factor: 0.0,
        now_millis: 0,
        relevance_score: 0.25,
        entity_names: &[],
    }
}

fn scalar_boost_config() -> ScoringConfig {
    ScoringConfig {
        weights: ScoringWeights {
            relevance: 1.0,
            temporal: 0.0,
            salience: 0.0,
        },
        adjustments: Vec::new(),
        ..ScoringConfig::default()
    }
}

fn assert_invalid_boost_stays_zero(mut input: ScoreInput<'_>, config: &ScoringConfig) {
    for relevance_score in [0.0, 0.25] {
        input.relevance_score = relevance_score;
        assert_eq!(
            calculate_score(&input, config).to_bits(),
            0.0_f32.to_bits(),
            "invalid boost at relevance {relevance_score}"
        );
    }
}

#[test]
fn calculate_score_isolates_temporal_boost() {
    let input = scalar_boost_input();
    let mut config = scalar_boost_config();

    for (weight, expected) in [(0.0, 0.25_f32), (1.0, 0.5), (2.0, 0.75)] {
        config.weights.temporal = weight;
        assert_eq!(
            calculate_score(&input, &config).to_bits(),
            expected.to_bits()
        );
    }
}

#[test]
fn calculate_score_temporal_boost_errors_return_terminal_zero() {
    let mut config = scalar_boost_config();
    config.adjustments.push(ScoreAdjustment {
        condition: AdjustmentCondition::MemoryType {
            kind: "episodic".to_string(),
        },
        operation: AdjustmentOp::Add { value: 0.5 },
    });

    for weight in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        config.weights.temporal = weight;
        assert_invalid_boost_stays_zero(scalar_boost_input(), &config);
    }

    let mut input = scalar_boost_input();
    input.now_millis = 86_400_000;
    input.decay_factor = f32::NEG_INFINITY;
    config.weights.temporal = 0.0;
    assert_eq!(
        khive_score::boost_factor_f32(0.0, f32::INFINITY),
        Err(khive_score::ArithmeticError::NonFinite { parameter: "x" })
    );
    assert_invalid_boost_stays_zero(input, &config);

    let mut input = scalar_boost_input();
    input.now_millis = 86_400_000;
    input.decay_factor = -80.0;
    config.weights.temporal = f32::MAX;
    let recency = 80.0_f32.exp();
    assert!(recency.is_finite());
    assert_eq!(
        khive_score::boost_factor_f32(f32::MAX, recency),
        Err(khive_score::ArithmeticError::Overflow {
            operation: "boost product",
        })
    );
    assert_invalid_boost_stays_zero(input, &config);
}

#[test]
fn calculate_score_salience_boost_errors_return_terminal_zero() {
    let mut config = scalar_boost_config();
    config.adjustments.push(ScoreAdjustment {
        condition: AdjustmentCondition::MemoryType {
            kind: "episodic".to_string(),
        },
        operation: AdjustmentOp::Add { value: 0.5 },
    });

    for weight in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        config.weights.salience = weight;
        assert_invalid_boost_stays_zero(scalar_boost_input(), &config);
    }

    config.weights.salience = 0.0;
    for salience in [f32::NAN, f32::INFINITY, f32::NEG_INFINITY] {
        let mut input = scalar_boost_input();
        input.salience = salience;
        assert_invalid_boost_stays_zero(input, &config);
    }

    let mut input = scalar_boost_input();
    input.salience = 2.0;
    config.weights.salience = f32::MAX;
    assert_eq!(
        khive_score::boost_factor_f32(f32::MAX, 2.0),
        Err(khive_score::ArithmeticError::Overflow {
            operation: "boost product",
        })
    );
    assert_invalid_boost_stays_zero(input, &config);
}

#[test]
fn calculate_score_clamps_only_after_subtract_adjustment() {
    let input = scalar_boost_input();
    let mut config = scalar_boost_config();
    config.weights.temporal = 3.0;
    config.weights.salience = 1.0;

    assert_eq!(calculate_score(&input, &config), 1.0);
    config.adjustments.push(ScoreAdjustment {
        condition: AdjustmentCondition::MemoryType {
            kind: "episodic".to_string(),
        },
        operation: AdjustmentOp::Subtract { value: 1.5 },
    });
    assert_eq!(calculate_score(&input, &config), 0.5);

    config.adjustments[0].operation = AdjustmentOp::Subtract { value: 2.5 };
    assert_eq!(
        calculate_score(&input, &config).to_bits(),
        0.0_f32.to_bits()
    );
}
