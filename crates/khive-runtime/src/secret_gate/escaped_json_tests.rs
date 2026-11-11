use super::{check, check_json, check_note_fields_at, scan};
use crate::RuntimeError;
use serde_json::json;

fn aws() -> String {
    ["AK", "IA", "QWERTYUIOPASDFGH"].concat()
}

fn github() -> String {
    ["gh", "p_", "aB3dE5gH7jK9mN1pQ3sT5vW7yZ9bC1dF3hJ5"].concat()
}

fn url() -> String {
    ["https://user:", "Zx9", "Qw7Er5Ty3Ui1", "@example.com/x"].concat()
}

/// (name, plain credential, the same credential with one JSON escape)
fn cases() -> Vec<(&'static str, String, String)> {
    let aws = aws();
    let github = github();
    let url = url();
    vec![
        ("aws", aws.clone(), format!("\\u0041{}", &aws[1..])),
        ("github", github.clone(), format!("\\u0067{}", &github[1..])),
        ("url", url.clone(), url.replace('/', "\\/")),
    ]
}

fn json_text(inner: &str) -> String {
    format!("{{\"v\": \"{inner}\"}}")
}

#[test]
fn escaped_credentials_in_json_text_were_invisible_to_the_raw_scan() {
    for (name, _, escaped) in cases() {
        assert!(
            scan(&json_text(&escaped)).is_none(),
            "{name}: the raw-text scan is the control and must read the escaped form clean"
        );
    }
}

#[test]
fn json_text_with_an_escaped_credential_is_refused() {
    for (name, _, escaped) in cases() {
        let content = json_text(&escaped);
        assert!(
            matches!(check(&content), Err(RuntimeError::SecretDetected(_))),
            "{name}: {content}"
        );
    }
}

#[test]
fn json_text_with_a_plain_credential_is_still_refused() {
    for (name, plain, _) in cases() {
        assert!(check(&json_text(&plain)).is_err(), "{name}");
        assert!(check(&plain).is_err(), "{name}");
    }
}

#[test]
fn an_escaped_credential_in_a_key_or_nested_array_is_refused() {
    let (_, _, escaped) = cases().remove(0);
    for content in [
        format!("{{\"{escaped}\": 1}}"),
        format!("[1, {{\"a\": [\"x\", \"{escaped}\"]}}]"),
        format!("\"{escaped}\""),
    ] {
        assert!(check(&content).is_err(), "{content}");
    }
}

#[test]
fn json_text_nested_inside_a_json_string_is_decoded_again() {
    let (_, _, escaped) = cases().remove(1);
    let inner = json_text(&escaped);
    let outer = json!({ "body": inner }).to_string();
    assert!(scan(&outer).is_none(), "control: raw scan reads it clean");
    assert!(check(&outer).is_err(), "{outer}");
}

#[test]
fn structured_properties_agree_with_text_content() {
    for (name, _, escaped) in cases() {
        let content = json_text(&escaped);
        let as_value: serde_json::Value = serde_json::from_str(&content).expect("fixture is JSON");
        assert!(check_json(&as_value).is_err(), "{name}: decoded leaf");
        let in_property = json!({ "payload": content });
        assert!(
            check_json(&in_property).is_err(),
            "{name}: JSON text in a property"
        );
        let error = check_note_fields_at(&content, None, None, "note")
            .expect_err("content field must refuse");
        let RuntimeError::SecretDetected(matched) = error else {
            panic!("{name}: expected a secret refusal");
        };
        assert_eq!(matched.location.as_deref(), Some("note.content"));
    }
}

#[test]
fn escaped_text_that_is_not_a_json_document_keeps_its_verdict() {
    for content in [
        "see C:\\\\Users\\\\docs and \"quoted \\\" text\"",
        "{not json \\u0041 at all",
        "plain prose with a \\n newline escape",
    ] {
        assert_eq!(check(content).is_ok(), scan(content).is_none(), "{content}");
    }
}

#[test]
fn benign_json_with_escapes_still_passes() {
    for content in [
        r#"{"path": "docs\/guide.md", "text": "line one\nline two \u00e9"}"#,
        r#"["tab\there", {"quote": "she said \"hi\""}]"#,
        r#"{"url": "https:\/\/example.com\/docs"}"#,
    ] {
        assert!(check(content).is_ok(), "{content}");
    }
}

fn wrap(levels: usize, inner: String) -> String {
    (0..levels).fold(inner, |text, _| json!({ "body": text }).to_string())
}

#[test]
fn credentials_nested_two_and_three_documents_deep_are_refused() {
    for (name, plain, escaped) in cases() {
        for levels in [2, 3] {
            let escaped_content = wrap(levels, json_text(&escaped));
            assert!(scan(&escaped_content).is_none(), "{name}/{levels}: control");
            for content in [wrap(levels, json_text(&plain)), escaped_content] {
                assert!(check(&content).is_err(), "{name}/{levels}: {content}");
            }
        }
    }
}

#[test]
fn nesting_past_the_decode_depth_is_refused_not_passed() {
    let benign = json_text("nothing secret here");
    let within = wrap(super::MAX_ESCAPED_JSON_DEPTH, benign.clone());
    assert!(check(&within).is_ok(), "benign nesting at the cap passes");
    let past = wrap(super::MAX_ESCAPED_JSON_DEPTH + 1, benign);
    let Err(RuntimeError::SecretDetected(matched)) = check(&past) else {
        panic!("nesting past the cap must refuse");
    };
    assert_eq!(matched.detector, "nested-escaped-json");
}
