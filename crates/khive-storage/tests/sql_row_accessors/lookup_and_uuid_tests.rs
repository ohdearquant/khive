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
