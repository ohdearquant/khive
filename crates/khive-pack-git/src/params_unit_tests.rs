use super::parse;
use serde_json::json;

#[test]
fn known_values_keep_null_presence_and_existing_semantic_validation() {
    for (verb, params) in [
        (
            "git.digest",
            json!({"source":"repo","project":false,"max_items":null,"include":null}),
        ),
        ("git.digest", json!({"source":"repo","max_items":-1})),
        ("git.digest", json!({"source":"repo","max_items":9000})),
        (
            "git.commit",
            json!({"repo":"repo","message":"message","paths":null,"author":null}),
        ),
        (
            "git.commit",
            json!({"repo":"repo","message":"message","tree":null}),
        ),
        ("git.push", json!({"repo":"repo","expected_remote":null})),
        ("git.push", json!({"repo":"repo"})),
        ("git.log", json!({"repo":"repo","ref":"HEAD","limit":null})),
    ] {
        assert_eq!(
            parse(verb, params.clone()).unwrap(),
            params,
            "SUPPLIED_VALUES_PRESERVED: {verb}"
        );
    }
}

#[test]
fn positional_arguments_are_not_named_objects() {
    for verb in ["git.digest", "git.commit", "git.init", "git.push"] {
        let error =
            parse(verb, json!([])).expect_err("NAMED_ARGUMENTS_ONLY: positional array accepted");
        assert!(
            error.to_string().contains("object with named fields"),
            "NAMED_ARGUMENTS_ONLY: {error}"
        );
    }
}
