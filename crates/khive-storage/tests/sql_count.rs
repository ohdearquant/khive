use async_trait::async_trait;
use khive_storage::{
    SqlReader, SqlRow, SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult,
};
use serde_json::json;

struct ScalarReader {
    result: Option<StorageResult<Option<SqlValue>>>,
    statements: Vec<SqlStatement>,
}

impl ScalarReader {
    fn new(result: StorageResult<Option<SqlValue>>) -> Self {
        Self {
            result: Some(result),
            statements: Vec::new(),
        }
    }
}

#[async_trait]
impl SqlReader for ScalarReader {
    async fn query_row(&mut self, _: SqlStatement) -> StorageResult<Option<SqlRow>> {
        panic!("count must delegate only to query_scalar")
    }

    async fn query_all(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        panic!("count must not materialize rows")
    }

    async fn query_scalar(&mut self, statement: SqlStatement) -> StorageResult<Option<SqlValue>> {
        self.statements.push(statement);
        self.result
            .take()
            .expect("count must issue exactly one scalar query")
    }

    async fn explain(&mut self, _: SqlStatement) -> StorageResult<Vec<SqlRow>> {
        panic!("count must not explain or rewrite the statement")
    }
}

fn statement() -> SqlStatement {
    SqlStatement {
        sql: "SELECT COUNT(*) FROM rows WHERE namespace = ?1 AND revision > ?2".into(),
        params: vec![SqlValue::Text("count-test".into()), SqlValue::Integer(7)],
        label: Some("count.contract".into()),
    }
}

#[tokio::test]
async fn count_accepts_zero_positive_and_maximum_signed_integer() {
    for value in [0, 42, i64::MAX] {
        let mut reader = ScalarReader::new(Ok(Some(SqlValue::Integer(value))));
        let erased: &mut dyn SqlReader = &mut reader;
        assert_eq!(erased.count(statement()).await.unwrap(), value as u64);
        assert_eq!(reader.statements.len(), 1);
    }
}

#[tokio::test]
async fn count_refuses_negative_missing_null_and_every_noninteger_variant() {
    let values = [
        Some(SqlValue::Integer(-1)),
        Some(SqlValue::Integer(i64::MIN)),
        None,
        Some(SqlValue::Null),
        Some(SqlValue::Bool(true)),
        Some(SqlValue::Float(7.0)),
        Some(SqlValue::Text("7".into())),
        Some(SqlValue::Blob(vec![7])),
        Some(SqlValue::Json(json!(7))),
        Some(SqlValue::Uuid(uuid::Uuid::nil())),
        Some(SqlValue::Timestamp(
            chrono::DateTime::from_timestamp_micros(0).unwrap(),
        )),
    ];
    for value in values {
        let mut reader = ScalarReader::new(Ok(value));
        let error = reader
            .count(statement())
            .await
            .expect_err("invalid count must fail");
        assert!(matches!(error, StorageError::Internal(message)
            if message == "SQL count expected a nonnegative integer scalar"));
        assert_eq!(reader.statements.len(), 1);
    }
}

#[tokio::test]
async fn count_passes_sql_parameters_and_label_through_unchanged() {
    for label in [None, Some("exact label with spaces".into())] {
        let expected = SqlStatement {
            sql: "  SELECT COUNT(*)\nFROM arbitrary_source WHERE value = ?1;  ".into(),
            params: vec![
                SqlValue::Null,
                SqlValue::Bool(false),
                SqlValue::Integer(i64::MIN),
                SqlValue::Float(1.25),
                SqlValue::Text("'quoted'\ntext".into()),
                SqlValue::Blob(vec![0, 255]),
                SqlValue::Json(json!({"key": [1, null, "x"]})),
                SqlValue::Uuid(uuid::Uuid::from_u128(1)),
                SqlValue::Timestamp(chrono::DateTime::from_timestamp_micros(123).unwrap()),
            ],
            label,
        };
        let mut reader = ScalarReader::new(Ok(Some(SqlValue::Integer(3))));
        assert_eq!(reader.count(expected.clone()).await.unwrap(), 3);
        assert_eq!(reader.statements.len(), 1);
        assert_eq!(
            serde_json::to_value(&reader.statements[0]).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }
}

#[tokio::test]
async fn count_preserves_backend_error_variant_fields_and_source() {
    let mut reader = ScalarReader::new(Err(StorageError::Timeout {
        operation: "fixture timeout".into(),
    }));
    let error = reader.count(statement()).await.unwrap_err();
    assert!(matches!(error, StorageError::Timeout { operation } if operation == "fixture timeout"));
    assert_eq!(reader.statements.len(), 1);

    let source: Box<dyn std::error::Error + Send + Sync> =
        Box::new(std::io::Error::other("fixture driver source"));
    let identity = source.as_ref() as *const (dyn std::error::Error + Send + Sync);
    let mut reader = ScalarReader::new(Err(StorageError::Driver {
        capability: StorageCapability::Sql,
        operation: "fixture scalar".into(),
        source,
    }));
    let error = reader.count(statement()).await.unwrap_err();
    match error {
        StorageError::Driver {
            capability,
            operation,
            source,
        } => {
            assert_eq!(capability, StorageCapability::Sql);
            assert_eq!(operation, "fixture scalar");
            assert!(std::ptr::eq(source.as_ref(), identity));
            assert_eq!(
                source.downcast_ref::<std::io::Error>().unwrap().kind(),
                std::io::ErrorKind::Other
            );
        }
        other => panic!("driver error must pass through unchanged: {other:?}"),
    }
    assert_eq!(reader.statements.len(), 1);
}
