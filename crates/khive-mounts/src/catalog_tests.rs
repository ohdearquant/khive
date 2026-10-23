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
fn catalog_pin_preserves_canonical_definition_bytes() {
    let tool = json!({
        "name":"A", "description":"one",
        "inputSchema":{"type":"object", "properties":{"x":{"type":"string"}}},
        "outputSchema":{"type":"object"}
    });
    let expected = br#"{"description":"one","inputSchema":{"properties":{"x":{"type":"string"}},"type":"object"},"name":"A","outputSchema":{"type":"object"}}"#;
    assert_eq!(
        pin(&config(), &[tool], 1).unwrap()[0].digest,
        blake3::hash(expected).to_hex().to_string(),
    );
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

#[test]
fn local_schema_references_validate_instances_without_external_resolvers() {
    let schema = json!({
        "$schema": "https://json-schema.org/draft/2020-12/schema",
        "$defs": {"value": {"type": "integer", "minimum": 1}},
        "type": "object",
        "properties": {"value": {"$ref": "#/$defs/value"}},
        "required": ["value"]
    });
    validate_schema(&schema).expect("local schema references are supported");
    assert!(validates(&schema, &json!({"value": 1})));
    assert!(!validates(&schema, &json!({"value": 0})));
    assert!(!validates(&schema, &json!({"value": "1"})));
    assert!(!validates(&schema, &json!({})));
}
