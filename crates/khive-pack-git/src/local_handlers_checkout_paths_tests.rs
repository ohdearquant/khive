//! `git.checkout`'s `paths` checks, which run before git (ADR-182 Amendment 24, item 1).

use serde_json::{json, Value};

use super::{checkout_paths, Failure};

fn refusal(paths: &Value) -> Failure {
    match checkout_paths(&json!({ "paths": paths })) {
        Ok(found) => panic!("{paths} was accepted as {found:?}"),
        Err(failure) => failure,
    }
}

fn accepted(paths: &Value) -> Option<Vec<String>> {
    match checkout_paths(&json!({ "paths": paths })) {
        Ok(found) => found,
        Err(failure) => panic!("{paths} refused {}: {:?}", failure.reason, failure.detail),
    }
}

#[test]
fn named_paths_come_back_sorted_and_an_absent_list_is_none() {
    assert!(matches!(
        checkout_paths(&json!({ "ref": "HEAD" })),
        Ok(None)
    ));
    assert_eq!(
        accepted(&json!(["docs", "a.txt", "docs-index.md"])),
        Some(vec![
            "a.txt".to_string(),
            "docs".to_string(),
            "docs-index.md".to_string()
        ])
    );
}

#[test]
fn a_malformed_path_refuses_invalid_path() {
    for path in [
        "/a.txt",
        "",
        "docs//one.md",
        "./a.txt",
        "docs/./one.md",
        "../a.txt",
        "sub/..",
        "sub/",
        "a\0b",
    ] {
        let failure = refusal(&json!(["a.txt", path]));
        assert_eq!(failure.reason, "invalid_path", "{path:?}");
        assert!(
            failure
                .detail
                .as_deref()
                .is_some_and(|detail| !detail.is_empty()),
            "{path:?}"
        );
    }
}

#[test]
fn a_path_inside_another_or_named_twice_refuses_overlapping_paths() {
    for (paths, detail) in [
        (
            json!(["docs", "docs/one.md"]),
            "\"docs/one.md\" lies inside \"docs\"",
        ),
        (
            json!(["docs/sub/two.md", "a.txt", "docs"]),
            "\"docs/sub/two.md\" lies inside \"docs\"",
        ),
        (json!(["a.txt", "a.txt"]), "\"a.txt\" repeats \"a.txt\""),
    ] {
        let failure = refusal(&paths);
        assert_eq!(failure.reason, "overlapping_paths", "{paths}");
        assert_eq!(failure.detail.as_deref(), Some(detail), "{paths}");
    }
    // Control: a name that begins with another's lies beside it, not inside it.
    assert_eq!(
        accepted(&json!(["docs", "docs-index.md", "docs.md", "docsx/a"])).map(|paths| paths.len()),
        Some(4)
    );
}

#[test]
fn the_count_and_the_element_type_refuse_invalid_params() {
    let names = |count: usize| {
        (0..count)
            .map(|index| format!("f{index}"))
            .collect::<Vec<_>>()
    };
    for paths in [json!([]), json!("a.txt"), json!([1]), json!(names(65))] {
        assert_eq!(refusal(&paths).reason, "invalid_params", "{paths}");
    }
    assert_eq!(
        accepted(&json!(names(64))).map(|paths| paths.len()),
        Some(64)
    );
}
