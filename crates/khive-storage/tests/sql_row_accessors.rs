use khive_storage::types::{SqlColumn, SqlColumnError, SqlRow, SqlValue};

fn row(value: SqlValue) -> SqlRow {
    SqlRow {
        columns: vec![SqlColumn {
            name: "value".into(),
            value,
        }],
    }
}

fn absent() -> SqlRow {
    SqlRow { columns: vec![] }
}

fn refusal<T: std::fmt::Debug>(result: Result<T, SqlColumnError>, found: Option<&'static str>) {
    let error = result.expect_err("typed column must be refused");
    assert_eq!(error.column, "value");
    assert_eq!(error.found, found);
}

fn variants() -> Vec<(SqlValue, &'static str)> {
    vec![
        (SqlValue::Null, "Null"),
        (SqlValue::Bool(true), "Bool"),
        (SqlValue::Integer(7), "Integer"),
        (SqlValue::Float(7.5), "Float"),
        (SqlValue::Text("text".into()), "Text"),
        (SqlValue::Blob(vec![1, 2]), "Blob"),
        (SqlValue::Json(serde_json::json!({"key": 1})), "Json"),
        (SqlValue::Uuid(uuid::Uuid::nil()), "Uuid"),
        (
            SqlValue::Timestamp(chrono::DateTime::from_timestamp_micros(0).unwrap()),
            "Timestamp",
        ),
    ]
}

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

#[test]
fn accessors_preserve_first_exact_column_lookup() {
    let row = SqlRow {
        columns: vec![
            SqlColumn {
                name: "Value".into(),
                value: SqlValue::Text("case".into()),
            },
            SqlColumn {
                name: "value".into(),
                value: SqlValue::Text("first".into()),
            },
            SqlColumn {
                name: "value".into(),
                value: SqlValue::Integer(2),
            },
        ],
    };
    assert_eq!(row.text("value").unwrap(), "first");
    assert_eq!(row.text("Value").unwrap(), "case");
    assert_eq!(row.i64("value").unwrap_err().found, Some("Text"));
    assert!(row.text("VALUE").is_err());
}

#[test]
fn column_error_is_an_owned_standard_error() {
    let error = {
        let name = "value".to_owned();
        absent().text(&name).unwrap_err()
    };
    assert_eq!(error.column, "value");
    assert_eq!(error.to_string(), "SQL column value is absent");
    let _: &dyn std::error::Error = &error;
    assert_eq!(
        row(SqlValue::Float(2.5))
            .i64("value")
            .unwrap_err()
            .to_string(),
        "SQL column value has value Float"
    );
}
