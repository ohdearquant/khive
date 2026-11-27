//! The forms read verbs. Each reads one snapshot of the namespace's forms graph.

use std::collections::{BTreeMap, HashMap, HashSet};

use khive_runtime::{EdgeListFilter, RuntimeError};
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::params;
use super::views::{field_reply, usage, value_reply};
use super::write::{DECISION, FORMAT, LOG_KEYS};
use super::Ctx;
use crate::store::{
    aliases, definition, invalid, is_general, label, pending_premises, prop, prop_text, props,
    quoted_names, scalar, strings, type_of, wordings, Graph,
};
use crate::text::normalize;
use crate::vocab::{
    self, ALL_TYPES, FIELD, FIELD_VALUE, FORM, FORM_ISSUER, FORM_ITEM, FORM_RULE, FORM_SECTION,
};

const ANNOTATION_PAGE: u32 = 1000;

/// A form as findings and replies cite it.
fn cite(form: &Entity) -> Value {
    json!({
        "id": form.id.to_string(),
        "form_id": prop_text(form, "form_id"),
        "revision": prop_text(form, "revision"),
        "title": form.name,
        "source_url": prop_text(form, "source_url"),
    })
}

fn form_name(form: &Entity) -> String {
    match (prop_text(form, "form_id"), prop_text(form, "revision")) {
        (Some(id), Some(revision)) => format!("{id} ({revision})"),
        _ => form.name.clone(),
    }
}

/// Where a node's content comes from.
fn source(node: &Entity) -> Value {
    match type_of(node) {
        FORM => json!({
            "url": prop_text(node, "source_url"),
            "sha256": prop_text(node, "source_sha256"),
        }),
        FORM_SECTION => json!({"form": prop_text(node, "form_key")}),
        FORM_ITEM => json!({"form": prop_text(node, "form_key"), "page": prop(node, "page")}),
        FORM_RULE => json!({
            "form": prop_text(node, "form_key"),
            "page": prop(node, "source_page"),
        }),
        _ => Value::Null,
    }
}

fn node_view(graph: &Graph, node: &Entity) -> Value {
    json!({
        "id": node.id.to_string(),
        "type": type_of(node),
        "name": node.name,
        "key": graph.key(node.id),
        "source": source(node),
    })
}

fn item_view(graph: &Graph, item: &Entity) -> Value {
    let mut view = json!({
        "id": item.id.to_string(),
        "label": label(item),
        "page": prop(item, "page"),
        "order": prop(item, "order"),
        "item_kind": prop_text(item, "item_kind"),
        "form": prop_text(item, "form_key"),
    });
    if let Some(target) = graph
        .out(item.id, EdgeRelation::VariantOf)
        .first()
        .and_then(|id| graph.get(*id))
    {
        let slot = if type_of(target) == FIELD {
            "field"
        } else {
            "value"
        };
        view[slot] = json!(target.name);
    }
    view
}

fn number_of(node: &Entity, key: &str) -> f64 {
    prop(node, key).and_then(Value::as_f64).unwrap_or(f64::MAX)
}

