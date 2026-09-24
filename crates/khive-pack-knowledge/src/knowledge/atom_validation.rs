use std::collections::HashSet;

use khive_runtime::secret_gate::{mask_for_redaction_surface, RedactionSurface};
use khive_runtime::{KhiveRuntime, NamespaceToken, RuntimeError};
use khive_storage::{SqlReader, SqlStatement, SqlValue};
use serde_json::{json, Value};

use super::schema::AtomWrite;
use super::util::{row_i64, row_str, sql_err};

/// Existing atom ID, or no row for a new slug. Reads never allocate a new identity.
pub(super) async fn target(
    reader: &mut dyn SqlReader,
    namespace: &str,
    atom: &AtomWrite,
) -> Result<Option<String>, RuntimeError> {
    let input = match atom {
        AtomWrite::Upsert(input) => input,
        AtomWrite::PropertiesOnly(input) => {
            let id = input.id.to_string();
            let domain = reader
                .query_row(SqlStatement {
                    sql: "SELECT id FROM knowledge_domains WHERE id = ?1".into(),
                    params: vec![SqlValue::Text(id.clone())],
                    label: None,
                })
                .await
                .map_err(|error| sql_err("upsert_atoms domain lookup", error))?;
            if domain.is_some() {
                return Err(RuntimeError::InvalidInput(
                    "properties-only target is a domain; use domain verbs instead".into(),
                ));
            }
            let row = reader
                .query_row(SqlStatement {
                    sql: "SELECT tags FROM knowledge_atoms WHERE id = ?1 AND deleted_at IS NULL"
                        .into(),
                    params: vec![SqlValue::Text(id.clone())],
                    label: None,
                })
                .await
                .map_err(|error| sql_err("upsert_atoms id lookup", error))?
                .ok_or_else(|| RuntimeError::NotFound(format!("atom not found: {id}")))?;
            if row_str(&row, "tags")
                .unwrap_or_default()
                .contains("type:domain")
            {
                return Err(RuntimeError::InvalidInput(
                    "properties-only target is a domain mirror; use domain verbs instead".into(),
                ));
            }
            return Ok(Some(id));
        }
    };
    let slug = input.slug.trim();
    // Tombstones and domain mirrors still own the unique namespace/slug entry.
    let existing = reader.query_row(SqlStatement {
        sql: "SELECT id, deleted_at, tags FROM knowledge_atoms WHERE namespace = ?1 AND slug = ?2 LIMIT 1".into(),
        params: vec![SqlValue::Text(namespace.to_owned()), SqlValue::Text(slug.to_owned())],
        label: None,
    }).await.map_err(|error| sql_err("upsert_atoms lookup", error))?;
    let Some(row) = existing else {
        return Ok(None);
    };
    if row_str(&row, "tags")
        .unwrap_or_default()
        .contains("type:domain")
    {
        return Err(RuntimeError::InvalidInput(format!(
            "atom slug {slug:?} collides with a domain mirror; use upsert_domains instead"
        )));
    }
    if row_i64(&row, "deleted_at").is_some() {
        return Err(RuntimeError::InvalidInput(format!(
            "atom slug {slug:?} was previously deleted; choose a new slug"
        )));
    }
    row_str(&row, "id")
        .map(Some)
        .ok_or_else(|| RuntimeError::Internal("missing id in existing atom row".into()))
}

pub(super) async fn dry_run(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    atoms: &[AtomWrite],
    preserve_content_whitespace: bool,
) -> Result<Value, RuntimeError> {
    let mut failures: Vec<_> = atoms
        .iter()
        .enumerate()
        .map(|(index, atom)| {
            super::refusal::validate_submission(atom, index, preserve_content_whitespace).err()
        })
        .collect();
    if failures.iter().any(Option::is_none) {
        let sql = runtime.sql();
        let mut reader = sql
            .reader()
            .await
            .map_err(|error| sql_err("upsert_atoms reader", error))?;
        let mut slugs = HashSet::new();
        for (index, atom) in atoms.iter().enumerate() {
            if failures[index].is_some() {
                continue;
            }
            if let AtomWrite::Upsert(input) = atom {
                if slugs.contains(input.slug.trim()) {
                    continue;
                }
            }
            match target(reader.as_mut(), token.namespace().as_str(), atom).await {
                Ok(_) => {
                    if let AtomWrite::Upsert(input) = atom {
                        slugs.insert(input.slug.trim().to_owned());
                    }
                }
                Err(error @ (RuntimeError::InvalidInput(_) | RuntimeError::NotFound(_))) => {
                    failures[index] = Some(error);
                }
                Err(error) => return Err(error),
            }
        }
    }
    let would_refuse_batch = failures.iter().any(Option::is_some);
    let results: Vec<_> = atoms
        .iter()
        .zip(failures)
        .enumerate()
        .map(|(index, (atom, error))| verdict(atom, index, error))
        .collect();
    Ok(json!({"dry_run":true,"would_refuse_batch":would_refuse_batch,"results":results}))
}

fn masked(value: &str) -> String {
    mask_for_redaction_surface(RedactionSurface::GateProbe, value).into_owned()
}

fn verdict(atom: &AtomWrite, index: usize, error: Option<RuntimeError>) -> Value {
    let mut result = json!({"index":index,"would_refuse":error.is_some(),"identity_masked":false,
        "reason":null,"detector":null,"trigger":null,"masked":null,"location":null,"message":null});
    match atom {
        AtomWrite::Upsert(input) => {
            let slug = input.slug.trim();
            let identity = masked(slug);
            result["identity_masked"] = json!(identity != slug);
            result["slug"] = json!(identity);
        }
        AtomWrite::PropertiesOnly(input) => result["id"] = json!(input.id.to_string()),
    }
    if let Some(error) = error {
        result["message"] = json!(masked(&error.to_string()));
        match error {
            RuntimeError::SecretDetected(found) => {
                result["reason"] = json!("secret_detected");
                result["detector"] = json!(found.detector);
                result["trigger"] = json!(found.trigger);
                result["masked"] = json!(found.masked);
                result["location"] = json!(found.location.as_deref().map(masked));
            }
            RuntimeError::NotFound(_) => result["reason"] = json!("not_found"),
            _ => result["reason"] = json!("invalid_input"),
        }
    }
    result
}
