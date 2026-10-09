use khive_storage::note::{NotePropertyPatch, NotePropertyPrecondition};
use khive_storage::{SqlStatement, SqlValue, StorageCapability, StorageError, StorageResult};
use uuid::Uuid;

use super::note_due_key_values;

const OPERATION: &str = "try_patch_note_properties";
const MAX_ENTRIES: usize = 32;

fn invalid_input(message: &str) -> StorageError {
    StorageError::InvalidInput {
        capability: StorageCapability::Notes,
        operation: OPERATION.into(),
        message: message.into(),
    }
}

fn property_path(key: &str) -> StorageResult<String> {
    // SQLite truncates path labels at U+0000 and can mutate a shorter sibling key.
    if key.contains('\0') {
        return Err(invalid_input("property key must not contain U+0000"));
    }
    Ok(format!("$.{}", serde_json::Value::String(key.to_string())))
}

fn bind(params: &mut Vec<SqlValue>, value: SqlValue) -> usize {
    params.push(value);
    params.len()
}

pub(crate) fn note_patch_properties_statement(
    id: Uuid,
    namespace: &str,
    kind: &str,
    patch: &NotePropertyPatch,
) -> StorageResult<SqlStatement> {
    if patch.set.is_empty() || patch.set.len() > MAX_ENTRIES {
        return Err(invalid_input(
            "property patch must contain between 1 and 32 writes",
        ));
    }
    if patch.preconditions.len() > MAX_ENTRIES {
        return Err(invalid_input(
            "property patch must contain at most 32 preconditions",
        ));
    }

    let mut params = vec![
        SqlValue::Text(id.to_string()),
        SqlValue::Text(namespace.to_string()),
        SqlValue::Text(kind.to_string()),
        SqlValue::Integer(patch.updated_at),
        patch
            .extend_expires_at
            .map_or(SqlValue::Null, SqlValue::Integer),
    ];
    let mut sql = "UPDATE notes SET properties = json_set(COALESCE(properties, '{}')".to_string();
    for (key, value) in &patch.set {
        let path = bind(&mut params, SqlValue::Text(property_path(key)?));
        let serialized = serde_json::to_string(value)
            .map_err(|error| StorageError::driver(StorageCapability::Notes, OPERATION, error))?;
        let value = bind(&mut params, SqlValue::Text(serialized));
        sql.push_str(&format!(", ?{path}, json(?{value})"));
    }
    sql.push_str(
        "), expires_at = CASE WHEN ?5 IS NULL THEN expires_at \
         WHEN expires_at IS NULL OR expires_at < ?5 THEN ?5 ELSE expires_at END, \
         updated_at = MAX(updated_at, ?4)",
    );
    if let Some(value) = patch.set.get("next_attempt_at") {
        let (due_key, due_source) =
            note_due_key_values(&Some(serde_json::json!({"next_attempt_at": value})));
        let key = bind(&mut params, due_key.map_or(SqlValue::Null, SqlValue::Blob));
        let source = bind(
            &mut params,
            due_source.map_or(SqlValue::Null, SqlValue::Text),
        );
        sql.push_str(&format!(
            ", strict_due_key = ?{key}, due_source = ?{source}"
        ));
    }
    sql.push_str(
        " WHERE id = ?1 AND namespace = ?2 AND kind = ?3 AND deleted_at IS NULL \
         AND (properties IS NULL OR json_type(properties) = 'object')",
    );
    for precondition in &patch.preconditions {
        let key = match precondition {
            NotePropertyPrecondition::ExtractEquals { key, .. }
            | NotePropertyPrecondition::AbsentOrExtractEquals { key, .. }
            | NotePropertyPrecondition::AbsentOrTextEquals { key, .. }
            | NotePropertyPrecondition::TrueOrTextTrue { key } => key,
        };
        let path = bind(&mut params, SqlValue::Text(property_path(key)?));
        match precondition {
            NotePropertyPrecondition::ExtractEquals { value, .. } => {
                let value = bind(&mut params, value.clone());
                sql.push_str(&format!(
                    " AND json_extract(properties, ?{path}) = ?{value}"
                ));
            }
            NotePropertyPrecondition::AbsentOrExtractEquals { value, .. } => {
                let value = bind(&mut params, value.clone());
                sql.push_str(&format!(
                    " AND (json_type(properties, ?{path}) IS NULL \
                     OR json_extract(properties, ?{path}) = ?{value})"
                ));
            }
            NotePropertyPrecondition::AbsentOrTextEquals { value, .. } => {
                let value = bind(&mut params, SqlValue::Text(value.clone()));
                sql.push_str(&format!(
                    " AND (json_type(properties, ?{path}) IS NULL \
                     OR (json_type(properties, ?{path}) = 'text' \
                     AND json_extract(properties, ?{path}) = ?{value}))"
                ));
            }
            NotePropertyPrecondition::TrueOrTextTrue { .. } => sql.push_str(&format!(
                " AND (json_extract(properties, ?{path}) = 'true' \
                 OR json_type(properties, ?{path}) = 'true')"
            )),
        }
    }
    Ok(SqlStatement {
        sql,
        params,
        label: Some("note-patch-properties".into()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn paths_and_values_are_bound_and_unrelated_due_columns_are_not_set() {
        let key = "key.with[0]\\\"' punctuation";
        let patch = NotePropertyPatch {
            preconditions: vec![NotePropertyPrecondition::AbsentOrTextEquals {
                key: key.into(),
                value: "predicate value".into(),
            }],
            set: [(key.into(), json!({"written value": true}))].into(),
            extend_expires_at: None,
            updated_at: 42,
        };
        let statement = note_patch_properties_statement(Uuid::nil(), "namespace", "kind", &patch)
            .expect("valid patch");
        for literal in [key, "predicate value", "written value", "namespace", "kind"] {
            assert!(!statement.sql.contains(&format!("'{literal}'")));
        }
        assert!(!statement.sql.contains("strict_due_key"));
        assert!(!statement.sql.contains("due_source"));
        assert!(statement.params.iter().any(|value| matches!(
            value,
            SqlValue::Text(value) if value == &format!("$.{}", json!(key))
        )));
    }
}
