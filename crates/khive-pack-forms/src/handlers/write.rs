//! The forms write verbs. Each stages its whole change and commits it in one
//! transaction, so a refused or failed write leaves nothing behind.

use std::collections::{HashMap, HashSet};

use khive_runtime::RuntimeError;
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::params::{self, Args};
use super::views::{field_reply, nearest};
use super::Ctx;
use crate::store::{
    aliases, form_key, invalid, is_general, label, prop_text, props, quoted_names, scalar, strings,
    type_of, wordings, Graph, Writes, GRAPH_RELATIONS,
};
use crate::text::normalize;
use crate::vocab::{
    self, FIELD, FIELD_VALUE, FORM, FORM_ISSUER, FORM_ITEM, FORM_RULE, FORM_SECTION, ITEM_KINDS,
    VALUE_TYPES,
};

/// The format tag of an export document, checked by replay.
pub(crate) const FORMAT: &str = "khive.forms/1";
/// The note kind that records a decision about what a wording means.
pub(crate) const DECISION: &str = "decision";
/// The properties a decision note carries; export and replay keep exactly these.
pub(crate) const LOG_KEYS: [&str; 7] = [
    "verb",
    "field",
    "wording",
    "form",
    "shared_name",
    "existing_name",
    "new_name",
];

fn map<const N: usize>(pairs: [(&str, Value); N]) -> Map<String, Value> {
    pairs
        .into_iter()
        .map(|(key, value)| (key.to_owned(), value))
        .collect()
}

struct FieldSpec {
    name: String,
    definition: String,
    value_type: String,
    aliases: Vec<String>,
    values: Vec<(String, String)>,
    exclusive: bool,
}

fn field_spec(obj: &Args) -> Result<FieldSpec, RuntimeError> {
    let name = params::text(obj, "name")?;
    let value_type = params::one_of(params::text(obj, "value_type")?, "value_type", &VALUE_TYPES)?;
    let mut values = Vec::new();
    for value in params::list(obj, "values")? {
        let value = params::entry(value, "value")?;
        values.push((
            params::text(value, "name")?,
            params::text(value, "definition")?,
        ));
    }
    if !values.is_empty() && value_type != "choice" {
        return Err(invalid(format!(
            "field \"{name}\" lists values, so its value_type must be choice"
        )));
    }
    Ok(FieldSpec {
        definition: params::text(obj, "definition")?,
        aliases: params::string_list(obj, "aliases")?,
        exclusive: obj
            .get("exclusive")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        name,
        value_type,
        values,
    })
}

/// The canonical name first, then each further wording once.
fn alias_list(name: &str, extra: &[String]) -> Vec<String> {
    let mut out = vec![name.to_owned()];
    let mut seen = vec![normalize(name)];
    for alias in extra {
        let folded = normalize(alias);
        if !folded.is_empty() && !seen.contains(&folded) {
            seen.push(folded);
            out.push(alias.clone());
        }
    }
    out
}

/// Drop each alias another live field already answers to, with a warning.
fn keep_free_aliases(graph: &Graph, spec: &mut FieldSpec, warnings: &mut Vec<String>) {
    let name = spec.name.clone();
    spec.aliases.retain(|alias| {
        let owners = graph.owners(alias, None);
        if owners.is_empty() || normalize(alias) == normalize(&name) {
            true
        } else {
            warnings.push(format!(
                "\"{alias}\" already names {}, so it was not added as an alias of \"{name}\"",
                quoted_names(&owners)
            ));
            false
        }
    });
}

/// Stage a new field with its values; returns its id and its values by folded name.
fn stage_field(
    writes: &mut Writes,
    spec: &FieldSpec,
) -> Result<(Uuid, HashMap<String, Uuid>), RuntimeError> {
    let field = writes.create_entity(
        FIELD,
        &spec.name,
        None,
        map([
            ("definition", json!(spec.definition)),
            ("value_type", json!(spec.value_type)),
            ("aliases", json!(alias_list(&spec.name, &spec.aliases))),
        ]),
    )?;
    let mut values = HashMap::new();
    let mut ordered = Vec::new();
    for (index, (name, definition)) in spec.values.iter().enumerate() {
        let folded = normalize(name);
        if values.contains_key(&folded) {
            return Err(invalid(format!(
                "field \"{}\" lists the value \"{name}\" twice",
                spec.name
            )));
        }
        let value = writes.create_entity(
            FIELD_VALUE,
            name,
            None,
            map([
                ("definition", json!(definition)),
                ("order", json!(index + 1)),
            ]),
        )?;
        writes.link(field, EdgeRelation::Contains, value);
        values.insert(folded, value);
        ordered.push(value);
    }
    if spec.exclusive {
        for (i, first) in ordered.iter().enumerate() {
            for second in &ordered[i + 1..] {
                writes.link(*first, EdgeRelation::CompetesWith, *second);
            }
        }
    }
    Ok((field, values))
}

