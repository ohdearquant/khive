use serde_json::{json, Value};
use uuid::Uuid;

use super::{Arm, Filter};

pub(super) fn validate_sanity(
    result: &Value,
    arm: Arm,
    filter: Filter,
) -> Result<(), &'static str> {
    // These are fixture facts, not values copied from the measured row or oracle.
    // Every eligible document has a unique increasing length and vector angle;
    // neither query exactly matches an entity name, so RRF adds no name bonus.
    let top_ids: Vec<_> = (1..=10).map(|id| Uuid::from_u128(id).to_string()).collect();
    let (text_count, vector_count) = match arm {
        Arm::Text => (
            match filter {
                Filter::Kind => 20,
                Filter::Type => 40,
                Filter::KindAndType => 30,
            },
            0,
        ),
        Arm::Vector => (0, 40),
    };
    for (field, expected) in [
        ("returned_ids", json!(top_ids)),
        ("oracle_ids", json!(top_ids)),
        ("text_candidates", json!(text_count)),
        ("vector_candidates", json!(vector_count)),
        ("total_rows", json!(40)),
        ("eligible_rows", json!(20)),
        ("limit", json!(10)),
        ("candidate_cap_per_arm", json!(40)),
        ("fill_rate", json!(1.0)),
        ("recall_at_k", json!(1.0)),
    ] {
        if result.get(field) != Some(&expected) {
            return Err(field);
        }
    }
    Ok(())
}

pub(super) fn assert_negative_controls(result: &Value, arm: Arm, filter: Filter) {
    let mut partial = result.clone();
    partial["returned_ids"].as_array_mut().unwrap().truncate(1);
    // Keep the derived metrics coherent so this catches a partial result rather
    // than relying on an inconsistent report to identify the missing records.
    partial["fill_rate"] = json!(0.1);
    partial["recall_at_k"] = json!(0.1);
    assert_eq!(
        validate_sanity(&partial, arm, filter),
        Err("returned_ids"),
        "one eligible result must not pass a complete top-ten fixture"
    );

    let mut wrong_top = result.clone();
    wrong_top["returned_ids"][9] = json!(Uuid::from_u128(20).to_string());
    assert_eq!(
        validate_sanity(&wrong_top, arm, filter),
        Err("returned_ids"),
        "ten distinct eligible IDs must still be the correct top ten"
    );

    let mut wrong_oracle = result.clone();
    wrong_oracle["oracle_ids"][9] = json!(Uuid::from_u128(20).to_string());
    assert_eq!(
        validate_sanity(&wrong_oracle, arm, filter),
        Err("oracle_ids"),
        "a corrupted oracle must not define its own expected result"
    );

    for field in ["text_candidates", "vector_candidates"] {
        let mut wrong_count = result.clone();
        wrong_count[field] = json!(1);
        assert_eq!(
            validate_sanity(&wrong_count, arm, filter),
            Err(field),
            "each arm count must be exact, including the disabled or empty arm"
        );
    }
}
