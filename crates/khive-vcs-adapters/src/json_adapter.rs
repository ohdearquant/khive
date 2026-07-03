// Copyright 2026 Haiyang Li. Licensed under Apache-2.0.
//
//! JSON array format adapter.

use crate::adapter::FormatAdapter;
use crate::error::AdapterError;
use crate::record::{EdgeRecord, EntityRecord};
use khive_types::{EdgeRelation, EntityKind};
use serde_json::Value;
use std::str::FromStr;
use uuid::Uuid;

/// A [`FormatAdapter`] that parses a JSON array of objects.
///
/// Entities and edges may be mixed in the same array — the adapter dispatches
/// by checking for `source` + `target` keys (edge) vs. their absence (entity).
///
/// Construct with [`JsonFormatAdapter::new`], passing the raw JSON bytes.
/// The constructor parses eagerly; iteration is cheap once constructed.
pub struct JsonFormatAdapter {
    entities: Vec<Result<EntityRecord, AdapterError>>,
    edges: Vec<Result<EdgeRecord, AdapterError>>,
    warnings: Vec<String>,
}

impl JsonFormatAdapter {
    /// Parse `json_input` and return a ready adapter.
    ///
    /// Returns `Err(AdapterError::Parse)` if `json_input` is not valid JSON or
    /// is not a JSON array at the top level.
    pub fn new(json_input: &str) -> Result<Self, AdapterError> {
        let value: Value =
            serde_json::from_str(json_input).map_err(|e| AdapterError::Parse(e.to_string()))?;

        let array = match value {
            Value::Array(a) => a,
            _ => {
                return Err(AdapterError::Parse(
                    "expected a JSON array at the top level".into(),
                ))
            }
        };

        let mut entities = Vec::new();
        let mut edges = Vec::new();
        let mut warnings = Vec::new();

        for (index, item) in array.into_iter().enumerate() {
            let obj = match item {
                Value::Object(m) => m,
                other => {
                    warnings.push(format!(
                        "record {index}: expected an object, got {}; skipped",
                        other.type_str()
                    ));
                    continue;
                }
            };

            // Normalise keys to lowercase once for dispatch detection.
            // Keys are matched case-insensitively.
            let has_source = obj.keys().any(|k| {
                let l = k.to_ascii_lowercase();
                l == "source" || l == "from"
            });
            let has_target = obj.keys().any(|k| {
                let l = k.to_ascii_lowercase();
                l == "target" || l == "to"
            });

            if has_source && has_target {
                edges.push(parse_edge(index, obj, &mut warnings));
            } else {
                entities.push(parse_entity(index, obj, &mut warnings));
            }
        }

        Ok(Self {
            entities,
            edges,
            warnings,
        })
    }
}

impl FormatAdapter for JsonFormatAdapter {
    fn name(&self) -> &str {
        "json"
    }

    fn entities(&mut self) -> impl Iterator<Item = Result<EntityRecord, AdapterError>> {
        self.entities.drain(..)
    }