struct ItemSpec {
    label: String,
    page: Value,
    order: Value,
    item_kind: String,
    filled_by: String,
    section: Option<String>,
    widget: Option<Value>,
}

fn item_spec(obj: &Args) -> Result<ItemSpec, RuntimeError> {
    let widget = match obj.get("widget") {
        None | Some(Value::Null) => None,
        Some(widget @ Value::Object(_)) => Some(widget.clone()),
        Some(_) => return Err(invalid("widget must be an object such as {name, rect}")),
    };
    Ok(ItemSpec {
        label: params::text(obj, "label")?,
        page: params::number(obj, "page")?,
        order: params::number(obj, "order")?,
        item_kind: params::one_of(params::text(obj, "item_kind")?, "item_kind", &ITEM_KINDS)?,
        filled_by: params::text(obj, "filled_by")?,
        section: params::opt_text(obj, "section")?,
        widget,
    })
}

fn item_key(form_key: &str, spec: &ItemSpec) -> String {
    format!(
        "{form_key}/item:{}:{}",
        scalar(&spec.order).unwrap_or_default(),
        normalize(&spec.label)
    )
}

fn item_props(spec: &ItemSpec, form_key: &str) -> Map<String, Value> {
    let mut props = map([
        ("label", json!(spec.label)),
        ("page", spec.page.clone()),
        ("order", spec.order.clone()),
        ("item_kind", json!(spec.item_kind)),
        ("filled_by", json!(spec.filled_by)),
        ("form_key", json!(form_key)),
    ]);
    if let Some(widget) = &spec.widget {
        props.insert("widget".to_owned(), widget.clone());
    }
    props
}

fn graph_values(graph: &Graph, field: Uuid) -> HashMap<String, Uuid> {
    graph
        .values_of_field(field)
        .into_iter()
        .filter_map(|value| graph.get(value).map(|e| (normalize(&e.name), value)))
        .collect()
}

fn relation_named(name: &str) -> Result<EdgeRelation, RuntimeError> {
    GRAPH_RELATIONS
        .iter()
        .copied()
        .find(|relation| relation.as_str() == name)
        .ok_or_else(|| invalid(format!("\"{name}\" is not a forms relation")))
}

#[derive(Default)]
struct Counts {
    forms: usize,
    issuers: usize,
    sections: usize,
    items: usize,
    fields: usize,
    fields_reused: usize,
    values: usize,
    rules: usize,
    relations: usize,
}

impl Counts {
    fn to_json(&self) -> Value {
        json!({
            "forms": self.forms,
            "issuers": self.issuers,
            "sections": self.sections,
            "items": self.items,
            "fields": self.fields,
            "fields_reused": self.fields_reused,
            "values": self.values,
            "rules": self.rules,
            "relations": self.relations,
        })
    }
}

/// A document field: ready with its id and values, or blocked by a live field
/// of the same name and another meaning.
type Slot = Option<(Uuid, HashMap<String, Uuid>)>;

fn document_field(
    fields: &HashMap<String, Slot>,
    graph: &Graph,
    name: &str,
) -> Result<Slot, RuntimeError> {
    if let Some(slot) = fields.get(&normalize(name)) {
        return Ok(slot.clone());
    }
    let owners = graph.owners(name, None);
    match owners.as_slice() {
        [one] if !is_general(one) => Ok(Some((one.id, graph_values(graph, one.id)))),
        [] => Err(invalid(format!(
            "field \"{name}\" is neither listed in the document nor loaded"
        ))),
        _ => Err(invalid(format!(
            "\"{name}\" may mean more than one field ({}); list the field in the document, or add the item with forms.add_item and decide",
            quoted_names(&owners)
        ))),
    }
}

fn stage_sections(
    writes: &mut Writes,
    form_key: &str,
    parent: Uuid,
    list: &[Value],
    sections: &mut HashMap<String, Uuid>,
    counts: &mut Counts,
) -> Result<(), RuntimeError> {
    for section in list {
        let section = params::entry(section, "section")?;
        let label = params::text(section, "label")?;
        let folded = normalize(&label);
        if sections.contains_key(&folded) {
            return Err(invalid(format!(
                "the document lists section \"{label}\" twice"
            )));
        }
        let id = writes.create_entity(
            FORM_SECTION,
            &label,
            None,
            map([
                ("label", json!(label)),
                ("order", params::number(section, "order")?),
                ("form_key", json!(form_key)),
            ]),
        )?;
        writes.link(parent, EdgeRelation::Contains, id);
        counts.sections += 1;
        sections.insert(folded, id);
        stage_sections(
            writes,
            form_key,
            id,
            params::list(section, "sections")?,
            sections,
            counts,
        )?;
    }
    Ok(())
}

