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
fn tail_exists_preserves_literal_sql_params_and_label() {
    for ns in ["local", "", "tenant:'quoted/知识"] {
        for model in ["model", "model.with-unsafe/标记'quoted"] {
            for s in [0, 1, i64::MAX as u64, u64::MAX] {
                let actual = knowledge_corpus(ns).tail_exists(model, s, "ann_tail_probe");
                let expected = SqlStatement {
                    sql: "SELECT EXISTS(SELECT 1 FROM ann_write_log \
                  WHERE namespace = ?1 AND embedding_model = ?2 \
                    AND field = 'knowledge.atom' AND seq > ?3) AS has_tail"
                        .into(),
                    params: vec![
                        SqlValue::Text(ns.to_owned()),
                        SqlValue::Text(model.to_owned()),
                        SqlValue::Integer(s as i64),
                    ],
                    label: Some("ann_tail_probe".into()),
                };
                assert_same(actual, expected);
            }
        }
    }
}

#[test]
fn scope_counts_preserves_literal_sql_params_and_label() {
    for ns in ["local", "", "tenant:'quoted/知识"] {
        for model in ["model", "model.with-unsafe/标记'quoted"] {
            for s in [0, 1, i64::MAX as u64, u64::MAX] {
                let table_name = format!("vec_{}", sanitize_model_key(model));
                let actual =
                    knowledge_corpus(ns).scope_counts(&table_name, model, s, "ann_scope_counts");
                let expected = SqlStatement {
                    sql: format!(
                        "SELECT \
                   (SELECT COUNT(*) FROM {table_name} \
                     WHERE namespace = ?1 AND embedding_model = ?2 \
                       AND field = 'knowledge.atom') AS live, \
                   (SELECT COUNT(*) FROM ann_write_log \
                     WHERE namespace = ?1 AND embedding_model = ?2 \
                       AND field = 'knowledge.atom' AND seq > ?3) AS tail"
                    ),
                    params: vec![
                        SqlValue::Text(ns.to_owned()),
                        SqlValue::Text(model.to_owned()),
                        SqlValue::Integer(s as i64),
                    ],
                    label: Some("ann_scope_counts".into()),
                };
                assert_same(actual, expected);
            }
        }
    }
}

#[test]
fn classification_scope_counts_preserves_literal_sql_params_and_label() {
    for ns in ["local", "", "tenant:'quoted/知识"] {
        for model in ["model", "model.with-unsafe/标记'quoted"] {
            for s in [0, 1, i64::MAX as u64, u64::MAX] {
                let table_name = format!("vec_{}", sanitize_model_key(model));
                for multiplier in [
                    SqlValue::Null,
                    SqlValue::Integer(0),
                    SqlValue::Integer(8),
                    SqlValue::Integer(i64::MAX),
                ] {
                    let actual = knowledge_corpus(ns).classification_scope_counts(
                        &table_name,
                        model,
                        s,
                        multiplier.clone(),
                        "ann_classification_scope_counts",
                    );
                    let expected = SqlStatement {
                        sql: format!(
                            "WITH tail AS MATERIALIZED (\
                   SELECT COUNT(*) AS tail_rows FROM ann_write_log \
                   WHERE namespace = ?1 AND embedding_model = ?2 \
                     AND field = 'knowledge.atom' AND seq > ?3\
                 ), cap AS MATERIALIZED (\
                   SELECT CASE \
                     WHEN tail_rows = 1 THEN 1 \
                     WHEN tail_rows = 0 OR ?4 IS NULL \
                       OR tail_rows > 9223372036854775807 / ?4 THEN -1 \
                     ELSE tail_rows * ?4 END AS max_rows FROM tail\
                 ), live AS (\
                   SELECT COUNT(*) AS live_rows FROM (\
                     SELECT 1 FROM {table_name} \
                     WHERE namespace = ?1 AND embedding_model = ?2 \
                       AND field = 'knowledge.atom' \
                     LIMIT (SELECT max_rows FROM cap)\
                   )\
                 ) \
                 SELECT live.live_rows AS live, tail.tail_rows AS tail, \
                        cap.max_rows AS cap FROM live CROSS JOIN tail CROSS JOIN cap"
                        ),
                        params: vec![
                            SqlValue::Text(ns.to_owned()),
                            SqlValue::Text(model.to_owned()),
                            SqlValue::Integer(s as i64),
                            multiplier,
                        ],
                        label: Some("ann_classification_scope_counts".into()),
                    };
                    assert_same(actual, expected);
                }
            }
        }
    }
}

#[test]
fn compaction_keeps_its_registry_scope() {
    for ns in ["local", "", "tenant:'quoted/知识"] {
        assert_eq!(
            knowledge_corpus(ns).compaction_scope(),
            khive_retrieval::ann::registry::CompactionScope::Namespace(ns.to_owned())
        );
    }
}