    fn edges(&mut self) -> impl Iterator<Item = Result<EdgeRecord, AdapterError>> {
        self.edges.drain(..)
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Remove a key from the map case-insensitively.
///
/// Looks for the first key whose ASCII-lowercase form equals `field_lower`.
/// Returns `(original_key, value)` if found, `None` otherwise.
fn remove_ci(
    obj: &mut serde_json::Map<String, Value>,
    field_lower: &str,
) -> Option<(String, Value)> {
    let key = obj
        .keys()
        .find(|k| k.to_ascii_lowercase() == field_lower)
        .cloned()?;
    let val = obj.remove(&key)?;
    Some((key, val))
}

/// Extract a required non-empty string field, case-insensitive.
fn extract_required_string(
    obj: &mut serde_json::Map<String, Value>,
    index: usize,
    field: &str,
) -> Result<String, AdapterError> {
    match remove_ci(obj, field) {
        Some((_, Value::String(s))) if !s.is_empty() => Ok(s),
        Some(_) => Err(AdapterError::InvalidField {
            index,
            field: field.into(),
            reason: "must be a non-empty string".into(),
        }),
        None => Err(AdapterError::MissingField {
            index,
            field: field.into(),
        }),
    }
}

/// Extract an optional UUID field, generating a new one if absent.
fn extract_uuid_field(
    obj: &mut serde_json::Map<String, Value>,
    index: usize,
    field: &str,
) -> Result<Uuid, AdapterError> {
    match remove_ci(obj, field) {
        Some((_, Value::String(s))) => s.parse::<Uuid>().map_err(|e| AdapterError::InvalidField {
            index,
            field: field.into(),
            reason: e.to_string(),
        }),
        Some(_) => Err(AdapterError::InvalidField {
            index,
            field: field.into(),
            reason: "must be a UUID string".into(),
        }),
        None => Ok(Uuid::new_v4()),
    }
}

/// Extract and validate an edge weight field.
fn extract_weight(
    obj: &mut serde_json::Map<String, Value>,
    index: usize,
) -> Result<f64, AdapterError> {
    match remove_ci(obj, "weight") {
        Some((_, Value::Number(n))) => {
            let w = n.as_f64().ok_or_else(|| AdapterError::InvalidField {
                index,
                field: "weight".into(),
                reason: "weight is not a finite f64".into(),
            })?;
            if !w.is_finite() || !(0.0..=1.0).contains(&w) {
                return Err(AdapterError::InvalidField {
                    index,
                    field: "weight".into(),
                    reason: format!("must be finite and in [0.0, 1.0], got {w}"),
                });
            }
            Ok(w)
        }
        Some(_) => Err(AdapterError::InvalidField {
            index,
            field: "weight".into(),
            reason: "must be a number".into(),
        }),
        None => Ok(0.7),
    }
}

fn parse_entity(
    index: usize,
    mut obj: serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<EntityRecord, AdapterError> {
    let name = extract_required_string(&mut obj, index, "name")?;

    let raw_kind = extract_required_string(&mut obj, index, "kind")?;
    let kind = EntityKind::from_str(&raw_kind)
        .map_err(|_| AdapterError::UnknownKind {
            index,
            kind: raw_kind.clone(),
        })?
        .name()
        .to_owned();

    // ADR-020 subtype: prefer an explicit `entity_type`, else preserve a
    // `kind="paper"`-style alias (which `EntityKind::from_str` canonicalizes
    // to the base kind "document") as `entity_type="paper"`.
    let entity_type = match remove_ci(&mut obj, "entity_type") {
        Some((_, Value::String(s))) => Some(s),
        Some(_) => {
            warnings.push(format!(
                "record {index}: 'entity_type' is not a string; ignored"
            ));
            None
        }
        None if raw_kind.trim().eq_ignore_ascii_case("paper") => Some("paper".to_string()),
        None => None,
    };

    let created_at = match remove_ci(&mut obj, "created_at") {
        Some((_, Value::String(s))) => Some(s),
        Some(_) => {
            warnings.push(format!(
                "record {index}: 'created_at' is not a string; ignored"
            ));
            None
        }
        None => None,
    };
    let updated_at = match remove_ci(&mut obj, "updated_at") {
        Some((_, Value::String(s))) => Some(s),
        Some(_) => {
            warnings.push(format!(
                "record {index}: 'updated_at' is not a string; ignored"
            ));
            None
        }
        None => None,
    };

    let id = extract_uuid_field(&mut obj, index, "id")?;

    let description = match remove_ci(&mut obj, "description") {
        Some((_, Value::String(s))) => Some(s),
        Some(_) => {
            warnings.push(format!(
                "record {index}: 'description' is not a string; ignored"
            ));
            None
        }
        None => None,
    };

    let tags: Vec<String> = match remove_ci(&mut obj, "tags") {
        Some((_, Value::Array(arr))) => arr
            .into_iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s),
                _ => {
                    warnings.push(format!("record {index}: non-string tag value ignored"));
                    None
                }
            })
            .collect(),
        Some(_) => {
            warnings.push(format!("record {index}: 'tags' is not an array; ignored"));
            Vec::new()
        }
        None => Vec::new(),
    };