/// forms.load: one form revision from one document, all or nothing.
pub(crate) async fn load(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let doc = params::object(&args, "document")?.ok_or_else(|| invalid("document is required"))?;
    let form_id = params::text(doc, "form_id")?;
    let revision = params::text(doc, "revision")?;
    let title = params::text(doc, "title")?;
    let source = params::object(doc, "source")?
        .ok_or_else(|| invalid("source is required: {url, sha256}"))?;
    let source_url = params::text(source, "url")?;
    let source_sha256 = params::text(source, "sha256")?;
    let key = form_key(&form_id, &revision);

    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    if let Some(existing) = graph.form_by_key(&key) {
        return Ok(json!({
            "loaded": false,
            "form": {"id": existing.id.to_string(), "key": key},
            "counts": Counts::default().to_json(),
            "items": [],
            "warnings": [format!("{key} is already loaded; nothing changed")],
        }));
    }

    let mut writes = Writes::new(ctx.token);
    let mut counts = Counts::default();
    let mut warnings = Vec::new();

    let form = writes.create_entity(
        FORM,
        &title,
        None,
        map([
            ("form_id", json!(form_id)),
            ("revision", json!(revision)),
            ("title", json!(title)),
            ("source_url", json!(source_url)),
            ("source_sha256", json!(source_sha256)),
        ]),
    )?;
    counts.forms = 1;

    if let Some(issuer) = params::object(doc, "issuer")? {
        let name = params::text(issuer, "name")?;
        let jurisdiction = params::text(issuer, "jurisdiction")?;
        let existing = graph
            .of_type(FORM_ISSUER)
            .into_iter()
            .find(|i| normalize(&i.name) == normalize(&name))
            .map(|i| i.id);
        let issuer = match existing {
            Some(id) => id,
            None => {
                counts.issuers += 1;
                writes.create_entity(
                    FORM_ISSUER,
                    &name,
                    None,
                    map([("jurisdiction", json!(jurisdiction))]),
                )?
            }
        };
        writes.link(issuer, EdgeRelation::Contains, form);
    }

    let mut fields: HashMap<String, Slot> = HashMap::new();
    for field in params::list(doc, "fields")? {
        let mut spec = field_spec(params::entry(field, "field")?)?;
        let folded = normalize(&spec.name);
        if fields.contains_key(&folded) {
            return Err(invalid(format!(
                "the document lists field \"{}\" twice, or as another field's alias",
                spec.name
            )));
        }
        let owners = graph.owners(&spec.name, None);
        let slot = match owners.as_slice() {
            [] => {
                keep_free_aliases(&graph, &mut spec, &mut warnings);
                let (id, values) = stage_field(&mut writes, &spec)?;
                counts.fields += 1;
                counts.values += values.len();
                Some((id, values))
            }
            [existing]
                if !is_general(existing)
                    && crate::store::definition(existing).map(|d| normalize(&d))
                        == Some(normalize(&spec.definition))
                    && prop_text(existing, "value_type").as_deref()
                        == Some(spec.value_type.as_str()) =>
            {
                counts.fields_reused += 1;
                let mut values = graph_values(&graph, existing.id);
                let mut next = values.len();
                for (name, definition) in &spec.values {
                    if values.contains_key(&normalize(name)) {
                        continue;
                    }
                    next += 1;
                    let value = writes.create_entity(
                        FIELD_VALUE,
                        name,
                        None,
                        map([("definition", json!(definition)), ("order", json!(next))]),
                    )?;
                    writes.link(existing.id, EdgeRelation::Contains, value);
                    values.insert(normalize(name), value);
                    counts.values += 1;
                }
                Some((existing.id, values))
            }
            _ => {
                warnings.push(format!(
                    "field \"{}\" is already named by {} with another meaning; the items that ask for it were loaded without a field. Decide each with forms.same or forms.split",
                    spec.name,
                    quoted_names(&owners)
                ));
                None
            }
        };
        for alias in &spec.aliases {
            let folded_alias = normalize(alias);
            if folded_alias != folded {
                if fields.contains_key(&folded_alias) {
                    return Err(invalid(format!(
                        "the document gives \"{alias}\" to more than one field"
                    )));
                }
                fields.insert(folded_alias, slot.clone());
            }
        }
        fields.insert(folded, slot);
    }

    let mut sections = HashMap::new();
    stage_sections(
        &mut writes,
        &key,
        form,
        params::list(doc, "sections")?,
        &mut sections,
        &mut counts,
    )?;

    let mut item_keys = HashSet::new();
    let mut items_by_label: HashMap<String, Vec<Uuid>> = HashMap::new();
    let mut pending_links = Vec::new();
    let mut item_replies = Vec::new();
    for item in params::list(doc, "items")? {
        let obj = params::entry(item, "item")?;
        let spec = item_spec(obj)?;
        if !item_keys.insert(item_key(&key, &spec)) {
            return Err(invalid(format!(
                "the document lists item \"{}\" at order {} twice",
                spec.label,
                scalar(&spec.order).unwrap_or_default()
            )));
        }
        let parent = match &spec.section {
            None => form,
            Some(section) => *sections.get(&normalize(section)).ok_or_else(|| {
                invalid(format!(
                    "item \"{}\" names section \"{section}\", which the document does not list",
                    spec.label
                ))
            })?,
        };
        let id = writes.create_entity(FORM_ITEM, &spec.label, None, item_props(&spec, &key))?;
        writes.link(parent, EdgeRelation::Contains, id);
        counts.items += 1;

        let field_name = params::opt_text(obj, "field")?;
        let value_name = params::opt_text(obj, "value")?;
        let mut bound = Value::Null;
        match (field_name, value_name) {
            (Some(field_name), value_name) => {
                if let Some((field, values)) = document_field(&fields, &graph, &field_name)? {
                    match value_name {
                        Some(value_name) => {
                            let value = values.get(&normalize(&value_name)).ok_or_else(|| {
                                invalid(format!(
                                    "item \"{}\" names value \"{value_name}\", which field \"{field_name}\" does not list",
                                    spec.label
                                ))
                            })?;
                            writes.link(id, EdgeRelation::VariantOf, *value);
                            bound = json!({"field": field_name, "value": value_name});
                        }
                        None => {
                            writes.link(id, EdgeRelation::VariantOf, field);
                            bound = json!(field_name);
                        }
                    }
                }
            }
            (None, Some(_)) => {
                return Err(invalid(format!(
                    "item \"{}\" names a value; name its field too",
                    spec.label
                )))
            }
            (None, None) => {
                if vocab::item_needs_field(&spec.item_kind) {
                    warnings.push(format!(
                        "item \"{}\" names no field; forms.lint lists it until it is bound",
                        spec.label
                    ));
                }
            }
        }
        if let Some(target) = params::opt_text(obj, "links_to")? {
            pending_links.push((id, spec.label.clone(), target));
        }
        items_by_label
            .entry(normalize(&spec.label))
            .or_default()
            .push(id);
        item_replies.push(json!({"id": id.to_string(), "label": spec.label, "field": bound}));
    }
    let unique_item = |label: &str| -> Result<Uuid, RuntimeError> {
        match items_by_label.get(&normalize(label)).map(Vec::as_slice) {
            Some([one]) => Ok(*one),
            Some([_, _, ..]) => Err(invalid(format!(
                "more than one item on {key} is labelled \"{label}\""
            ))),
            _ => Err(invalid(format!("no item on {key} is labelled \"{label}\""))),
        }
    };
    for (source, source_label, target) in pending_links {
        let target = unique_item(&target).map_err(|e| {
            invalid(format!(
                "item \"{source_label}\" links to a missing item: {e}"
            ))
        })?;
        writes.link(source, EdgeRelation::LinksTo, target);
    }

    for rule in params::list(doc, "rules")? {
        let obj = params::entry(rule, "rule")?;
        let text = params::text(obj, "text")?;
        let id = writes.create_entity(
            FORM_RULE,
            &text,
            None,
            map([
                ("text", json!(text)),
                ("source_page", params::number(obj, "page")?),
                ("form_key", json!(key)),
            ]),
        )?;
        counts.rules += 1;
        for premise in params::list(obj, "depends_on")? {
            let premise = params::entry(premise, "premise")?;
            let field_name = params::text(premise, "field")?;
            let Some((field, values)) = document_field(&fields, &graph, &field_name)? else {
                warnings.push(format!(
                    "rule \"{text}\" keeps no premise on \"{field_name}\" until that field is decided"
                ));
                continue;
            };
            match params::opt_text(premise, "value")? {
                Some(value_name) => {
                    let value = values.get(&normalize(&value_name)).ok_or_else(|| {
                        invalid(format!(
                            "rule \"{text}\" names value \"{value_name}\", which field \"{field_name}\" does not list"
                        ))
                    })?;
                    writes.link(id, EdgeRelation::DependsOn, *value);
                }
                None => writes.link(id, EdgeRelation::DependsOn, field),
            }
        }
        for conclusion in params::list(obj, "enables")? {
            let conclusion = params::entry(conclusion, "conclusion")?;
            let target = if let Some(section) = params::opt_text(conclusion, "section")? {
                *sections.get(&normalize(&section)).ok_or_else(|| {
                    invalid(format!(
                        "rule \"{text}\" names section \"{section}\", which the document does not list"
                    ))
                })?
            } else if let Some(item) = params::opt_text(conclusion, "item")? {
                unique_item(&item)?
            } else if let Some(other) = params::opt_text(conclusion, "form")? {
                if normalize(&other) == normalize(&form_id) || other == key {
                    form
                } else {
                    graph.find_form(&other)?.id
                }
            } else {
                return Err(invalid(format!(
                    "each conclusion of rule \"{text}\" names a section, an item or a form"
                )));
            };
            writes.link(id, EdgeRelation::Enables, target);
        }
    }

    for relation in params::list(doc, "relations")? {
        let obj = params::entry(relation, "relation")?;
        let name = params::one_of(
            params::text(obj, "relation")?,
            "relation",
            &["competes_with", "composed_with", "precedes", "depends_on"],
        )?;
        let target = match params::opt_text(obj, "revision")? {
            Some(revision) => format!("{}@{revision}", params::text(obj, "form")?),
            None => params::text(obj, "form")?,
        };
        match graph.find_form(&target) {
            Ok(other) => {
                writes.link(form, relation_named(&name)?, other.id);
                counts.relations += 1;
            }
            Err(error) => warnings.push(format!(
                "the {name} relation to {target} was not recorded: {error}"
            )),
        }
    }

    let committed = writes.commit(ctx.runtime, ctx.registry, ctx.token).await?;
    warnings.extend(committed.index_warning);
    Ok(json!({
        "loaded": true,
        "form": {"id": form.to_string(), "key": key},
        "counts": counts.to_json(),
        "items": item_replies,
        "warnings": warnings,
    }))
}

