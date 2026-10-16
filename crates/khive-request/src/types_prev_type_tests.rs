use super::{ArgValue, PrevFailure};
use serde_json::json;

#[test]
fn prev_type_errors_name_numbers_for_field_and_index_segments() {
    for (path, previous, prefix, segment, expected) in [
        ("user.id", json!({"user": 7}), "$prev.user", "id", "object"),
        (
            "items[0]",
            json!({"items": 7}),
            "$prev.items",
            "[0]",
            "array",
        ),
    ] {
        let argument = ArgValue::PrevRef { path: path.into() };
        let failure = argument
            .find_prev_failure(&previous)
            .expect("a number cannot be traversed");
        assert_eq!(
            failure,
            PrevFailure::WrongType {
                arg_path: "".into(),
                prev_path: format!("$prev.{path}"),
                resolved_prefix: prefix.into(),
                segment: segment.into(),
                expected,
                found: "number",
            }
        );
        assert_eq!(
                failure.to_string(),
                format!(
                    "$prev.{path}: {prefix} is a number, not an {expected}, so {segment:?} cannot be applied to it"
                )
            );
    }
}
