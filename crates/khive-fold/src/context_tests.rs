use super::*;

#[test]
fn test_context_at_time() {
    let past = Utc::now() - chrono::Duration::hours(1);
    let ctx = FoldContext::at(past);
    assert_eq!(ctx.as_of, past);
}

#[test]
fn test_context_new_is_epoch() {
    let ctx = FoldContext::new();
    assert_eq!(ctx.as_of, DateTime::<Utc>::default());
}

#[cfg(feature = "serde")]
#[test]
fn test_shared_json_round_trip() {
    let ctx = FoldContext::new().with_extra(serde_json::json!({"count": 3, "flag": true}));
    let encoded = serde_json::to_string(&ctx).unwrap();
    let decoded: FoldContext = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded.extra, serde_json::json!({"count": 3, "flag": true}));
}

#[test]
fn test_shared_json_make_mut() {
    let mut ctx = FoldContext::new().with_extra(serde_json::json!({"count": 1}));
    let _clone = ctx.clone();
    *ctx.extra_mut() = serde_json::json!({"count": 2});
    assert_eq!(ctx.extra, serde_json::json!({"count": 2}));
}

#[test]
fn test_shared_json_clone_is_cheap_arc_refcount() {
    let value = serde_json::json!({"large": "payload", "nested": {"a": 1, "b": 2}});
    let shared = SharedJson::new(value.clone());
    let clone = shared.clone();
    assert_eq!(
        shared.as_value() as *const _,
        clone.as_value() as *const _,
        "clone should share the same Arc allocation"
    );
    assert_eq!(*shared, *clone);
    drop(clone);
    let extracted = shared.into_inner();
    assert_eq!(extracted, value);
}

#[test]
fn test_shared_json_extra_mut_creates_independent_copy() {
    let original = FoldContext::new().with_extra(serde_json::json!({"x": 1}));
    let mut mutated = original.clone();
    assert_eq!(original.extra(), mutated.extra());
    *mutated.extra_mut() = serde_json::json!({"x": 99});
    assert_eq!(original.extra(), &serde_json::json!({"x": 1}));
    assert_eq!(mutated.extra(), &serde_json::json!({"x": 99}));
}

#[test]
fn test_shared_json_from_value_transparent() {
    let value = serde_json::json!([1, 2, 3]);
    let shared: SharedJson = value.clone().into();
    assert_eq!(shared.as_value(), &value);
}

#[test]
fn test_shared_json_default_is_null() {
    let default = SharedJson::default();
    assert_eq!(default.as_value(), &serde_json::Value::Null);
}