fn item_of<'g>(graph: &'g Graph, spec: &str) -> Result<&'g Entity, RuntimeError> {
    Uuid::parse_str(spec)
        .ok()
        .and_then(|id| graph.get(id))
        .filter(|e| type_of(e) == FORM_ITEM)
        .ok_or_else(|| {
            invalid(format!(
                "no form item has id {spec}; pass the id forms.add_item returned"
            ))
        })
}

fn refuse_bound(graph: &Graph, item: &Entity) -> Result<(), RuntimeError> {
    match graph
        .out(item.id, EdgeRelation::VariantOf)
        .first()
        .and_then(|id| graph.get(*id))
    {
        Some(target) => Err(invalid(format!(
            "item \"{}\" already means \"{}\"",
            label(item),
            target.name
        ))),
        None => Ok(()),
    }
}

fn refuse_general(graph: &Graph, field: &Entity) -> Result<(), RuntimeError> {
    if !is_general(field) {
        return Ok(());
    }
    let children: Vec<&Entity> = graph
        .children_of_field(field.id)
        .into_iter()
        .filter_map(|c| graph.get(c))
        .collect();
    Err(invalid(format!(
        "\"{}\" is a general wording that may mean {}; name one of them",
        field.name,
        quoted_names(&children)
    )))
}

