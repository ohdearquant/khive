use std::collections::{BTreeMap, BTreeSet};

use khive_runtime::{mount_config::MountConfig, mounted_verb::MountedVerb};
use serde_json::{json, Value};

use crate::error::Failure;

pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let sorted: BTreeMap<_, _> = map
                .iter()
                .map(|(key, value)| (key.clone(), canonical(value)))
                .collect();
            serde_json::to_value(sorted).expect("JSON values serialize")
        }
        Value::Array(values) => Value::Array(values.iter().map(canonical).collect()),
        _ => value.clone(),
    }
}

fn local_references(value: &Value) -> bool {
    match value {
        Value::Object(map) => map.iter().all(|(key, value)| {
            if matches!(key.as_str(), "$ref" | "$dynamicRef" | "$recursiveRef") {
                value.as_str().is_some_and(|value| value.starts_with('#'))
            } else {
                local_references(value)
            }
        }),
        Value::Array(values) => values.iter().all(local_references),
        _ => true,
    }
}

pub(crate) fn validate_schema(schema: &Value) -> Result<(), Failure> {
    if !local_references(schema) || jsonschema::validator_for(schema).is_err() {
        return Err(Failure::malformed());
    }
    Ok(())
}

pub(crate) fn validates(schema: &Value, value: &Value) -> bool {
    local_references(schema)
        && jsonschema::validator_for(schema).is_ok_and(|validator| validator.is_valid(value))
}

pub(crate) fn pin(
    config: &MountConfig,
    published: &[Value],
    generation: i64,
) -> Result<Vec<MountedVerb>, Failure> {
    let mut catalog = BTreeMap::new();
    for entry in published {
        let name = entry
            .get("name")
            .and_then(Value::as_str)
            .ok_or_else(Failure::malformed)?;
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || catalog.insert(name, entry).is_some()
        {
            return Err(Failure::malformed());
        }
    }
    let mut pins = Vec::with_capacity(config.tools.len());
    for tool in &config.tools {
        let entry = catalog
            .get(tool.name.as_str())
            .ok_or_else(|| Failure::error("configured_tool_missing"))?;
        let input = entry
            .get("inputSchema")
            .filter(|value| value.is_object())
            .ok_or_else(Failure::malformed)?;
        validate_schema(input)?;
        let description = match entry.get("description") {
            None => None,
            Some(Value::String(value)) => Some(value.clone()),
            _ => return Err(Failure::malformed()),
        };
        let output = entry.get("outputSchema").cloned();
        if let Some(schema) = &output {
            validate_schema(schema)?;
        }
        let definition = json!({"name": tool.name, "description": description, "inputSchema": input, "outputSchema": output});
        let bytes = serde_json::to_vec(&canonical(&definition)).expect("JSON values serialize");
        pins.push(MountedVerb {
            name: tool.name.clone(),
            description,
            input_schema: input.clone(),
            output_schema: output,
            effect: tool.effect,
            digest: blake3::hash(&bytes).to_hex().to_string(),
            generation,
        });
    }
    Ok(pins)
}

pub(crate) fn diff(old: &[MountedVerb], new: &[MountedVerb]) -> Value {
    let old: BTreeMap<_, _> = old.iter().map(|tool| (&tool.name, tool)).collect();
    let new: BTreeMap<_, _> = new.iter().map(|tool| (&tool.name, tool)).collect();
    let old_names: BTreeSet<_> = old.keys().copied().collect();
    let new_names: BTreeSet<_> = new.keys().copied().collect();
    let changed: Vec<_> = old_names
        .intersection(&new_names)
        .filter(|name| {
            old[**name].digest != new[**name].digest || old[**name].effect != new[**name].effect
        })
        .collect();
    json!({"added": new_names.difference(&old_names).collect::<Vec<_>>(), "removed": old_names.difference(&new_names).collect::<Vec<_>>(), "changed": changed})
}

#[cfg(test)]
mod tests {
    use super::*;
    use khive_runtime::mount_config::{MountEffect, MountToolConfig};

    fn config() -> MountConfig {
        MountConfig {
            name: "demo".into(),
            transport: "stdio".into(),
            command: "fixture".into(),
            args: vec![],
            env: vec![],
            credential: None,
            tools: vec![MountToolConfig {
                name: "A".into(),
                effect: MountEffect::Mutating,
            }],
            timeout_ms: 30000,
        }
    }
    #[test]
    fn canonical_digest_covers_all_four_fields_but_ignores_object_order() {
        let cfg = config();
        let original: Value = serde_json::from_str(r#"{"name":"A","description":"one","inputSchema":{"type":"object","properties":{"x":{"type":"string"}}},"outputSchema":{"type":"object"}}"#).unwrap();
        let reordered: Value = serde_json::from_str(r#"{"outputSchema":{"type":"object"},"inputSchema":{"properties":{"x":{"type":"string"}},"type":"object"},"description":"one","name":"A"}"#).unwrap();
        let digest = pin(&cfg, std::slice::from_ref(&original), 1).unwrap()[0]
            .digest
            .clone();
        assert_eq!(pin(&cfg, &[reordered], 2).unwrap()[0].digest, digest);
        for (field, value) in [
            ("description", json!("two")),
            ("inputSchema", json!({"type":"object"})),
            (
                "outputSchema",
                json!({"type":"object", "required":["answer"]}),
            ),
        ] {
            let mut changed = original.clone();
            changed[field] = value;
            assert_ne!(pin(&cfg, &[changed], 1).unwrap()[0].digest, digest);
        }
        let mut renamed = original;
        renamed["name"] = json!("B");
        let mut renamed_config = cfg;
        renamed_config.tools[0].name = "B".into();
        assert_ne!(
            pin(&renamed_config, &[renamed], 1).unwrap()[0].digest,
            digest
        );
    }
    #[test]
    fn remote_schema_references_and_duplicate_identifiers_are_refused() {
        let cfg = config();
        let tool = json!({"name":"A","inputSchema":{"type":"object", "$ref":"https://invalid.example/schema"}});
        assert!(pin(&cfg, &[tool], 1).is_err());
        let tool = json!({"name":"A","inputSchema":{"type":"object"}});
        assert!(pin(&cfg, &[tool.clone(), tool], 1).is_err());
    }
}
