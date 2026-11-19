use super::*;
use serde_json::json;

const NOW: i64 = 1_748_016_480; // 2025-05-23T16:08:00Z

#[test]
fn agent_exact_utc_preserves_fraction_digits_and_instant() {
    for (utc_clock, plus_clock, minus_clock) in [
        (
            "2025-05-23T16:05:00",
            "2025-05-23T20:05:00",
            "2025-05-23T12:05:00",
        ),
        (
            "2020-01-01T10:30:45",
            "2020-01-01T14:30:45",
            "2020-01-01T06:30:45",
        ),
    ] {
        for fraction in ["", ".123", ".123456"] {
            let expected = format!("{utc_clock}{fraction}Z");
            for (clock, offset) in [
                (utc_clock, "Z"),
                (utc_clock, "+00:00"),
                (plus_clock, "+0400"),
                (minus_clock, "-04:00"),
            ] {
                let input = format!("{clock}{fraction}{offset}");
                let shown = present(
                    json!({"updated_at": input.clone()}),
                    PresentationMode::Agent,
                    NOW,
                );
                assert_eq!(shown["updated_at"], expected, "{input}");
                assert_eq!(
                    parse_iso8601_unix(&input),
                    parse_iso8601_unix(&expected),
                    "{input}"
                );
            }
        }
    }
}

#[test]
fn agent_preserves_timestamp_prefix_body() {
    let body = "2025-05-23T16:05:00Z followed by ordinary message text";
    let shown = present(json!({"content": body}), PresentationMode::Agent, NOW);
    assert_eq!(shown["content"], body);
}

#[test]
fn agent_preserves_offsetless_timestamp() {
    let body = "2025-05-23T16:05:00";
    let shown = present(json!({"content": body}), PresentationMode::Agent, NOW);
    assert_eq!(shown["content"], body);
}

#[test]
fn agent_preserves_malformed_offset() {
    let body = "2025-05-23T16:05:00+25:00";
    let shown = present(json!({"content": body}), PresentationMode::Agent, NOW);
    assert_eq!(shown["content"], body);
}

#[test]
fn agent_list_rows_pair_exact_and_relative_time_in_each_band() {
    let bands = [
        (42, "42s ago"),
        (180, "3m ago"),
        (3 * 3600, "3h ago"),
        (2 * 86400, "2d ago"),
    ];
    let rows: Vec<Value> = bands
        .into_iter()
        .map(|(age, _)| {
            let created = chrono::DateTime::<chrono::Utc>::from_timestamp(NOW - age, 0)
                .unwrap()
                .to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
            json!({"created_at": created, "updated_at": created})
        })
        .collect();
    let shown = present(json!({"items": rows.clone()}), PresentationMode::Agent, NOW);
    for (index, (_, relative)) in bands.into_iter().enumerate() {
        assert_eq!(
            shown["items"][index]["created_at"],
            rows[index]["created_at"]
        );
        assert_eq!(
            shown["items"][index]["updated_at"],
            rows[index]["updated_at"]
        );
        assert_eq!(shown["items"][index]["created_at_relative"], relative);
        assert!(shown["items"][index].get("updated_at_relative").is_none());
    }
}

#[test]
fn agent_relative_sibling_obeys_array_properties_existing_and_future_bounds() {
    let recent = "2025-05-23T16:05:00+00:00";
    let future = "2025-05-23T16:08:10Z";
    let shown = present(
        json!({
            "created_at": recent,
            "items": [
                {"created_at": future},
                {"created_at": recent, "created_at_relative": "canonical"},
                {"created_at": "2025-05-23T16:05:00"},
                {"updated_at": recent},
                {"created_at": "2025-05-23T16:08:00.000001Z"},
                {"created_at": "2025-05-23T16:07:59.999999Z"},
                {"created_at": "2025-05-23T16:08:00.000000Z"},
                {"properties": [{"created_at": recent, "empty": ""}]}
            ],
            "properties": {"nested": [{"created_at": recent}]}
        }),
        PresentationMode::Agent,
        NOW,
    );
    assert_eq!(shown["created_at"], "2025-05-23T16:05:00Z");
    assert!(shown.get("created_at_relative").is_none());
    assert_eq!(shown["items"][0]["created_at"], future);
    assert!(shown["items"][0].get("created_at_relative").is_none());
    assert_eq!(shown["items"][1]["created_at_relative"], "canonical");
    assert_eq!(shown["items"][2]["created_at"], "2025-05-23T16:05:00");
    assert!(shown["items"][2].get("created_at_relative").is_none());
    assert!(shown["items"][3].get("created_at_relative").is_none());
    assert!(shown["items"][4].get("created_at_relative").is_none());
    assert_eq!(shown["items"][5]["created_at_relative"], "0s ago");
    assert_eq!(shown["items"][6]["created_at_relative"], "0s ago");
    assert_eq!(shown["items"][7]["properties"][0]["created_at"], recent);
    assert!(shown["items"][7]["properties"][0]
        .get("created_at_relative")
        .is_none());
    assert!(shown["items"][7]["properties"][0].get("empty").is_none());
    assert_eq!(shown["properties"]["nested"][0]["created_at"], recent);
    assert!(shown["properties"]["nested"][0]
        .get("created_at_relative")
        .is_none());
}

#[test]
fn agent_relative_sibling_uses_sampled_fraction_within_same_second() {
    let sampled = chrono::DateTime::parse_from_rfc3339("2025-05-23T16:08:00.500000000Z")
        .unwrap()
        .with_timezone(&chrono::Utc);
    let shown = present_with_policy_at(
        json!({"items": [
            {"created_at": "2025-05-23T16:08:00.499999999Z"},
            {"created_at": "2025-05-23T16:08:00.500000000Z"},
            {"created_at": "2025-05-23T16:08:00.5000000000Z"},
            {"created_at": "2025-05-23T16:08:00.5000000001Z"},
            {"created_at": "2025-05-23T16:08:00.500000001Z"},
            {"created_at": "2025-05-23T16:07:59.500000001Z"},
            {"created_at": "2025-05-23T16:07:59.500000000Z"}
        ]}),
        PresentationMode::Agent,
        sampled.into(),
        VerbPresentationPolicy::Standard,
    );
    for index in [0, 1, 2, 5] {
        assert_eq!(shown["items"][index]["created_at_relative"], "0s ago");
    }
    for index in [3, 4] {
        assert!(shown["items"][index].get("created_at_relative").is_none());
    }
    assert_eq!(shown["items"][6]["created_at_relative"], "1s ago");
}
