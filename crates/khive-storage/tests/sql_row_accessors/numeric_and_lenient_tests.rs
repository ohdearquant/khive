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
