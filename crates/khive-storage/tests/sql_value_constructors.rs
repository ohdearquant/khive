use khive_storage::types::SqlValue;

#[test]
fn from_opt_text_wraps_some_as_text() {
    let value = SqlValue::from_opt_text(Some("x"));
    assert!(
        matches!(&value, SqlValue::Text(text) if text == "x"),
        "{value:?}"
    );
}

#[test]
fn from_opt_text_keeps_the_empty_string_as_text() {
    let value = SqlValue::from_opt_text(Some(""));
    assert!(
        matches!(&value, SqlValue::Text(text) if text.is_empty()),
        "{value:?}"
    );
}

#[test]
fn from_opt_text_maps_none_to_null() {
    let value = SqlValue::from_opt_text(None);
    assert!(matches!(value, SqlValue::Null), "{value:?}");
}

#[test]
fn from_opt_i64_wraps_zero_as_integer() {
    let value = SqlValue::from_opt_i64(Some(0));
    assert!(matches!(value, SqlValue::Integer(0)), "{value:?}");
}

#[test]
fn from_opt_i64_maps_none_to_null() {
    let value = SqlValue::from_opt_i64(None);
    assert!(matches!(value, SqlValue::Null), "{value:?}");
}
