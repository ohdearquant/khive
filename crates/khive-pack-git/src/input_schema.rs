use serde_json::Value;
use std::sync::LazyLock;

static SCHEMAS: LazyLock<Value> = LazyLock::new(|| {
    serde_json::from_str(include_str!("input_schema.json"))
        .expect("git input schemas are valid JSON")
});

pub(crate) fn for_verb(verb: &str) -> Option<Value> {
    SCHEMAS.get(verb).cloned()
}
