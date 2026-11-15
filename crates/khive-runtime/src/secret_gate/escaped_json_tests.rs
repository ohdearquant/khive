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

#[test]
fn an_escaped_credential_in_a_fragment_inside_prose_is_refused() {
    for (name, _, escaped) in cases() {
        for content in [
            format!("config was {}", json_text(&escaped)),
            format!("{} was the config", json_text(&escaped)),
            format!("we saw \"{escaped}\" in the log"),
        ] {
            assert!(scan(&content).is_none(), "{name}: control: {content}");
            assert!(check(&content).is_err(), "{name}: {content}");
        }
    }
}

#[test]
fn escaped_credentials_after_stray_prose_backslashes_are_refused() {
    for (name, plain, escaped) in cases() {
        let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
            panic!("{name}: plain credential control must refuse");
        };
        for content in [
            format!(r#"we saw \"{escaped}" in the log"#),
            format!(r#"we saw \\"{escaped}" in the log"#),
            format!(r#"we saw \\\"{escaped}" in the log"#),
            format!(r#"she said "hello" then \"{escaped}" in the log"#),
            format!(r#"an earlier \"C:\Users\docs then \"{escaped}" in the log"#),
            format!(r#"an earlier \"\uD800 then \"{escaped}" in the log"#),
            format!(r#"we saw \"{escaped} and \"quoted\" text" in the log"#),
            format!(r#"we saw \"quoted\" text and {escaped}" in the log"#),
        ] {
            assert!(scan(&content).is_none(), "{name}: raw control: {content}");
            let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
                panic!("{name}: escaped credential must refuse: {content}");
            };
            assert_eq!(actual.detector, expected.detector, "{name}: {content}");
        }
    }
}

#[test]
fn unclosed_escaped_credentials_are_refused_at_end_of_field() {
    for (name, plain, escaped) in cases() {
        let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
            panic!("{name}: plain credential control must refuse");
        };
        for content in [
            format!(r#"she said "hi then \"{escaped} in the log"#),
            format!(r#"we saw \"{escaped} in the log"#),
            format!(r#""{escaped}"#),
        ] {
            assert!(scan(&content).is_none(), "{name}: raw control: {content}");
            let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
                panic!("{name}: unclosed escaped credential must refuse: {content}");
            };
            assert_eq!(actual.detector, expected.detector, "{name}: {content}");
            let Err(RuntimeError::SecretDetected(located)) =
                check_note_fields_at(&content, None, None, "note")
            else {
                panic!("{name}: note content must refuse");
            };
            assert_eq!(located.detector, expected.detector);
            assert_eq!(located.location.as_deref(), Some("note.content"));
        }
    }
}

#[test]
fn benign_unclosed_text_and_invalid_escape_tails_keep_their_verdict() {
    for content in [
        r#"she said "hello"#,
        r#"she said "hello then \"goodbye"#,
        r#"we saw \"docs\/guide.md"#,
        r#""line\none and \u00e9"#,
        r#""a final escaped backslash \\"#,
    ] {
        assert!(scan(content).is_none(), "raw control: {content}");
        assert!(check(content).is_ok(), "{content}");
    }
    for content in [
        r#""C:\Users\docs"#,
        r#""invalid \q"#,
        r#""short unicode \u004"#,
        r#""unpaired surrogate \uD800"#,
        "\"dangling backslash \\",
        "\"literal newline\nthen \\n",
    ] {
        if content == "\"literal newline\nthen \\n" {
            assert_eq!(
                super::escaped_json::escaped_strings(content),
                vec!["then \n"],
                "{content}"
            );
        } else {
            assert!(
                super::escaped_json::escaped_strings(content).is_empty(),
                "{content}"
            );
        }
        assert!(check(content).is_ok(), "{content}");
    }
}

#[test]
fn a_stray_prose_backslash_does_not_make_invalid_json_escapes_decodable() {
    for (name, plain, escaped) in cases() {
        let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
            panic!("{name}: plain credential control must refuse");
        };
        for suffix in [r"\q", r"\uD800", r"\uDC00", "\n"] {
            let content = format!(r#"we saw \"{escaped}{suffix}" in the log"#);
            assert!(scan(&content).is_none(), "{name}: raw control: {content}");
            assert_eq!(
                super::escaped_json::escaped_strings(&content),
                vec![plain.clone()],
                "{name}: {content}"
            );
            let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
                panic!("{name}: valid credential prefix must refuse: {content}");
            };
            assert_eq!(actual.detector, expected.detector, "{name}: {content}");
        }
    }
}

#[test]
fn prose_quotes_and_backslash_paths_keep_their_verdict() {
    for content in [
        r#"she said "hello" and "goodbye" then left"#,
        r#"open C:\Users\me\docs and "C:\Program Files\app""#,
        r#"a 5" screen and a "quoted \" phrase" here"#,
        r#"see "docs\/guide.md" and "line\none""#,
        r#"a lone \" quote and \\ backslashes"#,
        r#"we saw \"docs\/guide.md" in the log"#,
        r#"an earlier \"bad\q then \"line\none" in the log"#,
        r#"we saw \"music \uD834\uDD1E and \"quoted\" text" in the log"#,
    ] {
        assert!(check(content).is_ok(), "{content}");
    }
}

#[test]
fn each_quoted_run_is_decoded_once() {
    let content = "\"a\\nb\" ".repeat(50_000) + "\"x\\\" ";
    let decoded = super::escaped_json::escaped_strings(&content);
    // The final valid unfinished run now contributes its own decoded value.
    assert_eq!(decoded.len(), 50_001);
    assert_eq!(decoded.last().map(String::as_str), Some("x\" "));
    assert!(check(&content).is_ok());
}

#[test]
fn many_quoted_runs_do_not_hide_an_unclosed_escaped_credential() {
    let prefix = "\"a\\nb\" ".repeat(50_000) + &r#"\"bad\q "#.repeat(50_000);
    for (name, plain, escaped) in cases() {
        let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
            panic!("{name}: plain credential control must refuse");
        };
        for (valid_runs, malformed_runs, mut expected_views) in [
            (1, 1, vec!["a\nb", " \"bad", " \""]),
            (1, 2, vec!["a\nb", " \"bad", " \"", " \""]),
            (2, 1, vec!["a\nb", "a\nb", " \"bad", " \""]),
            (2, 2, vec!["a\nb", "a\nb", " \"bad", " \"", " \""]),
        ] {
            let small = "\"a\\nb\" ".repeat(valid_runs)
                + &r#"\"bad\q "#.repeat(malformed_runs)
                + &format!("\\\"{escaped}");
            expected_views.push(&plain);
            assert_eq!(
                super::escaped_json::escaped_strings(&small),
                expected_views,
                "{name}: {valid_runs} valid / {malformed_runs} malformed runs"
            );
        }
        let content = format!("{prefix}\\\"{escaped}");
        assert!(scan(&content).is_none(), "{name}: raw control");
        let decoded = super::escaped_json::escaped_strings(&content);
        // N valid runs + M recovered quote spans + the first boundary span
        // and the final credential, for N >= 1 and M >= 1.
        assert_eq!(decoded.len(), 50_000 + 50_000 + 2, "{name}: N + M + 2");
        assert_eq!(decoded.last(), Some(&plain));
        let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
            panic!("{name}: final escaped credential must refuse");
        };
        assert_eq!(actual.detector, expected.detector);
    }
}

#[test]
fn many_escaped_quotes_in_one_unclosed_run_are_decoded_once() {
    let prefix = "\"intro ".to_owned() + &r#"\"quoted\" "#.repeat(50_000);
    for (name, plain, escaped) in cases() {
        let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
            panic!("{name}: plain credential control must refuse");
        };
        let content = format!("{prefix}{escaped}");
        assert!(scan(&content).is_none(), "{name}: raw control");
        let decoded = super::escaped_json::escaped_strings(&content);
        assert_eq!(
            decoded.len(),
            1,
            "{name}: one candidate across escaped quotes"
        );
        assert!(decoded[0].ends_with(&plain));
        let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
            panic!("{name}: unclosed escaped credential must refuse");
        };
        assert_eq!(actual.detector, expected.detector);
    }
}

#[test]
fn many_malformed_prose_quotes_do_not_hide_a_later_escaped_credential() {
    let (_, plain, escaped) = cases().remove(0);
    let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
        panic!("plain credential control must refuse");
    };
    let prefix = r#"\"bad\q "#.repeat(50_000);
    let content = format!("{prefix}\\\"{escaped}\"");
    assert!(scan(&content).is_none(), "raw control");
    let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
        panic!("escaped credential must refuse");
    };
    assert_eq!(actual.detector, expected.detector);
}

