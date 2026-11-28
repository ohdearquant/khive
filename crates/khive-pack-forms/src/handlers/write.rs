//! The forms write verbs. Each stages its whole change and commits it in one
//! transaction, so a refused or failed write leaves nothing behind.

use std::collections::{HashMap, HashSet};

use khive_runtime::RuntimeError;
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::lines::{
    check_subjects, constraint_specs, constraints_prop, declared_in, declared_on, line_props,
    subject_props, subject_specs, term_specs, KnownTerms,
};
use super::params::{self, Args};
use super::views::{field_reply, nearest};
use super::Ctx;
use crate::store::{
    aliases, document_type_key, form_key, invalid, is_general, is_system, label, pending_premise,
    pending_premises, prop, prop_text, props, quoted_names, scalar, strings, type_of, wordings,
    Graph, Writes, GRAPH_RELATIONS,
};
use crate::text::normalize;
use crate::vocab::{
    self, COMPARISONS, DOCUMENT_TYPE, FIELD, FIELD_VALUE, FORM, FORM_ISSUER, FORM_ITEM, FORM_RULE,
    FORM_SECTION, FORM_SUBJECT, FORM_TERM, ITEM_KINDS, LINE_RELATIONS, ORIGINS, TERM_LISTS,
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

/// The most values an exclusive choice field holds. Each pair of its values is
/// linked, so the bound keeps one load's links (48 values: 1,128) well inside
/// one transaction.
pub(crate) const MAX_EXCLUSIVE_VALUES: usize = 48;

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
    /// How the field's answers compare when a constraint holds two lines to
    /// match or differ; `exact` when not stated.
    comparison: Option<String>,
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
    let exclusive = obj
        .get("exclusive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    refuse_exclusive_size(&name, exclusive, values.len())?;
    Ok(FieldSpec {
        definition: params::text(obj, "definition")?,
        aliases: params::string_list(obj, "aliases")?,
        comparison: params::opt_text(obj, "comparison")?
            .map(|c| params::one_of(c, "comparison", &COMPARISONS))
            .transpose()?,
        exclusive,
        name,
        value_type,
        values,
    })
}

/// An exclusive field links every pair of its values, so its size is bounded
/// before the pairs are staged.
fn refuse_exclusive_size(name: &str, exclusive: bool, count: usize) -> Result<(), RuntimeError> {
    if exclusive && count > MAX_EXCLUSIVE_VALUES {
        return Err(invalid(format!(
            "field \"{name}\" is exclusive and has {count} values; an exclusive field holds at most {MAX_EXCLUSIVE_VALUES}, since each pair of its values is linked"
        )));
    }
    Ok(())
}

/// Whether any two of these values already exclude each other.
fn competing(graph: &Graph, values: &[Uuid]) -> bool {
    values.iter().any(|value| {
        graph
            .either(*value, EdgeRelation::CompetesWith)
            .iter()
            .any(|other| values.contains(other))
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

/// Whether `alias` is free to become an alias of `field`: no other live field
/// answers to it. Warns when it is not.
fn free_alias(graph: &Graph, field: &Entity, alias: &str, warnings: &mut Vec<String>) -> bool {
    let others = graph.owners(alias, Some(field.id));
    if others.is_empty() {
        return true;
    }
    warnings.push(format!(
        "\"{alias}\" already names {}, so it was not added as an alias of \"{}\"",
        quoted_names(&others),
        field.name
    ));
    false
}

/// The aliases a document gives a field it reuses that the field does not
/// answer to yet. An alias another live field answers to is warned about and
/// dropped from the document, so no item of it reaches the field through it.
fn new_aliases(
    graph: &Graph,
    field: &Entity,
    spec: &mut FieldSpec,
    warnings: &mut Vec<String>,
) -> Vec<String> {
    let mut known = wordings(field);
    let mut added = Vec::new();
    spec.aliases.retain(|alias| {
        let folded = normalize(alias);
        if folded.is_empty() || known.contains(&folded) {
            return true;
        }
        if !free_alias(graph, field, alias, warnings) {
            return false;
        }
        known.push(folded);
        added.push(alias.clone());
        true
    });
    added
}

/// Stage a new field with its values; returns its id and its values by folded name.
fn stage_field(
    writes: &mut Writes,
    spec: &FieldSpec,
) -> Result<(Uuid, HashMap<String, Uuid>), RuntimeError> {
    let mut properties = map([
        ("definition", json!(spec.definition)),
        ("value_type", json!(spec.value_type)),
        ("aliases", json!(alias_list(&spec.name, &spec.aliases))),
    ]);
    if let Some(comparison) = &spec.comparison {
        properties.insert("comparison".to_owned(), json!(comparison));
    }
    let field = writes.create_entity(FIELD, &spec.name, None, properties)?;
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
    /// What the line does with its field: asks, states or requires.
    relation: String,
    /// Who writes the line; a stating line has nobody.
    filled_by: Option<String>,
    /// The id of the document type a requires line takes.
    requires: Option<String>,
    section: Option<String>,
    widget: Option<Value>,
}

fn item_spec(obj: &Args) -> Result<ItemSpec, RuntimeError> {
    let widget = match obj.get("widget") {
        None | Some(Value::Null) => None,
        Some(widget @ Value::Object(_)) => Some(widget.clone()),
        Some(_) => return Err(invalid("widget must be an object such as {name, rect}")),
    };
    let label = params::text(obj, "label")?;
    let item_kind = params::one_of(params::text(obj, "item_kind")?, "item_kind", &ITEM_KINDS)?;
    let relation = params::one_of(
        params::opt_text(obj, "relation")?.unwrap_or_else(|| "asks".to_owned()),
        "relation",
        &LINE_RELATIONS,
    )?;
    let requires = params::opt_text(obj, "document_type")?;
    let given = |key: &str| obj.get(key).is_some_and(|v| !v.is_null());
    match relation.as_str() {
        "states" => {
            // A stating line is a value every document of the type carries:
            // nobody fills it in and nothing makes it necessary, so a document
            // that says otherwise is refused rather than read past.
            for (key, why) in [
                ("filled_by", "no subject fills it in"),
                ("necessity", "it has no necessity"),
                ("group", "it belongs to no one_of group"),
                (
                    "known_to",
                    "no subject holds its value apart from the document",
                ),
                ("document_type", "it takes no attached document"),
            ] {
                if given(key) {
                    return Err(invalid(format!(
                        "item \"{label}\" states its field, so {why}; drop {key}"
                    )));
                }
            }
            if !vocab::item_needs_field(&item_kind) {
                return Err(invalid(format!(
                    "item \"{label}\" states a field, so it is a text, choice or signature item, not {item_kind}"
                )));
            }
        }
        "requires" => {
            if item_kind != "attachment" {
                return Err(invalid(format!(
                    "item \"{label}\" requires an attached document, so its item_kind is attachment"
                )));
            }
            if requires.is_none() {
                return Err(invalid(format!(
                    "item \"{label}\" requires an attached document; name its type (document_type)"
                )));
            }
        }
        _ => {
            if requires.is_some() {
                return Err(invalid(format!(
                    "item \"{label}\" names a document type, which only a requires line takes"
                )));
            }
        }
    }
    let filled_by = match relation.as_str() {
        "states" => None,
        _ => Some(params::text(obj, "filled_by")?),
    };
    Ok(ItemSpec {
        label,
        page: params::number(obj, "page")?,
        order: params::number(obj, "order")?,
        item_kind,
        relation,
        filled_by,
        requires,
        section: params::opt_text(obj, "section")?,
        widget,
    })
}

/// A requires line takes a document of a loaded type other than its own
/// form's. Refusing an unloaded target means two types that require each
/// other cannot both load.
fn check_requires(graph: &Graph, spec: &ItemSpec, own_type: &str) -> Result<(), RuntimeError> {
    let Some(target) = &spec.requires else {
        return Ok(());
    };
    if target == own_type {
        return Err(invalid(format!(
            "item \"{}\" requires a document of its own form's type \"{target}\"; a document is not attached to itself",
            spec.label
        )));
    }
    if graph.document_type_by_id(target).is_none() {
        return Err(invalid(format!(
            "item \"{}\" requires a document of type \"{target}\", which is not loaded; load a document of that type first",
            spec.label
        )));
    }
    Ok(())
}

/// A document type as a document declares it. A system is an outside
/// system's record schema: it has no origin, since nobody brings it in, and
/// its lines only state.
struct TypeSpec {
    id: String,
    name: String,
    origin: Option<String>,
    system: bool,
    issuer_class: Option<String>,
}

fn type_spec(doc: &Args) -> Result<Option<TypeSpec>, RuntimeError> {
    let Some(obj) = params::object(doc, "document_type")? else {
        return Ok(None);
    };
    let id = params::text(obj, "id")?;
    let system = match obj.get("system") {
        None | Some(Value::Null) => false,
        Some(Value::Bool(system)) => *system,
        Some(_) => return Err(invalid("document_type.system must be true or false")),
    };
    let origin = params::opt_text(obj, "origin")?;
    let origin = match (system, origin) {
        (true, Some(_)) => {
            return Err(invalid(format!(
                "document type \"{id}\" is a system, so it has no origin; drop origin"
            )))
        }
        (true, None) => None,
        (false, None) => {
            return Err(invalid(format!(
                "document type \"{id}\" needs an origin, one of {}, or system: true",
                ORIGINS.join(", ")
            )))
        }
        (false, Some(origin)) => Some(params::one_of(origin, "origin", &ORIGINS)?),
    };
    Ok(Some(TypeSpec {
        name: params::text(obj, "name")?,
        issuer_class: params::opt_text(obj, "issuer_class")?,
        id,
        origin,
        system,
    }))
}

fn type_props(spec: &TypeSpec) -> Map<String, Value> {
    let mut props = map([("document_type_id", json!(spec.id))]);
    match &spec.origin {
        Some(origin) => props.insert("origin".to_owned(), json!(origin)),
        None => props.insert("system".to_owned(), json!(true)),
    };
    if let Some(issuer_class) = &spec.issuer_class {
        props.insert("issuer_class".to_owned(), json!(issuer_class));
    }
    props
}

/// A system's record only states what it holds: nobody fills a line of it in
/// and it attaches nothing.
fn check_system_line(spec: &ItemSpec, system: Option<&str>) -> Result<(), RuntimeError> {
    match system {
        Some(own_type) if spec.relation != "states" => Err(invalid(format!(
            "item \"{}\" {} its field, but document type \"{own_type}\" is a system, whose lines only state",
            spec.label, spec.relation
        ))),
        _ => Ok(()),
    }
}

fn is_minted(document_type: &Entity) -> bool {
    prop(document_type, "minted").and_then(Value::as_bool) == Some(true)
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
        ("form_key", json!(form_key)),
    ]);
    if let Some(filled_by) = &spec.filled_by {
        props.insert("filled_by".to_owned(), json!(filled_by));
    }
    if spec.relation != "asks" {
        props.insert("relation".to_owned(), json!(spec.relation));
    }
    if let Some(requires) = &spec.requires {
        props.insert("document_type".to_owned(), json!(requires));
    }
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
    document_types: usize,
    issuers: usize,
    sections: usize,
    items: usize,
    fields: usize,
    fields_reused: usize,
    aliases_added: usize,
    values: usize,
    rules: usize,
    relations: usize,
    subjects: usize,
    terms: usize,
}

impl Counts {
    fn to_json(&self) -> Value {
        json!({
            "forms": self.forms,
            "document_types": self.document_types,
            "issuers": self.issuers,
            "sections": self.sections,
            "items": self.items,
            "fields": self.fields,
            "fields_reused": self.fields_reused,
            "aliases_added": self.aliases_added,
            "values": self.values,
            "rules": self.rules,
            "relations": self.relations,
            "subjects": self.subjects,
            "terms": self.terms,
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

    // What the form is for and what it holds its lines to are kept on the
    // form; both are checked below, before anything is written.
    let purposes = params::string_list(doc, "purposes")?;
    let constraints = constraint_specs(doc)?;
    let mut form_properties = map([
        ("form_id", json!(form_id)),
        ("revision", json!(revision)),
        ("title", json!(title)),
        ("source_url", json!(source_url)),
        ("source_sha256", json!(source_sha256)),
    ]);
    if !purposes.is_empty() {
        form_properties.insert("purposes".to_owned(), json!(purposes));
    }
    if !constraints.is_empty() {
        form_properties.insert("constraints".to_owned(), constraints_prop(&constraints));
    }
    let form = writes.create_entity(FORM, &title, None, form_properties)?;
    counts.forms = 1;

    // Every revision belongs to a document type. A document that names none
    // gets one minted from its form id and title, marked as minted so that a
    // guessed origin never reads as a declared one; a later document that
    // declares the type replaces the guess.
    let declared_type = type_spec(doc)?;
    let own_type = declared_type
        .as_ref()
        .map(|t| t.id.clone())
        .unwrap_or_else(|| form_id.clone());
    let (document_type, minted) = match (graph.document_type_by_id(&own_type), &declared_type) {
        (Some(existing), Some(spec)) if is_minted(existing) => {
            if spec.system {
                // The forms of a minted type were loaded as forms to fill in.
                return Err(invalid(format!(
                    "document type \"{own_type}\" was minted for a form already loaded, so it cannot be declared a system"
                )));
            }
            writes.update_entity(existing, Some(&spec.name), None, Some(type_props(spec)))?;
            (existing.id, false)
        }
        (Some(existing), Some(spec)) => {
            let origin = prop_text(existing, "origin");
            let issuer_class = prop_text(existing, "issuer_class");
            if normalize(&existing.name) != normalize(&spec.name)
                // A system is the type with no origin, so this also
                // compares the mark.
                || origin != spec.origin
                || issuer_class != spec.issuer_class
            {
                let origin_of = |origin: Option<&str>, system: bool| match (origin, system) {
                    (_, true) => "system".to_owned(),
                    (Some(origin), false) => format!("origin {origin}"),
                    (None, false) => "origin none".to_owned(),
                };
                return Err(invalid(format!(
                    "document type \"{own_type}\" is loaded as \"{}\" ({}, issuer class {}); the document declares \"{}\" ({}, issuer class {})",
                    existing.name,
                    origin_of(origin.as_deref(), is_system(existing)),
                    issuer_class.as_deref().unwrap_or("none"),
                    spec.name,
                    origin_of(spec.origin.as_deref(), spec.system),
                    spec.issuer_class.as_deref().unwrap_or("none")
                )));
            }
            (existing.id, false)
        }
        (Some(existing), None) => (existing.id, is_minted(existing)),
        (None, Some(spec)) => {
            counts.document_types = 1;
            let id = writes.create_entity(DOCUMENT_TYPE, &spec.name, None, type_props(spec))?;
            (id, false)
        }
        (None, None) => {
            counts.document_types = 1;
            let id = writes.create_entity(
                DOCUMENT_TYPE,
                &title,
                None,
                map([
                    ("document_type_id", json!(own_type)),
                    ("origin", json!("applicant")),
                    ("minted", json!(true)),
                ]),
            )?;
            (id, true)
        }
    };
    writes.link(document_type, EdgeRelation::Contains, form);
    let own_system = match (graph.document_type_by_id(&own_type), &declared_type) {
        (_, Some(spec)) => spec.system,
        (Some(existing), None) => is_system(existing),
        (None, None) => false,
    };

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

    // The term lists first, so the subjects below can name what they load.
    let (terms, carrying) = term_specs(doc)?;
    let mut known: KnownTerms = HashMap::new();
    for (_, list) in TERM_LISTS {
        for (wording, term) in graph.terms(list) {
            known.insert((list.to_owned(), wording), term.name.clone());
        }
    }
    let carrying_folded: HashSet<String> = carrying.iter().map(|n| normalize(n)).collect();
    // Loaded terms this document gives a new alias to or marks as carrying,
    // with the properties they are to hold.
    let mut grown: HashMap<Uuid, Map<String, Value>> = HashMap::new();
    for term in &terms {
        let list = term.list.to_owned();
        let mut owners: Vec<String> = term
            .wordings()
            .iter()
            .filter_map(|w| known.get(&(list.clone(), w.clone())).cloned())
            .collect();
        owners.sort();
        owners.dedup();
        if owners.len() > 1 {
            return Err(invalid(format!(
                "terms: \"{}\" and its aliases name more than one loaded {list}: {}",
                term.name,
                owners.join(", ")
            )));
        }
        let wordings: Vec<&String> = std::iter::once(&term.name).chain(&term.aliases).collect();
        match owners.pop() {
            None => {
                let mut properties = map([("list", json!(term.list))]);
                if let Some(definition) = &term.definition {
                    properties.insert("definition".to_owned(), json!(definition));
                }
                if !term.aliases.is_empty() {
                    properties.insert("aliases".to_owned(), json!(term.aliases));
                }
                if term.list == "relation"
                    && term.wordings().iter().any(|w| carrying_folded.contains(w))
                {
                    properties.insert("carries".to_owned(), json!(true));
                }
                writes.create_entity(FORM_TERM, &term.name, None, properties)?;
                counts.terms += 1;
                for wording in wordings {
                    known.insert((list.clone(), normalize(wording)), term.name.clone());
                }
            }
            Some(canonical) => {
                // Terms named twice in one document are refused above, so the
                // owner is a term loaded before this document.
                let terms_of_list = graph.terms(term.list);
                let existing = terms_of_list
                    .get(&normalize(&canonical))
                    .ok_or_else(|| invalid(format!("the {list} \"{canonical}\" is not loaded")))?;
                let fresh: Vec<String> = wordings
                    .into_iter()
                    .filter(|w| !known.contains_key(&(list.clone(), normalize(w))))
                    .cloned()
                    .collect();
                if !fresh.is_empty() {
                    let properties = grown.entry(existing.id).or_insert_with(|| props(existing));
                    let mut held = strings(properties.get("aliases"));
                    for wording in &fresh {
                        known.insert((list.clone(), normalize(wording)), canonical.clone());
                        held.push(wording.clone());
                    }
                    properties.insert("aliases".to_owned(), json!(held));
                }
            }
        }
    }
    for name in &carrying {
        let Some(canonical) = known
            .get(&("relation".to_owned(), normalize(name)))
            .cloned()
        else {
            return Err(invalid(format!(
                "terms.carrying_relations names \"{name}\", which is not a loaded relation; list it under terms.relations"
            )));
        };
        let relations = graph.terms("relation");
        if let Some(existing) = relations.get(&normalize(&canonical)) {
            if prop(existing, "carries").and_then(Value::as_bool) != Some(true) {
                grown
                    .entry(existing.id)
                    .or_insert_with(|| props(existing))
                    .insert("carries".to_owned(), json!(true));
            }
        }
    }
    for (id, properties) in grown {
        if let Some(before) = graph.get(id) {
            writes.update_entity(before, None, None, Some(properties))?;
        }
    }
    // A purpose named twice, by one wording or by two wordings of one term, is
    // refused.
    let mut named = HashSet::new();
    for purpose in &purposes {
        let canonical = known.get(&("purpose".to_owned(), normalize(purpose)));
        if !named.insert(normalize(
            canonical.map_or(purpose.as_str(), String::as_str),
        )) {
            return Err(invalid(format!(
                "the document names the purpose \"{purpose}\" twice"
            )));
        }
        if canonical.is_none() {
            return Err(invalid(format!(
                "the form is for \"{purpose}\", which is not a loaded purpose; list it under terms"
            )));
        }
    }
    let mut subjects = subject_specs(doc)?;
    check_subjects(&mut subjects, &known)?;
    let mut subject_ids: HashMap<String, Uuid> = HashMap::new();
    for spec in &subjects {
        let id = writes.create_entity(FORM_SUBJECT, &spec.id, None, subject_props(spec, &key))?;
        writes.link(form, EdgeRelation::Contains, id);
        subject_ids.insert(normalize(&spec.id), id);
        counts.subjects += 1;
    }
    let declared = declared_in(&subjects);

    let mut fields: HashMap<String, Slot> = HashMap::new();
    // How each field of the document compares its answers.
    let mut comparisons: HashMap<Uuid, String> = HashMap::new();
    // Each wording of a field left undecided, to the name the document gives it.
    let mut blocked: HashMap<String, String> = HashMap::new();
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
                comparisons.insert(
                    id,
                    spec.comparison
                        .clone()
                        .unwrap_or_else(|| "exact".to_owned()),
                );
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
                // A reused field compares one way for every form that prints it,
                // and a field loaded with no comparison compares exactly: a later
                // document never changes what an earlier form's constraint means.
                let held = prop_text(existing, "comparison").unwrap_or_else(|| "exact".to_owned());
                if let Some(stated) = &spec.comparison {
                    if *stated != held {
                        return Err(invalid(format!(
                            "field \"{}\" compares its answers by \"{held}\"; the document says \"{stated}\"",
                            spec.name
                        )));
                    }
                }
                // A later revision may print a shorter wording of a field it
                // reuses; the field keeps it.
                let added_aliases = new_aliases(&graph, existing, &mut spec, &mut warnings);
                let mut properties = props(existing);
                let mut changed = false;
                if !added_aliases.is_empty() {
                    let mut list = aliases(existing);
                    list.extend(added_aliases.iter().cloned());
                    properties.insert("aliases".to_owned(), json!(list));
                    counts.aliases_added += added_aliases.len();
                    changed = true;
                }
                if changed {
                    writes.update_entity(existing, None, None, Some(properties))?;
                }
                comparisons.insert(existing.id, held);
                let mut values = graph_values(&graph, existing.id);
                let old: Vec<Uuid> = values.values().copied().collect();
                let mut added = Vec::new();
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
                    added.push(value);
                    counts.values += 1;
                }
                // A field whose values exclude each other keeps doing so for the
                // values this document adds.
                if !added.is_empty() && (spec.exclusive || competing(&graph, &old)) {
                    refuse_exclusive_size(&spec.name, true, old.len() + added.len())?;
                    for (i, first) in added.iter().enumerate() {
                        for second in old.iter().chain(&added[i + 1..]) {
                            writes.link(*first, EdgeRelation::CompetesWith, *second);
                        }
                    }
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
        if slot.is_none() {
            blocked.insert(folded.clone(), spec.name.clone());
            for alias in &spec.aliases {
                blocked.insert(normalize(alias), spec.name.clone());
            }
        }
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
    // Each printed line that asks for a field: its folded subject, its field
    // and its folded state, for the constraints below.
    let mut printed: Vec<(Option<String>, Uuid, Option<String>)> = Vec::new();
    let mut items_by_label: HashMap<String, Vec<Uuid>> = HashMap::new();
    let mut pending_links = Vec::new();
    let mut item_replies = Vec::new();
    for item in params::list(doc, "items")? {
        let obj = params::entry(item, "item")?;
        let spec = item_spec(obj)?;
        check_requires(&graph, &spec, &own_type)?;
        check_system_line(&spec, own_system.then_some(own_type.as_str()))?;
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
        let field_name = params::opt_text(obj, "field")?;
        let value_name = params::opt_text(obj, "value")?;
        if field_name.is_none() && value_name.is_some() {
            return Err(invalid(format!(
                "item \"{}\" names a value; name its field too",
                spec.label
            )));
        }
        let slot = match &field_name {
            Some(name) => Some(document_field(&fields, &graph, name)?),
            None => None,
        };
        let mut item_properties = item_props(&spec, &key);
        item_properties.extend(line_props(
            obj,
            &spec.label,
            &spec.item_kind,
            &spec.relation,
            spec.filled_by.as_deref(),
            &declared,
        )?);
        if let (Some(name), Some(None)) = (&field_name, &slot) {
            // The wording waits for forms.same or forms.split, and so do the
            // rules of this form that take it as a premise.
            let waiting = blocked
                .get(&normalize(name))
                .cloned()
                .unwrap_or_else(|| name.clone());
            item_properties.insert("field_pending".to_owned(), json!(waiting));
        }
        // A constraint holds lines someone fills in; a stating line is not one.
        if let (Some(Some((field, _))), false) = (&slot, spec.relation == "states") {
            let folded = |key: &str| {
                item_properties
                    .get(key)
                    .and_then(Value::as_str)
                    .map(normalize)
            };
            printed.push((folded("about"), *field, folded("state")));
        }
        let id = writes.create_entity(FORM_ITEM, &spec.label, None, item_properties)?;
        writes.link(parent, EdgeRelation::Contains, id);
        counts.items += 1;

        let mut bound = Value::Null;
        match (field_name, slot) {
            (Some(field_name), Some(slot)) => {
                if let Some((field, values)) = slot {
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
            (Some(_), None) => {}
            (None, _) => {
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

    // Each end of a constraint is a line the form prints, and both ends
    // compare their answers one way.
    for constraint in &constraints {
        let mut compared: Vec<(String, String)> = Vec::new();
        for end in &constraint.ends {
            let what = end.describe();
            let Some((field, _)) = document_field(&fields, &graph, &end.field)? else {
                return Err(invalid(format!(
                    "a constraint names the line {what}, whose field waits for forms.same or forms.split"
                )));
            };
            let subject = match &end.subject {
                Some(id) if declared.contains_key(&normalize(id)) => Some(normalize(id)),
                Some(id) => {
                    return Err(invalid(format!(
                        "a constraint names the line {what}, and the form does not declare \"{id}\""
                    )));
                }
                None if !declared.is_empty() => {
                    return Err(invalid(format!(
                        "a constraint names the line {what} without a subject; its form declares subjects, so each line of a constraint names one"
                    )));
                }
                None => None,
            };
            let state = end.state.as_deref().map(normalize);
            if !printed
                .iter()
                .any(|(about, f, s)| *about == subject && *f == field && *s == state)
            {
                return Err(invalid(format!(
                    "a constraint names the line {what}, which the form does not print"
                )));
            }
            let comparison = comparisons
                .get(&field)
                .cloned()
                .or_else(|| graph.get(field).and_then(|f| prop_text(f, "comparison")))
                .unwrap_or_else(|| "exact".to_owned());
            compared.push((what, comparison));
        }
        if compared[0].1 != compared[1].1 {
            return Err(invalid(format!(
                "a constraint compares {} by \"{}\" and {} by \"{}\"; both lines of a constraint compare one way",
                compared[0].0, compared[0].1, compared[1].0, compared[1].1
            )));
        }
    }

    for rule in params::list(doc, "rules")? {
        let obj = params::entry(rule, "rule")?;
        let text = params::text(obj, "text")?;
        let mut premises = Vec::new();
        let mut waiting = Vec::new();
        for premise in params::list(obj, "depends_on")? {
            let premise = params::entry(premise, "premise")?;
            let field_name = params::text(premise, "field")?;
            let value_name = params::opt_text(premise, "value")?;
            let Some((field, values)) = document_field(&fields, &graph, &field_name)? else {
                let name = blocked
                    .get(&normalize(&field_name))
                    .cloned()
                    .unwrap_or(field_name);
                warnings.push(format!(
                    "rule \"{text}\" is not checked until \"{name}\" is decided"
                ));
                waiting.push(pending_premise(&name, value_name.as_deref()));
                continue;
            };
            match value_name {
                Some(value_name) => {
                    let value = values.get(&normalize(&value_name)).ok_or_else(|| {
                        invalid(format!(
                            "rule \"{text}\" names value \"{value_name}\", which field \"{field_name}\" does not list"
                        ))
                    })?;
                    premises.push(*value);
                }
                None => premises.push(field),
            }
        }
        let mut rule_properties = map([
            ("text", json!(text)),
            ("source_page", params::number(obj, "page")?),
            ("form_key", json!(key)),
        ]);
        if !waiting.is_empty() {
            rule_properties.insert("premises_pending".to_owned(), json!(waiting));
        }
        let id = writes.create_entity(FORM_RULE, &text, None, rule_properties)?;
        counts.rules += 1;
        for target in premises {
            writes.link(id, EdgeRelation::DependsOn, target);
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
            } else if let Some(subject) = params::opt_text(conclusion, "subject")? {
                *subject_ids.get(&normalize(&subject)).ok_or_else(|| {
                    invalid(format!(
                        "rule \"{text}\" names subject \"{subject}\", which the document does not declare"
                    ))
                })?
            } else if let Some(other) = params::opt_text(conclusion, "form")? {
                if normalize(&other) == normalize(&form_id) || other == key {
                    form
                } else {
                    graph.find_form(&other)?.id
                }
            } else {
                return Err(invalid(format!(
                    "each conclusion of rule \"{text}\" names a section, an item, a subject or a form"
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
        "document_type": {
            "id": document_type.to_string(),
            "key": document_type_key(&own_type),
            "minted": minted,
        },
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

/// What binding an item to a field added as aliases: the item's label, and
/// the further wordings the caller gave.
type AliasesAdded = (Option<String>, Vec<String>);

/// The printed item a decision binds: its id, its label and its form's key.
struct Printed<'a> {
    id: Uuid,
    label: &'a str,
    form: &'a str,
}

/// Bind an item to a field it means the same as: the edge, the label and any
/// further wordings as aliases when no other field answers to them, in one
/// update, and a logged decision.
fn bind_same(
    graph: &Graph,
    writes: &mut Writes,
    printed: Printed<'_>,
    field: &Entity,
    further: &[String],
    warnings: &mut Vec<String>,
) -> Result<AliasesAdded, RuntimeError> {
    let Printed {
        id: item,
        label: item_label,
        form,
    } = printed;
    writes.link(item, EdgeRelation::VariantOf, field.id);
    let mut list = aliases(field);
    let mut known = wordings(field);
    let mut added = None;
    let mut more = Vec::new();
    for (wording, is_label) in std::iter::once((item_label, true))
        .chain(further.iter().map(|alias| (alias.as_str(), false)))
    {
        let folded = normalize(wording);
        if folded.is_empty()
            || known.contains(&folded)
            || !free_alias(graph, field, wording, warnings)
        {
            continue;
        }
        known.push(folded);
        list.push(wording.to_owned());
        if is_label {
            added = Some(wording.to_owned());
        } else {
            more.push(wording.to_owned());
        }
    }
    if added.is_some() || !more.is_empty() {
        let mut properties = props(field);
        properties.insert("aliases".to_owned(), json!(list));
        writes.update_entity(field, None, None, Some(properties))?;
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
    Ok((added, more))
}

/// An item that waited on a wording is decided: the rules of its form that
/// waited on the same wording take their premise on the field it now means.
/// Returns the rules left with no premise waiting.
fn settle_pending(
    graph: &Graph,
    writes: &mut Writes,
    item: &Entity,
    field: Uuid,
    values: &HashMap<String, Uuid>,
    warnings: &mut Vec<String>,
) -> Result<Vec<String>, RuntimeError> {
    let Some(wording) = prop_text(item, "field_pending") else {
        return Ok(Vec::new());
    };
    let mut item_properties = props(item);
    item_properties.remove("field_pending");
    writes.update_entity(item, None, None, Some(item_properties))?;

    let form = prop_text(item, "form_key");
    let folded = normalize(&wording);
    let mut settled = Vec::new();
    for rule in graph.of_type(FORM_RULE) {
        if prop_text(rule, "form_key") != form {
            continue;
        }
        let pending = pending_premises(rule);
        if !pending.iter().any(|(w, _)| normalize(w) == folded) {
            continue;
        }
        let text = prop_text(rule, "text").unwrap_or_else(|| rule.name.clone());
        let mut left = Vec::new();
        for (name, value) in pending {
            if normalize(&name) != folded {
                left.push(pending_premise(&name, value.as_deref()));
                continue;
            }
            match value {
                None => writes.link(rule.id, EdgeRelation::DependsOn, field),
                Some(value_name) => match values.get(&normalize(&value_name)) {
                    Some(value) => writes.link(rule.id, EdgeRelation::DependsOn, *value),
                    None => {
                        warnings.push(format!(
                            "rule \"{text}\" names value \"{value_name}\" of \"{name}\", which the field it now means does not list; the rule stays unchecked"
                        ));
                        left.push(pending_premise(&name, Some(&value_name)));
                    }
                },
            }
        }
        let mut rule_properties = props(rule);
        if left.is_empty() {
            rule_properties.remove("premises_pending");
            settled.push(text);
        } else {
            rule_properties.insert("premises_pending".to_owned(), json!(left));
        }
        writes.update_entity(rule, None, None, Some(rule_properties))?;
    }
    Ok(settled)
}

/// forms.add_item: one printed item on a loaded form, and what it means.
pub(crate) async fn add_item(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let form_spec = params::text(&args, "form")?;
    let item_obj = params::object(&args, "item")?.ok_or_else(|| invalid("item is required"))?;
    let spec = item_spec(item_obj)?;
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

    let own_type = graph
        .document_type_of(form.id)
        .and_then(|t| prop_text(t, "document_type_id"))
        .or_else(|| prop_text(form, "form_id"))
        .unwrap_or_default();
    let own_system = graph.document_type_of(form.id).is_some_and(is_system);
    check_requires(&graph, &spec, &own_type)?;
    check_system_line(&spec, own_system.then_some(own_type.as_str()))?;
    let mut properties = item_props(&spec, &key);
    properties.extend(line_props(
        item_obj,
        &spec.label,
        &spec.item_kind,
        &spec.relation,
        spec.filled_by.as_deref(),
        &declared_on(&graph.subjects_of(&key)),
    )?);

    let mut writes = Writes::new(ctx.token);
    let mut warnings = Vec::new();
    let item = writes.create_entity(FORM_ITEM, &spec.label, None, properties)?;
    writes.link(parent, EdgeRelation::Contains, item);

    let mut reply = json!({
        "item": {"id": item.to_string(), "label": spec.label, "form": key},
    });
    if let Some(target) = same_as {
        let field = graph.find_field(&target)?;
        refuse_general(&graph, field)?;
        let (added, _) = bind_same(
            &graph,
            &mut writes,
            Printed {
                id: item,
                label: &spec.label,
                form: &key,
            },
            field,
            &[],
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
    let further = params::string_list(&args, "aliases")?;
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
    let mut settled = Vec::new();
    if let Some(field) = field {
        let field = graph.find_field(&field)?;
        refuse_general(&graph, field)?;
        let (added, more) = bind_same(
            &graph,
            &mut writes,
            Printed {
                id: item.id,
                label: &item_label,
                form: &form,
            },
            field,
            &further,
            &mut warnings,
        )?;
        settled = settle_pending(
            &graph,
            &mut writes,
            item,
            field.id,
            &graph_values(&graph, field.id),
            &mut warnings,
        )?;
        reply["field"] = json!({"id": field.id.to_string(), "name": field.name});
        reply["alias_added"] = json!(added);
        reply["aliases_added"] = json!(more);
    } else if let Some(new_field) = new_field {
        let mut proposed = field_spec(new_field)?;
        proposed.aliases.extend(further);
        let owners = graph.owners(&proposed.name, None);
        if !owners.is_empty() {
            return Err(invalid(format!(
                "\"{}\" already names {}; pass field to bind to it, or use forms.split",
                proposed.name,
                quoted_names(&owners)
            )));
        }
        keep_free_aliases(&graph, &mut proposed, &mut warnings);
        let (id, values) = stage_field(&mut writes, &proposed)?;
        writes.link(item.id, EdgeRelation::VariantOf, id);
        settled = settle_pending(&graph, &mut writes, item, id, &values, &mut warnings)?;
        reply["field"] = json!({"id": id.to_string(), "name": proposed.name});
        reply["aliases"] = json!(alias_list(&proposed.name, &proposed.aliases));
    }
    reply["rules_settled"] = json!(settled);

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
            comparison: prop_text(field, "comparison"),
        },
    )?;
    writes.link(item.id, EdgeRelation::VariantOf, narrow);
    let mut warnings = Vec::new();
    let settled = settle_pending(
        &graph,
        &mut writes,
        item,
        narrow,
        &HashMap::new(),
        &mut warnings,
    )?;

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
    warnings.extend(committed.index_warning);
    Ok(json!({
        "rules_settled": settled,
        "status": "split",
        "general": {"id": general.to_string(), "name": shared},
        "existing": {"id": field.id.to_string(), "name": existing_name},
        "new": {"id": narrow.to_string(), "name": new_name},
        "item": {"id": item.id.to_string(), "label": item_label, "form": form},
        "warnings": warnings,
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