fn decision_note(
    writes: &mut Writes,
    content: &str,
    properties: Map<String, Value>,
    annotates: Uuid,
) -> Result<(), RuntimeError> {
    let note = writes.create_note(DECISION, content, properties)?;
    writes.link(note, EdgeRelation::Annotates, annotates);
    Ok(())
}

/// Bind an item to a field it means the same as: the edge, the label as an
/// alias when no other field answers to it, and a logged decision.
fn bind_same(
    graph: &Graph,
    writes: &mut Writes,
    item: Uuid,
    item_label: &str,
    form: &str,
    field: &Entity,
    warnings: &mut Vec<String>,
) -> Result<Option<String>, RuntimeError> {
    writes.link(item, EdgeRelation::VariantOf, field.id);
    let mut added = None;
    if !wordings(field).contains(&normalize(item_label)) {
        let others = graph.owners(item_label, Some(field.id));
        if others.is_empty() {
            let mut properties = props(field);
            let mut list = aliases(field);
            list.push(item_label.to_owned());
            properties.insert("aliases".to_owned(), json!(list));
            writes.update_entity(field, None, None, Some(properties))?;
            added = Some(item_label.to_owned());
        } else {
            warnings.push(format!(
                "\"{item_label}\" already names {}, so it was not added as an alias of \"{}\"",
                quoted_names(&others),
                field.name
            ));
        }
    }
    decision_note(
        writes,
        &format!(
            "The item \"{item_label}\" on {form} means the field \"{}\".",
            field.name
        ),
        map([
            ("verb", json!("forms.same")),
            ("field", json!(field.name)),
            ("wording", json!(item_label)),
            ("form", json!(form)),
        ]),
        field.id,
    )?;
    Ok(added)
}

