#[test]
fn calculate_score_high_salience_ranks_higher() {
    let config = ScoringConfig {
        adjustments: vec![],
        ..ScoringConfig::default()
    };
    let now_ms = 1_000_000i64;
    let score_high = calculate_score(
        &ScoreInput {
            salience: 0.9,
            memory_type_str: "episodic",
            content: "content",
            created_at_millis: 0,
            decay_factor: 0.01,
            now_millis: now_ms,
            relevance_score: 0.7,
            entity_names: &[],
        },
        &config,
    );
    let score_low = calculate_score(
        &ScoreInput {
            salience: 0.1,
            memory_type_str: "episodic",
            content: "content",
            created_at_millis: 0,
            decay_factor: 0.01,
            now_millis: now_ms,
            relevance_score: 0.7,
            entity_names: &[],
        },
        &config,
    );
    assert!(score_high > score_low, "high salience should rank higher");
}

#[test]
fn calculate_score_entity_match_from_auto_extraction_lifts_equal_relevance_candidate() {
    // Wires `extract_entity_candidates` straight into `calculate_score` with
    // the default adjustments (EntityMatch ×1.3 included) — everything else
    // held identical between the two candidates — to isolate exactly what
    // the entity boost contributes, decoupled from retrieval-stage relevance
    // differences (which the handler-level tests in recall.rs cover instead).
    let config = ScoringConfig::default();
    let now_ms = 1_000_000i64;

    // Capitalized-signal query so extraction yields a single isolated
    // candidate ("gamma") — that keeps this test's two candidates
    // differing by exactly one factor (does the content mention the
    // named entity or not).
    let query = "sibling transfer to Gamma university";
    let entity_names = extract_entity_candidates(query);
    assert_eq!(
        entity_names,
        vec!["gamma"],
        "capitalized-signal query must extract exactly the one proper noun: {entity_names:?}"
    );

    let matching_score = calculate_score(
        &ScoreInput {
            salience: 0.5,
            memory_type_str: "semantic",
            content: "sibling transferred to gamma university this fall",
            created_at_millis: 0,
            decay_factor: 0.005,
            now_millis: now_ms,
            relevance_score: 0.5,
            entity_names: &entity_names,
        },
        &config,
    );
    let non_matching_score = calculate_score(
        &ScoreInput {
            salience: 0.5,
            memory_type_str: "semantic",
            content: "sibling transferred to a different university this fall",
            created_at_millis: 0,
            decay_factor: 0.005,
            now_millis: now_ms,
            relevance_score: 0.5,
            entity_names: &entity_names,
        },
        &config,
    );

    assert!(
        matching_score > non_matching_score,
        "memory matching an auto-extracted entity name must outrank an \
             equal-relevance memory that doesn't: matching={matching_score} \
             non_matching={non_matching_score}"
    );
    // The EntityMatch adjustment is a flat ×1.3 multiply applied after the
    // base formula and the other (inactive, for this age/salience combo)
    // adjustments — assert the lift is approximately that factor, not just
    // "higher", so a future change to the adjustment order/weights is caught.
    let ratio = matching_score / non_matching_score;
    assert!(
        (ratio - 1.3).abs() < 0.01,
        "expected ~1.3x lift from the EntityMatch adjustment, got ratio {ratio}"
    );
}
