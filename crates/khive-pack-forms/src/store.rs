//! Store access for the forms verbs: a snapshot of the forms graph for reads,
//! and one transaction for every write.

use std::collections::{BTreeMap, HashMap};

use khive_changeset::{
    ChangeSet, CreateOp, CreateTarget, EntityCreateFields, EntityPatch, EntityPreimage, Envelope,
    LinkOp, NoteCreateFields, Op, UpdateOp, UpdatePatch, UpdatePreimage,
};
use khive_runtime::{
    AtomicRunOutcome, EdgeListFilter, KhiveRuntime, NamespaceToken, RuntimeError, VerbRegistry,
};
use khive_storage::Entity;
use khive_types::{EdgeRelation, EntityKind, Id128, Namespace, PropertyValue, Timestamp};
use serde_json::{Map, Value};
use uuid::Uuid;

use crate::text::normalize;
use crate::vocab::{
    self, FIELD, FIELD_VALUE, FORM, FORM_ISSUER, FORM_ITEM, FORM_RULE, FORM_SECTION,
};

const ENTITY_PAGE: u32 = 500;
const EDGE_PAGE: u32 = 1000;

/// The relations the forms graph is drawn with.
pub(crate) const GRAPH_RELATIONS: [EdgeRelation; 8] = [
    EdgeRelation::Contains,
    EdgeRelation::VariantOf,
    EdgeRelation::LinksTo,
    EdgeRelation::CompetesWith,
    EdgeRelation::ComposedWith,
    EdgeRelation::Precedes,
    EdgeRelation::DependsOn,
    EdgeRelation::Enables,
];

pub(crate) fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidInput(message.into())
}

/// A string or a number, as text; anything else is absent.
pub(crate) fn scalar(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// The entity's properties as an object, empty when it has none.
pub(crate) fn props(entity: &Entity) -> Map<String, Value> {
    match &entity.properties {
        Some(Value::Object(map)) => map.clone(),
        _ => Map::new(),
    }
}

pub(crate) fn prop<'a>(entity: &'a Entity, key: &str) -> Option<&'a Value> {
    entity.properties.as_ref()?.get(key)
}

/// A string or number property as non-blank text.
pub(crate) fn prop_text(entity: &Entity, key: &str) -> Option<String> {
    prop(entity, key)
        .and_then(scalar)
        .filter(|s| !s.trim().is_empty())
}

