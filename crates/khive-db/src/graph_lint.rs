//! Pure, caller-executed graph-lint reads and exact transaction predicates.
use chrono::{DateTime, Utc};
use khive_storage::{Edge, Entity, Note, SqlRow, SqlStatement, SqlValue, StorageError};
use uuid::Uuid;

pub fn edge_page(namespace: &str, after: Option<&str>, limit: u32) -> SqlStatement {
    SqlStatement::new(
        include_str!("../sql/graph-lint-edge-page.sql").trim_end(),
        vec![
            SqlValue::Text(namespace.into()),
            SqlValue::from_opt_text(after),
            SqlValue::Integer(i64::from(limit)),
        ],
    )
    .labelled("graph-lint-edge-page")
}
fn decode(error: impl std::fmt::Display) -> StorageError {
    StorageError::Internal(format!("graph lint row decode: {error}"))
}
fn time(value: i64) -> Result<DateTime<Utc>, StorageError> {
    DateTime::from_timestamp_micros(value).ok_or_else(|| decode("invalid timestamp"))
}
pub fn decode_edge(row: &SqlRow) -> Result<Edge, StorageError> {
    let weight = match row.get("weight") {
        Some(SqlValue::Float(value)) => *value,
        Some(SqlValue::Integer(value)) => *value as f64,
        _ => return Err(decode("invalid edge weight")),
    };
    if !weight.is_finite() {
        return Err(decode("nonfinite edge weight"));
    }
    Ok(Edge {
        id: Uuid::parse_str(row.text("id").map_err(decode)?)
            .map_err(decode)?
            .into(),
        namespace: row.text("namespace").map_err(decode)?.into(),
        source_id: Uuid::parse_str(row.text("source_id").map_err(decode)?).map_err(decode)?,
        target_id: Uuid::parse_str(row.text("target_id").map_err(decode)?).map_err(decode)?,
        relation: row
            .text("relation")
            .map_err(decode)?
            .parse()
            .map_err(decode)?,
        weight,
        created_at: time(row.i64("created_at").map_err(decode)?)?,
        updated_at: time(row.i64("updated_at").map_err(decode)?)?,
        deleted_at: row
            .opt_i64("deleted_at")
            .map_err(decode)?
            .map(time)
            .transpose()?,
        metadata: row
            .opt_text("metadata")
            .map_err(decode)?
            .map(serde_json::from_str)
            .transpose()
            .map_err(decode)?,
        target_backend: row
            .opt_text("target_backend")
            .map_err(decode)?
            .map(str::to_owned),
    })
}
/// The entity revision is the persisted mutation fence, with identity/liveness
/// retained explicitly so a malformed or externally rewritten row cannot pass.
pub fn entity_witness(entity: &Entity) -> SqlStatement {
    SqlStatement::new("SELECT 1 FROM entities WHERE id=?1 AND namespace=?2 AND kind=?3 AND name=?4 AND version=?5 AND deleted_at IS ?6", vec![
        SqlValue::Text(entity.id.to_string()), SqlValue::Text(entity.namespace.clone()),
        SqlValue::Text(entity.kind.clone()), SqlValue::Text(entity.name.clone()), SqlValue::Integer(entity.version),
        SqlValue::from_opt_i64(entity.deleted_at),
    ]).labelled("graph-lint-entity-witness")
}
pub fn note_witness(note: &Note) -> SqlStatement {
    SqlStatement::new("SELECT 1 FROM notes WHERE id=?1 AND namespace=?2 AND kind=?3 AND version=?4 AND deleted_at IS ?5", vec![
        SqlValue::Text(note.id.to_string()), SqlValue::Text(note.namespace.clone()), SqlValue::Text(note.kind.clone()),
        SqlValue::Integer(note.version), SqlValue::from_opt_i64(note.deleted_at),
    ]).labelled("graph-lint-note-witness")
}
/// Edges have no row revision. Compare every persisted field, including NULLs.
pub fn edge_witness(row: &SqlRow) -> Result<SqlStatement, StorageError> {
    // Retain raw storage values: serializing parsed metadata would change its
    // spelling and could falsely reject an otherwise unchanged row.
    let columns = [
        "id",
        "namespace",
        "source_id",
        "target_id",
        "relation",
        "weight",
        "created_at",
        "updated_at",
        "deleted_at",
        "metadata",
        "target_backend",
    ];
    let params = columns
        .into_iter()
        .map(|column| {
            row.get(column)
                .cloned()
                .ok_or_else(|| decode(format!("missing {column}")))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SqlStatement::new("SELECT 1 FROM graph_edges WHERE id IS ?1 AND namespace IS ?2 AND source_id IS ?3 AND target_id IS ?4 AND relation IS ?5 AND weight IS ?6 AND created_at IS ?7 AND updated_at IS ?8 AND deleted_at IS ?9 AND metadata IS ?10 AND target_backend IS ?11", params).labelled("graph-lint-edge-witness"))
}
#[cfg(test)]
#[path = "graph_lint_tests.rs"]
mod tests;
