use super::*;
use khive_storage::types::SqlColumn;

fn valid_row() -> SqlRow {
    SqlRow {
        columns: vec![
            ("id", SqlValue::Text(uuid::Uuid::nil().to_string())),
            ("namespace", SqlValue::Text("local".into())),
            ("actor", SqlValue::Text("actor:a".into())),
            ("session_id", SqlValue::Null),
            ("verb", SqlValue::Text("git.commit".into())),
            ("repo", SqlValue::Text("/repo".into())),
            ("inputs", SqlValue::Text("{}".into())),
            ("gate", SqlValue::Text("{}".into())),
            ("policy", SqlValue::Null),
            ("fork_policy", SqlValue::Null),
            ("credential", SqlValue::Null),
            ("started_at", SqlValue::Integer(0)),
            ("finished_at", SqlValue::Null),
            ("disposition", SqlValue::Text("unknown".into())),
            ("result", SqlValue::Text("{}".into())),
            ("reason", SqlValue::Null),
        ]
        .into_iter()
        .map(|(name, value)| SqlColumn {
            name: name.into(),
            value,
        })
        .collect(),
    }
}

fn invalid_column(row: &SqlRow, column: &str) {
    let error = decode(row).expect_err("malformed receipt column must be refused");
    match error {
        RuntimeError::Internal(message) => {
            assert_eq!(message, format!("invalid git receipt column: {column}"))
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

#[test]
fn missing_optional_receipt_columns_are_refused() {
    let decoded = decode(&valid_row()).expect("complete row with NULL optionals must decode");
    assert_eq!(decoded.id, uuid::Uuid::nil().to_string());
    assert_eq!(decoded.session_id, None);
    assert_eq!(decoded.finished_at, None);
    assert_eq!(decoded.reason, None);
    for column in ["session_id", "finished_at", "reason"] {
        let mut row = valid_row();
        row.columns.retain(|entry| entry.name != column);
        assert!(row.get(column).is_none());
        invalid_column(&row, column);
    }
}

#[test]
fn receipt_column_diagnostics_preserve_null_wrong_type_and_absence() {
    for (column, value) in [
        ("id", SqlValue::Null),
        ("started_at", SqlValue::Float(0.5)),
        ("session_id", SqlValue::Integer(1)),
        ("finished_at", SqlValue::Text("1".into())),
    ] {
        let mut row = valid_row();
        row.columns
            .iter_mut()
            .find(|entry| entry.name == column)
            .unwrap()
            .value = value;
        invalid_column(&row, column);
    }
    for column in ["id", "started_at"] {
        let mut row = valid_row();
        row.columns.retain(|entry| entry.name != column);
        invalid_column(&row, column);
    }
}