#[test]
fn invalid_escapes_preserve_credential_spans_before_and_after() {
    for (name, plain, escaped) in cases() {
        let Err(RuntimeError::SecretDetected(expected)) = check(&plain) else {
            panic!("{name}: plain credential control must refuse");
        };
        for (invalid, remainder) in [
            (r"\q", ""),
            (r"\u00", ""),
            (r"\u00Z", "Z"),
            (r"\uD800", ""),
            (r"\uDC00", ""),
            (r"\é", ""),
            ("\n", ""),
            ("\r", ""),
            ("\t", ""),
            ("\0", ""),
        ] {
            for (content, decoded) in [
                (format!("\"{escaped} {invalid}\""), format!("{plain} ")),
                (
                    format!("\"{invalid} {escaped}\""),
                    format!("{remainder} {plain}"),
                ),
                (format!("\"{escaped} {invalid}"), format!("{plain} ")),
                (
                    format!("\"{invalid} {escaped}"),
                    format!("{remainder} {plain}"),
                ),
            ] {
                assert!(scan(&content).is_none(), "{name}: raw control: {content:?}");
                let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
                    panic!("{name}: recovered credential must refuse: {content:?}");
                };
                assert_eq!(actual.detector, expected.detector, "{name}: {content:?}");
                let Err(RuntimeError::SecretDetected(located)) =
                    check_note_fields_at(&content, None, None, "note")
                else {
                    panic!("{name}: recovered note content must refuse");
                };
                assert_eq!(located.detector, expected.detector);
                assert_eq!(located.location.as_deref(), Some("note.content"));
                assert_eq!(
                    super::escaped_json::escaped_strings(&content),
                    vec![decoded],
                    "{name}: maximal valid span: {content:?}"
                );
            }
        }
        let content = format!("\"{escaped}\\");
        assert!(
            scan(&content).is_none(),
            "{name}: dangling escape raw control"
        );
        let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
            panic!("{name}: credential before dangling escape must refuse");
        };
        assert_eq!(actual.detector, expected.detector);
        assert_eq!(
            super::escaped_json::escaped_strings(&content),
            vec![plain.clone()]
        );
        for levels in [1, 2] {
            let content = wrap(levels, format!("\"{escaped}\\q\""));
            assert!(scan(&content).is_none(), "{name}: nested raw control");
            let Err(RuntimeError::SecretDetected(actual)) = check(&content) else {
                panic!("{name}: nested malformed candidate must refuse");
            };
            assert_eq!(actual.detector, expected.detector);
            let Err(RuntimeError::SecretDetected(located)) =
                check_note_fields_at(&content, None, None, "note")
            else {
                panic!("{name}: nested note content must refuse");
            };
            assert_eq!(located.detector, expected.detector);
            assert_eq!(located.location.as_deref(), Some("note.content"));
        }
    }
}

