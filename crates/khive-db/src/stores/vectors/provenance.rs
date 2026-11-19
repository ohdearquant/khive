use rusqlite::OptionalExtension;

pub(super) fn provenance_sidecar_exists(
    conn: &rusqlite::Connection,
) -> Result<bool, rusqlite::Error> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'vector_provenance'",
        [],
        |row| row.get::<_, i64>(0),
    )
    .optional()
    .map(|row| row.is_some())
}

pub(super) fn provenance_read_sql(table: &str, has_sidecar: bool) -> String {
    if has_sidecar {
        format!(
            "SELECT v.embedding_model, v.field, v.embedding, p.embedding_digest, \
                    p.text_fingerprint, p.updated_at \
             FROM {table} AS v \
             LEFT JOIN vector_provenance AS p \
               ON p.model_key = ?1 AND p.subject_id = v.subject_id \
              AND p.namespace = v.namespace \
             WHERE v.subject_id = ?2 AND v.namespace = ?3"
        )
    } else {
        format!(
            "SELECT v.embedding_model, v.field, v.embedding, NULL, NULL, NULL \
             FROM {table} AS v \
             WHERE v.subject_id = ?1 AND v.namespace = ?2"
        )
    }
}
