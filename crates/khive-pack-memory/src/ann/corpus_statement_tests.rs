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
fn scope_counts_preserves_literal_sql_params_and_label() {
    for model in ["model", "model.with-unsafe/标记'quoted"] {
        for s in [0, 1, i64::MAX as u64, u64::MAX] {
            let table_name = format!("vec_{}", sanitize_model_key(model));
            let actual =
                memory_corpus().scope_counts(&table_name, model, s, "memory_ann_scope_counts");
            let expected = SqlStatement {
                sql: format!(
                    "SELECT \
                   (SELECT COUNT(*) FROM {table_name} v \
                     JOIN notes n ON n.id = v.subject_id \
                     WHERE v.embedding_model = ?1 \
                       AND v.kind = 'note' AND v.field = 'note.content' \
                       AND n.deleted_at IS NULL) AS live, \
                   (SELECT COUNT(*) FROM ann_write_log \
                     WHERE embedding_model = ?1 \
                       AND kind = 'note' AND field = 'note.content' \
                       AND seq > ?2) AS tail"
                ),
                params: vec![
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(s as i64),
                ],
                label: Some("memory_ann_scope_counts".into()),
            };
            assert_same(actual, expected);
        }
    }
}

#[test]
fn tail_exists_preserves_literal_sql_params_and_label() {
    for model in ["model", "model.with-unsafe/标记'quoted"] {
        for s in [0, 1, i64::MAX as u64, u64::MAX] {
            let actual = memory_corpus().tail_exists(model, s, "memory_ann_tail_exists");
            let expected = SqlStatement {
                sql: "SELECT EXISTS(SELECT 1 FROM ann_write_log \
                    WHERE embedding_model = ?1 \
                      AND kind = 'note' AND field = 'note.content' \
                      AND seq > ?2) AS has_tail"
                    .into(),
                params: vec![
                    SqlValue::Text(model.to_owned()),
                    SqlValue::Integer(s as i64),
                ],
                label: Some("memory_ann_tail_exists".into()),
            };
            assert_same(actual, expected);
        }
    }
}

#[test]
fn compaction_keeps_its_registry_scope() {
    assert_eq!(
        memory_corpus().compaction_scope(),
        khive_retrieval::ann::registry::CompactionScope::Model
    );
}