#[test]
fn invalid_escape_boundaries_do_not_join_decoded_spans() {
    for (name, plain, _) in cases() {
        assert!(
            check(&plain).is_err(),
            "{name}: complete credential control"
        );
        let (left, right) = plain.split_at(plain.len() / 2);
        let encode = |part: &str| {
            part.chars()
                .map(|c| format!("\\u{:04x}", u32::from(c)))
                .collect::<String>()
        };
        for invalid in [r"\q", r"\uD800", r"\uDC00", "\n"] {
            let content = format!("\"{}{invalid}{}\"", encode(left), encode(right));
            assert!(scan(&content).is_none(), "{name}: raw control");
            assert_eq!(
                super::escaped_json::escaped_strings(&content),
                vec![left, right],
                "{name}: {invalid:?} must separate the spans"
            );
            assert!(check(&content).is_ok(), "{name}: spans must not join");
            assert!(check_note_fields_at(&content, None, None, "note").is_ok());
        }
    }
    for (content, expected) in [
        (r#""é\n\q雪\t""#, vec!["é\n", "雪\t"]),
        (r#""a\uD834\uDD1E\q\u00e9""#, vec!["a𝄞", "é"]),
        (r#""\uD800\u0041""#, vec!["A"]),
        (r#""\uD800\q\uDC00""#, vec![]),
        (r#""\u00é\n""#, vec!["é\n"]),
        (r#""\é雪\n""#, vec!["雪\n"]),
        (r#""\uD800\uD834\uDD1E""#, vec!["𝄞"]),
    ] {
        assert_eq!(super::escaped_json::escaped_strings(content), expected);
        assert!(check(content).is_ok(), "benign strict span: {content}");
    }
}

#[test]
fn many_invalid_spans_are_processed_once() {
    let repetitions = 50_000;
    let content = format!("\"{}\"", r"\q\n".repeat(repetitions));
    let (decoded, work) = super::escaped_json::escaped_strings_with_work(&content, 2 * repetitions);
    assert_eq!(work.passes, 1, "one candidate requires one recovery pass");
    assert_eq!(work.tokens, 100_000, "each token is classified once");
    assert_eq!(decoded.len(), repetitions);
    assert!(decoded.iter().all(|span| span == "\n"));
    assert!(
        check(&content).is_ok(),
        "separate newline spans stay benign"
    );
    for (repetitions, expected) in [(1, vec!["\n"]), (2, vec!["\n", "\n"])] {
        let content = format!("\"{}\"", r"\q\n".repeat(repetitions));
        let (decoded, work) =
            super::escaped_json::escaped_strings_with_work(&content, 2 * repetitions);
        assert_eq!(decoded, expected);
        assert_eq!(work.passes, 1);
        assert_eq!(work.tokens, 2 * repetitions);
    }
}
