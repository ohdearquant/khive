// ── Regression tests for issue #94: token-budget truncation signal + rank order ──

/// #94 regression (budget cap): when the token budget caps the returned count below
/// the requested `limit`, fewer results than `limit` are returned.
///
/// The default budget-capped path returns an envelope with top-level
/// `truncated: true`; each surviving result also retains its per-item signal.
/// Uncapped responses remain bare arrays.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_budget_capped_surfaces_signal() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Create 5 notes whose content is long enough that the tiny budget below
    // cannot accommodate all of them.  Each note content is ~75 chars.
    let shared_query = "budget signal regression unique phrase alpha";
    for i in 0..5_u8 {
        let content =
            format!("budget signal regression unique phrase alpha memory content item number {i}");
        registry
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": 0.8 }),
            )
            .await
            .expect("remember");
    }

    // default_token_budget=20 tokens × chars_per_token=4 = 80 chars budget.
    // Each note is ~75 chars; only 1 fits; the remaining 4 are dropped by the prefix cut.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": shared_query,
                "limit": 5,
                "config": {
                    "scoring": {
                        "default_token_budget": 20,
                        "chars_per_token": 4,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall with tiny budget succeeds");

    let returned = result
        .as_array()
        .expect("#94: non-empty budget-capped recall keeps the bare-array shape");
    assert!(
        !returned.is_empty(),
        "#94: at least one result must fit within the budget"
    );
    assert!(
        returned.len() < 5,
        "#94: returned count ({}) must be < requested limit (5) when budget is exhausted; \
         prefix-cut is not working if all 5 fit in an 80-char budget",
        returned.len()
    );

    // #49: every item in a budget-capped bare-array response must carry an
    // explicit `truncated: true` stamp, distinguishing "capped by budget" from
    // "only N matched" without requiring include_breakdown.
    for item in returned {
        assert_eq!(
            item.get("truncated").and_then(serde_json::Value::as_bool),
            Some(true),
            "#49: budget-capped default-path result must carry truncated:true, got: {item}"
        );
    }
}

/// A budget cutoff at the first ranked candidate must remain distinguishable
/// from a genuine recall miss even though both contain zero returned items.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_oversized_single_candidate_surfaces_top_level_truncation() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    let query = "oversized single candidate budget regression";
    registry
        .dispatch(
            "memory.remember",
            json!({
                "content": format!("{query} {}", "oversized".repeat(32)),
                "salience": 0.8
            }),
        )
        .await
        .expect("remember oversized candidate");

    let truncated = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 1,
                "config": {
                    "scoring": {
                        "default_token_budget": 1,
                        "chars_per_token": 1,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall with oversized first candidate succeeds");

    assert_eq!(
        truncated
            .get("truncated")
            .and_then(serde_json::Value::as_bool),
        Some(true),
        "a budget cutoff at the first candidate must surface top-level truncated:true: {truncated}"
    );
    assert!(
        truncated
            .get("results")
            .and_then(serde_json::Value::as_array)
            .is_some_and(Vec::is_empty),
        "the oversized candidate must not be returned: {truncated}"
    );

    let no_match = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "genuine unmatched recall phrase", "limit": 1 }),
        )
        .await
        .expect("genuine no-match recall succeeds");

    assert!(no_match.as_array().is_some_and(Vec::is_empty));
    assert!(
        no_match.get("truncated").is_none(),
        "a genuine no-match must not carry a truncation signal: {no_match}"
    );
}

/// #94 regression (no false positive): when all results fit within the token budget,
/// the response is a plain array with count equal to available results.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_no_budget_cap_returns_plain_array() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    registry
        .dispatch(
            "memory.remember",
            json!({ "content": "plain array uncapped recall test phrase", "salience": 0.7 }),
        )
        .await
        .expect("remember");

    // Large budget: 4000 tokens × 4 chars = 16 000 chars — one short note trivially fits.
    let result = registry
        .dispatch(
            "memory.recall",
            json!({ "query": "plain array uncapped recall test", "limit": 3 }),
        )
        .await
        .expect("recall with generous budget succeeds");

    assert!(
        result.is_array(),
        "#94 no-cap path: recall must return a plain array; got: {result}"
    );

    // #49: an uncapped result must NOT carry the truncated stamp — the field is
    // present only when budget_capped is true, matching the pre-existing
    // `degraded` stamp's absent-when-false convention.
    for item in result.as_array().expect("array") {
        assert!(
            item.get("truncated").is_none(),
            "#49: uncapped default-path result must not carry a truncated field, got: {item}"
        );
    }
}

