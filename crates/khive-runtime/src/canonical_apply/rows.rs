use chrono::{DateTime, Utc};
use khive_storage::{Entity, Note, SqlRow, SqlStatement, SqlValue, SqlWriter, StorageError};
use uuid::Uuid;

fn invalid(field: &str) -> StorageError {
    StorageError::Internal(format!("canonical writer record has invalid {field}"))
}

fn id(row: &SqlRow, field: &str) -> Result<Uuid, StorageError> {
    Uuid::parse_str(row.text(field).map_err(|_| invalid(field))?).map_err(|_| invalid(field))
}

fn optional_id(row: &SqlRow, field: &str) -> Result<Option<Uuid>, StorageError> {
    row.opt_text(field)
        .map_err(|_| invalid(field))?
        .map(|text| Uuid::parse_str(text).map_err(|_| invalid(field)))
        .transpose()
}

fn text(row: &SqlRow, field: &str) -> Result<String, StorageError> {
    row.text(field)
        .map(str::to_owned)
        .map_err(|_| invalid(field))
}

fn optional_text(row: &SqlRow, field: &str) -> Result<Option<String>, StorageError> {
    row.opt_text(field)
        .map(|text| text.map(str::to_owned))
        .map_err(|_| invalid(field))
}

fn integer(row: &SqlRow, field: &str) -> Result<i64, StorageError> {
    row.i64(field).map_err(|_| invalid(field))
}

fn optional_integer(row: &SqlRow, field: &str) -> Result<Option<i64>, StorageError> {
    row.opt_i64(field).map_err(|_| invalid(field))
}

fn number(row: &SqlRow, field: &str) -> Result<Option<f64>, StorageError> {
    match row.get(field) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Float(number)) if number.is_finite() => Ok(Some(*number)),
        Some(SqlValue::Integer(number)) => {
            let value = *number as f64;
            if value as i128 == i128::from(*number) {
                Ok(Some(value))
            } else {
                Err(invalid(field))
            }
        }
        _ => Err(invalid(field)),
    }
}

fn json(row: &SqlRow, field: &str) -> Result<Option<serde_json::Value>, StorageError> {
    match row.get(field) {
        Some(SqlValue::Null) => Ok(None),
        Some(SqlValue::Json(value)) => Ok(Some(value.clone())),
        Some(SqlValue::Text(value)) => serde_json::from_str(value)
            .map(Some)
            .map_err(|_| invalid(field)),
        _ => Err(invalid(field)),
    }
}

fn instant(micros: i64, field: &str) -> Result<DateTime<Utc>, StorageError> {
    DateTime::from_timestamp_micros(micros).ok_or_else(|| invalid(field))
}

async fn read(
    writer: &mut dyn SqlWriter,
    sql: &str,
    id: Uuid,
) -> Result<Option<SqlRow>, StorageError> {
    let mut rows = writer
        .query_all(
            SqlStatement::new(sql, vec![SqlValue::Text(id.to_string())])
                .labelled("canonical-writer-record"),
        )
        .await?;
    if rows.len() > 1 {
        return Err(invalid("record identity"));
    }
    Ok(rows.pop())
}

pub(super) async fn entity(
    writer: &mut dyn SqlWriter,
    target: Uuid,
) -> Result<Option<Entity>, StorageError> {
    let Some(row) = read(
        writer,
        include_str!("../../sql/canonical_entity_read.sql"),
        target,
    )
    .await?
    else {
        return Ok(None);
    };
    let tags = json(&row, "tags")?.ok_or_else(|| invalid("tags"))?;
    Ok(Some(Entity {
        id: id(&row, "id")?,
        namespace: text(&row, "namespace")?,
        kind: text(&row, "kind")?,
        entity_type: optional_text(&row, "entity_type")?,
        name: text(&row, "name")?,
        description: optional_text(&row, "description")?,
        properties: json(&row, "properties")?,
        tags: serde_json::from_value(tags).map_err(|_| invalid("tags"))?,
        created_at: integer(&row, "created_at")?,
        updated_at: integer(&row, "updated_at")?,
        version: integer(&row, "version")?,
        deleted_at: optional_integer(&row, "deleted_at")?,
        merged_into: optional_id(&row, "merged_into")?,
        merge_event_id: optional_id(&row, "merge_event_id")?,
        content_ref: optional_text(&row, "content_ref")?,
    }))
}

pub(super) async fn note(
    writer: &mut dyn SqlWriter,
    target: Uuid,
) -> Result<Option<Note>, StorageError> {
    let Some(row) = read(
        writer,
        include_str!("../../sql/canonical_note_read.sql"),
        target,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(Note {
        id: id(&row, "id")?,
        namespace: text(&row, "namespace")?,
        kind: text(&row, "kind")?,
        status: text(&row, "status")?,
        name: optional_text(&row, "name")?,
        content: text(&row, "content")?,
        salience: number(&row, "salience")?,
        decay_factor: number(&row, "decay_factor")?,
        expires_at: optional_integer(&row, "expires_at")?,
        properties: json(&row, "properties")?,
        created_at: integer(&row, "created_at")?,
        updated_at: integer(&row, "updated_at")?,
        deleted_at: optional_integer(&row, "deleted_at")?,
        key: optional_text(&row, "key")?,
        version: integer(&row, "version")?,
    }))
}

pub(super) async fn edge(
    writer: &mut dyn SqlWriter,
    target: Uuid,
) -> Result<Option<khive_storage::Edge>, StorageError> {
    let Some(row) = read(
        writer,
        include_str!("../../sql/canonical_edge_read.sql"),
        target,
    )
    .await?
    else {
        return Ok(None);
    };
    Ok(Some(khive_storage::Edge {
        id: id(&row, "id")?.into(),
        namespace: text(&row, "namespace")?,
        source_id: id(&row, "source_id")?,
        target_id: id(&row, "target_id")?,
        relation: text(&row, "relation")?
            .parse()
            .map_err(|_| invalid("relation"))?,
        weight: number(&row, "weight")?.ok_or_else(|| invalid("weight"))?,
        created_at: instant(integer(&row, "created_at")?, "created_at")?,
        updated_at: instant(integer(&row, "updated_at")?, "updated_at")?,
        deleted_at: optional_integer(&row, "deleted_at")?
            .map(|value| instant(value, "deleted_at"))
            .transpose()?,
        metadata: json(&row, "metadata")?,
        target_backend: optional_text(&row, "target_backend")?,
    }))
}