/// forms.add_item: one printed item on a loaded form, and what it means.
pub(crate) async fn add_item(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let form_spec = params::text(&args, "form")?;
    let spec =
        item_spec(params::object(&args, "item")?.ok_or_else(|| invalid("item is required"))?)?;
    let field = params::object(&args, "field")?;
    let same_as = params::opt_text(&args, "same_as")?;
    if field.is_some() && same_as.is_some() {
        return Err(invalid("pass field or same_as, not both"));
    }
    if field.is_none() && same_as.is_none() && vocab::item_needs_field(&spec.item_kind) {
        return Err(invalid(format!(
            "a {} item means a field: pass field with the proposed field, or same_as",
            spec.item_kind
        )));
    }

    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let form = graph.find_form(&form_spec)?;
    let key = graph
        .key(form.id)
        .ok_or_else(|| invalid(format!("form {form_spec} lacks its form_id or revision")))?;
    let wanted = item_key(&key, &spec);
    if let Some(existing) = graph
        .of_type(FORM_ITEM)
        .into_iter()
        .find(|i| graph.key(i.id).as_deref() == Some(wanted.as_str()))
    {
        return Err(invalid(format!(
            "item \"{}\" at order {} is already on {key} with id {}",
            spec.label,
            scalar(&spec.order).unwrap_or_default(),
            existing.id
        )));
    }
    let parent = match &spec.section {
        None => form.id,
        Some(section) => graph
            .of_type(FORM_SECTION)
            .into_iter()
            .find(|s| {
                prop_text(s, "form_key").as_deref() == Some(key.as_str())
                    && normalize(&label(s)) == normalize(section)
            })
            .map(|s| s.id)
            .ok_or_else(|| invalid(format!("{key} has no section \"{section}\"")))?,
    };

    let mut writes = Writes::new(ctx.token);
    let mut warnings = Vec::new();
    let item = writes.create_entity(FORM_ITEM, &spec.label, None, item_props(&spec, &key))?;
    writes.link(parent, EdgeRelation::Contains, item);

    let mut reply = json!({
        "item": {"id": item.to_string(), "label": spec.label, "form": key},
    });
    if let Some(target) = same_as {
        let field = graph.find_field(&target)?;
        refuse_general(&graph, field)?;
        let added = bind_same(
            &graph,
            &mut writes,
            item,
            &spec.label,
            &key,
            field,
            &mut warnings,
        )?;
        reply["status"] = json!("same");
        reply["field"] = json!({"id": field.id.to_string(), "name": field.name});
        reply["alias_added"] = json!(added);
    } else if let Some(field) = field {
        let mut proposed = field_spec(field)?;
        let owners = graph.owners(&proposed.name, None);
        if !owners.is_empty() {
            reply["status"] = json!("match");
            reply["proposed"] = json!(proposed.name);
            reply["candidates"] =
                Value::Array(owners.iter().map(|f| field_reply(&graph, f)).collect());
            reply["next"] = json!("The item was added without a field. If it means what a candidate means, call forms.same with the item and that field; if it means something else, call forms.split with that field, the item and two new names.");
        } else {
            let near = nearest(
                &graph,
                &proposed.name,
                &proposed.definition,
                &proposed.value_type,
            );
            if near.is_empty() {
                keep_free_aliases(&graph, &mut proposed, &mut warnings);
                let (id, _) = stage_field(&mut writes, &proposed)?;
                writes.link(item, EdgeRelation::VariantOf, id);
                reply["status"] = json!("new_field");
                reply["field"] = json!({"id": id.to_string(), "name": proposed.name});
            } else {
                reply["status"] = json!("no_match");
                reply["proposed"] = json!(proposed.name);
                reply["nearest"] = Value::Array(near);
                reply["next"] = json!("The item was added without a field. Call forms.same with the item and one of the nearest fields, or with new_field to create the proposed field.");
            }
        }
    } else {
        reply["status"] = json!("no_field_needed");
    }

    let committed = writes.commit(ctx.runtime, ctx.registry, ctx.token).await?;
    warnings.extend(committed.index_warning);
    reply["warnings"] = json!(warnings);
    Ok(reply)
}

