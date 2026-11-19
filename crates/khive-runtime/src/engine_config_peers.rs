use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

use super::ConfigError;
use crate::config::{parse_embedding_model_alias, sanitize_key};

/// One ordered peer. Its canonical name is also its existing provider/index identity.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct EngineConfig {
    pub name: String,
    #[serde(default = "unit_weight")]
    pub weight: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dims: Option<i64>,
}

impl EngineConfig {
    pub(crate) fn check_dimensions(&self, actual: usize) -> Result<(), ConfigError> {
        if let Some(expected) = self.dims {
            if usize::try_from(expected).ok() != Some(actual) {
                return Err(ConfigError::EngineDimensionMismatch {
                    name: self.name.clone(),
                    expected,
                    actual,
                });
            }
        }
        Ok(())
    }
}

fn unit_weight() -> f64 {
    1.0
}

pub(crate) fn canonical_engine_name(name: &str) -> String {
    parse_embedding_model_alias(name)
        .map(|model| model.to_string())
        .unwrap_or_else(|| name.to_owned())
}

pub(crate) fn validate_peer_engines(engines: &[EngineConfig]) -> Result<(), ConfigError> {
    let mut names = HashSet::new();
    let mut canonical_names = HashSet::new();
    let mut keys = HashMap::new();
    for engine in engines {
        if engine.name.trim().is_empty() {
            return Err(ConfigError::InvalidEngineName {
                name: engine.name.clone(),
            });
        }
        if !names.insert(&engine.name) {
            return Err(ConfigError::DuplicateName {
                name: engine.name.clone(),
            });
        }
        let canonical = canonical_engine_name(&engine.name);
        if !canonical_names.insert(canonical.clone()) {
            return Err(ConfigError::AliasCollision {
                name: engine.name.clone(),
                canonical,
            });
        }
        let key = sanitize_key(&canonical);
        if let Some(other) = keys.insert(key.clone(), canonical.clone()) {
            return Err(ConfigError::EngineKeyCollision {
                name: canonical,
                other,
                key,
            });
        }
        if !engine.weight.is_finite() || engine.weight <= 0.0 {
            return Err(ConfigError::InvalidEngineWeight {
                name: engine.name.clone(),
                value: engine.weight,
            });
        }
        if let Some(value) = engine.dims {
            if value <= 0 || value > i64::from(u32::MAX) {
                return Err(ConfigError::InvalidEngineDimensions {
                    name: engine.name.clone(),
                    value,
                });
            }
        }
    }
    if let Some(engine) = engines.iter().find(|engine| engine.weight != 1.0) {
        return Err(ConfigError::UnsupportedEngineWeight {
            name: engine.name.clone(),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct LegacyEngine {
    name: String,
    model: String,
    #[serde(default)]
    default: bool,
    fusion_weight: Option<f64>,
    dims: Option<i64>,
}

/// Convert only legacy input. Canonical deserialization remains a closed schema.
pub(super) fn normalize_engine_input(
    document: &mut toml::Value,
    path: &std::path::Path,
) -> Result<bool, ConfigError> {
    let Some(value) = document.get_mut("engines") else {
        return Ok(false);
    };
    let Some(entries) = value.as_array_mut() else {
        // Preserve the typed deserializer's ordinary type diagnostic.
        return Ok(true);
    };
    let legacy = entries.iter().any(|entry| {
        ["model", "default", "fusion_weight"]
            .iter()
            .any(|key| entry.get(key).is_some())
    });
    if !legacy {
        return Ok(true);
    }
    let mut parsed = Vec::with_capacity(entries.len());
    let mut labels = HashSet::new();
    let mut providers = HashSet::new();
    for entry in entries.iter() {
        if entry.get("weight").is_some() || entry.get("model").is_none() {
            return Err(ConfigError::EngineKeyConflict {
                name: entry
                    .get("name")
                    .and_then(toml::Value::as_str)
                    .unwrap_or("<missing>")
                    .to_owned(),
            });
        }
        let old: LegacyEngine = entry
            .clone()
            .try_into()
            .map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
        if !labels.insert(old.name.clone()) {
            return Err(ConfigError::DuplicateName { name: old.name });
        }
        let model =
            parse_embedding_model_alias(&old.model).ok_or_else(|| ConfigError::UnknownModel {
                name: old.name.clone(),
                model: old.model.clone(),
            })?;
        let name = model.to_string();
        if !providers.insert(name.clone()) {
            return Err(ConfigError::AliasCollision {
                name: old.name,
                canonical: name,
            });
        }
        parsed.push((
            old.default,
            EngineConfig {
                name,
                weight: old.fusion_weight.unwrap_or(1.0),
                dims: old.dims,
            },
        ));
    }
    let found = parsed.iter().filter(|(default, _)| *default).count();
    if found != 1 {
        return Err(ConfigError::DefaultCount { found });
    }
    let first = parsed
        .iter()
        .position(|(default, _)| *default)
        .expect("one default");
    let primary = parsed.remove(first);
    parsed.insert(0, primary);
    *entries = parsed
        .into_iter()
        .map(|(_, engine)| {
            let mut table = toml::map::Map::new();
            table.insert("name".into(), toml::Value::String(engine.name));
            table.insert("weight".into(), toml::Value::Float(engine.weight));
            if let Some(dims) = engine.dims {
                table.insert("dims".into(), toml::Value::Integer(dims));
            }
            toml::Value::Table(table)
        })
        .collect();
    tracing::warn!("legacy engine configuration converted to ordered peers; migrate model/default/fusion_weight to name/weight");
    Ok(true)
}
