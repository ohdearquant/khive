//! The working type graph's write (ADR-222 Amendment 2 A2.10). After a define's publish
//! commits, the graph of every id the document names is derived again from the published rows
//! and written in a unit of its own.
//!
//! The publish stands whatever this write does: each id's outcome is reported as written,
//! unchanged or failed. The shown version is read from `operation_definitions`, never taken
//! from the version just published. A write commits only while the definition node it read
//! is unchanged: every write that changes anything asserts the node as it was read and moves
//! it by raising `graph_revision`, so of two writes that read the same state, the one that
//! commits second reads again. Each id is read and tried up to three times.

use std::collections::{BTreeMap, BTreeSet};

use khive_changeset::{
    ChangeSet, CreateOp, CreateTarget, EntityCreateFields, EntityPatch, EntityPreimage, Envelope,
    LinkOp, Op, UpdateOp, UpdatePatch, UpdatePreimage,
};
use khive_runtime::{
    AtomicOpPlan, AtomicRunOutcome, EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError,
    VerbRegistry,
};
use khive_storage::{Edge, Entity, SqlValue};
use khive_types::{EdgeRelation, EntityKind, Id128, Namespace, PropertyValue, Timestamp};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::definition::{Document, OperationType, Refusal, StepType};
use crate::graph::{
    check_key_length, check_products, definition_key, names_in_source, relation_of,
    requirement_key, slot_key, EdgeKey, NameSpec, NodeSpec, Target, TypeGraph, FIELD,
    GRAPH_REVISION, OPERATION_TYPE, REQUIREMENT, SLOT, STEP_TYPE,
};
use crate::pack::{checked_definition, col_i64, col_opt_text, col_text, forms_read, stmt, text};

/// How many times one id is read and tried before its write is reported failed.
const ATTEMPTS: usize = 3;

const EDGE_PAGE: u32 = 1000;

/// A step type given inside an operation type has the id `<operation type>/<step>`; a
/// published step type's id holds no `/`.
fn is_shorthand(step_type_id: &str) -> bool {
    step_type_id.contains('/')
}

