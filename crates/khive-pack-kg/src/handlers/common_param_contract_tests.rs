use super::*;

#[test]
fn an_object_parameter_refuses_every_non_object_and_names_what_it_got() {
    // The shape that was accepted and persisted.
    let err = require_object_param(Some(&json!("not-an-object")), "properties")
        .expect_err("a string is not an object");
    let message = err.to_string();
    assert!(message.contains("properties"), "{message}");
    assert!(message.contains("string"), "names what arrived: {message}");

    // An array is the near miss a caller is most likely to send next, so it
    // must be refused by type rather than by a map-specific probe.
    assert!(require_object_param(Some(&json!([1, 2])), "properties").is_err());
    let err =
        require_object_param(Some(&json!(7)), "properties").expect_err("a number is not an object");
    assert_eq!(
        err.to_string(),
        "invalid input: properties must be an object; got number"
    );
    assert!(require_object_param(Some(&json!(true)), "properties").is_err());
}

#[test]
fn absent_and_explicit_null_are_not_type_errors() {
    // Absent means unchanged and null is how the update path clears a field;
    // an implementation that refuses anything that is not an object breaks both.
    require_object_param(None, "properties").expect("absent is allowed");
    require_object_param(Some(&Value::Null), "properties").expect("null is allowed");
    require_object_param(Some(&json!({"a": 1})), "properties").expect("an object is allowed");
}
