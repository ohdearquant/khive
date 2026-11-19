use super::*;
use serde_json::json;

const TIMESTAMP: &str = "2026-01-01T00:00:00.123456Z";
const NOW: i64 = 1_767_225_780;
const UUID: &str = "aabbccdd-1234-4321-1234-abcdefabcdef";

#[test]
fn issue_2537_receipt_policies_preserve_only_the_named_fields() {
    let append = json!({
        "id": UUID, "created_at": TIMESTAMP, "updated_at": TIMESTAMP,
        "score": 0.123456, "empty": [], "null": null,
        "details": {"created_at": TIMESTAMP},
        "ticker": {"last_tick_at": TIMESTAMP},
    });
    let batch = json!({
        "id": UUID, "created_at": TIMESTAMP, "updated_at": TIMESTAMP,
        "results": [{
            "id": UUID, "created_at": TIMESTAMP, "updated_at": TIMESTAMP,
            "details": {"created_at": TIMESTAMP, "updated_at": TIMESTAMP},
            "record": {"updated_at": TIMESTAMP},
            "score": 0.123456, "empty": [], "null": null,
        }],
        "properties": {"created_at": TIMESTAMP, "id": UUID, "empty": ""},
        "entry": {"seq": 1, "id": UUID, "created_at": TIMESTAMP,
            "record": {"id": UUID, "updated_at": TIMESTAMP, "empty": [], "null": null}},
        "trigger_at": TIMESTAMP, "due": TIMESTAMP,
        "ticker": {"last_tick_at": TIMESTAMP},
    });
    for (value, policy) in [
        (append, VerbPresentationPolicy::StreamAppendReceipt),
        (batch, VerbPresentationPolicy::StreamBatchReceipts),
    ] {
        let mut expected = present(value.clone(), PresentationMode::Agent, NOW);
        assert_eq!(expected["created_at"], TIMESTAMP);
        assert_eq!(expected["updated_at"], TIMESTAMP);
        assert_eq!(expected["id"], "aabbccdd");
        assert_eq!(expected["ticker"]["last_tick_at"], TIMESTAMP);
        if policy == VerbPresentationPolicy::StreamAppendReceipt {
            expected["created_at"] = json!(TIMESTAMP);
        } else {
            expected["results"][0]["created_at"] = json!(TIMESTAMP);
            expected["results"][0]["updated_at"] = json!(TIMESTAMP);
            expected["results"][0]
                .as_object_mut()
                .unwrap()
                .remove("created_at_relative");
        }
        assert_eq!(
            present_with_policy(value.clone(), PresentationMode::Agent, NOW, policy),
            expected
        );
        for mode in [PresentationMode::Verbose, PresentationMode::Human] {
            assert_eq!(present_with_policy(value.clone(), mode, NOW, policy), value);
        }
        assert_eq!(
            present_with_policy(
                value.clone(),
                PresentationMode::Agent,
                NOW,
                VerbPresentationPolicy::AlwaysVerbose
            ),
            value
        );
    }
}

#[test]
fn issue_2537_receipt_paths_do_not_match_other_containers_or_descendants() {
    let row = json!({"created_at": TIMESTAMP, "updated_at": TIMESTAMP});
    for (policy, cases) in [
        (
            VerbPresentationPolicy::StreamAppendReceipt,
            vec![
                json!([row.clone()]),
                json!({"details": row.clone()}),
                json!({"results": [row.clone()]}),
                json!({"created_at": {"created_at": TIMESTAMP}}),
            ],
        ),
        (
            VerbPresentationPolicy::StreamBatchReceipts,
            vec![
                json!([{"results": [row.clone()]}]),
                json!({"results": row.clone()}),
                json!({"results": [[row.clone()]]}),
                json!({"details": {"results": [row.clone()]}}),
                json!({"results": [{"details": row.clone(), "record": row.clone(), "results": [row.clone()]}]}),
                json!({"results": [{"created_at": {"updated_at": TIMESTAMP}, "updated_at": null}]}),
            ],
        ),
    ] {
        for value in cases {
            assert_eq!(
                present_with_policy(value.clone(), PresentationMode::Agent, NOW, policy),
                present(value.clone(), PresentationMode::Agent, NOW),
                "container must not acquire receipt protection: {value}"
            );
        }
    }
}

#[test]
fn issue_2537_receipt_strings_are_preserved_without_parsing() {
    for text in ["", "not a timestamp", "2026-01-01T05:30:00.123456+05:30"] {
        let append = json!({"created_at": text});
        assert_eq!(
            present_with_policy(
                append.clone(),
                PresentationMode::Agent,
                NOW,
                VerbPresentationPolicy::StreamAppendReceipt
            ),
            append
        );
        let batch = json!({"results": [{"created_at": text, "updated_at": text}]});
        assert_eq!(
            present_with_policy(
                batch.clone(),
                PresentationMode::Agent,
                NOW,
                VerbPresentationPolicy::StreamBatchReceipts
            ),
            batch
        );
    }
    for non_string in [Value::Null, json!([]), json!({}), json!(42), json!(true)] {
        for (value, policy) in [
            (
                json!({"created_at": non_string}),
                VerbPresentationPolicy::StreamAppendReceipt,
            ),
            (
                json!({"results": [{"created_at": non_string, "updated_at": non_string}]}),
                VerbPresentationPolicy::StreamBatchReceipts,
            ),
        ] {
            assert_eq!(
                present_with_policy(value.clone(), PresentationMode::Agent, NOW, policy),
                present(value, PresentationMode::Agent, NOW)
            );
        }
    }
}
