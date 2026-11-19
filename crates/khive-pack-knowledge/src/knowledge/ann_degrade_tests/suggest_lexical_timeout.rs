/// A lexical-stage-*only*
/// timeout must not skip member-token pricing either. The seeded domain has
/// a real member atom rather than an empty `members: []` so a
/// non-zero `size` is only possible if `load_domain_member_token_sizes`
/// actually executed.
#[tokio::test]
async fn suggest_still_prices_members_after_a_lexical_stage_only_timeout() {
    let rt = rt_with_fake_embedder();
    let registry = build_registry(&rt);

    registry
        .dispatch(
            "knowledge.upsert_atoms",
            json!({
                "atoms": [{
                    "slug": "lexical-budget-member-atom",
                    "name": "Lexical Budget Member Atom",
                    "finalized": true,
                    "content": "a member atom with enough body content to price a non-zero token size for the owning domain once suggest reaches its member-sizing stage after a lexical-stage-only timeout"
                }]
            }),
        )
        .await
        .expect("upsert member atom");
    registry
        .dispatch(
            "knowledge.upsert_domains",
            json!({"domains": [{
                "slug": "lexical-budget-domain",
                "name": "Lexical Budget Domain",
                "description": "a domain seeded so suggest's member-sizing stage has real content to price after a lexical-stage-only timeout, not just an empty members list",
                "members": ["lexical-budget-member-atom"]
            }]}),
        )
        .await
        .expect("upsert domain");
    registry
        .dispatch("knowledge.index", json!({ "rebuild_ann": false }))
        .await
        .expect("index");

    crate::knowledge::search::seed_low_overlap_corpus(&rt, 1_000, 20).await;

    let ann = vamana::new_shared();
    let token = rt.authorize(Namespace::local()).expect("authorize");

    // A fresh slot otherwise starts a detached warm during suggest. Its real
    // blocking work can exhaust the bounded ANN wait under load independently
    // of the lexical deadline, so finish warming before pausing Tokio time.
    assert_eq!(
        vamana::ensure_ann_for_model(&rt, &token, &ann, MODEL_KEY).await,
        vamana::AnnWarmOutcome::Ready,
        "the ANN slot must be ready before the lexical-only timeout"
    );
    let loaded = vamana::search_loaded(
        &ann,
        &vamana::AnnKey::new("local", MODEL_KEY),
        &vec![1.0 / (DIM as f32).sqrt(); DIM],
        8,
    )
    .await
    .expect("the warmed ANN slot must be loaded");
    assert!(
        !loaded.is_empty(),
        "the warmed ANN slot must have candidates"
    );

    let query = "term0 term1 term2 term3 term4 term5 term6 term7";
    // Eight query words expand beyond the per-pass scaling cap (4x base).
    let stage_budget =
        std::time::Duration::from_millis(crate::knowledge::search::LEXICAL_STAGE_BUDGET_MS * 4);
    tokio::time::pause();
    let result =
        khive_storage::scope_request_read_deadline(std::time::Duration::from_secs(30), async {
            let result = crate::knowledge::search::with_fts_deadline_advance_after_term(
                1,
                stage_budget,
                KnowledgeHandlers::suggest(&rt, &token, json!({ "query": query }), &ann),
            )
            .await;
            assert!(khive_storage::ensure_request_read_active("test.suggest_stage").is_ok());
            result
        })
        .await
        .expect("suggest must not Err on a lexical-stage-only timeout");

    assert_eq!(
        result["degraded"]["lexical_timeout"], true,
        "suggest must flag degraded.lexical_timeout when only the lexical \
         stage's own budget expires; got: {result}"
    );
    assert_ne!(
        result["ann_unavailable"], true,
        "the lexical-only timeout must leave ANN available; got: {result}"
    );
    assert!(
        result["total"].as_u64().unwrap_or(0) > 0,
        "the seeded, ANN-indexed domain must still surface as a candidate; got: {result}"
    );
    assert_eq!(
        result["results"][0]["name"], "Lexical Budget Domain",
        "the only vector-backed candidate must be the seeded domain; got: {result}"
    );
    assert!(
        result["results"][0]["size"].as_u64().unwrap_or(0) > 0,
        "member token sizing must still run once the lexical-stage-only \
         timeout returns control to a healthy ambient deadline; got: {result}"
    );
}
