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
fn corpus_scan_preserves_log_high_water() {
    for namespace in ["local", "", "tenant:'quoted/知识"] {
        for model in ["model", "model.with-unsafe/标记'quoted"] {
            let ns = namespace.to_owned();
            let model_str = model.to_owned();
            let table_name = format!("vec_{}", sanitize_model_key(model));
            let actual =
                knowledge_corpus(&ns).corpus_scan(&table_name, &model_str, "vamana_corpus_scan");
            let expected = SqlStatement {
                sql: format!(
                    "SELECT subject_id, embedding, \
                        (SELECT COALESCE(\
                           (SELECT seq FROM sqlite_sequence \
                            WHERE name = 'ann_write_log'), 0)) AS log_s \
                 FROM {table_name} \
                 WHERE namespace = ?1 AND embedding_model = ?2 \
                   AND field = 'knowledge.atom' \
                 ORDER BY subject_id"
                ),
                params: vec![SqlValue::Text(ns), SqlValue::Text(model_str)],
                label: Some("vamana_corpus_scan".into()),
            };
            assert_same(actual, expected);
        }
    }
}