/// How a field was found: its rank, the kind of match, the general field it
/// was reached through, and the wording that matched.
type Found = (u8, &'static str, Option<String>, Option<String>);

/// Keep the lowest rank a field was found at.
fn offer(found: &mut BTreeMap<Uuid, Found>, id: Uuid, entry: Found) {
    match found.get(&id) {
        Some((known, ..)) if *known <= entry.0 => {}
        _ => {
            found.insert(id, entry);
        }
    }
}

/// The printed wording of `field` that folds to `folded`.
fn wording_of(field: &Entity, folded: &str) -> Option<String> {
    aliases(field)
        .into_iter()
        .chain(std::iter::once(field.name.clone()))
        .find(|w| normalize(w) == folded)
}

/// forms.resolve: text to fields, values and forms.
pub(crate) async fn resolve(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let text = params::text(&args, "text")?;
    let types = params::string_list(&args, "types")?;
    for t in &types {
        params::one_of(t.clone(), "types", &[FIELD, FIELD_VALUE, FORM])?;
    }
    let wants = |t: &str| types.is_empty() || types.iter().any(|x| x == t);
    let folded = normalize(&text);
    if folded.is_empty() {
        return Err(invalid("text must contain a letter or a digit"));
    }
    let graph = Graph::load(ctx.runtime, ctx.token).await?;

    let mut found = BTreeMap::new();
    let mut general = Vec::new();
    for field in graph.of_type(FIELD) {
        let how = if normalize(&field.name) == folded {
            Some((0, "name"))
        } else if wordings(field).contains(&folded) {
            Some((1, "alias"))
        } else {
            None
        };
        let Some((rank, how)) = how else { continue };
        let wording = wording_of(field, &folded);
        if is_general(field) {
            general.push(json!({"id": field.id.to_string(), "name": field.name}));
            for child in graph.children_of_field(field.id) {
                offer(
                    &mut found,
                    child,
                    (rank, how, Some(field.name.clone()), wording.clone()),
                );
            }
        } else {
            offer(&mut found, field.id, (rank, how, None, wording));
        }
    }
    let labelled: Vec<&Entity> = graph
        .of_type(FORM_ITEM)
        .into_iter()
        .filter(|item| normalize(&label(item)) == folded)
        .collect();
    for item in &labelled {
        for field in graph.fields_of_item(item.id) {
            offer(&mut found, field, (3, "label", None, Some(label(item))));
        }
    }
    let ambiguous = found.len() > 1;

    let fields: Vec<(&Entity, &Found)> = found
        .iter()
        .filter_map(|(id, entry)| graph.get(*id).map(|f| (f, entry)))
        .collect();

    // Every hit with its rank and the name and id it is ordered by, so fields,
    // values and forms come back as one list in rank order.
    let mut hits: Vec<(u8, String, Uuid, Value)> = Vec::new();
    if wants(FIELD) {
        for (field, (rank, how, via, wording)) in fields {
            let rank = *rank;
            let items: Vec<&Entity> = graph
                .items_of_field(field.id)
                .into_iter()
                .filter_map(|i| graph.get(i))
                .collect();
            let printing: Vec<Value> = items
                .iter()
                .filter(|i| normalize(&label(i)) == folded)
                .map(|i| usage(i))
                .collect();
            let mut hit = field_reply(&graph, field);
            hit["type"] = json!(FIELD);
            hit["rank"] = json!(rank);
            hit["matched_by"] = json!(how);
            hit["matched_wording"] = json!(wording);
            hit["ambiguous"] = json!(ambiguous);
            if let Some(via) = via {
                hit["via"] = json!(via);
            }
            hit["forms"] = Value::Array(if printing.is_empty() {
                items.iter().map(|i| usage(i)).collect()
            } else {
                printing
            });
            hits.push((rank, field.name.clone(), field.id, hit));
        }
    }
    if wants(FIELD_VALUE) {
        for value in graph.of_type(FIELD_VALUE) {
            let by_name = normalize(&value.name) == folded;
            let options: Vec<Value> = labelled
                .iter()
                .filter(|i| graph.out(i.id, EdgeRelation::VariantOf).contains(&value.id))
                .map(|i| usage(i))
                .collect();
            if !by_name && options.is_empty() {
                continue;
            }
            let rank = if by_name { 2 } else { 3 };
            let mut hit = value_reply(value);
            hit["type"] = json!(FIELD_VALUE);
            hit["rank"] = json!(rank);
            hit["matched_by"] = json!(if by_name { "name" } else { "label" });
            hit["matched_wording"] = json!(if by_name {
                value.name.clone()
            } else {
                text.clone()
            });
            hit["field"] = json!(graph
                .parent_field(value.id)
                .and_then(|f| graph.get(f))
                .map(|f| f.name.clone()));
            hit["forms"] = Value::Array(options);
            hits.push((rank, value.name.clone(), value.id, hit));
        }
    }
    if wants(FORM) {
        for form in graph.of_type(FORM) {
            let by_title = normalize(&form.name) == folded;
            let by_id = prop_text(form, "form_id").is_some_and(|id| normalize(&id) == folded)
                || graph.key(form.id).as_deref() == Some(text.as_str());
            if !(by_title || by_id) {
                continue;
            }
            let rank = if by_id { 2 } else { 4 };
            let mut hit = cite(form);
            hit["type"] = json!(FORM);
            hit["rank"] = json!(rank);
            hit["matched_by"] = json!(if by_id { "form_id" } else { "title" });
            hits.push((rank, form.name.clone(), form.id, hit));
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));
    Ok(json!({
        "text": text,
        "ambiguous": ambiguous,
        "general": general,
        "hits": hits.into_iter().map(|(.., hit)| hit).collect::<Vec<_>>(),
    }))
}

fn find_node<'g>(graph: &'g Graph, spec: &str) -> Result<&'g Entity, RuntimeError> {
    if let Ok(id) = Uuid::parse_str(spec) {
        return graph
            .get(id)
            .ok_or_else(|| invalid(format!("no forms node has id {spec}")));
    }
    graph
        .find_form(spec)
        .or_else(|_| graph.find_field(spec))
        .map_err(|_| invalid(format!("no form or field is named \"{spec}\"")))
}

