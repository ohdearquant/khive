//! The working type graph (ADR-222 section 2, Amendment 2 A2.10): the nodes and edges that
//! show the published definitions in the type namespace, built from the published bytes of each
//! definition's shown version.
//!
//! A node is one per key and its key carries no version. An operation type and a published step
//! type show the highest version published under their id; a shorthand step type, and every
//! slot, requirement and edge, belong to the definition that holds them and follow its shown
//! version. Every derived read works from published bytes, never from this graph.

use std::collections::BTreeMap;

use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind, EntityKind, EntityTypeDef};
use serde_json::{json, Map, Value};

use crate::definition::{OperationType, Refusal, StepType};

/// An operation type, by its id.
pub(crate) const OPERATION_TYPE: &str = "operation_type";
/// A slot of one operation type.
pub(crate) const SLOT: &str = "slot";
/// A step type, published alone or given inside an operation type.
pub(crate) const STEP_TYPE: &str = "step_type";
/// A value a step type requires.
pub(crate) const REQUIREMENT: &str = "requirement";
/// The forms pack's field, which requirements and products name.
const FIELD: &str = "field";

/// ADR-221's longest key.
pub(crate) const MAX_KEY_BYTES: usize = 512;

const fn concept(type_name: &'static str) -> EntityTypeDef {
    EntityTypeDef {
        kind: EntityKind::Concept,
        type_name,
        aliases: &[],
    }
}

/// The node types this pack writes.
pub(crate) static OPERATION_ENTITY_TYPES: [EntityTypeDef; 4] = [
    concept(OPERATION_TYPE),
    concept(SLOT),
    concept(STEP_TYPE),
    concept(REQUIREMENT),
];

const fn typed(entity_type: &'static str) -> EndpointKind {
    EndpointKind::EntityOfType {
        kind: "concept",
        entity_type,
    }
}

const fn rule(
    relation: EdgeRelation,
    source: &'static str,
    target: &'static str,
) -> EdgeEndpointRule {
    EdgeEndpointRule {
        relation,
        source: typed(source),
        target: typed(target),
    }
}

/// The typed edges of the working graph (A2.10).
pub(crate) static OPERATION_EDGE_RULES: [EdgeEndpointRule; 5] = [
    rule(EdgeRelation::Contains, OPERATION_TYPE, SLOT),
    rule(EdgeRelation::Contains, OPERATION_TYPE, STEP_TYPE),
    rule(EdgeRelation::Contains, STEP_TYPE, REQUIREMENT),
    rule(EdgeRelation::LinksTo, REQUIREMENT, FIELD),
    rule(EdgeRelation::LinksTo, STEP_TYPE, FIELD),
];

/// A field name or a name in a vocabulary, written so it adds no segment to a key.
pub(crate) fn escape(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2F")
}

/// The key of an operation type or a step type. A shorthand step type's id is
/// `<operation type>/<step>`, so its key is `operation/step_type/<operation type>/<step>`.
pub(crate) fn definition_key(entity_type: &str, id: &str) -> String {
    format!("operation/{entity_type}/{id}")
}

pub(crate) fn slot_key(operation_type: &str, slot: &str) -> String {
    format!("operation/{SLOT}/{operation_type}/{slot}")
}

/// A requirement's key: its step type as that step type's key writes it, its slot (empty for
/// a requirement about the operation) and its field, escaped.
pub(crate) fn requirement_key(step_type: &str, slot: Option<&str>, field: &str) -> String {
    format!(
        "operation/{REQUIREMENT}/{step_type}/{}/{}",
        slot.unwrap_or(""),
        escape(field)
    )
}

/// The key a name in a vocabulary is held under (section 7, A2.10). A slot and a shorthand
/// step type are named within their operation type.
pub(crate) fn name_key(
    vocabulary: &str,
    entity_type: &str,
    within: Option<&str>,
    name: &str,
) -> String {
    match within {
        Some(owner) => format!("operation/{vocabulary}/{entity_type}/{owner}/{}", escape(name)),
        None => format!("operation/{vocabulary}/{entity_type}/{}", escape(name)),
    }
}

/// Refuse a key longer than ADR-221 allows, naming what it would key.
pub(crate) fn check_key_length(key: &str, what: &str) -> Result<(), Refusal> {
    if key.len() > MAX_KEY_BYTES {
        return Err(Refusal::new(
            "invalid_name",
            format!(
                "{what} would be keyed by {} bytes, more than the {MAX_KEY_BYTES} a key may hold; use a shorter name",
                key.len()
            ),
        ));
    }
    Ok(())
}

/// One node the shown versions hold.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct NodeSpec {
    pub entity_type: &'static str,
    pub name: String,
    pub properties: Map<String, Value>,
}

/// What an edge points at: a node of this graph by key, or a forms field by canonical name.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Target {
    Node(String),
    Field(String),
}

/// One edge: its source node's key, its relation and its target. The store holds one edge per
/// (source, target, relation), so the reasons for one are a list in its properties.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct EdgeKey {
    pub source: String,
    pub relation: &'static str,
    pub target: Target,
}

