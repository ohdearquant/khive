// ── CRIT-1 regression tests: seed-prior ESS gate + no panic/poison path ──────

#[tokio::test]
async fn crit1_create_profile_rejects_seed_priors_exceeding_ess_cap() {
    // alpha + beta = 1000 > DEFAULT_ESS_CAP (100.0) → must be rejected.
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let err = pack
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "bad-ess-profile",
                "consumer_kind": "search",
                "seed_priors": {
                    "section_posteriors": {
                        "operational_guidance": {"alpha": 500.0, "beta": 500.0}
                    }
                }
            }),
            &registry,
            &token,
        )
        .await
        .unwrap_err();

    if let RuntimeError::InvalidInput(msg) = &err {
        assert!(
            msg.contains("ESS") || msg.contains("alpha+beta") || msg.contains("exceeds"),
            "error must mention the ESS constraint; got: {msg}"
        );
    } else {
        panic!("expected InvalidInput, got {err:?}");
    }
}

#[tokio::test]
async fn crit1_create_profile_accepts_seed_priors_within_ess_cap() {
    // alpha + beta = 7.5 <= DEFAULT_ESS_CAP (100.0) → must succeed.
    let (pack, rt) = make_pack();
    let registry = empty_registry();
    let token = rt.authorize(Namespace::local()).unwrap();

    let result = pack
        .dispatch(
            "brain.create_profile",
            json!({
                "name": "ok-ess-profile",
                "consumer_kind": "search",
                "seed_priors": {
                    "section_posteriors": {
                        "operational_guidance": {"alpha": 6.0, "beta": 1.5}
                    }
                }
            }),
            &registry,
            &token,
        )
        .await
        .expect("create with valid seed priors must succeed");

    assert_eq!(result["created"], json!(true));
}