/// The strings of a list value; absent or other shapes read as empty.
pub(crate) fn strings(value: Option<&Value>) -> Vec<String> {
    value
        .and_then(Value::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn aliases(entity: &Entity) -> Vec<String> {
    strings(prop(entity, "aliases"))
}

/// A general field keeps a shared wording and contains the fields it may mean.
pub(crate) fn is_general(entity: &Entity) -> bool {
    prop(entity, "general").and_then(Value::as_bool) == Some(true)
}

pub(crate) fn type_of(entity: &Entity) -> &str {
    entity.entity_type.as_deref().unwrap_or("")
}

/// Every wording that names a field: its canonical name and its aliases, folded.
pub(crate) fn wordings(entity: &Entity) -> Vec<String> {
    let mut out = vec![normalize(&entity.name)];
    for alias in aliases(entity) {
        let folded = normalize(&alias);
        if !out.contains(&folded) {
            out.push(folded);
        }
    }
    out
}

pub(crate) fn definition(entity: &Entity) -> Option<String> {
    prop_text(entity, "definition").or_else(|| entity.description.clone())
}

/// The printed label of a section or item.
pub(crate) fn label(entity: &Entity) -> String {
    prop_text(entity, "label").unwrap_or_else(|| entity.name.clone())
}

/// The premises a rule could not bind when it was loaded: each a field wording
/// and an optional value name, waiting for that wording to be decided.
pub(crate) fn pending_premises(rule: &Entity) -> Vec<(String, Option<String>)> {
    prop(rule, "premises_pending")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|entry| {
                    let field = entry.get("field")?.as_str()?.to_owned();
                    let value = entry
                        .get("value")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    Some((field, value))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// One pending premise as a rule stores it.
pub(crate) fn pending_premise(field: &str, value: Option<&str>) -> Value {
    match value {
        Some(value) => serde_json::json!({"field": field, "value": value}),
        None => serde_json::json!({"field": field}),
    }
}

/// The content key of one form revision.
pub(crate) fn form_key(form_id: &str, revision: &str) -> String {
    format!("form:{form_id}@{revision}")
}

pub(crate) fn quoted_names(entities: &[&Entity]) -> String {
    entities
        .iter()
        .map(|e| format!("\"{}\"", e.name))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Every live forms node of a namespace and the forms edges between them.
pub(crate) struct Graph {
    nodes: HashMap<Uuid, Entity>,
    edges: Vec<(Uuid, EdgeRelation, Uuid)>,
}

impl Graph {
    pub(crate) async fn load(
        runtime: &KhiveRuntime,
        token: &NamespaceToken,
    ) -> Result<Self, RuntimeError> {
        let mut nodes = HashMap::new();
        for entity_type in vocab::ALL_TYPES {
            let mut offset = 0;
            loop {
                let page = runtime
                    .list_entities(
                        token,
                        Some("concept"),
                        Some(entity_type),
                        ENTITY_PAGE,
                        offset,
                    )
                    .await?;
                let read = page.len() as u32;
                for entity in page {
                    if entity.deleted_at.is_none()
                        && entity.merged_into.is_none()
                        && entity.entity_type.as_deref() == Some(entity_type)
                    {
                        nodes.insert(entity.id, entity);
                    }
                }
                if read < ENTITY_PAGE {
                    break;
                }
                offset += read;
            }
        }
        let mut edges = Vec::new();
        let mut offset = 0;
        loop {
            let filter = EdgeListFilter {
                relations: GRAPH_RELATIONS.to_vec(),
                ..Default::default()
            };
            let page = runtime.list_edges(token, filter, EDGE_PAGE, offset).await?;
            let read = page.len() as u32;
            for edge in page {
                if edge.deleted_at.is_none()
                    && nodes.contains_key(&edge.source_id)
                    && nodes.contains_key(&edge.target_id)
                {
                    edges.push((edge.source_id, edge.relation, edge.target_id));
                }
            }
            if read < EDGE_PAGE {
                break;
            }
            offset += read;
        }
        Ok(Self { nodes, edges })
    }

    pub(crate) fn get(&self, id: Uuid) -> Option<&Entity> {
        self.nodes.get(&id)
    }

    pub(crate) fn is(&self, id: Uuid, entity_type: &str) -> bool {
        self.get(id).is_some_and(|e| type_of(e) == entity_type)
    }

    pub(crate) fn nodes(&self) -> impl Iterator<Item = &Entity> {
        self.nodes.values()
    }

    pub(crate) fn edges(&self) -> &[(Uuid, EdgeRelation, Uuid)] {
        &self.edges
    }

    fn sorted(&self, ids: impl Iterator<Item = Uuid>) -> Vec<Uuid> {
        let mut ids: Vec<Uuid> = ids.filter(|id| self.nodes.contains_key(id)).collect();
        ids.sort_by(|a, b| self.nodes[a].name.cmp(&self.nodes[b].name).then(a.cmp(b)));
        ids.dedup();
        ids
    }

    /// Live nodes of one type, by name.
    pub(crate) fn of_type(&self, entity_type: &str) -> Vec<&Entity> {
        let mut out: Vec<&Entity> = self
            .nodes
            .values()
            .filter(|e| type_of(e) == entity_type)
            .collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        out
    }

    pub(crate) fn out(&self, id: Uuid, relation: EdgeRelation) -> Vec<Uuid> {
        self.sorted(
            self.edges
                .iter()
                .filter(|(s, r, _)| *s == id && *r == relation)
                .map(|(_, _, t)| *t),
        )
    }

    pub(crate) fn incoming(&self, id: Uuid, relation: EdgeRelation) -> Vec<Uuid> {
        self.sorted(
            self.edges
                .iter()
                .filter(|(_, r, t)| *t == id && *r == relation)
                .map(|(s, _, _)| *s),
        )
    }

    /// Both directions, for the symmetric relations.
    pub(crate) fn either(&self, id: Uuid, relation: EdgeRelation) -> Vec<Uuid> {
        self.sorted(
            self.out(id, relation)
                .into_iter()
                .chain(self.incoming(id, relation)),
        )
    }

    /// The content key a node is exported under; absent when the properties
    /// it is made of are missing.
    pub(crate) fn key(&self, id: Uuid) -> Option<String> {
        let e = self.get(id)?;
        Some(match type_of(e) {
            FORM_ISSUER => format!("issuer:{}", normalize(&e.name)),
            FORM => form_key(&prop_text(e, "form_id")?, &prop_text(e, "revision")?),
            FORM_SECTION => format!(
                "{}/section:{}",
                prop_text(e, "form_key")?,
                normalize(&label(e))
            ),
            FORM_ITEM => format!(
                "{}/item:{}:{}",
                prop_text(e, "form_key")?,
                prop_text(e, "order")?,
                normalize(&label(e))
            ),
            FIELD => format!("field:{}", normalize(&e.name)),
            FIELD_VALUE => {
                let parent = self.parent_field(id)?;
                format!("{}/value:{}", self.key(parent)?, normalize(&e.name))
            }
            FORM_RULE => format!(
                "{}/rule:{}:{}",
                prop_text(e, "form_key")?,
                prop_text(e, "source_page")?,
                normalize(&prop_text(e, "text").unwrap_or_else(|| e.name.clone()))
            ),
            _ => return None,
        })
    }

    pub(crate) fn form_by_key(&self, key: &str) -> Option<&Entity> {
        self.of_type(FORM)
            .into_iter()
            .find(|f| self.key(f.id).as_deref() == Some(key))
    }

    /// The form a section, item or rule belongs to.
    pub(crate) fn form_of(&self, node: &Entity) -> Option<&Entity> {
        prop_text(node, "form_key").and_then(|key| self.form_by_key(&key))
    }

    /// A form by id, by form id, or by `form_id@revision`.
    pub(crate) fn find_form(&self, spec: &str) -> Result<&Entity, RuntimeError> {
        if let Ok(id) = Uuid::parse_str(spec) {
            return self
                .get(id)
                .filter(|e| type_of(e) == FORM)
                .ok_or_else(|| invalid(format!("no form has id {spec}")));
        }
        let (form_id, revision) = match spec.rsplit_once('@') {
            Some((form_id, revision)) => (form_id, Some(revision)),
            None => (spec, None),
        };
        let hits: Vec<&Entity> = self
            .of_type(FORM)
            .into_iter()
            .filter(|e| {
                prop_text(e, "form_id").as_deref() == Some(form_id)
                    && revision.is_none_or(|r| prop_text(e, "revision").as_deref() == Some(r))
            })
            .collect();
        match hits.as_slice() {
            [one] => Ok(one),
            [] => Err(invalid(format!("no form {spec} is loaded"))),
            many => Err(invalid(format!(
                "form {form_id} is loaded at several revisions ({}); name one as {form_id}@<revision>",
                many.iter()
                    .filter_map(|e| prop_text(e, "revision"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ))),
        }
    }

    /// Live fields a wording names, as canonical name or alias.
    pub(crate) fn owners(&self, wording: &str, except: Option<Uuid>) -> Vec<&Entity> {
        let folded = normalize(wording);
        if folded.is_empty() {
            return Vec::new();
        }
        self.of_type(FIELD)
            .into_iter()
            .filter(|f| Some(f.id) != except && wordings(f).contains(&folded))
            .collect()
    }

    /// A field by id, or by the wording that names it.
    pub(crate) fn find_field(&self, spec: &str) -> Result<&Entity, RuntimeError> {
        if let Ok(id) = Uuid::parse_str(spec) {
            return self
                .get(id)
                .filter(|e| type_of(e) == FIELD)
                .ok_or_else(|| invalid(format!("no field has id {spec}")));
        }
        let folded = normalize(spec);
        let by_name: Vec<&Entity> = self
            .of_type(FIELD)
            .into_iter()
            .filter(|f| normalize(&f.name) == folded)
            .collect();
        if let [one] = by_name.as_slice() {
            return Ok(one);
        }
        let owners = self.owners(spec, None);
        match owners.as_slice() {
            [one] => Ok(one),
            [] => Err(invalid(format!("no field is named \"{spec}\""))),
            many => Err(invalid(format!(
                "\"{spec}\" names several fields ({}); pass the id of one",
                quoted_names(many)
            ))),
        }
    }

    pub(crate) fn fields_of_item(&self, item: Uuid) -> Vec<Uuid> {
        self.out(item, EdgeRelation::VariantOf)
            .into_iter()
            .filter(|f| self.is(*f, FIELD))
            .collect()
    }

    pub(crate) fn items_of_field(&self, field: Uuid) -> Vec<Uuid> {
        self.incoming(field, EdgeRelation::VariantOf)
            .into_iter()
            .filter(|i| self.is(*i, FORM_ITEM))
            .collect()
    }

    /// A field's values in their printed order.
    pub(crate) fn values_of_field(&self, field: Uuid) -> Vec<Uuid> {
        let mut values: Vec<Uuid> = self
            .out(field, EdgeRelation::Contains)
            .into_iter()
            .filter(|v| self.is(*v, FIELD_VALUE))
            .collect();
        values.sort_by_key(|v| {
            self.get(*v)
                .and_then(|e| prop(e, "order"))
                .and_then(Value::as_i64)
                .unwrap_or(i64::MAX)
        });
        values
    }

    /// The fields a general field contains.
    pub(crate) fn children_of_field(&self, field: Uuid) -> Vec<Uuid> {
        self.out(field, EdgeRelation::Contains)
            .into_iter()
            .filter(|f| self.is(*f, FIELD))
            .collect()
    }

    pub(crate) fn parent_field(&self, value: Uuid) -> Option<Uuid> {
        self.incoming(value, EdgeRelation::Contains)
            .into_iter()
            .find(|f| self.is(*f, FIELD))
    }

    /// Every item inside a form or section, nested sections included.
    pub(crate) fn items_in(&self, container: Uuid) -> Vec<Uuid> {
        let mut items = Vec::new();
        let mut pending = vec![container];
        let mut seen = vec![container];
        while let Some(next) = pending.pop() {
            for child in self.out(next, EdgeRelation::Contains) {
                if seen.contains(&child) {
                    continue;
                }
                seen.push(child);
                if self.is(child, FORM_ITEM) {
                    items.push(child);
                } else if self.is(child, FORM_SECTION) {
                    pending.push(child);
                }
            }
        }
        items
    }
}

fn id128(id: Uuid) -> Id128 {
    Id128::from_bytes(*id.as_bytes())
}

fn property_map(map: Map<String, Value>) -> Result<BTreeMap<String, PropertyValue>, RuntimeError> {
    serde_json::from_value(Value::Object(map))
        .map_err(|e| invalid(format!("a property value cannot be stored: {e}")))
}

/// What a committed write reports beside success.
pub(crate) struct Committed {
    /// Set when the rows committed but search indexing did not finish.
    pub(crate) index_warning: Option<String>,
}

/// The staged operations of one forms write, committed together or not at all.
pub(crate) struct Writes {
    namespace: Namespace,
    ops: Vec<Op>,
}

impl Writes {
    pub(crate) fn new(token: &NamespaceToken) -> Self {
        Self {
            namespace: token.namespace().clone(),
            ops: Vec::new(),
        }
    }

    pub(crate) fn create_entity(
        &mut self,
        entity_type: &str,
        name: &str,
        description: Option<&str>,
        properties: Map<String, Value>,
    ) -> Result<Uuid, RuntimeError> {
        let id = Uuid::new_v4();
        self.create_entity_with_id(id, entity_type, name, description, properties)?;
        Ok(id)
    }

    pub(crate) fn create_entity_with_id(
        &mut self,
        id: Uuid,
        entity_type: &str,
        name: &str,
        description: Option<&str>,
        properties: Map<String, Value>,
    ) -> Result<(), RuntimeError> {
        self.ops.push(Op::Create(CreateOp {
            id: id128(id),
            namespace: self.namespace.clone(),
            target: CreateTarget::Entity(EntityCreateFields {
                entity_kind: EntityKind::Concept,
                entity_type: Some(entity_type.to_owned()),
                name: name.to_owned(),
                description: description.map(str::to_owned),
                properties: property_map(properties)?,
                tags: Vec::new(),
                key: None,
                aliases: vec![],
            }),
        }));
        Ok(())
    }

    pub(crate) fn create_note(
        &mut self,
        note_kind: &str,
        content: &str,
        properties: Map<String, Value>,
    ) -> Result<Uuid, RuntimeError> {
        let id = Uuid::new_v4();
        self.ops.push(Op::Create(CreateOp {
            id: id128(id),
            namespace: self.namespace.clone(),
            target: CreateTarget::Note(NoteCreateFields {
                note_kind: note_kind.to_owned(),
                content: content.to_owned(),
                properties: property_map(properties)?,
                tags: Vec::new(),
                salience: None,
                decay_factor: None,
            }),
        }));
        Ok(id)
    }

    pub(crate) fn link(&mut self, source: Uuid, relation: EdgeRelation, target: Uuid) {
        self.ops.push(Op::Link(LinkOp {
            id: id128(Uuid::new_v4()),
            namespace: self.namespace.clone(),
            source: id128(source),
            target: id128(target),
            relation,
            weight: 1.0,
            properties: BTreeMap::new(),
        }));
    }

    /// Stage an update of `before`, carrying the touched fields' current values
    /// so the write refuses if the row changed since it was read.
    pub(crate) fn update_entity(
        &mut self,
        before: &Entity,
        name: Option<&str>,
        description: Option<&str>,
        properties: Option<Map<String, Value>>,
    ) -> Result<(), RuntimeError> {
        let patch = EntityPatch {
            name: name.map(str::to_owned),
            description: description.map(|d| Some(d.to_owned())),
            properties: properties.map(property_map).transpose()?,
            tags: None,
            entity_type: None,
            aliases: None,
        };
        let preimage = EntityPreimage {
            name: name.map(|_| before.name.clone()),
            description: description.map(|_| before.description.clone()),
            properties: match patch.properties {
                Some(_) => Some(property_map(props(before))?),
                None => None,
            },
            tags: None,
            entity_type: None,
            aliases: None,
        };
        let op = UpdateOp::new(
            id128(before.id),
            UpdatePatch::Entity(patch),
            UpdatePreimage::Entity(preimage),
        )
        .map_err(invalid)?;
        self.ops.push(Op::Update(op));
        Ok(())
    }

    /// Commit every staged operation in one transaction.
    pub(crate) async fn commit(
        self,
        runtime: &KhiveRuntime,
        registry: &VerbRegistry,
        token: &NamespaceToken,
    ) -> Result<Committed, RuntimeError> {
        if self.ops.is_empty() {
            return Ok(Committed {
                index_warning: None,
            });
        }
        let staged_at = Timestamp::from_secs(chrono::Utc::now().timestamp().max(0) as u64);
        let producer = format!("{}:{}", token.actor().kind, token.actor().id);
        let changeset = ChangeSet::new(Envelope::new(producer, "forms", staged_at), self.ops);
        let plans =
            khive_runtime::prepare_canonical_changeset(runtime, registry, token, &changeset)
                .await
                .map_err(|error| match error {
                    RuntimeError::CanonicalAdmission { op_index, source } => invalid(format!(
                "the forms write was refused at operation {op_index} and wrote nothing: {source}"
            )),
                    other => other,
                })?;
        match khive_runtime::run_atomic_unit(runtime.sql().as_ref(), plans).await {
            Err(error) => Err(RuntimeError::Storage(error.0)),
            Ok(AtomicRunOutcome::RolledBack {
                failed_op_index,
                failure,
            }) => Err(invalid(format!(
                "the forms write rolled back at operation {failed_op_index} and wrote nothing: {failure:?}"
            ))),
            Ok(AtomicRunOutcome::Committed { post_commit }) => {
                let index_warning =
                    khive_runtime::atomic_prepare::apply_post_commit_effects_with_report(
                        runtime,
                        token,
                        post_commit,
                    )
                    .await
                    .err()
                    .map(|e| format!("written, but indexing for search did not finish: {e}"));
                Ok(Committed { index_warning })
            }
        }
    }
}