pub(crate) fn relation_of(name: &str) -> EdgeRelation {
    match name {
        "links_to" => EdgeRelation::LinksTo,
        _ => EdgeRelation::Contains,
    }
}

/// The nodes and edges one or more definitions hold.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct TypeGraph {
    pub nodes: BTreeMap<String, NodeSpec>,
    pub edges: BTreeMap<EdgeKey, Map<String, Value>>,
}

fn props(value: Value) -> Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => Map::new(),
    }
}

impl TypeGraph {
    fn node(&mut self, key: String, entity_type: &'static str, name: &str, properties: Value) {
        self.nodes.insert(
            key,
            NodeSpec {
                entity_type,
                name: name.to_owned(),
                properties: props(properties),
            },
        );
    }

    fn edge(&mut self, source: &str, relation: &'static str, target: Target, properties: Value) {
        self.edges.insert(
            EdgeKey {
                source: source.to_owned(),
                relation,
                target,
            },
            props(properties),
        );
    }

    /// Add another definition's nodes and edges.
    pub fn extend(&mut self, other: TypeGraph) {
        self.nodes.extend(other.nodes);
        self.edges.extend(other.edges);
    }

    /// A step type node, its requirement nodes, and the fields it requires and produces, from
    /// the bytes of the version it shows. A product of one field about several slots is one
    /// `produces` edge naming every slot; `slots` is empty for a value about the operation.
    pub fn of_step_type(step: &StepType, digest: &str) -> Self {
        let mut graph = Self::default();
        let key = definition_key(STEP_TYPE, &step.id);
        graph.node(
            key.clone(),
            STEP_TYPE,
            &step.id,
            json!({
                "step_type_id": step.id,
                "step": step.name,
                "version": step.version,
                "digest": digest,
                "act": step.act,
                "by": step.by,
                "quantifier": step.quantifier.map(|q| q.as_str()),
            }),
        );
        for requirement in &step.requires {
            let requirement_key =
                requirement_key(&step.id, requirement.slot.as_deref(), &requirement.field);
            graph.node(
                requirement_key.clone(),
                REQUIREMENT,
                &requirement.field,
                json!({
                    "step_type_id": step.id,
                    "field": requirement.field,
                    "slot": requirement.slot,
                    "value_type": requirement.value_type,
                }),
            );
            graph.edge(
                &key,
                "contains",
                Target::Node(requirement_key.clone()),
                json!({}),
            );
            graph.edge(
                &requirement_key,
                "links_to",
                Target::Field(requirement.field.clone()),
                json!({"role": "requires"}),
            );
        }
        let mut produced: BTreeMap<&str, Vec<String>> = BTreeMap::new();
        for product in &step.produces {
            let slots = produced.entry(product.field.as_str()).or_default();
            if let Some(slot) = &product.slot {
                if !slots.contains(slot) {
                    slots.push(slot.clone());
                }
            }
        }
        for (field, mut slots) in produced {
            slots.sort();
            graph.edge(
                &key,
                "links_to",
                Target::Field(field.to_owned()),
                json!({"role": "produces", "slots": slots}),
            );
        }
        graph
    }

    /// An operation type node, its slot nodes, and one `pins` edge per step type it pins,
    /// carrying the version and digest pinned and, for each step that pins it, the order the
    /// published bytes hold.
    pub fn of_operation_type(type_: &OperationType, digest: &str) -> Self {
        let mut graph = Self::default();
        let key = definition_key(OPERATION_TYPE, &type_.id);
        graph.node(
            key.clone(),
            OPERATION_TYPE,
            &type_.id,
            json!({
                "operation_type_id": type_.id,
                "version": type_.version,
                "digest": digest,
            }),
        );
        for slot in &type_.slots {
            let slot_key = slot_key(&type_.id, &slot.name);
            graph.node(
                slot_key.clone(),
                SLOT,
                &slot.name,
                json!({
                    "operation_type_id": type_.id,
                    "slot": slot.name,
                    "repeats": slot.repeats,
                    "required": slot.required,
                    "kind": slot.kind,
                    "role": slot.role,
                    "qualifier": slot.qualifier,
                }),
            );
            graph.edge(&key, "contains", Target::Node(slot_key), json!({}));
        }
        let mut pins: BTreeMap<&str, (i64, &str, Vec<Value>)> = BTreeMap::new();
        for step in &type_.steps {
            let entry = pins.entry(step.step_type.id.as_str()).or_insert((
                step.step_type.version,
                step.step_type.digest.as_str(),
                Vec::new(),
            ));
            entry.2.push(json!({
                "name": step.name,
                "after": step.after,
                "derived_after": step.derived_after,
                "supplies": step.supplies,
            }));
        }
        for (step_type, (version, digest, steps)) in pins {
            graph.edge(
                &key,
                "contains",
                Target::Node(definition_key(STEP_TYPE, step_type)),
                json!({
                    "role": "pins",
                    "version": version,
                    "digest": digest,
                    "steps": steps,
                }),
            );
        }
        graph
    }
}

#[cfg(test)]
#[path = "graph_tests.rs"]
mod tests;
