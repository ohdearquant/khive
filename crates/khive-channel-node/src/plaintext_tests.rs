use crate::plaintext::{
    classify_plaintext, InvalidPlaintextReason, PlaintextClassification, PlaintextKind,
};
use serde_json::json;

const VALID: &str = r#"{"v":1,"subject":null,"body":"message","sent_at":"2026-09-23T20:00:00Z","thread_id":null,"in_reply_to":null}"#;

fn with_extension(member: &str) -> String {
    format!("{},{member}}}", VALID.trim_end_matches('}'))
}

#[test]
fn reserved_identity_null_kind_and_duplicate_members_are_invalid() {
    for (input, expected) in [
        (
            with_extension(r#""from":"x""#),
            InvalidPlaintextReason::ReservedIdentityMember,
        ),
        (
            with_extension(r#""kind":null"#),
            InvalidPlaintextReason::InvalidKind,
        ),
        (
            with_extension(r#""x":1,"x":2"#),
            InvalidPlaintextReason::DuplicateMember,
        ),
    ] {
        assert_eq!(
            classify_plaintext(input.as_bytes()),
            PlaintextClassification::Invalid(expected),
            "{input}"
        );
    }
}

#[test]
fn nested_duplicate_extensions_stay_invalid_and_unique_extensions_stay_ignored() {
    for member in [
        r#""extension":{"x":1,"x":2}"#,
        r#""extension":[{"nested":{"x":1,"x":2}}]"#,
        r#""extension":{"nested":{"x":1,"x":2}}"#,
    ] {
        assert_eq!(
            classify_plaintext(with_extension(member).as_bytes()),
            PlaintextClassification::Invalid(InvalidPlaintextReason::DuplicateMember)
        );
    }
    let unique = with_extension(r#""extension":{"nested":[{"x":1},{"x":2}]}"#);
    let PlaintextClassification::Valid(parsed) = classify_plaintext(unique.as_bytes()) else {
        panic!("distinct extension object members must remain accepted");
    };
    assert_eq!(parsed.body, "message");
    assert_eq!(parsed.subject, None);
    assert_eq!(parsed.kind, None);
    let written = serde_json::to_value(parsed).unwrap();
    assert!(written.get("extension").is_none());
    assert!(written.get("kind").is_none());
}

#[test]
fn plaintext_required_nullable_fields_and_kind_values_are_preserved() {
    for field in ["subject", "thread_id", "in_reply_to"] {
        let mut value: serde_json::Value = serde_json::from_str(VALID).unwrap();
        value.as_object_mut().unwrap().remove(field);
        assert_eq!(
            classify_plaintext(&serde_json::to_vec(&value).unwrap()),
            PlaintextClassification::Invalid(InvalidPlaintextReason::NotObject),
            "{field}"
        );
    }
    for (wire_kind, expected) in [
        ("announce", PlaintextKind::Announce),
        ("report", PlaintextKind::Report),
        ("ask", PlaintextKind::Ask),
    ] {
        let raw = with_extension(&format!("\"kind\":\"{wire_kind}\""));
        let PlaintextClassification::Valid(parsed) = classify_plaintext(raw.as_bytes()) else {
            panic!("declared kind {wire_kind} must remain accepted");
        };
        assert_eq!(parsed.kind, Some(expected));
        assert_eq!(serde_json::to_value(parsed).unwrap()["kind"], wire_kind);
    }
    for kind in [
        json!("reply"),
        json!("unspecified"),
        json!({"ask":null}),
        json!(1),
    ] {
        let raw = with_extension(&format!("\"kind\":{kind}"));
        assert_eq!(
            classify_plaintext(raw.as_bytes()),
            PlaintextClassification::Invalid(InvalidPlaintextReason::InvalidKind)
        );
    }
}

#[test]
fn plaintext_sent_at_uses_the_strict_utc_profile() {
    for sent_at in ["2026-09-23T20:00:00Z", "2026-09-23T20:00:00+00:00"] {
        let mut raw: serde_json::Value = serde_json::from_str(VALID).unwrap();
        raw["sent_at"] = json!(sent_at);
        let PlaintextClassification::Valid(parsed) =
            classify_plaintext(&serde_json::to_vec(&raw).unwrap())
        else {
            panic!("UTC spelling {sent_at} must remain accepted");
        };
        assert_eq!(
            serde_json::to_value(parsed).unwrap()["sent_at"],
            "2026-09-23T20:00:00Z"
        );
    }
    for sent_at in [
        "2026-09-23T20:00:00+01:00",
        "2026-09-23 20:00:00Z",
        "2026-09-23t20:00:00Z",
        "2026-09-23T20:00:00z",
        "2026-09-23T20:00:00-00:00",
        "2026-09-23T23:59:60Z",
        "2026-09-23T20:00:00.1234567890Z",
    ] {
        let mut raw: serde_json::Value = serde_json::from_str(VALID).unwrap();
        raw["sent_at"] = json!(sent_at);
        assert_eq!(
            classify_plaintext(&serde_json::to_vec(&raw).unwrap()),
            PlaintextClassification::Invalid(InvalidPlaintextReason::NotObject),
            "{sent_at}"
        );
    }
}
