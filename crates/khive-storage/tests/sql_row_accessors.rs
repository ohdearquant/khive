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

include!("sql_row_accessors/required_and_nullable_tests.rs");

include!("sql_row_accessors/lookup_and_uuid_tests.rs");

include!("sql_row_accessors/numeric_and_lenient_tests.rs");
