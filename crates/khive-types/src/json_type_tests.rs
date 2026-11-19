use super::json_type_name;
use serde_json::json;

#[test]
fn json_type_names_cover_all_six_variants() {
    for (value, expected) in [
        (json!(null), "null"),
        (json!(true), "boolean"),
        (json!(7), "number"),
        (json!(-1.25), "number"),
        (json!("text"), "string"),
        (json!([]), "array"),
        (json!({}), "object"),
    ] {
        assert_eq!(json_type_name(&value), expected);
    }
}