    let mut props_base = match remove_ci(&mut obj, "properties") {
        Some((_, Value::Object(m))) => m,
        Some((_, other)) => {
            warnings.push(format!(
                "record {index}: 'properties' is not an object (got {}); ignored",
                other.type_str()
            ));
            serde_json::Map::new()
        }
        None => serde_json::Map::new(),
    };
    for (k, v) in obj {
        props_base.insert(k, v);
    }

    Ok(EntityRecord {
        id,
        kind,
        entity_type,
        name,
        description,
        properties: Value::Object(props_base),
        tags,
        created_at,
        updated_at,
    })
}

fn parse_edge(
    index: usize,
    mut obj: serde_json::Map<String, Value>,
    warnings: &mut Vec<String>,
) -> Result<EdgeRecord, AdapterError> {
    let source = remove_ci(&mut obj, "source")
        .or_else(|| remove_ci(&mut obj, "from"))
        .and_then(|(_, v)| v.as_str().map(|s| s.to_owned()))
        .ok_or_else(|| AdapterError::MissingField {
            index,
            field: "source".into(),
        })?;

    let target = remove_ci(&mut obj, "target")
        .or_else(|| remove_ci(&mut obj, "to"))
        .and_then(|(_, v)| v.as_str().map(|s| s.to_owned()))
        .ok_or_else(|| AdapterError::MissingField {
            index,
            field: "target".into(),
        })?;

    let relation = {
        let raw = extract_required_string(&mut obj, index, "relation")?;
        EdgeRelation::from_str(&raw)
            .map_err(|_| AdapterError::UnknownRelation {
                index,
                relation: raw.clone(),
            })?
            .as_str()
            .to_owned()
    };

    let edge_id = match remove_ci(&mut obj, "edge_id").or_else(|| remove_ci(&mut obj, "id")) {
        Some((_, Value::String(s))) => {
            s.parse::<Uuid>().map_err(|e| AdapterError::InvalidField {
                index,
                field: "edge_id".into(),
                reason: e.to_string(),
            })?
        }
        Some(_) => {
            return Err(AdapterError::InvalidField {
                index,
                field: "edge_id".into(),
                reason: "must be a UUID string".into(),
            })
        }
        None => Uuid::new_v4(),
    };

    let weight = extract_weight(&mut obj, index)?;

    let created_at = match remove_ci(&mut obj, "created_at") {
        Some((_, Value::String(s))) => Some(s),
        Some(_) => {
            warnings.push(format!(
                "record {index}: edge 'created_at' is not a string; ignored"
            ));
            None
        }
        None => None,
    };
    let updated_at = match remove_ci(&mut obj, "updated_at") {
        Some((_, Value::String(s))) => Some(s),
        Some(_) => {
            warnings.push(format!(
                "record {index}: edge 'updated_at' is not a string; ignored"
            ));
            None
        }
        None => None,
    };

    let mut properties = match remove_ci(&mut obj, "properties") {
        Some((_, Value::Object(m))) => m,
        Some(_) | None => serde_json::Map::new(),
    };
    for (k, v) in obj {
        properties.insert(k, v);
    }

    Ok(EdgeRecord {
        edge_id,
        source,
        target,
        relation,
        weight,
        properties: Value::Object(properties),
        created_at,
        updated_at,
    })
}

// ---------------------------------------------------------------------------
// Helper trait: readable type name for error messages
// ---------------------------------------------------------------------------

trait TypeStr {
    fn type_str(&self) -> &'static str;
}

impl TypeStr for Value {
    fn type_str(&self) -> &'static str {
        match self {
            Value::Null => "null",
            Value::Bool(_) => "bool",
            Value::Number(_) => "number",
            Value::String(_) => "string",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        }
    }
}
