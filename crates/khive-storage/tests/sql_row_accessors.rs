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

#[test]
fn uuid_accessors_accept_native_and_text_representations() {
    let expected = uuid::Uuid::from_u128(0x550e8400e29b41d4a716446655440000);
    for value in [expected, uuid::Uuid::nil()] {
        let row = row(SqlValue::Uuid(value));
        assert_eq!(row.uuid("value").unwrap(), value);
        assert_eq!(row.opt_uuid("value").unwrap(), Some(value));
    }
    for text in [
        "550e8400-e29b-41d4-a716-446655440000",
        "550E8400-E29B-41D4-A716-446655440000",
        "550e8400e29b41d4a716446655440000",
        "urn:uuid:550e8400-e29b-41d4-a716-446655440000",
        "{550e8400-e29b-41d4-a716-446655440000}",
    ] {
        let row = row(SqlValue::Text(text.into()));
        assert_eq!(row.uuid("value").unwrap(), expected);
        assert_eq!(row.opt_uuid("value").unwrap(), Some(expected));
    }
}

#[test]
fn uuid_accessors_distinguish_null_absence_and_invalid_values() {
    refusal(row(SqlValue::Null).uuid("value"), Some("Null"));
    assert_eq!(row(SqlValue::Null).opt_uuid("value").unwrap(), None);
    refusal(absent().uuid("value"), None);
    refusal(absent().opt_uuid("value"), None);
    for (value, name) in variants() {
        if !matches!(name, "Uuid" | "Null") {
            let row = row(value);
            refusal(row.uuid("value"), Some(name));
            refusal(row.opt_uuid("value"), Some(name));
        }
    }
    for text in ["", "550e8400-e29b-41d4-a716-44665544000z"] {
        let row = row(SqlValue::Text(text.into()));
        refusal(row.uuid("value"), Some("Text"));
        refusal(row.opt_uuid("value"), Some("Text"));
    }
}

#[test]
fn f64_accessors_preserve_floats_and_round_integers() {
    for value in [
        0.0,
        -0.0,
        7.5,
        f64::INFINITY,
        f64::NEG_INFINITY,
        f64::from_bits(0x7ff8000000000042),
    ] {
        let row = row(SqlValue::Float(value));
        assert_eq!(row.f64("value").unwrap().to_bits(), value.to_bits());
        assert_eq!(
            row.opt_f64("value").unwrap().unwrap().to_bits(),
            value.to_bits()
        );
    }
    for (value, expected) in [
        (0, 0.0),
        (-7, -7.0),
        (9_007_199_254_740_993, 9_007_199_254_740_992.0),
        (i64::MIN, -9_223_372_036_854_775_808.0),
        (i64::MAX, 9_223_372_036_854_775_808.0),
    ] {
        let row = row(SqlValue::Integer(value));
        assert_eq!(row.f64("value").unwrap(), expected);
        assert_eq!(row.opt_f64("value").unwrap(), Some(expected));
    }
}

#[test]
fn f64_accessors_distinguish_null_absence_and_wrong_variants() {
    refusal(row(SqlValue::Null).f64("value"), Some("Null"));
    assert_eq!(row(SqlValue::Null).opt_f64("value").unwrap(), None);
    refusal(absent().f64("value"), None);
    refusal(absent().opt_f64("value"), None);
    for (value, name) in variants() {
        if !matches!(name, "Float" | "Integer" | "Null") {
            let row = row(value);
            refusal(row.f64("value"), Some(name));
            refusal(row.opt_f64("value"), Some(name));
        }
    }
    refusal(row(SqlValue::Text("7.5".into())).f64("value"), Some("Text"));
    refusal(
        row(SqlValue::Text("7.5".into())).opt_f64("value"),
        Some("Text"),
    );
}

#[test]
fn lenient_accessors_preserve_values_and_ignore_all_other_variants() {
    for text in ["text", ""] {
        let row = row(SqlValue::Text(text.into()));
        assert_eq!(row.text_or_none("value"), Some(text));
    }
    for value in [i64::MIN, 0, i64::MAX] {
        assert_eq!(
            row(SqlValue::Integer(value)).i64_or_none("value"),
            Some(value)
        );
    }
    assert_eq!(absent().text_or_none("value"), None);
    assert_eq!(absent().i64_or_none("value"), None);
    for (value, name) in variants() {
        let row = row(value);
        if name != "Text" {
            assert_eq!(row.text_or_none("value"), None);
        }
        if name != "Integer" {
            assert_eq!(row.i64_or_none("value"), None);
        }
    }
}

#[test]
fn new_accessors_keep_first_exact_column_lookup() {
    let row = SqlRow {
        columns: vec![
            SqlColumn {
                name: "Value".into(),
                value: SqlValue::Uuid(uuid::Uuid::nil()),
            },
            SqlColumn {
                name: "value".into(),
                value: SqlValue::Bool(true),
            },
            SqlColumn {
                name: "value".into(),
                value: SqlValue::Text("550e8400-e29b-41d4-a716-446655440000".into()),
            },
            SqlColumn {
                name: "value".into(),
                value: SqlValue::Integer(7),
            },
        ],
    };
    assert_eq!(row.uuid("Value").unwrap(), uuid::Uuid::nil());
    refusal(row.uuid("value"), Some("Bool"));
    refusal(row.opt_uuid("value"), Some("Bool"));
    refusal(row.f64("value"), Some("Bool"));
    refusal(row.opt_f64("value"), Some("Bool"));
    assert_eq!(row.text_or_none("value"), None);
    assert_eq!(row.i64_or_none("value"), None);
    assert_eq!(row.uuid("VALUE").unwrap_err().found, None);
    assert_eq!(row.opt_uuid("VALUE").unwrap_err().found, None);
    assert_eq!(row.f64("VALUE").unwrap_err().found, None);
    assert_eq!(row.opt_f64("VALUE").unwrap_err().found, None);
    assert_eq!(row.text_or_none("VALUE"), None);
    assert_eq!(row.i64_or_none("VALUE"), None);
}
