// Shared with the integration query-plan checks so they explain the production SQL.
pub(super) fn cursor_query(
    table: &str,
    columns: &str,
    subsequent: bool,
    status_clause: &str,
) -> String {
    let atom_filter = if table == "knowledge_atoms" {
        " AND tags NOT LIKE '%type:domain%'"
    } else {
        ""
    };
    let (seek, limit) = if subsequent {
        (
            " AND (created_at > ?2 OR (created_at = ?2 AND id > ?3))",
            "?4",
        )
    } else {
        ("", "?2")
    };
    format!(
        "SELECT {columns} FROM {table} WHERE namespace = ?1 AND deleted_at IS NULL\
         {atom_filter}{seek}{status_clause} ORDER BY created_at ASC, id ASC LIMIT {limit}"
    )
}
