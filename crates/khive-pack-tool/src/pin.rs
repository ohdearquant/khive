//! The registry definition approved by a grant or checked by an executor.

use khive_runtime::{KhiveRuntime, RuntimeError};
use khive_storage::{Entity, SqlReader, SqlStatement, SqlValue, StorageError};
use serde_json::{json, Value};
use uuid::Uuid;

/// Only these four properties affect a grant's definition pin. Missing
/// properties are represented explicitly as JSON null, like tool.describe.
pub fn registry_policy_inputs(entity: &Entity) -> Value {
    policy_inputs(entity.properties.as_ref().unwrap_or(&Value::Null))
}

fn policy_inputs(properties: &Value) -> Value {
    json!({
        "source": properties.get("source").unwrap_or(&Value::Null),
        "side_effect": properties.get("side_effect").unwrap_or(&Value::Null),
        "trust": properties.get("trust").unwrap_or(&Value::Null),
        "schema": properties.get("schema").unwrap_or(&Value::Null),
    })
}

/// A registry row identity and the digest of the exact canonical policy-input
/// bytes consumed by a grant or executor. The bytes are not a wire field.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RegistryPin {
    registry_id: Uuid,
    definition_digest: String,
    canonical_bytes: Vec<u8>,
}

impl RegistryPin {
    pub fn from_entity(entity: &Entity) -> Result<Self, RuntimeError> {
        Self::from_inputs(entity.id, &registry_policy_inputs(entity))
    }

    fn from_inputs(entity_id: Uuid, inputs: &Value) -> Result<Self, RuntimeError> {
        let bytes = khive_types::canonical_json_bytes(inputs)
            .map_err(|error| RuntimeError::InvalidInput(format!("tool definition: {error}")))?;
        Ok(Self::from_canonical_bytes(entity_id, bytes))
    }

    /// The caller must use khive_types::canonical_json_bytes on
    /// registry_policy_inputs. Hash the supplied bytes without reserializing.
    pub fn from_canonical_bytes(entity_id: Uuid, bytes: Vec<u8>) -> Self {
        Self {
            registry_id: entity_id,
            definition_digest: blake3::hash(&bytes).to_hex().to_string(),
            canonical_bytes: bytes,
        }
    }

    pub fn registry_id(&self) -> Uuid {
        self.registry_id
    }

    pub fn definition_digest(&self) -> &str {
        &self.definition_digest
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        &self.canonical_bytes
    }
}

/// Keep the persisted spelling for cheap writer-side snapshot revalidation;
/// parsing and canonical serialization happen before writer admission.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RegistrationSnapshot {
    pub id: Uuid,
    pub name: String,
    properties: Option<String>,
}

impl RegistrationSnapshot {
    pub(crate) fn pin(&self) -> Result<RegistryPin, RuntimeError> {
        let properties = self
            .properties
            .as_deref()
            .map(serde_json::from_str::<Value>)
            .transpose()
            .map_err(|error| RuntimeError::InvalidInput(format!("tool definition: {error}")))?
            .unwrap_or(Value::Null);
        RegistryPin::from_inputs(self.id, &policy_inputs(&properties))
    }
}

pub(crate) async fn registration_snapshot<R: SqlReader + ?Sized>(
    reader: &mut R,
    namespace: &str,
    name: &str,
) -> Result<Option<RegistrationSnapshot>, StorageError> {
    let row = reader
        .query_row(SqlStatement {
            sql: "SELECT id, name, properties FROM entities \
                  WHERE namespace = ?1 AND kind = 'project' AND deleted_at IS NULL \
                    AND CAST(lower(name) AS BLOB) = ?2 \
                    AND EXISTS (SELECT 1 FROM json_each(entities.tags) \
                                WHERE lower(json_each.value) = 'tool-registry') \
                  ORDER BY CASE WHEN name = ?3 COLLATE BINARY THEN 0 ELSE 1 END, \
                           created_at DESC, id ASC LIMIT 1"
                .into(),
            params: vec![
                SqlValue::Text(namespace.into()),
                SqlValue::Blob(name.to_ascii_lowercase().into_bytes()),
                SqlValue::Text(name.into()),
            ],
            label: Some("tool_registry_snapshot".into()),
        })
        .await?;
    row.map(|row| {
        let (Some(SqlValue::Text(id)), Some(SqlValue::Text(name))) =
            (row.get("id"), row.get("name"))
        else {
            return Err(StorageError::Internal(
                "invalid tool registry identity".into(),
            ));
        };
        let properties = match row.get("properties") {
            Some(SqlValue::Text(value)) => Some(value.clone()),
            Some(SqlValue::Null) => None,
            _ => {
                return Err(StorageError::Internal(
                    "invalid tool registry properties".into(),
                ))
            }
        };
        Ok(RegistrationSnapshot {
            id: Uuid::parse_str(id)
                .map_err(|_| StorageError::Internal("invalid tool registry UUID".into()))?,
            name: name.clone(),
            properties,
        })
    })
    .transpose()
}

pub(crate) async fn current_registration(
    rt: &KhiveRuntime,
    namespace: &str,
    name: &str,
) -> Result<Option<RegistrationSnapshot>, RuntimeError> {
    let mut reader = rt.sql().reader().await?;
    Ok(registration_snapshot(reader.as_mut(), namespace, name).await?)
}

pub(crate) async fn invalidating_registration<R: SqlReader + ?Sized>(
    reader: &mut R,
    namespace: &str,
    pattern: &str,
) -> Result<Option<(String, i64)>, StorageError> {
    let prefix = pattern.strip_suffix('*').map_or(SqlValue::Null, |prefix| {
        SqlValue::Blob(prefix.as_bytes().to_vec())
    });
    let row = reader.query_row(SqlStatement {
        sql: "SELECT id, created_at FROM entities \
              WHERE namespace = ?1 AND kind = 'project' AND deleted_at IS NULL \
                AND EXISTS (SELECT 1 FROM json_each(entities.tags) WHERE lower(value) = 'tool-registry') \
                AND (CAST(lower(name) AS BLOB) = ?2 \
                     OR (?3 IS NOT NULL AND substr(CAST(name AS BLOB), 1, length(?3)) = ?3)) \
              ORDER BY (CAST(lower(name) AS BLOB) = ?2) DESC, created_at, id LIMIT 1".into(),
        params: vec![SqlValue::Text(namespace.into()), SqlValue::Blob(pattern.to_ascii_lowercase().into_bytes()), prefix],
        label: Some("tool_grant_invalidation_evidence".into()),
    }).await?;
    row.map(|row| match (row.get("id"), row.get("created_at")) {
        (Some(SqlValue::Text(id)), Some(SqlValue::Integer(created_at))) => {
            Ok((id.clone(), *created_at))
        }
        _ => Err(StorageError::Internal(
            "invalid registry invalidation evidence".into(),
        )),
    })
    .transpose()
}
