#[test]
fn required_text_accepts_text() {
    assert_eq!(
        row(SqlValue::Text("text".into())).text("value").unwrap(),
        "text"
    );
    assert_eq!(
        row(SqlValue::Text(String::new())).text("value").unwrap(),
        ""
    );
}

#[test]
fn required_text_rejects_null() {
    refusal(row(SqlValue::Null).text("value"), Some("Null"));
}
#[test]
fn required_text_rejects_absence() {
    refusal(absent().text("value"), None);
}
#[test]
fn required_text_rejects_other_variants() {
    for (value, name) in variants() {
        if name != "Text" && name != "Null" {
            refusal(row(value).text("value"), Some(name));
        }
    }
}

#[test]
fn required_i64_accepts_integer() {
    for value in [i64::MIN, 0, i64::MAX] {
        assert_eq!(row(SqlValue::Integer(value)).i64("value").unwrap(), value);
    }
}
#[test]
fn required_i64_rejects_null() {
    refusal(row(SqlValue::Null).i64("value"), Some("Null"));
}
#[test]
fn required_i64_rejects_absence() {
    refusal(absent().i64("value"), None);
}
#[test]
fn required_i64_rejects_float() {
    refusal(row(SqlValue::Float(7.5)).i64("value"), Some("Float"));
}
#[test]
fn required_i64_rejects_other_variants() {
    for (value, name) in variants() {
        if !matches!(name, "Integer" | "Null" | "Float") {
            refusal(row(value).i64("value"), Some(name));
        }
    }
}

#[test]
fn strict_optional_text_accepts_text_and_null() {
    assert_eq!(
        row(SqlValue::Text("text".into()))
            .opt_text("value")
            .unwrap(),
        Some("text")
    );
    assert_eq!(row(SqlValue::Null).opt_text("value").unwrap(), None);
}
#[test]
fn strict_optional_text_rejects_absence() {
    refusal(absent().opt_text("value"), None);
}
#[test]
fn strict_optional_text_rejects_other_variants() {
    for (value, name) in variants() {
        if !matches!(name, "Text" | "Null") {
            refusal(row(value).opt_text("value"), Some(name));
        }
    }
}

#[test]
fn strict_optional_i64_accepts_integer_and_null() {
    assert_eq!(row(SqlValue::Integer(7)).opt_i64("value").unwrap(), Some(7));
    assert_eq!(row(SqlValue::Null).opt_i64("value").unwrap(), None);
}
#[test]
fn strict_optional_i64_rejects_absence() {
    refusal(absent().opt_i64("value"), None);
}
#[test]
fn strict_optional_i64_rejects_float() {
    refusal(row(SqlValue::Float(7.5)).opt_i64("value"), Some("Float"));
}
#[test]
fn strict_optional_i64_rejects_other_variants() {
    for (value, name) in variants() {
        if !matches!(name, "Integer" | "Null" | "Float") {
            refusal(row(value).opt_i64("value"), Some(name));
        }
    }
}

#[test]
fn absent_tolerant_text_accepts_text_null_and_absence() {
    assert_eq!(
        row(SqlValue::Text("text".into()))
            .opt_text_or_absent("value")
            .unwrap(),
        Some("text")
    );
    assert_eq!(
        row(SqlValue::Null).opt_text_or_absent("value").unwrap(),
        None
    );
    assert_eq!(absent().opt_text_or_absent("value").unwrap(), None);
}
#[test]
fn absent_tolerant_text_rejects_other_variants() {
    for (value, name) in variants() {
        if !matches!(name, "Text" | "Null") {
            refusal(row(value).opt_text_or_absent("value"), Some(name));
        }
    }
}
#[test]
fn absent_tolerant_i64_accepts_integer_null_and_absence() {
    assert_eq!(
        row(SqlValue::Integer(7))
            .opt_i64_or_absent("value")
            .unwrap(),
        Some(7)
    );
    assert_eq!(
        row(SqlValue::Null).opt_i64_or_absent("value").unwrap(),
        None
    );
    assert_eq!(absent().opt_i64_or_absent("value").unwrap(), None);
}
#[test]
fn absent_tolerant_i64_rejects_float() {
    refusal(
        row(SqlValue::Float(7.5)).opt_i64_or_absent("value"),
        Some("Float"),
    );
}
#[test]
fn absent_tolerant_i64_rejects_other_variants() {
    for (value, name) in variants() {
        if !matches!(name, "Integer" | "Null" | "Float") {
            refusal(row(value).opt_i64_or_absent("value"), Some(name));
        }
    }
}