/// forms.same: an unbound item means an existing field, or a new one.
pub(crate) async fn same(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let item_spec_text = params::text(&args, "item")?;
    let field = params::opt_text(&args, "field")?;
    let new_field = params::object(&args, "new_field")?;
    if field.is_some() == new_field.is_some() {
        return Err(invalid("pass exactly one of field and new_field"));
    }

    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let item = item_of(&graph, &item_spec_text)?;
    refuse_bound(&graph, item)?;
    let form = prop_text(item, "form_key").unwrap_or_default();
    let item_label = label(item);

    let mut writes = Writes::new(ctx.token);
    let mut warnings = Vec::new();
    let mut reply = json!({"status": "bound", "item": {"id": item.id.to_string(), "label": item_label, "form": form}});
    if let Some(field) = field {
        let field = graph.find_field(&field)?;
        refuse_general(&graph, field)?;
        let added = bind_same(
            &graph,
            &mut writes,
            item.id,
            &item_label,
            &form,
            field,
            &mut warnings,
        )?;
        reply["field"] = json!({"id": field.id.to_string(), "name": field.name});
        reply["alias_added"] = json!(added);
    } else if let Some(new_field) = new_field {
        let mut proposed = field_spec(new_field)?;
        let owners = graph.owners(&proposed.name, None);
        if !owners.is_empty() {
            return Err(invalid(format!(
                "\"{}\" already names {}; pass field to bind to it, or use forms.split",
                proposed.name,
                quoted_names(&owners)
            )));
        }
        keep_free_aliases(&graph, &mut proposed, &mut warnings);
        let (id, _) = stage_field(&mut writes, &proposed)?;
        writes.link(item.id, EdgeRelation::VariantOf, id);
        reply["field"] = json!({"id": id.to_string(), "name": proposed.name});
    }

    let committed = writes.commit(ctx.runtime, ctx.registry, ctx.token).await?;
    warnings.extend(committed.index_warning);
    reply["warnings"] = json!(warnings);
    Ok(reply)
}

/// forms.split: the item's wording names a different field than the live
/// field of that name. One transaction, logged.
pub(crate) async fn split(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let field_spec_text = params::text(&args, "field")?;
    let item_spec_text = params::text(&args, "item")?;
    let existing_name = params::text(&args, "existing_name")?;
    let new_name = params::text(&args, "new_name")?;
    let new_definition = params::text(&args, "new_definition")?;
    let existing_definition = params::opt_text(&args, "existing_definition")?;
    let general_definition = params::opt_text(&args, "general_definition")?;

    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let field = graph.find_field(&field_spec_text)?;
    refuse_general(&graph, field)?;
    let item = item_of(&graph, &item_spec_text)?;
    refuse_bound(&graph, item)?;

    let shared = field.name.clone();
    let shared_folded = normalize(&shared);
    let existing_folded = normalize(&existing_name);
    let new_folded = normalize(&new_name);
    if existing_folded == new_folded {
        return Err(invalid("existing_name and new_name must differ"));
    }
    if existing_folded == shared_folded || new_folded == shared_folded {
        return Err(invalid(format!(
            "the new names must differ from the shared wording \"{shared}\""
        )));
    }
    for (name, except) in [(&existing_name, Some(field.id)), (&new_name, None)] {
        let owners = graph.owners(name, except);
        if !owners.is_empty() {
            return Err(invalid(format!(
                "\"{name}\" already names {}",
                quoted_names(&owners)
            )));
        }
    }
    let others = graph.owners(&shared, Some(field.id));
    if !others.is_empty() {
        return Err(invalid(format!(
            "\"{shared}\" also names {}; resolve that first",
            quoted_names(&others)
        )));
    }
    let value_type = prop_text(field, "value_type").unwrap_or_else(|| "text".to_owned());
    let form = prop_text(item, "form_key").unwrap_or_default();
    let item_label = label(item);

    let mut writes = Writes::new(ctx.token);

    let mut properties = props(field);
    let mut kept = vec![existing_name.clone()];
    for alias in aliases(field) {
        let folded = normalize(&alias);
        if folded != shared_folded
            && folded != existing_folded
            && !kept.iter().any(|k| normalize(k) == folded)
        {
            kept.push(alias);
        }
    }
    properties.insert("aliases".to_owned(), json!(kept));
    let mut former = strings(properties.get("former_names"));
    former.push(shared.clone());
    properties.insert("former_names".to_owned(), json!(former));
    if let Some(definition) = &existing_definition {
        properties.insert("definition".to_owned(), json!(definition));
    }
    writes.update_entity(field, Some(&existing_name), None, Some(properties))?;

    let (narrow, _) = stage_field(
        &mut writes,
        &FieldSpec {
            name: new_name.clone(),
            definition: new_definition,
            value_type: value_type.clone(),
            aliases: Vec::new(),
            values: Vec::new(),
            exclusive: false,
        },
    )?;
    writes.link(item.id, EdgeRelation::VariantOf, narrow);

    let general = writes.create_entity(
        FIELD,
        &shared,
        None,
        map([
            (
                "definition",
                json!(general_definition
                    .unwrap_or_else(|| format!("Any of: {existing_name}; {new_name}."))),
            ),
            ("value_type", json!(value_type)),
            ("aliases", json!([shared])),
            ("general", json!(true)),
            ("means", json!([field.id.to_string(), narrow.to_string()])),
        ]),
    )?;
    writes.link(general, EdgeRelation::Contains, field.id);
    writes.link(general, EdgeRelation::Contains, narrow);

    decision_note(
        &mut writes,
        &format!(
            "The wording \"{shared}\" names two fields: the items that already meant it now mean \"{existing_name}\", and the item \"{item_label}\" on {form} means \"{new_name}\"."
        ),
        map([
            ("verb", json!("forms.split")),
            ("shared_name", json!(shared)),
            ("existing_name", json!(existing_name)),
            ("new_name", json!(new_name)),
            ("form", json!(form)),
        ]),
        general,
    )?;

    let committed = writes.commit(ctx.runtime, ctx.registry, ctx.token).await?;
    Ok(json!({
        "status": "split",
        "general": {"id": general.to_string(), "name": shared},
        "existing": {"id": field.id.to_string(), "name": existing_name},
        "new": {"id": narrow.to_string(), "name": new_name},
        "item": {"id": item.id.to_string(), "label": item_label, "form": form},
        "warnings": committed.index_warning.into_iter().collect::<Vec<_>>(),
    }))
}

