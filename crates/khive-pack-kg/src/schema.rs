use khive_runtime::{base_entity_endpoint_rules, RuntimeError, VerbRegistry};
use khive_types::{canonical_json_bytes, EdgeRelation, EndpointKind, Hash32};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SchemaParams {}

// Field order is the canonical declaration-order key, not a deduplication key.
#[derive(Serialize, PartialEq, Eq, PartialOrd, Ord)]
struct EndpointRow {
    origin: String,
    source_substrate: &'static str,
    source: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    source_entity_type: Option<&'static str>,
    relation: &'static str,
    target_substrate: &'static str,
    target: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target_entity_type: Option<&'static str>,
}

fn endpoint(kind: EndpointKind) -> (&'static str, &'static str, Option<&'static str>) {
    match kind {
        EndpointKind::EntityOfKind(kind) => ("entity", kind, None),
        EndpointKind::NoteOfKind(kind) => ("note", kind, None),
        EndpointKind::EntityOfType { kind, entity_type } => ("entity", kind, Some(entity_type)),
    }
}

#[derive(Serialize)]
struct Counts {
    entity_kinds: usize,
    note_kinds: usize,
    edge_relations: usize,
    endpoint_rules: usize,
    packs_loaded: usize,
}

#[derive(Serialize)]
struct Schema<'a> {
    entity_kinds: Vec<&'static str>,
    note_kinds: Vec<&'static str>,
    edge_relations: Vec<&'static str>,
    endpoint_rules: Vec<EndpointRow>,
    packs_loaded: Vec<&'a str>,
    counts: Counts,
}

pub(crate) fn handle_schema(params: Value, registry: &VerbRegistry) -> Result<Value, RuntimeError> {
    if !params.is_object() {
        return Err(RuntimeError::InvalidInput(
            "schema expects an empty object".into(),
        ));
    }
    let _: SchemaParams = serde_json::from_value(params)
        .map_err(|error| RuntimeError::InvalidInput(error.to_string()))?;
    let mut entity_kinds = registry.all_entity_kinds();
    let mut note_kinds = registry.all_note_kinds();
    let mut packs_loaded = registry.pack_names();
    entity_kinds.sort_unstable();
    entity_kinds.dedup();
    note_kinds.sort_unstable();
    note_kinds.dedup();
    packs_loaded.sort_unstable();
    packs_loaded.dedup();
    let mut edge_relations: Vec<_> = EdgeRelation::ALL
        .iter()
        .map(|relation| relation.as_str())
        .collect();
    edge_relations.sort_unstable();
    let mut endpoint_rules: Vec<_> = base_entity_endpoint_rules()
        .iter()
        .map(|&(source, relation, target)| EndpointRow {
            origin: "base".into(),
            source_substrate: "entity",
            source,
            source_entity_type: None,
            relation: relation.as_str(),
            target_substrate: "entity",
            target,
            target_entity_type: None,
        })
        .collect();
    for (owner, rule) in registry.all_edge_rules_with_packs() {
        let (source_substrate, source, source_entity_type) = endpoint(rule.source);
        let (target_substrate, target, target_entity_type) = endpoint(rule.target);
        endpoint_rules.push(EndpointRow {
            origin: format!("pack:{owner}"),
            source_substrate,
            source,
            source_entity_type,
            relation: rule.relation.as_str(),
            target_substrate,
            target,
            target_entity_type,
        });
    }
    endpoint_rules.sort();
    let counts = Counts {
        entity_kinds: entity_kinds.len(),
        note_kinds: note_kinds.len(),
        edge_relations: edge_relations.len(),
        endpoint_rules: endpoint_rules.len(),
        packs_loaded: packs_loaded.len(),
    };
    let mut value = serde_json::to_value(Schema {
        entity_kinds,
        note_kinds,
        edge_relations,
        endpoint_rules,
        packs_loaded,
        counts,
    })
    .map_err(|error| RuntimeError::Internal(error.to_string()))?;
    let canonical =
        canonical_json_bytes(&value).map_err(|error| RuntimeError::Internal(error.to_string()))?;
    value["contract_version"] = Value::String(Hash32::from_blake3(&canonical).to_string());
    Ok(value)
}
