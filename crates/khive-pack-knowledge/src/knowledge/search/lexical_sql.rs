/// Recover eligible matches in rowid order after the bounded global window.
/// The namespace index drives the join; FTS tests membership for each local row.
/// Clauses are generated internally by the shared status/type eligibility rules.
pub(super) fn eligibility_fallback_sql(status_clause: &str, type_clause: &str) -> String {
    format!(
        "SELECT a.* FROM knowledge_atoms AS a INDEXED BY idx_knowledge_atoms_ns \
         CROSS JOIN fts_knowledge ON fts_knowledge.rowid = a.rowid \
         WHERE a.namespace = ?2 AND fts_knowledge MATCH ?1 \
           AND a.deleted_at IS NULL{status_clause}{type_clause} \
         ORDER BY a.rowid LIMIT ?3"
    )
}
