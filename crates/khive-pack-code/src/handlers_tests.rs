use serde_json::json;

use super::{parse_params, parse_tiers, TierSelection};

#[test]
fn tiers_default_to_l1_and_l1_5() {
    let expected = TierSelection {
        enable_l1: true,
        enable_l1_5: true,
        enable_l2: false,
    };

    assert_eq!(parse_tiers(None).unwrap(), expected);
}

#[test]
fn tiers_accept_an_empty_selection() {
    let tiers = Vec::new();
    assert_eq!(
        parse_tiers(Some(&tiers)).unwrap(),
        TierSelection {
            enable_l1: false,
            enable_l1_5: false,
            enable_l2: false,
        }
    );
}

#[test]
fn tiers_accept_each_supported_value() {
    let cases = [
        (
            "l1",
            TierSelection {
                enable_l1: true,
                enable_l1_5: false,
                enable_l2: false,
            },
        ),
        (
            "l1.5",
            TierSelection {
                enable_l1: false,
                enable_l1_5: true,
                enable_l2: false,
            },
        ),
        (
            "l2",
            TierSelection {
                enable_l1: false,
                enable_l1_5: false,
                enable_l2: true,
            },
        ),
    ];

    for (tier, expected) in cases {
        assert_eq!(parse_tiers(Some(&[tier.to_string()])).unwrap(), expected);
    }
}

#[test]
fn tiers_deduplicate_entries_and_ignore_input_order() {
    let tiers = ["l2", "l1.5", "l1", "l2"].map(str::to_string);
    assert_eq!(
        parse_tiers(Some(&tiers)).unwrap(),
        TierSelection {
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: true,
        }
    );
}

#[test]
fn tiers_reject_a_scalar_value() {
    let scalar = parse_params(json!({"path": ".", "tiers": "l2"})).unwrap_err();
    assert!(scalar.to_string().contains("invalid type: string"));
}

#[test]
fn tiers_reject_non_string_entries() {
    let non_string = parse_params(json!({"path": ".", "tiers": ["l1", 2]})).unwrap_err();
    assert!(non_string.to_string().contains("invalid type: integer"));
}

#[test]
fn tiers_reject_unknown_values() {
    let unknown = parse_tiers(Some(&["L2".to_string()])).unwrap_err();
    assert_eq!(
        unknown.to_string(),
        "invalid input: unknown tier \"L2\"; valid: l1, l1.5, l2"
    );
}