/// Sections and items of a container in printed order, nested.
fn outline(graph: &Graph, container: Uuid) -> Value {
    let mut children: Vec<&Entity> = graph
        .out(container, EdgeRelation::Contains)
        .into_iter()
        .filter_map(|c| graph.get(c))
        .filter(|c| matches!(type_of(c), FORM_SECTION | FORM_ITEM))
        .collect();
    // A section prints no page of its own; it sorts at the first page of
    // anything it contains, so a mixed form keeps its printed order.
    let page_of = |node: &Entity| {
        if type_of(node) == FORM_SECTION {
            graph
                .items_in(node.id)
                .iter()
                .filter_map(|item| graph.get(*item))
                .map(|item| number_of(item, "page"))
                .fold(f64::MAX, f64::min)
        } else {
            number_of(node, "page")
        }
    };
    children.sort_by(|a, b| {
        page_of(a)
            .total_cmp(&page_of(b))
            .then(number_of(a, "order").total_cmp(&number_of(b, "order")))
            .then(a.name.cmp(&b.name))
    });
    Value::Array(
        children
            .into_iter()
            .map(|child| {
                if type_of(child) == FORM_SECTION {
                    json!({
                        "section": label(child),
                        "id": child.id.to_string(),
                        "order": prop(child, "order"),
                        "contents": outline(graph, child.id),
                    })
                } else {
                    item_view(graph, child)
                }
            })
            .collect(),
    )
}

fn names(graph: &Graph, ids: &[Uuid]) -> Vec<String> {
    ids.iter()
        .filter_map(|id| graph.get(*id))
        .map(label)
        .collect()
}

fn rules_of(graph: &Graph, form_key: &str) -> Vec<Value> {
    graph
        .of_type(FORM_RULE)
        .into_iter()
        .filter(|r| prop_text(r, "form_key").as_deref() == Some(form_key))
        .map(|rule| {
            json!({
                "id": rule.id.to_string(),
                "text": prop_text(rule, "text"),
                "page": prop(rule, "source_page"),
                "premises": names(graph, &graph.out(rule.id, EdgeRelation::DependsOn)),
                "conclusions": names(graph, &graph.out(rule.id, EdgeRelation::Enables)),
            })
        })
        .collect()
}

fn forms_cited(graph: &Graph, ids: Vec<Uuid>) -> Vec<Value> {
    ids.into_iter()
        .filter(|id| graph.is(*id, FORM))
        .filter_map(|id| graph.get(id))
        .map(cite)
        .collect()
}

