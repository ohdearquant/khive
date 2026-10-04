use khive_storage::types::{SqlStatement, SqlValue};

use super::{parse_final_tail_rows, sanitize_model_key, FinalTail};

/// Runs the single fresh-tail snapshot on a caller-supplied reader. Serving
/// paths use this seam while coordinating the ADR-118 registry guard.
pub(super) async fn fetch_final_tail_on(
    reader: &mut dyn khive_storage::SqlReader,
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> Result<FinalTail, String> {
    let rows = reader
        .query_all(final_tail_statement(model, s, live_threshold))
        .await
        .map_err(|e| e.to_string())?;
    parse_final_tail_rows(&rows, model, s)
}

pub(super) fn final_tail_statement(
    model: &str,
    s: u64,
    live_threshold: Option<f64>,
) -> SqlStatement {
    // Cap the newest raw writes before coalescing, as required by ADR-118.
    let table_name = format!("vec_{}", sanitize_model_key(model));
    let (live_cte, order_limit) = match live_threshold {
        Some(_) => (
            format!(
                "live AS (\
                   SELECT COUNT(*) AS live_count FROM {table_name} v \
                   JOIN notes n ON n.id = v.subject_id \
                   WHERE v.embedding_model = ?1 \
                     AND v.kind = 'note' AND v.field = 'note.content' \
                     AND n.deleted_at IS NULL\
                 ), "
            ),
            "ORDER BY seq DESC \
             LIMIT (SELECT CAST(live_count * ?3 AS INTEGER) + \
                       CASE WHEN CAST(live_count * ?3 AS INTEGER) < live_count * ?3 \
                            THEN 1 ELSE 0 END FROM live)"
                .to_string(),
        ),
        None => (String::new(), "ORDER BY seq".to_string()),
    };
    let mut params = vec![
        SqlValue::Text(model.to_owned()),
        SqlValue::Integer(s as i64),
    ];
    if let Some(threshold) = live_threshold {
        params.push(SqlValue::Float(threshold));
    }
    SqlStatement {
        sql: format!(
            "WITH {live_cte}selected AS (\
               SELECT seq, subject_id, op FROM ann_write_log \
               WHERE embedding_model = ?1 \
                 AND kind = 'note' AND field = 'note.content' AND seq > ?2 \
               {order_limit}\
             ), finals AS (\
               SELECT seq, subject_id, op, first_seq FROM (\
                 SELECT seq, subject_id, op, \
                        MIN(seq) OVER (PARTITION BY subject_id) AS first_seq, \
                        ROW_NUMBER() OVER (\
                          PARTITION BY subject_id ORDER BY seq DESC\
                        ) AS final_rank \
                 FROM selected\
               ) WHERE final_rank = 1\
             ) \
             SELECT finals.seq, finals.subject_id, finals.op, \
                    vectors.embedding_model AS vector_model, \
                    vectors.kind AS vector_kind, \
                    vectors.field AS vector_field, vectors.embedding, \
                    live_note.id AS live_note_id \
             FROM finals \
             LEFT JOIN {table_name} AS vectors \
               ON vectors.subject_id = finals.subject_id \
             LEFT JOIN notes AS live_note \
               ON live_note.id = finals.subject_id \
              AND live_note.deleted_at IS NULL \
             ORDER BY finals.first_seq"
        ),
        params,
        label: Some("memory_ann_fresh_tail_snapshot".into()),
    }
}
