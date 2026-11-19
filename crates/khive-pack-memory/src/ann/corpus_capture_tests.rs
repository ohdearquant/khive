use super::*;

fn assert_same(actual: SqlStatement, expected: SqlStatement) {
    assert_eq!(actual.sql.as_bytes(), expected.sql.as_bytes());
    assert_eq!(
        serde_json::to_value(&actual.params).unwrap(),
        serde_json::to_value(&expected.params).unwrap()
    );
    assert_eq!(actual.label, expected.label);
}

#[test]
fn fingerprint_preserves_literal_sql_params_and_label() {
    for model in ["model", "model.with-unsafe/标记'quoted"] {
        let table_name = format!("vec_{}", sanitize_model_key(model));
        let actual = memory_corpus().fingerprint(&table_name, model, "memory_ann_fingerprint");
        let expected = SqlStatement {
            sql: format!(
                "SELECT COUNT(*) AS n FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL"
            ),
            params: vec![SqlValue::Text(model.to_owned())],
            label: Some("memory_ann_fingerprint".into()),
        };
        assert_same(actual, expected);
    }
}

#[test]
fn corpus_scan_preserves_scoped_maximum_and_own_floor() {
    for model in ["model", "model.with-unsafe/标记'quoted"] {
        let table_name = format!("vec_{}", sanitize_model_key(model));
        let actual = memory_corpus().corpus_scan(&table_name, model, "memory_ann_corpus_scan");
        let expected = SqlStatement {
            sql: format!(
                "SELECT v.subject_id, v.embedding, n.namespace, \
                        MAX( \
                          (SELECT COALESCE(MAX(seq), 0) FROM ann_write_log \
                            WHERE embedding_model = ?1 \
                              AND kind = 'note' AND field = 'note.content'), \
                          (SELECT COALESCE(MAX(watermark), 0) \
                             FROM ann_consumer_watermark \
                            WHERE consumer = ?2 AND namespace = ?3 \
                              AND embedding_model = ?1 AND watermark >= 0) \
                        ) AS log_s \
                 FROM {table_name} v \
                 JOIN notes n ON n.id = v.subject_id \
                 WHERE v.embedding_model = ?1 \
                   AND v.kind = 'note' AND v.field = 'note.content' \
                   AND n.deleted_at IS NULL \
                 ORDER BY v.subject_id"
            ),
            params: vec![
                SqlValue::Text(model.to_owned()),
                SqlValue::Text(ANN_CONSUMER.into()),
                SqlValue::Text(ANN_WILDCARD_NS.into()),
            ],
            label: Some("memory_ann_corpus_scan".into()),
        };
        assert_same(actual, expected);
    }
}