/// forms.neighbourhood: what surrounds one node.
pub(crate) async fn neighbourhood(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let spec = params::text(&args, "node")?;
    let depth = match args.get("depth") {
        None | Some(Value::Null) => 1,
        Some(value) => value
            .as_u64()
            .filter(|d| (1..=3).contains(d))
            .ok_or_else(|| invalid("depth must be 1, 2 or 3"))? as usize,
    };
    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let root = find_node(&graph, &spec)?;

    let mut reached = vec![root.id];
    let mut frontier = vec![root.id];
    for _ in 0..depth {
        let mut next = Vec::new();
        for id in &frontier {
            for (source, _, target) in graph.edges() {
                let other = if source == id {
                    *target
                } else if target == id {
                    *source
                } else {
                    continue;
                };
                if !reached.contains(&other) {
                    reached.push(other);
                    next.push(other);
                }
            }
        }
        frontier = next;
    }
    let within: HashSet<Uuid> = reached.iter().copied().collect();
    let mut others: Vec<&Entity> = reached[1..]
        .iter()
        .filter_map(|id| graph.get(*id))
        .collect();
    let rank = |e: &Entity| {
        ALL_TYPES
            .iter()
            .position(|t| *t == type_of(e))
            .unwrap_or(ALL_TYPES.len())
    };
    others.sort_by(|a, b| {
        rank(a)
            .cmp(&rank(b))
            .then(a.name.cmp(&b.name))
            .then(a.id.cmp(&b.id))
    });
    let mut edges: Vec<(String, &'static str, String)> = graph
        .edges()
        .iter()
        .filter(|(s, _, t)| within.contains(s) && within.contains(t))
        .map(|(s, r, t)| {
            let (source, target) = (s.to_string(), t.to_string());
            if r.is_symmetric() && target < source {
                (target, r.as_str(), source)
            } else {
                (source, r.as_str(), target)
            }
        })
        .collect();
    edges.sort();
    edges.dedup();

    let mut reply = json!({
        "node": node_view(&graph, root),
        "depth": depth,
        "nodes": others.iter().map(|n| node_view(&graph, n)).collect::<Vec<_>>(),
        "edges": edges.iter().map(|(s, r, t)| json!({"source": s, "relation": r, "target": t})).collect::<Vec<_>>(),
    });
    match type_of(root) {
        FORM => {
            let key = graph.key(root.id).unwrap_or_default();
            reply["outline"] = outline(&graph, root.id);
            reply["rules"] = json!(rules_of(&graph, &key));
            reply["related"] = json!({
                "competes_with": forms_cited(&graph, graph.either(root.id, EdgeRelation::CompetesWith)),
                "composed_with": forms_cited(&graph, graph.either(root.id, EdgeRelation::ComposedWith)),
                "precedes": forms_cited(&graph, graph.out(root.id, EdgeRelation::Precedes)),
                "preceded_by": forms_cited(&graph, graph.incoming(root.id, EdgeRelation::Precedes)),
                "depends_on": forms_cited(&graph, graph.out(root.id, EdgeRelation::DependsOn)),
                "required_by": forms_cited(&graph, graph.incoming(root.id, EdgeRelation::DependsOn)),
            });
        }
        FIELD => {
            reply["field"] = field_reply(&graph, root);
            reply["asked_by"] = Value::Array(
                graph
                    .items_of_field(root.id)
                    .into_iter()
                    .filter_map(|i| graph.get(i))
                    .map(usage)
                    .collect(),
            );
            reply["general"] = json!(names(
                &graph,
                &graph
                    .incoming(root.id, EdgeRelation::Contains)
                    .into_iter()
                    .filter(|p| graph.is(*p, FIELD))
                    .collect::<Vec<_>>()
            ));
            reply["narrower"] = json!(names(&graph, &graph.children_of_field(root.id)));
        }
        _ => {}
    }
    Ok(reply)
}

fn item_answered(
    graph: &Graph,
    item: Uuid,
    fields: &HashSet<Uuid>,
    values: &HashSet<Uuid>,
) -> bool {
    graph
        .out(item, EdgeRelation::VariantOf)
        .iter()
        .any(|t| fields.contains(t) || values.contains(t))
}

fn met(
    graph: &Graph,
    target: Uuid,
    fields: &HashSet<Uuid>,
    values: &HashSet<Uuid>,
    forms: &HashSet<Uuid>,
) -> bool {
    match graph.get(target).map(type_of) {
        Some(FORM) => forms.contains(&target),
        Some(FORM_ITEM) => item_answered(graph, target, fields, values),
        Some(FORM_SECTION) => {
            // A section is filled when every item in it that asks for a field
            // is answered; a section of attachments alone, when any item is.
            let items = graph.items_in(target);
            let asking: Vec<Uuid> = items
                .iter()
                .copied()
                .filter(|item| {
                    graph
                        .get(*item)
                        .and_then(|i| prop_text(i, "item_kind"))
                        .is_some_and(|k| vocab::item_needs_field(&k))
                })
                .collect();
            if asking.is_empty() {
                items
                    .iter()
                    .any(|item| item_answered(graph, *item, fields, values))
            } else {
                asking
                    .iter()
                    .all(|item| item_answered(graph, *item, fields, values))
            }
        }
        _ => true,
    }
}

/// The items on the checked forms that ask for a field, as citations.
fn asked_on(graph: &Graph, field: Uuid, forms: &HashSet<Uuid>) -> Vec<Value> {
    graph
        .items_of_field(field)
        .into_iter()
        .filter_map(|i| graph.get(i))
        .filter(|i| graph.form_of(i).is_some_and(|f| forms.contains(&f.id)))
        .map(usage)
        .collect()
}

/// forms.check: a set of forms and answers against the forms' own rules.
pub(crate) async fn check(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let specs = params::string_list(&args, "forms")?;
    if specs.is_empty() {
        return Err(invalid("forms must name at least one form"));
    }
    let answers = params::object(&args, "answers")?
        .cloned()
        .unwrap_or_default();
    let graph = Graph::load(ctx.runtime, ctx.token).await?;

    let mut set: Vec<&Entity> = Vec::new();
    for spec in &specs {
        let form = graph.find_form(spec)?;
        if !set.iter().any(|f| f.id == form.id) {
            set.push(form);
        }
    }
    let in_set: HashSet<Uuid> = set.iter().map(|f| f.id).collect();
    let mut findings = Vec::new();

    for (i, first) in set.iter().enumerate() {
        for second in &set[i + 1..] {
            if graph
                .either(first.id, EdgeRelation::CompetesWith)
                .contains(&second.id)
            {
                findings.push(json!({
                    "kind": "competing_forms",
                    "forms": [cite(first), cite(second)],
                    "message": format!(
                        "{} and {} are filed one or the other, never both",
                        form_name(first),
                        form_name(second)
                    ),
                }));
            }
        }
    }
    for form in &set {
        for required in graph.out(form.id, EdgeRelation::DependsOn) {
            if in_set.contains(&required) {
                continue;
            }
            let Some(required) = graph.get(required).filter(|r| type_of(r) == FORM) else {
                continue;
            };
            findings.push(json!({
                "kind": "missing_prerequisite",
                "form": cite(form),
                "requires": cite(required),
                "message": format!(
                    "{} must be filed with {}, which is not in the set",
                    form_name(form),
                    form_name(required)
                ),
            }));
        }
    }

    let mut answered: HashSet<Uuid> = HashSet::new();
    let mut given: Vec<(Uuid, Uuid)> = Vec::new();
    let mut unresolved = Vec::new();
    for (key, raw) in &answers {
        let field = match graph.find_field(key) {
            Ok(field) if is_general(field) => {
                let children: Vec<&Entity> = graph
                    .children_of_field(field.id)
                    .into_iter()
                    .filter_map(|c| graph.get(c))
                    .collect();
                unresolved.push(json!({
                    "answer": key,
                    "reason": format!("\"{key}\" may mean {}; answer one of them", quoted_names(&children)),
                }));
                continue;
            }
            Ok(field) => field,
            Err(error) => {
                unresolved.push(json!({"answer": key, "reason": error.to_string()}));
                continue;
            }
        };
        let texts: Vec<String> = match raw {
            Value::Array(list) => list.iter().filter_map(scalar).collect(),
            other => scalar(other).into_iter().collect(),
        };
        if texts.is_empty() {
            unresolved.push(
                json!({"answer": key, "reason": "an answer is text, a number or a list of them"}),
            );
            continue;
        }
        // A choice field is answered only through a value it lists: an answer
        // outside its values is reported below and holds no premise or item.
        if prop_text(field, "value_type").as_deref() != Some("choice") {
            answered.insert(field.id);
            continue;
        }
        let values = graph.values_of_field(field.id);
        for text in &texts {
            let hit = values.iter().copied().find(|v| {
                graph
                    .get(*v)
                    .is_some_and(|e| normalize(&e.name) == normalize(text))
            });
            match hit {
                Some(value) => {
                    answered.insert(field.id);
                    given.push((field.id, value));
                }
                None => findings.push(json!({
                    "kind": "value_outside_domain",
                    "field": field.name,
                    "answer": text,
                    "allowed": names(&graph, &values),
                    "asked_on": asked_on(&graph, field.id, &in_set),
                    "message": format!("\"{text}\" is not one of the values of \"{}\"", field.name),
                })),
            }
        }
    }
    for (i, (first_field, first)) in given.iter().enumerate() {
        for (second_field, second) in &given[i + 1..] {
            if !graph
                .either(*first, EdgeRelation::CompetesWith)
                .contains(second)
            {
                continue;
            }
            let first_name = graph
                .get(*first)
                .map(|e| e.name.clone())
                .unwrap_or_default();
            let second_name = graph
                .get(*second)
                .map(|e| e.name.clone())
                .unwrap_or_default();
            let mut cited = asked_on(&graph, *first_field, &in_set);
            if second_field != first_field {
                cited.extend(asked_on(&graph, *second_field, &in_set));
            }
            findings.push(json!({
                "kind": "exclusive_values",
                "values": [first_name, second_name],
                "asked_on": cited,
                "message": format!("\"{first_name}\" and \"{second_name}\" exclude each other"),
            }));
        }
    }

    let given_values: HashSet<Uuid> = given.iter().map(|(_, v)| *v).collect();
    let mut unchecked = Vec::new();
    for rule in graph.of_type(FORM_RULE) {
        let Some(form) = graph.form_of(rule) else {
            continue;
        };
        if !in_set.contains(&form.id) {
            continue;
        }
        let pending = pending_premises(rule);
        if !pending.is_empty() {
            // Its bound premises are only part of the printed rule, so it is
            // not evaluated until every premise names a field.
            let mut waiting: Vec<String> = Vec::new();
            for (wording, _) in pending {
                if !waiting.contains(&wording) {
                    waiting.push(wording);
                }
            }
            let text = prop_text(rule, "text").unwrap_or_else(|| rule.name.clone());
            unchecked.push(json!({
                "rule": text,
                "form": cite(form),
                "page": prop(rule, "source_page"),
                "pending": waiting,
                "message": format!(
                    "{} states \"{text}\"; it is not checked until {} is decided with forms.same or forms.split",
                    form_name(form),
                    waiting.iter().map(|w| format!("\"{w}\"")).collect::<Vec<_>>().join(", ")
                ),
            }));
            continue;
        }
        let premises = graph.out(rule.id, EdgeRelation::DependsOn);
        if premises.is_empty() {
            continue;
        }
        let holds = premises.iter().all(|p| {
            if graph.is(*p, FIELD_VALUE) {
                given_values.contains(p)
            } else {
                answered.contains(p)
            }
        });
        if !holds {
            continue;
        }
        let unmet: Vec<&Entity> = graph
            .out(rule.id, EdgeRelation::Enables)
            .into_iter()
            .filter(|c| !met(&graph, *c, &answered, &given_values, &in_set))
            .filter_map(|c| graph.get(c))
            .collect();
        if unmet.is_empty() {
            continue;
        }
        let text = prop_text(rule, "text").unwrap_or_else(|| rule.name.clone());
        let page = prop(rule, "source_page")
            .and_then(scalar)
            .unwrap_or_default();
        findings.push(json!({
            "kind": "rule_unmet",
            "rule": text,
            "form": cite(form),
            "page": prop(rule, "source_page"),
            "unmet": unmet.iter().map(|c| json!({"id": c.id.to_string(), "type": type_of(c), "name": label(c)})).collect::<Vec<_>>(),
            "message": format!(
                "{} page {page} states \"{text}\"; it applies, and {} is not filled",
                form_name(form),
                unmet.iter().map(|c| format!("\"{}\"", label(c))).collect::<Vec<_>>().join(", ")
            ),
        }));
    }

    Ok(json!({
        "forms": set.iter().map(|f| cite(f)).collect::<Vec<_>>(),
        "consistent": findings.is_empty() && unresolved.is_empty() && unchecked.is_empty(),
        "findings": findings,
        "unresolved_answers": unresolved,
        "rules_unchecked": unchecked,
    }))
}

fn present(node: &Entity, key: &str) -> bool {
    match prop(node, key) {
        Some(Value::Array(list)) => !list.is_empty(),
        Some(value) => scalar(value).is_some_and(|s| !s.trim().is_empty()),
        None => false,
    }
}

fn brief_of(node: &Entity) -> Value {
    json!({"id": node.id.to_string(), "type": type_of(node), "name": node.name})
}

/// forms.lint: what breaks the forms invariants.
pub(crate) async fn lint(ctx: &Ctx<'_>, _params: Value) -> Result<Value, RuntimeError> {
    let graph = Graph::load(ctx.runtime, ctx.token).await?;

    let unbound_items: Vec<Value> = graph
        .of_type(FORM_ITEM)
        .into_iter()
        .filter(|i| {
            prop_text(i, "item_kind").is_some_and(|k| vocab::item_needs_field(&k))
                && graph.out(i.id, EdgeRelation::VariantOf).is_empty()
        })
        .map(|i| item_view(&graph, i))
        .collect();
    // An item means one field, or one value when it is a choice option.
    let malformed_bindings: Vec<Value> = graph
        .of_type(FORM_ITEM)
        .into_iter()
        .filter_map(|i| {
            let targets = graph.out(i.id, EdgeRelation::VariantOf);
            let reason = if targets.len() > 1 {
                "means more than one field or value"
            } else if targets.iter().any(|t| graph.is(*t, FIELD_VALUE))
                && prop_text(i, "item_kind").as_deref() != Some("choice")
            {
                "is not a choice item but means a choice value"
            } else {
                return None;
            };
            let mut view = item_view(&graph, i);
            view["reason"] = json!(reason);
            view["means"] = json!(names(&graph, &targets));
            Some(view)
        })
        .collect();
    let fields = graph.of_type(FIELD);
    let without_definition: Vec<Value> = fields
        .iter()
        .filter(|f| definition(f).is_none_or(|d| d.trim().is_empty()))
        .map(|f| brief_of(f))
        .collect();
    // Every field no item points at directly; one whose values are printed as
    // options is marked as reached through its values.
    let without_items: Vec<Value> = fields
        .iter()
        .filter(|f| !is_general(f) && graph.items_of_field(f.id).is_empty())
        .map(|f| {
            let mut entry = brief_of(f);
            entry["through_values"] = json!(graph
                .values_of_field(f.id)
                .iter()
                .any(|v| !graph.incoming(*v, EdgeRelation::VariantOf).is_empty()));
            entry
        })
        .collect();
    let mut owners: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for field in &fields {
        for wording in wordings(field) {
            owners.entry(wording).or_default().push(field.name.clone());
        }
    }
    let shared_names: Vec<Value> = owners
        .into_iter()
        .filter(|(_, names)| names.len() > 1)
        .map(|(wording, names)| json!({"wording": wording, "fields": names}))
        .collect();
    let values_without_options: Vec<Value> = graph
        .of_type(FIELD_VALUE)
        .into_iter()
        .filter(|v| graph.incoming(v.id, EdgeRelation::VariantOf).is_empty())
        .map(|v| {
            json!({
                "id": v.id.to_string(),
                "name": v.name,
                "field": graph.parent_field(v.id).and_then(|f| graph.get(f)).map(|f| f.name.clone()),
            })
        })
        .collect();
    let rules = graph.of_type(FORM_RULE);
    let rules_without_premise: Vec<Value> = rules
        .iter()
        .filter(|r| graph.out(r.id, EdgeRelation::DependsOn).is_empty())
        .map(|r| brief_of(r))
        .collect();
    let rules_without_conclusion: Vec<Value> = rules
        .iter()
        .filter(|r| graph.out(r.id, EdgeRelation::Enables).is_empty())
        .map(|r| brief_of(r))
        .collect();
    let mut missing_required = Vec::new();
    let mut missing_source = Vec::new();
    for entity_type in ALL_TYPES {
        for node in graph.of_type(entity_type) {
            let missing: Vec<&str> = vocab::required_properties(entity_type)
                .iter()
                .copied()
                .filter(|key| !present(node, key))
                .collect();
            if !missing.is_empty() {
                let mut entry = brief_of(node);
                entry["missing"] = json!(missing);
                missing_required.push(entry);
            }
            let sourced = match entity_type {
                FORM => present(node, "source_url") && present(node, "source_sha256"),
                FORM_SECTION | FORM_ITEM | FORM_RULE => graph.form_of(node).is_some(),
                FIELD => {
                    is_general(node)
                        || !graph.items_of_field(node.id).is_empty()
                        || graph
                            .values_of_field(node.id)
                            .iter()
                            .any(|v| !graph.incoming(*v, EdgeRelation::VariantOf).is_empty())
                }
                FIELD_VALUE => graph.parent_field(node.id).is_some(),
                FORM_ISSUER => !graph.out(node.id, EdgeRelation::Contains).is_empty(),
                _ => true,
            };
            if !sourced {
                missing_source.push(brief_of(node));
            }
        }
    }

    let report = json!({
        "unbound_items": unbound_items,
        "malformed_bindings": malformed_bindings,
        "fields_without_definition": without_definition,
        "fields_without_items": without_items,
        "shared_names": shared_names,
        "values_without_options": values_without_options,
        "rules_without_premise": rules_without_premise,
        "rules_without_conclusion": rules_without_conclusion,
        "missing_required": missing_required,
        "missing_source": missing_source,
    });
    let clean = report
        .as_object()
        .is_some_and(|m| m.values().all(|v| v.as_array().is_some_and(Vec::is_empty)));
    let mut report = report;
    report["clean"] = json!(clean);
    Ok(report)
}

/// forms.export: the namespace's forms graph by content key, with its log.
pub(crate) async fn export(ctx: &Ctx<'_>, _params: Value) -> Result<Value, RuntimeError> {
    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let warnings: Vec<String> = Vec::new();
    let mut keys: HashMap<Uuid, String> = HashMap::new();
    let mut by_key: BTreeMap<String, &Entity> = BTreeMap::new();
    // A document that dropped a node would not replay to the same graph, so
    // every node that cannot be written under its own key refuses the export.
    let mut lossy = Vec::new();
    for node in graph.nodes() {
        match graph.key(node.id) {
            Some(key) => {
                if let Some(previous) = by_key.insert(key.clone(), node) {
                    lossy.push(format!(
                        "two nodes share the key {key} ({} and {})",
                        previous.id, node.id
                    ));
                }
                keys.insert(node.id, key);
            }
            None => lossy.push(format!(
                "{} \"{}\" ({}) lacks the properties its key is made of",
                type_of(node),
                node.name,
                node.id
            )),
        }
    }
    if !lossy.is_empty() {
        lossy.sort();
        return Err(invalid(format!(
            "the forms graph cannot be exported without loss, {} node(s) affected: {}",
            lossy.len(),
            lossy.iter().take(5).cloned().collect::<Vec<_>>().join("; ")
        )));
    }

    let nodes: Vec<Value> = by_key
        .iter()
        .map(|(key, node)| {
            let mut properties = props(node);
            if let Some(means) = properties.get("means") {
                let mapped: Vec<String> = strings(Some(means))
                    .iter()
                    .filter_map(|id| Uuid::parse_str(id).ok())
                    .filter_map(|id| keys.get(&id).cloned())
                    .collect();
                properties.insert("means".to_owned(), json!(mapped));
            }
            json!({
                "key": key,
                "type": type_of(node),
                "name": node.name,
                "description": node.description,
                "properties": properties,
            })
        })
        .collect();
    let mut edges: Vec<(String, &'static str, String)> = graph
        .edges()
        .iter()
        .filter_map(|(s, r, t)| {
            let (source, target) = (keys.get(s)?.clone(), keys.get(t)?.clone());
            // A symmetric edge has no stored direction a reader can rely on, so the
            // document writes it with the smaller key first.
            if r.is_symmetric() && target < source {
                Some((target, r.as_str(), source))
            } else {
                Some((source, r.as_str(), target))
            }
        })
        .collect();
    edges.sort();
    edges.dedup();

    /// A decision note's content, its kept properties and the keys of the fields it annotates.
    type LoggedNote = (String, Map<String, Value>, Vec<String>);
    let mut notes: HashMap<Uuid, LoggedNote> = HashMap::new();
    for field in graph.of_type(FIELD) {
        let Some(field_key) = keys.get(&field.id) else {
            continue;
        };
        let mut offset = 0;
        loop {
            let filter = EdgeListFilter {
                target_id: Some(field.id),
                relations: vec![EdgeRelation::Annotates],
                ..Default::default()
            };
            let page = ctx
                .runtime
                .list_edges(ctx.token, filter, ANNOTATION_PAGE, offset)
                .await?;
            let read = page.len() as u32;
            for edge in page {
                if edge.deleted_at.is_some() {
                    continue;
                }
                if let Some(entry) = notes.get_mut(&edge.source_id) {
                    entry.2.push(field_key.clone());
                    continue;
                }
                let Some(note) = ctx
                    .runtime
                    .get_note_including_deleted(ctx.token, edge.source_id)
                    .await?
                else {
                    continue;
                };
                if note.deleted_at.is_some() || note.kind != DECISION {
                    continue;
                }
                let Some(Value::Object(properties)) = note.properties else {
                    continue;
                };
                if !properties
                    .get("verb")
                    .and_then(Value::as_str)
                    .is_some_and(|v| v.starts_with("forms."))
                {
                    continue;
                }
                let kept: Map<String, Value> = properties
                    .into_iter()
                    .filter(|(k, _)| LOG_KEYS.contains(&k.as_str()))
                    .collect();
                notes.insert(note.id, (note.content, kept, vec![field_key.clone()]));
            }
            if read < ANNOTATION_PAGE {
                break;
            }
            offset += read;
        }
    }
    let mut log: Vec<(Vec<String>, String, String, Value)> = notes
        .into_values()
        .map(|(content, properties, mut annotates)| {
            annotates.sort();
            annotates.dedup();
            let properties = Value::Object(properties);
            (annotates, content, properties.to_string(), properties)
        })
        .collect();
    log.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)).then(a.2.cmp(&b.2)));

    Ok(json!({
        "format": FORMAT,
        "nodes": nodes,
        "edges": edges.iter().map(|(s, r, t)| json!({"source": s, "relation": r, "target": t})).collect::<Vec<_>>(),
        "log": log.into_iter().map(|(annotates, content, _, properties)| json!({
            "content": content,
            "properties": properties,
            "annotates": annotates,
        })).collect::<Vec<_>>(),
        "warnings": warnings,
    }))
}