/// forms.replay: rebuild an exported graph in one transaction.
pub(crate) async fn replay(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let doc = params::object(&args, "document")?.ok_or_else(|| invalid("document is required"))?;
    let format = params::text(doc, "format")?;
    if format != FORMAT {
        return Err(invalid(format!(
            "document format is \"{format}\"; forms.replay reads \"{FORMAT}\""
        )));
    }
    let nodes = params::list(doc, "nodes")?;
    let edges = params::list(doc, "edges")?;
    let log = params::list(doc, "log")?;

    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let present: HashSet<String> = graph.nodes().filter_map(|n| graph.key(n.id)).collect();

    let mut ids: HashMap<String, Uuid> = HashMap::new();
    let mut clashes = Vec::new();
    for node in nodes {
        let key = params::text(params::entry(node, "node")?, "key")?;
        if present.contains(&key) {
            clashes.push(key.clone());
        }
        if ids.insert(key.clone(), Uuid::new_v4()).is_some() {
            return Err(invalid(format!("the document lists node {key} twice")));
        }
    }
    if !clashes.is_empty() {
        clashes.sort();
        return Err(invalid(format!(
            "{} node(s) of the document are already present, nothing was written: {}",
            clashes.len(),
            clashes
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        )));
    }
    let id_of = |key: &str| -> Result<Uuid, RuntimeError> {
        ids.get(key).copied().ok_or_else(|| {
            invalid(format!(
                "the document refers to {key}, which it does not list"
            ))
        })
    };

    let mut writes = Writes::new(ctx.token);
    for node in nodes {
        let node = params::entry(node, "node")?;
        let key = params::text(node, "key")?;
        let entity_type = params::text(node, "type")?;
        if !vocab::ALL_TYPES.contains(&entity_type.as_str()) {
            return Err(invalid(format!(
                "node {key} has type \"{entity_type}\", not a forms type"
            )));
        }
        let mut properties = params::object(node, "properties")?
            .cloned()
            .unwrap_or_default();
        if let Some(means) = properties.get("means") {
            let mapped = strings(Some(means))
                .iter()
                .map(|k| id_of(k).map(|id| id.to_string()))
                .collect::<Result<Vec<_>, _>>()?;
            properties.insert("means".to_owned(), json!(mapped));
        }
        writes.create_entity_with_id(
            id_of(&key)?,
            &entity_type,
            &params::text(node, "name")?,
            params::opt_text(node, "description")?.as_deref(),
            properties,
        )?;
    }
    for edge in edges {
        let edge = params::entry(edge, "edge")?;
        writes.link(
            id_of(&params::text(edge, "source")?)?,
            relation_named(&params::text(edge, "relation")?)?,
            id_of(&params::text(edge, "target")?)?,
        );
    }
    for entry in log {
        let entry = params::entry(entry, "log entry")?;
        let properties: Map<String, Value> = params::object(entry, "properties")?
            .map(|p| {
                p.iter()
                    .filter(|(k, _)| LOG_KEYS.contains(&k.as_str()))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect()
            })
            .unwrap_or_default();
        let note = writes.create_note(DECISION, &params::text(entry, "content")?, properties)?;
        for target in params::string_list(entry, "annotates")? {
            writes.link(note, EdgeRelation::Annotates, id_of(&target)?);
        }
    }

    let committed = writes.commit(ctx.runtime, ctx.registry, ctx.token).await?;
    Ok(json!({
        "replayed": true,
        "counts": {"nodes": nodes.len(), "edges": edges.len(), "log": log.len()},
        "warnings": committed.index_warning.into_iter().collect::<Vec<_>>(),
    }))
}
