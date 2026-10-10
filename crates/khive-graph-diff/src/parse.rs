use std::collections::BTreeMap;

use khive_types::ParseIdError;
use serde_json::Value;

use crate::{Id128, Records, Substrate};

/// Input refusal with the substrate and one-based physical NDJSON line.
#[derive(Debug, thiserror::Error)]
pub enum DiffInputError {
    #[error("{substrate} line {line}: invalid JSON: {source}")]
    InvalidJson {
        substrate: Substrate,
        line: usize,
        #[source]
        source: serde_json::Error,
    },
    #[error("{substrate} line {line}: expected a JSON object")]
    NotObject { substrate: Substrate, line: usize },
    #[error("{substrate} line {line}: missing identity field {field}")]
    MissingIdentity {
        substrate: Substrate,
        line: usize,
        field: &'static str,
    },
    #[error("{substrate} line {line}: identity field {field} must be a string")]
    NonStringIdentity {
        substrate: Substrate,
        line: usize,
        field: &'static str,
    },
    #[error("{substrate} line {line}: invalid identity: {source}")]
    InvalidIdentity {
        substrate: Substrate,
        line: usize,
        #[source]
        source: ParseIdError,
    },
    #[error("{substrate} line {line}: duplicate identity {id}")]
    DuplicateIdentity {
        substrate: Substrate,
        line: usize,
        id: Id128,
    },
}

pub(crate) fn records(input: &str, substrate: Substrate) -> Result<Records, DiffInputError> {
    let mut records = Records::new();
    for (offset, text) in input.lines().enumerate() {
        if text.trim().is_empty() {
            continue;
        }
        let line = offset + 1;
        let value: Value =
            serde_json::from_str(text).map_err(|source| DiffInputError::InvalidJson {
                substrate,
                line,
                source,
            })?;
        let Value::Object(mut fields) = value else {
            return Err(DiffInputError::NotObject { substrate, line });
        };
        let field = substrate.identity_field();
        let identity = fields
            .remove(field)
            .ok_or(DiffInputError::MissingIdentity {
                substrate,
                line,
                field,
            })?;
        let Value::String(identity) = identity else {
            return Err(DiffInputError::NonStringIdentity {
                substrate,
                line,
                field,
            });
        };
        let id = identity
            .parse()
            .map_err(|source| DiffInputError::InvalidIdentity {
                substrate,
                line,
                source,
            })?;
        if records.contains_key(&id) {
            return Err(DiffInputError::DuplicateIdentity {
                substrate,
                line,
                id,
            });
        }
        records.insert(
            id,
            fields
                .into_iter()
                .map(|(key, value)| (key, canonical_value(value)))
                .collect(),
        );
    }
    Ok(records)
}

// Reinsert sorted keys even when serde_json's globally additive preserve_order
// feature is enabled. Arrays keep their order and all scalar types are retained.
fn canonical_value(value: Value) -> Value {
    match value {
        Value::Object(fields) => {
            let sorted: BTreeMap<_, _> = fields.into_iter().collect();
            Value::Object(
                sorted
                    .into_iter()
                    .map(|(key, value)| (key, canonical_value(value)))
                    .collect(),
            )
        }
        Value::Array(values) => Value::Array(values.into_iter().map(canonical_value).collect()),
        scalar => scalar,
    }
}