fn reason(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn id128(id: Uuid) -> Id128 {
    Id128::from_bytes(*id.as_bytes())
}

fn property_map(map: Map<String, Value>) -> Result<BTreeMap<String, PropertyValue>, String> {
    serde_json::from_value(Value::Object(map))
        .map_err(|e| format!("a property value cannot be stored: {e}"))
}

/// Properties as this pack writes them: a value that is absent is left out, not stored null.
fn clean(properties: &Map<String, Value>) -> Map<String, Value> {
    properties
        .iter()
        .filter(|(_, value)| !value.is_null())
        .map(|(name, value)| (name.clone(), value.clone()))
        .collect()
}

fn stored_properties(entity: &Entity) -> Map<String, Value> {
    entity
        .properties
        .as_ref()
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default()
}

fn prop_str(entity: &Entity, name: &str) -> Option<String> {
    entity
        .properties
        .as_ref()?
        .get(name)?
        .as_str()
        .map(str::to_owned)
}

/// The ids a document names, step types first: those it publishes on their own and those its
/// steps pin, then its operation types, each once.
pub(crate) fn targets(document: &Document) -> Vec<(&'static str, String)> {
    let mut step_types: BTreeSet<String> = document
        .step_types
        .iter()
        .map(|stored| stored.id.clone())
        .collect();
    for compiled in &document.operation_types {
        for step in &compiled.definition.steps {
            if !is_shorthand(&step.step_type.id) {
                step_types.insert(step.step_type.id.clone());
            }
        }
    }
    let mut out: Vec<(&'static str, String)> =
        step_types.into_iter().map(|id| (STEP_TYPE, id)).collect();
    let mut seen = BTreeSet::new();
    for compiled in &document.operation_types {
        if seen.insert(compiled.definition.id.clone()) {
            out.push((OPERATION_TYPE, compiled.definition.id.clone()));
        }
    }
    out
}

fn source_names(kind: &str, source: &str) -> Vec<NameSpec> {
    serde_json::from_str::<Value>(source)
        .map(|source| names_in_source(kind, &source))
        .unwrap_or_default()
}

/// Refuse, before anything is published, a node key or a name key longer than a key may hold,
/// one name the document gives two nodes, and a name another node holds (section 7, A2.10). A
/// name stays on the node that holds it while that node is shown, so a name is never moved to
/// another node.
pub(crate) async fn check_document(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    document: &Document,
) -> Result<Result<(), Refusal>, RuntimeError> {
    let mut graph = TypeGraph::default();
    let mut names = Vec::new();
    let step_graph = |stored: &crate::definition::Stored| {
        serde_json::from_slice::<StepType>(&stored.bytes)
            .map(|step| TypeGraph::of_step_type(&step, &stored.digest))
            .unwrap_or_default()
    };
    for stored in &document.step_types {
        graph.extend(step_graph(stored));
        names.extend(source_names(STEP_TYPE, &stored.source));
    }
    for compiled in &document.operation_types {
        graph.extend(TypeGraph::of_operation_type(
            &compiled.definition,
            &compiled.operation_type.digest,
        ));
        for stored in &compiled.step_types {
            graph.extend(step_graph(stored));
        }
        names.extend(source_names(
            OPERATION_TYPE,
            &compiled.operation_type.source,
        ));
    }
    for (key, spec) in &graph.nodes {
        if let Err(refusal) = check_key_length(key, &format!("{} {}", spec.entity_type, spec.name))
        {
            return Ok(Err(refusal));
        }
    }
    if let Err(refusal) = check_products(&graph) {
        return Ok(Err(refusal));
    }
    let mut claimed: BTreeMap<&str, &NameSpec> = BTreeMap::new();
    for name in &names {
        let what = format!(
            "the name \"{}\" in vocabulary \"{}\"",
            name.name, name.vocabulary
        );
        if let Err(refusal) = check_key_length(&name.key, &what) {
            return Ok(Err(refusal));
        }
        if let Some(other) = claimed.insert(name.key.as_str(), name) {
            if other.node != name.node {
                return Ok(Err(Refusal::new(
                    "key_conflict",
                    format!(
                        "the document gives {what} to {} and to {}; a name in a vocabulary names one node of its type",
                        other.node, name.node
                    ),
                )));
            }
        }
    }
    for (key, name) in claimed {
        let Some(holder) = runtime.get_entity_by_key(token, "concept", key).await? else {
            continue;
        };
        let node = runtime
            .get_entity_by_key(token, "concept", &name.node)
            .await?;
        if node.map(|node| node.id) != Some(holder.id) {
            return Ok(Err(Refusal::new(
                "key_conflict",
                format!(
                    "{} is known in vocabulary \"{}\" as \"{}\", which already names {} \"{}\" (existing_id {})",
                    name.node,
                    name.vocabulary,
                    name.name,
                    holder.entity_type.as_deref().unwrap_or("entity"),
                    holder.name,
                    holder.id
                ),
            )));
        }
    }
    Ok(Ok(()))
}

/// Write the graph of each target in turn and report each one's outcome.
pub(crate) async fn write_graphs(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    targets: &[(&'static str, String)],
) -> Vec<Value> {
    let mut out = Vec::with_capacity(targets.len());
    for (kind, id) in targets {
        out.push(write_graph(runtime, registry, token, kind, id).await);
    }
    out
}

fn outcome(kind: &str, id: &str, status: &str) -> Value {
    json!({"id": id, "kind": kind, "status": status})
}

/// Write one id's graph: read, plan and commit, reading again when the commit finds that
/// something it read moved, up to [`ATTEMPTS`] times.
pub(crate) async fn write_graph(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    kind: &str,
    id: &str,
) -> Value {
    let mut last = String::new();
    for _ in 0..ATTEMPTS {
        let planned = match plan(runtime, registry, token, kind, id).await {
            Ok(planned) => planned,
            Err(error) => {
                last = error;
                continue;
            }
        };
        if planned.plans.is_empty() {
            let mut reply = outcome(kind, id, "unchanged");
            reply["shown_version"] = json!(planned.shown);
            return reply;
        }
        match commit(runtime, token, planned.plans).await {
            Ok(warning) => {
                let mut reply = outcome(kind, id, "written");
                reply["shown_version"] = json!(planned.shown);
                if let Some(warning) = warning {
                    reply["warning"] = json!(warning);
                }
                return reply;
            }
            Err(error) => last = error,
        }
    }
    let mut reply = outcome(kind, id, "failed");
    reply["reason"] = json!(last);
    reply
}

/// One id's write, planned against what was read: empty when the graph already holds what the
/// shown version gives.
pub(crate) struct Planned {
    pub shown: i64,
    pub plans: Vec<AtomicOpPlan>,
}

struct Version {
    version: i64,
    digest: String,
    bytes: Vec<u8>,
    source: Value,
}

/// Every published version of one id, highest first, each read back against its digest.
async fn versions(
    runtime: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    id: &str,
) -> Result<Vec<Version>, String> {
    let mut reader = runtime.sql().reader().await.map_err(reason)?;
    let rows = reader
        .query_all(stmt(
            include_str!("../sql/definition-versions.sql"),
            vec![text(namespace), text(id)],
        ))
        .await
        .map_err(reason)?;
    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        let held_kind = col_text(row, "kind").map_err(reason)?;
        if held_kind != kind {
            return Err(format!("{id} is published in {namespace} as {held_kind}"));
        }
        let digest = col_text(row, "digest").map_err(reason)?;
        let bytes = checked_definition(row, kind, &digest).map_err(reason)?;
        let source = col_opt_text(row, "source_bytes")
            .map_err(reason)?
            .and_then(|source| serde_json::from_str(&source).ok())
            .unwrap_or(Value::Null);
        out.push(Version {
            version: col_i64(row, "version").map_err(reason)?,
            digest,
            bytes,
            source,
        });
    }
    Ok(out)
}

/// The graph one published version's bytes give: a step type's own, or an operation type's
/// with the graphs of the step types given inside it, read at their pins.
async fn version_graph(
    runtime: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    id: &str,
    version: &Version,
) -> Result<TypeGraph, String> {
    if kind == STEP_TYPE {
        let step: StepType = serde_json::from_slice(&version.bytes)
            .map_err(|e| format!("{id} v{}: {e}", version.version))?;
        return Ok(TypeGraph::of_step_type(&step, &version.digest));
    }
    let type_: OperationType = serde_json::from_slice(&version.bytes)
        .map_err(|e| format!("{id} v{}: {e}", version.version))?;
    let mut graph = TypeGraph::of_operation_type(&type_, &version.digest);
    let mut reader = runtime.sql().reader().await.map_err(reason)?;
    for pin in type_
        .steps
        .iter()
        .map(|step| &step.step_type)
        .filter(|pin| is_shorthand(&pin.id))
    {
        let row = reader
            .query_row(stmt(
                include_str!("../sql/definition-get.sql"),
                vec![
                    text(namespace),
                    text(&pin.id),
                    SqlValue::Integer(pin.version),
                ],
            ))
            .await
            .map_err(reason)?
            .ok_or_else(|| {
                format!(
                    "{} v{} is pinned by {id} v{} and is not published",
                    pin.id, pin.version, version.version
                )
            })?;
        let bytes = checked_definition(&row, STEP_TYPE, &pin.digest).map_err(reason)?;
        let step: StepType = serde_json::from_slice(&bytes)
            .map_err(|e| format!("{} v{}: {e}", pin.id, pin.version))?;
        graph.extend(TypeGraph::of_step_type(&step, &pin.digest));
    }
    Ok(graph)
}

/// What one id's published versions give the write.
struct Desired {
    shown: i64,
    /// The shown version's graph.
    graph: TypeGraph,
    /// The names every published version's source gives the nodes the shown graph holds.
    names: Vec<NameSpec>,
    /// The key of every node any published version's graph holds, so a node is found by its
    /// key when no edge leads to it any more.
    known: BTreeSet<String>,
}

async fn desired(
    runtime: &KhiveRuntime,
    namespace: &str,
    kind: &str,
    id: &str,
) -> Result<Desired, String> {
    let versions = versions(runtime, namespace, kind, id).await?;
    let Some(shown) = versions.first() else {
        return Err(format!("{id} is not published in {namespace}"));
    };
    let mut graph = version_graph(runtime, namespace, kind, id, shown).await?;
    let mut known: BTreeSet<String> = graph.nodes.keys().cloned().collect();
    for older in &versions[1..] {
        let older = version_graph(runtime, namespace, kind, id, older).await?;
        known.extend(older.nodes.into_keys());
    }
    let names: Vec<NameSpec> = versions
        .iter()
        .flat_map(|version| names_in_source(kind, &version.source))
        .collect();
    let names = graph.name_nodes(&names);
    Ok(Desired {
        shown: shown.version,
        graph,
        names,
        known,
    })
}

/// The nodes one definition holds in the store, by key, and the edges that leave them.
#[derive(Default)]
struct Live {
    owner: Option<Entity>,
    nodes: BTreeMap<String, Entity>,
    edges: BTreeMap<EdgeKey, (Uuid, Map<String, Value>)>,
}

/// The key a node of this graph is held under, worked out from its properties, or the field a
/// forms field entity is; `None` for any other entity.
fn target_of(entity: &Entity) -> Option<Target> {
    if entity.kind != "concept" {
        return None;
    }
    let s = |name: &str| prop_str(entity, name);
    let key = match entity.entity_type.as_deref()? {
        FIELD => return Some(Target::Field(entity.name.clone())),
        OPERATION_TYPE => definition_key(OPERATION_TYPE, &s("operation_type_id")?),
        SLOT => slot_key(&s("operation_type_id")?, &s("slot")?),
        STEP_TYPE => definition_key(STEP_TYPE, &s("step_type_id")?),
        REQUIREMENT => requirement_key(&s("step_type_id")?, s("slot").as_deref(), &s("field")?),
        _ => return None,
    };
    Some(Target::Node(key))
}

fn relation_name(relation: &EdgeRelation) -> Option<&'static str> {
    match relation {
        EdgeRelation::Contains => Some("contains"),
        EdgeRelation::LinksTo => Some("links_to"),
        _ => None,
    }
}

/// Whether `node`, reached from a node this definition holds, belongs to the definition: an
/// operation type holds its slots and the step types given inside it, and a step type holds
/// its requirements. A published step type an operation type pins is not held by it.
fn belongs(kind: &str, id: &str, node: &Entity) -> bool {
    match node.entity_type.as_deref() {
        Some(SLOT) => {
            kind == OPERATION_TYPE && prop_str(node, "operation_type_id").as_deref() == Some(id)
        }
        Some(STEP_TYPE) => {
            kind == OPERATION_TYPE
                && prop_str(node, "step_type_id")
                    .is_some_and(|step| step.starts_with(&format!("{id}/")))
        }
        Some(REQUIREMENT) => true,
        _ => false,
    }
}

async fn out_edges(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    source: Uuid,
) -> Result<Vec<Edge>, String> {
    let mut all = Vec::new();
    let mut offset = 0;
    loop {
        let filter = EdgeListFilter {
            source_id: Some(source),
            relations: vec![EdgeRelation::Contains, EdgeRelation::LinksTo],
            ..Default::default()
        };
        let page = runtime
            .list_edges(token, filter, EDGE_PAGE, offset)
            .await
            .map_err(reason)?;
        let count = page.len() as u32;
        all.extend(page);
        if count < EDGE_PAGE {
            return Ok(all);
        }
        offset += count;
    }
}

async fn live(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    kind: &str,
    id: &str,
    owner_key: &str,
    known: &BTreeSet<String>,
) -> Result<Live, String> {
    let mut live = Live::default();
    let Some(owner) = runtime
        .get_entity_by_key(token, "concept", owner_key)
        .await
        .map_err(reason)?
    else {
        return Ok(live);
    };
    live.nodes.insert(owner_key.to_owned(), owner.clone());
    let mut queue = vec![(owner_key.to_owned(), owner.clone())];
    live.owner = Some(owner);
    // A node no edge leads to any more is still this definition's: find it by its key.
    for key in known {
        if live.nodes.contains_key(key) {
            continue;
        }
        let Some(node) = runtime
            .get_entity_by_key(token, "concept", key)
            .await
            .map_err(reason)?
        else {
            continue;
        };
        let held_here = matches!(target_of(&node), Some(Target::Node(held)) if held == *key);
        if held_here && belongs(kind, id, &node) {
            live.nodes.insert(key.clone(), node.clone());
            queue.push((key.clone(), node));
        }
    }
    while let Some((source_key, node)) = queue.pop() {
        for edge in out_edges(runtime, token, node.id).await? {
            let Some(relation) = relation_name(&edge.relation) else {
                continue;
            };
            let Ok(target) = runtime.get_entity(token, edge.target_id).await else {
                continue;
            };
            let Some(target_key) = target_of(&target) else {
                continue;
            };
            let held = edge
                .metadata
                .as_ref()
                .and_then(Value::as_object)
                .cloned()
                .unwrap_or_default();
            live.edges.insert(
                EdgeKey {
                    source: source_key.clone(),
                    relation,
                    target: target_key.clone(),
                },
                (edge.id.0, held),
            );
            if let Target::Node(key) = target_key {
                if belongs(kind, id, &target) && !live.nodes.contains_key(&key) {
                    live.nodes.insert(key.clone(), target.clone());
                    queue.push((key, target));
                }
            }
        }
    }
    Ok(live)
}

async fn field_id(
    registry: &VerbRegistry,
    token: &NamespaceToken,
    name: &str,
) -> Result<Uuid, String> {
    let reply = forms_read(
        registry,
        token,
        "forms.neighbourhood",
        json!({ "node": name }),
    )
    .await
    .map_err(reason)?;
    if reply["node"]["type"].as_str() != Some("field") {
        return Err(format!("field \"{name}\" is not loaded in the forms pack"));
    }
    reply["field"]["id"]
        .as_str()
        .and_then(|id| Uuid::parse_str(id).ok())
        .ok_or_else(|| format!("the forms pack gives field \"{name}\" no id"))
}

fn create(
    namespace: &Namespace,
    id: Uuid,
    spec: &NodeSpec,
    properties: Map<String, Value>,
    key: &str,
) -> Result<Op, String> {
    Ok(Op::Create(CreateOp {
        id: id128(id),
        namespace: namespace.clone(),
        target: CreateTarget::Entity(EntityCreateFields {
            entity_kind: EntityKind::Concept,
            entity_type: Some(spec.entity_type.to_owned()),
            name: spec.name.clone(),
            description: None,
            properties: property_map(properties)?,
            tags: Vec::new(),
            key: Some(key.to_owned()),
            aliases: Vec::new(),
        }),
    }))
}

/// An update of `before`'s properties, carrying them as read so the write refuses if they
/// changed since.
fn update(before: &Entity, properties: Map<String, Value>) -> Result<Op, String> {
    let patch = EntityPatch {
        name: None,
        description: None,
        properties: Some(property_map(properties)?),
        tags: None,
        entity_type: None,
        aliases: None,
    };
    let preimage = EntityPreimage {
        name: None,
        description: None,
        properties: Some(property_map(stored_properties(before))?),
        tags: None,
        entity_type: None,
        aliases: None,
    };
    UpdateOp::new(
        id128(before.id),
        UpdatePatch::Entity(patch),
        UpdatePreimage::Entity(preimage),
    )
    .map(Op::Update)
}

/// Read the shown version and the store, and plan the write that makes the store hold the
/// shown version's graph. The plan is empty when it already does.
pub(crate) async fn plan(
    runtime: &KhiveRuntime,
    registry: &VerbRegistry,
    token: &NamespaceToken,
    kind: &str,
    id: &str,
) -> Result<Planned, String> {
    let namespace = token.namespace().clone();
    let Desired {
        shown,
        graph: desired,
        names,
        known,
    } = desired(runtime, namespace.as_str(), kind, id).await?;
    let owner_key = definition_key(kind, id);
    let Some(owner_spec) = desired.nodes.get(&owner_key) else {
        return Err(format!("{id} v{shown} gives no node for {owner_key}"));
    };
    let live = live(runtime, token, kind, id, &owner_key, &known).await?;
    let mut ids: BTreeMap<String, Uuid> = live
        .nodes
        .iter()
        .map(|(key, entity)| (key.clone(), entity.id))
        .collect();
    let owner_id = *ids.entry(owner_key.clone()).or_insert_with(Uuid::new_v4);

    let mut creates = Vec::new();
    let mut updates = Vec::new();
    for (key, spec) in &desired.nodes {
        if *key == owner_key {
            continue;
        }
        let wanted = clean(&spec.properties);
        match live.nodes.get(key) {
            None => {
                let node = Uuid::new_v4();
                ids.insert(key.clone(), node);
                creates.push(create(&namespace, node, spec, wanted, key)?);
            }
            Some(entity) if stored_properties(entity) != wanted => {
                updates.push(update(entity, wanted)?);
            }
            Some(_) => {}
        }
    }
    let dropped: Vec<Uuid> = live
        .nodes
        .iter()
        .filter(|(key, _)| **key != owner_key && !desired.nodes.contains_key(*key))
        .map(|(_, entity)| entity.id)
        .collect();
    let unlinked: Vec<Uuid> = live
        .edges
        .iter()
        .filter(|(edge, (_, held))| desired.edges.get(*edge).map(clean).as_ref() != Some(held))
        .map(|(_, (edge_id, _))| *edge_id)
        .collect();
    let mut fields: BTreeMap<String, Uuid> = BTreeMap::new();
    let mut links = Vec::new();
    for (edge, properties) in &desired.edges {
        let wanted = clean(properties);
        if live
            .edges
            .get(edge)
            .is_some_and(|(_, held)| *held == wanted)
        {
            continue;
        }
        let source = *ids
            .get(&edge.source)
            .ok_or_else(|| format!("no node is planned for {}", edge.source))?;
        let target = match &edge.target {
            Target::Node(key) => match ids.get(key) {
                Some(node) => *node,
                None => {
                    let node = runtime
                        .get_entity_by_key(token, "concept", key)
                        .await
                        .map_err(reason)?
                        .ok_or_else(|| {
                            format!("{key} has no node yet; define its step type again")
                        })?;
                    ids.insert(key.clone(), node.id);
                    node.id
                }
            },
            Target::Field(name) => match fields.get(name) {
                Some(field) => *field,
                None => {
                    let field = field_id(registry, token, name).await?;
                    fields.insert(name.clone(), field);
                    field
                }
            },
        };
        links.push(Op::Link(LinkOp {
            id: id128(Uuid::new_v4()),
            namespace: namespace.clone(),
            source: id128(source),
            target: id128(target),
            relation: relation_of(edge.relation),
            weight: 1.0,
            properties: property_map(wanted)?,
        }));
    }
    let mut keys = Vec::new();
    for name in &names {
        let Some(node) = ids.get(&name.node) else {
            continue;
        };
        let holder = runtime
            .get_entity_by_key(token, "concept", &name.key)
            .await
            .map_err(reason)?;
        if holder.map(|holder| holder.id) != Some(*node) {
            keys.push((*node, name.key.clone()));
        }
    }

    let mut owner_properties = clean(&owner_spec.properties);
    let changed = !(creates.is_empty()
        && updates.is_empty()
        && links.is_empty()
        && dropped.is_empty()
        && unlinked.is_empty()
        && keys.is_empty());
    let owner_same = live.owner.as_ref().is_some_and(|owner| {
        let mut held = stored_properties(owner);
        held.remove(GRAPH_REVISION);
        held == owner_properties
    });
    if !changed && owner_same {
        return Ok(Planned {
            shown,
            plans: Vec::new(),
        });
    }

    let mut plans = Vec::new();
    match &live.owner {
        Some(owner) => {
            // The node as it was read: a write that committed since moved it, and this one
            // reads again.
            plans.push(khive_runtime::keyed_replay_assertion_plan(owner));
            let revision = stored_properties(owner)
                .get(GRAPH_REVISION)
                .and_then(Value::as_i64)
                .unwrap_or(0);
            owner_properties.insert(GRAPH_REVISION.to_owned(), json!(revision + 1));
            updates.insert(0, update(owner, owner_properties)?);
        }
        None => {
            owner_properties.insert(GRAPH_REVISION.to_owned(), json!(1));
            creates.insert(
                0,
                create(
                    &namespace,
                    owner_id,
                    owner_spec,
                    owner_properties,
                    &owner_key,
                )?,
            );
        }
    }
    // The store holds one edge per (source, target, relation), so an edge whose properties
    // change is removed before it is linked again.
    for edge in unlinked {
        plans.push(
            khive_runtime::atomic_prepare::prepare_delete(
                runtime,
                token,
                &json!({"id": edge.to_string(), "hard": true}),
                None,
            )
            .await
            .map_err(reason)?,
        );
    }
    let ops: Vec<Op> = creates.into_iter().chain(updates).chain(links).collect();
    let staged_at = Timestamp::from_secs(chrono::Utc::now().timestamp().max(0) as u64);
    let producer = format!("{}:{}", token.actor().kind, token.actor().id);
    let changeset = ChangeSet::new(Envelope::new(producer, "operation", staged_at), ops);
    plans.extend(
        khive_runtime::prepare_canonical_changeset(runtime, registry, token, &changeset)
            .await
            .map_err(|error| match error {
                RuntimeError::CanonicalAdmission { op_index, source } => {
                    format!("the graph write was refused at operation {op_index}: {source}")
                }
                other => other.to_string(),
            })?,
    );
    for node in dropped {
        plans.push(
            khive_runtime::atomic_prepare::prepare_delete(
                runtime,
                token,
                &json!({"id": node.to_string(), "hard": false}),
                None,
            )
            .await
            .map_err(reason)?,
        );
    }
    let at = chrono::Utc::now().timestamp_micros();
    for (node, key) in keys {
        plans.extend(
            khive_runtime::entity_key_add_plans(namespace.as_str(), node, "concept", &key, at)
                .map_err(reason)?,
        );
    }
    Ok(Planned { shown, plans })
}

/// Commit a planned write in one unit. A commit whose search indexing did not finish answers
/// the warning beside success.
pub(crate) async fn commit(
    runtime: &KhiveRuntime,
    token: &NamespaceToken,
    plans: Vec<AtomicOpPlan>,
) -> Result<Option<String>, String> {
    match khive_runtime::run_atomic_unit(runtime.sql().as_ref(), plans).await {
        Err(error) => Err(format!("the graph write did not run: {}", error.0)),
        Ok(AtomicRunOutcome::RolledBack {
            failed_op_index,
            failure,
        }) => Err(format!(
            "the graph write rolled back at operation {failed_op_index}: {failure:?}"
        )),
        Ok(AtomicRunOutcome::Committed { post_commit }) => Ok(
            khive_runtime::atomic_prepare::apply_post_commit_effects_with_report(
                runtime,
                token,
                post_commit,
            )
            .await
            .err()
            .map(|e| format!("written, but indexing for search did not finish: {e}")),
        ),
    }
}

#[cfg(test)]
#[path = "graph_write_tests.rs"]
mod tests;