/// #94 regression (rank order): when the token budget forces truncation, the
/// returned items must be the top-ranked contiguous prefix — no higher-ranked
/// item may be skipped to accommodate a shorter lower-ranked item.
///
/// Setup uses THREE notes with DIFFERENT content lengths so the old greedy
/// `retain` bug actually fires:
///   rank #1 — short content (~55 chars) — fits within the 80-char budget
///   rank #2 — long content (~110 chars) — overflows the remaining budget after #1
///   rank #3 — short content (~55 chars) — would fit if reached by old retain
///
/// With the fix (prefix cut): result = [#1] — #2 overflows → cut, #3 never reached.
/// With the old retain bug:    result = [#1, #3] — #2 skipped, #3 kept (rank violation).
///
/// Assert: exactly 1 result returned AND it is rank #1 AND rank #3 is absent.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_budget_truncation_preserves_rank_order() {
    let rt = make_runtime();
    let registry = make_registry(rt.clone());

    // Shared keyword prefix so all 3 notes score on the same query.
    // Unique distinguishing suffix per note controls both length and identity.
    let query = "rankorder prefix budget truncation";

    // rank #1: short — fits in 80-char budget (~55 chars with prefix).
    let content_rank1 = format!("{query} item short alpha unique");
    // rank #2: long — overflows the remaining budget after rank #1.
    // budget = 80 chars; rank #1 uses ~55 chars; remaining = ~25 chars.
    // rank #2 must exceed 25 chars — pad to ~110 chars total.
    let content_rank2 = format!("{query} item long beta {}", "x".repeat(50));
    // rank #3: short — would fit if the old retain reached it.
    let content_rank3 = format!("{query} item short gamma unique");

    // Salience drives rank: 0.9 → rank #1, 0.6 → rank #2, 0.3 → rank #3.
    // mmr_penalty=0 prevents diversity suppression from reordering.
    let notes = [
        (&content_rank1, 0.9_f64),
        (&content_rank2, 0.6_f64),
        (&content_rank3, 0.3_f64),
    ];
    let mut stored_ids = Vec::new();
    for (content, sal) in &notes {
        let res = registry
            .dispatch(
                "memory.remember",
                json!({ "content": content, "salience": sal }),
            )
            .await
            .expect("remember");
        stored_ids.push(res["id"].as_str().unwrap().to_owned());
    }
    let id_rank1 = &stored_ids[0];
    let id_rank3 = &stored_ids[2];

    // budget = 20 tokens × 4 chars = 80 chars.
    // rank #1 (~55 chars) admitted. rank #2 (~110 chars) pushes total to ~165 > 80 → cut.
    // rank #3 is never considered (prefix cut stops at rank #2 overflow).
    let result = registry
        .dispatch(
            "memory.recall",
            json!({
                "query": query,
                "limit": 3,
                "config": {
                    "scoring": {
                        "default_token_budget": 20,
                        "chars_per_token": 4,
                        "mmr_penalty": 0.0
                    }
                }
            }),
        )
        .await
        .expect("recall with rank-order budget test");

    let returned = result
        .as_array()
        .expect("#94 rank-order: non-empty budget-capped recall keeps the bare-array shape");

    // Exactly 1 result (rank #1 fits, prefix cut stops at rank #2 overflow).
    // Old retain would return 2 results: [rank#1, rank#3].
    assert_eq!(
        returned.len(),
        1,
        "#94 rank-order: exactly 1 result expected (rank #1 fits, rank #2 overflows, \
         rank #3 must not be reached); got {} — old retain bug returns [#1,#3]",
        returned.len()
    );

    let returned_id = returned[0]["id"].as_str().expect("note_id present");
    assert_eq!(
        returned_id, id_rank1,
        "#94 rank-order: the single returned note must be rank #1 ({id_rank1}), got {returned_id}"
    );

    // rank #3 must not appear — its presence is the rank-skip signature.
    let has_rank3 = returned.iter().any(|r| r["id"].as_str() == Some(id_rank3));
    assert!(
        !has_rank3,
        "#94 rank-order: rank #3 ({id_rank3}) must NOT be in results — \
         its presence proves the old greedy retain was used instead of the prefix cut"
    );
}

// ── Regression tests for issue #1749: recall budget and supersession bounds ──

/// Degenerate or overflowing effective character budgets are caller errors,
/// not empty recall results. Validation must happen before candidate retrieval
/// so the behavior does not depend on whether the namespace contains a match.
#[tokio::test]
#[serial_test::serial(config_ledger)]
async fn test_recall_rejects_degenerate_and_overflowing_token_budgets() {
    let rt = make_runtime();
    let registry = make_registry(rt);
    let cases = [
        (json!({ "default_token_budget": 0 }), "default_token_budget"),
        (json!({ "chars_per_token": 0 }), "chars_per_token"),
        (
            json!({
                "default_token_budget": 16_000,
                "chars_per_token": usize::MAX
            }),
            "overflows",
        ),
    ];

    for (scoring, expected_message) in cases {
        let err = registry
            .dispatch(
                "memory.recall",
                json!({
                    "query": "issue 1749 checked token budget regression",
                    "config": { "scoring": scoring }
                }),
            )
            .await
            .expect_err("degenerate effective token budget must be rejected");
        assert!(
            matches!(&err, RuntimeError::InvalidInput(_)),
            "budget validation must return InvalidInput, got {err:?}"
        );
        assert!(
            err.to_string().contains(expected_message),
            "budget validation error must identify {expected_message:?}, got {err}"
        );
    }
}
