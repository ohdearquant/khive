use khive_runtime::presentation::{present, present_with_policy, PresentationMode};
use khive_types::VerbPresentationPolicy;
use serde_json::{json, Value};

fn presented_score(value: f64) -> f64 {
    let input = json!({"score": value});
    assert!(input["score"].is_number());
    let output = present(input, PresentationMode::Agent, 0);
    let score = output["score"]
        .as_f64()
        .expect("finite scores must remain JSON numbers");
    assert!(score.is_finite());
    score
}

#[test]
fn tiny_scores_remain_numeric_and_round_to_three_significant_digits() {
    for (input, expected) in [
        (1.23456e-307, 1.23e-307),
        (f64::MIN_POSITIVE, 2.23e-308),
        (1.23456e-310, 1.23e-310),
        (1.23456e-320, 1.23e-320),
        (f64::from_bits(1), f64::from_bits(1)),
    ] {
        for sign in [1.0, -1.0] {
            assert_eq!(presented_score(sign * input), sign * expected);
        }
    }
}

#[test]
fn unrepresentable_rounding_keeps_the_original_finite_score() {
    for input in [f64::MAX, -f64::MAX] {
        assert_eq!(presented_score(input).to_bits(), input.to_bits());
    }
}

#[test]
fn ordinary_rounding_and_signed_zero_are_preserved() {
    for (input, expected) in [
        (0.12345678, 0.123_f64),
        (-0.12345678, -0.123),
        (12.34567, 12.3),
        (999.5, 1000.0),
        (0.0, 0.0),
        (-0.0, -0.0),
    ] {
        assert_eq!(presented_score(input).to_bits(), expected.to_bits());
    }
}

#[test]
fn nested_scores_are_rounded_but_non_score_fields_keep_their_values() {
    let tiny = 1.23456e-310;
    let input = json!({
        "items": [
            {"score": tiny, "weight": tiny, "similarity": -tiny},
            {"salience": tiny, "decay_factor": tiny, "weight": f64::MAX},
        ]
    });
    let output = present(input.clone(), PresentationMode::Agent, 0);
    assert_eq!(output["items"][0]["score"], json!(1.23e-310));
    assert_eq!(output["items"][0]["similarity"], json!(-1.23e-310));
    assert_eq!(output["items"][1]["salience"], json!(1.23e-310));
    assert_eq!(output["items"][1]["decay_factor"], json!(1.23e-310));
    for index in 0..2 {
        assert_eq!(
            output["items"][index]["weight"],
            input["items"][index]["weight"]
        );
    }
}

#[test]
fn canonical_modes_and_policy_leave_scores_untouched() {
    let input: Value = json!({
        "score": f64::MAX,
        "items": [{"score": 1.23456e-310}, {"score": -0.0}],
    });
    for mode in [PresentationMode::Verbose, PresentationMode::Human] {
        assert_eq!(present(input.clone(), mode, 0), input);
    }
    assert_eq!(
        present_with_policy(
            input.clone(),
            PresentationMode::Agent,
            0,
            VerbPresentationPolicy::AlwaysVerbose,
        ),
        input,
    );
}
