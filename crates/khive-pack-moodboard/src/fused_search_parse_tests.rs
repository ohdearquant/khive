// Argument handling for `moodboard.search` rerank modes.

use super::*;

fn refusal(params: Value) -> String {
    match parse_rerank(&params) {
        Err(RuntimeError::InvalidInput(message)) => message,
        other => panic!("expected an InvalidInput refusal for {params}, got {other:?}"),
    }
}

#[test]
fn rerank_refuses_unknown_values_naming_the_value() {
    assert!(refusal(json!({ "rerank": "blend" })).contains("blend"));
    assert!(refusal(json!({ "rerank": "Fused" })).contains("Fused"));
    assert!(refusal(json!({ "rerank": 3 })).contains('3'));
}

#[test]
fn descriptor_weight_must_be_finite_and_strictly_between_zero_and_one() {
    for weight in [
        json!(0),
        json!(0.0),
        json!(1),
        json!(1.0),
        json!(-0.25),
        json!(1.5),
        json!("0.5"),
        json!(true),
    ] {
        let message = refusal(json!({ "rerank": "fused", "descriptor_weight": weight }));
        assert!(message.contains("descriptor_weight"), "{message}");
    }
    // JSON cannot carry non-finite numbers, so the finite check is exercised directly.
    for weight in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(matches!(
            validate_descriptor_weight(weight),
            Err(RuntimeError::InvalidInput(_))
        ));
    }
    assert_eq!(validate_descriptor_weight(0.0001).unwrap(), 0.0001);
    assert_eq!(validate_descriptor_weight(0.9999).unwrap(), 0.9999);
}

#[test]
fn rrf_k_must_be_a_positive_integer_within_the_cap() {
    for k in [json!(0), json!(-1), json!(1.5), json!("60"), json!(10_001)] {
        let message = refusal(json!({ "rerank": "fused", "rrf_k": k }));
        assert!(message.contains("rrf_k"), "{message}");
    }
    for k in [1_usize, 10_000] {
        assert_eq!(
            parse_rerank(&json!({ "rerank": "fused", "rrf_k": k })).unwrap(),
            RerankMode::Fused {
                descriptor_weight: 0.5,
                rrf_k: k,
            }
        );
    }
}

#[test]
fn tuning_arguments_are_refused_without_fused_mode() {
    for params in [
        json!({ "descriptor_weight": 0.3 }),
        json!({ "rerank": "visual", "descriptor_weight": 0.3 }),
        json!({ "rrf_k": 60 }),
        json!({ "rerank": "visual", "rrf_k": 60 }),
    ] {
        let message = refusal(params);
        assert!(message.contains("fused"), "{message}");
    }
}

#[test]
fn fused_mode_defaults_and_accepted_arguments() {
    assert_eq!(
        parse_rerank(&json!({ "rerank": "fused" })).unwrap(),
        RerankMode::Fused {
            descriptor_weight: 0.5,
            rrf_k: 60,
        }
    );
    assert_eq!(
        parse_rerank(&json!({ "rerank": "fused", "descriptor_weight": 0.25, "rrf_k": 1 })).unwrap(),
        RerankMode::Fused {
            descriptor_weight: 0.25,
            rrf_k: 1,
        }
    );
}

#[test]
fn search_accepts_the_rerank_arguments_and_still_refuses_unknown_ones() {
    let all = json!({
        "asset_id": "00000000-0000-0000-0000-000000000001",
        "top_k": 5,
        "rerank": "fused",
        "descriptor_weight": 0.25,
        "rrf_k": 10,
    });
    assert!(require_fields(&all, "moodboard.search", SEARCH_FIELDS).is_ok());
    let unknown = json!({ "asset_id": "x", "blend": 1 });
    assert!(matches!(
        require_fields(&unknown, "moodboard.search", SEARCH_FIELDS),
        Err(RuntimeError::InvalidInput(_))
    ));
}
