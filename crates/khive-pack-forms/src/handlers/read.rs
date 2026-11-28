//! The forms read verbs. Each reads one snapshot of the namespace's forms graph.

use std::collections::{BTreeMap, HashMap, HashSet};

use khive_runtime::{EdgeListFilter, RuntimeError};
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use super::params;
use super::views::{field_reply, usage};
use super::write::{DECISION, FORMAT, LOG_KEYS};
use super::Ctx;
use crate::store::{
    aliases, definition, invalid, is_general, is_stating, label, prop, prop_text, props, scalar,
    strings, type_of, wordings, Graph,
};
use crate::text::normalize;
use crate::vocab::{
    self, ALL_TYPES, DOCUMENT_TYPE, FIELD, FIELD_VALUE, FORM, FORM_ISSUER, FORM_ITEM, FORM_RULE,
    FORM_SECTION, FORM_SUBJECT, FORM_TERM, TERM_LISTS,
};

const ANNOTATION_PAGE: u32 = 1000;

/// A form as findings and replies cite it.
pub(super) fn cite(form: &Entity) -> Value {
    json!({
        "id": form.id.to_string(),
        "form_id": prop_text(form, "form_id"),
        "revision": prop_text(form, "revision"),
        "title": form.name,
        "source_url": prop_text(form, "source_url"),
    })
}

pub(super) fn form_name(form: &Entity) -> String {
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
        FORM_SUBJECT => json!({"form": prop_text(node, "form_key")}),
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
    for key in [
        "relation",
        "document_type",
        "about",
        "known_to",
        "known_to_via",
        "state",
        "repeats",
        "necessity",
        "group",
        "selects",
    ] {
        if let Some(value) = prop(item, key) {
            view[key] = value.clone();
        }
    }
    view
}

/// A subject as a form's neighbourhood shows it.
fn subject_view(subject: &Entity) -> Value {
    let mut view = json!({
        "id": prop_text(subject, "subject_id"),
        "kind": prop_text(subject, "kind"),
        "role": prop_text(subject, "role"),
    });
    for key in ["qualifier", "repeats", "relations"] {
        if let Some(value) = prop(subject, key) {
            view[key] = value.clone();
        }
    }
    view
}

/// Whether a subject names a folded term of a list: as its kind, its role, or
/// the name of one of its relations.
fn names_term(subject: &Entity, list: &str, term: &str) -> bool {
    if list != "relation" {
        return prop_text(subject, list).is_some_and(|v| normalize(&v) == term);
    }
    prop(subject, "relations")
        .and_then(Value::as_array)
        .is_some_and(|relations| {
            relations.iter().any(|r| {
                r.get("relation")
                    .and_then(Value::as_str)
                    .is_some_and(|r| normalize(r) == term)
            })
        })
}

/// The fields each line about a subject asks for, in printed order.
fn fields_asked_of(graph: &Graph, subject: &Entity) -> Vec<String> {
    let (Some(form_key), Some(id)) = (
        prop_text(subject, "form_key"),
        prop_text(subject, "subject_id"),
    ) else {
        return Vec::new();
    };
    let mut items: Vec<&Entity> = graph
        .of_type(FORM_ITEM)
        .into_iter()
        .filter(|i| {
            prop_text(i, "form_key").as_deref() == Some(form_key.as_str())
                && prop_text(i, "about").is_some_and(|a| normalize(&a) == normalize(&id))
        })
        .collect();
    items.sort_by(|a, b| {
        number_of(a, "page")
            .total_cmp(&number_of(b, "page"))
            .then(number_of(a, "order").total_cmp(&number_of(b, "order")))
    });
    let mut fields = Vec::new();
    for item in items {
        for target in graph.out(item.id, EdgeRelation::VariantOf) {
            let field = if graph.is(target, FIELD_VALUE) {
                graph.parent_field(target)
            } else {
                Some(target)
            };
            if let Some(name) = field.and_then(|f| graph.get(f)).map(|f| f.name.clone()) {
                if !fields.contains(&name) {
                    fields.push(name);
                }
            }
        }
    }
    fields
}

fn number_of(node: &Entity, key: &str) -> f64 {
    prop(node, key).and_then(Value::as_f64).unwrap_or(f64::MAX)
}

fn find_node<'g>(graph: &'g Graph, spec: &str) -> Result<&'g Entity, RuntimeError> {
    if let Ok(id) = Uuid::parse_str(spec) {
        return graph
            .get(id)
            .ok_or_else(|| invalid(format!("no forms node has id {spec}")));
    }
    if let Ok(found) = graph.find_form(spec).or_else(|_| graph.find_field(spec)) {
        return Ok(found);
    }
    // A term is named by its list and itself: kind:party, role:payer.
    spec.split_once(':')
        .filter(|(list, _)| TERM_LISTS.iter().any(|(_, name)| name == list))
        .and_then(|(list, name)| graph.terms(list).remove(&normalize(name)))
        .ok_or_else(|| invalid(format!("no form, field or term is named \"{spec}\"")))
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

pub(super) fn names(graph: &Graph, ids: &[Uuid]) -> Vec<String> {
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
            let mut subjects: Vec<&Entity> = graph.subjects_of(&key).into_values().collect();
            subjects.sort_by(|a, b| a.name.cmp(&b.name));
            reply["subjects"] = Value::Array(subjects.into_iter().map(subject_view).collect());
            reply["purposes"] = json!(strings(prop(root, "purposes")));
            reply["constraints"] = prop(root, "constraints").cloned().unwrap_or(json!([]));
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
            // Whom each line on this field asks it of, on forms that declare subjects.
            reply["asked_of"] = Value::Array(
                graph
                    .items_of_field(root.id)
                    .into_iter()
                    .filter_map(|i| graph.get(i))
                    .filter_map(|item| {
                        let form = prop_text(item, "form_key")?;
                        let about = prop_text(item, "about")?;
                        let role = graph
                            .subjects_of(&form)
                            .get(&normalize(&about))
                            .and_then(|s| prop_text(s, "role"));
                        Some(json!({
                            "form": form,
                            "item": label(item),
                            "subject": about,
                            "role": role,
                            "known_to": prop_text(item, "known_to"),
                        }))
                    })
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
        FORM_SUBJECT => {
            reply["subject"] = subject_view(root);
            reply["fields"] = json!(fields_asked_of(&graph, root));
        }
        FORM_TERM => {
            // Every subject this term names, as its kind, its role or one of
            // its relations, with the form that declares it and the fields
            // that form asks of it.
            let list = prop_text(root, "list").unwrap_or_default();
            let term = normalize(&root.name);
            let mut subjects: Vec<&Entity> = graph
                .of_type(FORM_SUBJECT)
                .into_iter()
                .filter(|s| names_term(s, &list, &term))
                .collect();
            subjects.sort_by(|a, b| {
                prop_text(a, "form_key")
                    .cmp(&prop_text(b, "form_key"))
                    .then(a.name.cmp(&b.name))
            });
            reply["term"] = json!({
                "list": list,
                "name": root.name,
                "definition": prop_text(root, "definition"),
                "aliases": aliases(root),
                "carries": prop(root, "carries").and_then(Value::as_bool).unwrap_or(false),
            });
            if list == "purpose" {
                // Every form that says it is for this purpose, under its name
                // or one of its aliases.
                let wordings: Vec<String> = std::iter::once(term.clone())
                    .chain(aliases(root).iter().map(|a| normalize(a)))
                    .collect();
                let mut forms: Vec<&Entity> = graph
                    .of_type(FORM)
                    .into_iter()
                    .filter(|f| {
                        strings(prop(f, "purposes"))
                            .iter()
                            .any(|p| wordings.contains(&normalize(p)))
                    })
                    .collect();
                forms.sort_by_key(|f| graph.key(f.id));
                reply["forms"] = Value::Array(forms.into_iter().map(cite).collect());
            }
            reply["subjects"] = Value::Array(
                subjects
                    .into_iter()
                    .map(|s| {
                        let mut view = subject_view(s);
                        view["form"] = json!(prop_text(s, "form_key"));
                        view["fields"] = json!(fields_asked_of(&graph, s));
                        view
                    })
                    .collect(),
            );
        }
        _ => {}
    }
    Ok(reply)
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
            // A stating line has nobody who fills it in.
            let missing: Vec<&str> = vocab::required_properties(entity_type)
                .iter()
                .copied()
                .filter(|key| !(*key == "filled_by" && is_stating(node)))
                .filter(|key| !present(node, key))
                .collect();
            if !missing.is_empty() {
                let mut entry = brief_of(node);
                entry["missing"] = json!(missing);
                missing_required.push(entry);
            }
            let sourced = match entity_type {
                FORM => present(node, "source_url") && present(node, "source_sha256"),
                FORM_SECTION | FORM_ITEM | FORM_RULE | FORM_SUBJECT => {
                    graph.form_of(node).is_some()
                }
                FIELD => {
                    is_general(node)
                        || !graph.items_of_field(node.id).is_empty()
                        || graph
                            .values_of_field(node.id)
                            .iter()
                            .any(|v| !graph.incoming(*v, EdgeRelation::VariantOf).is_empty())
                }
                FIELD_VALUE => graph.parent_field(node.id).is_some(),
                FORM_ISSUER | DOCUMENT_TYPE => {
                    !graph.out(node.id, EdgeRelation::Contains).is_empty()
                }
                _ => true,
            };
            if !sourced {
                missing_source.push(brief_of(node));
            }
        }
    }

    // Two lists of what was guessed or not yet said, not of what is broken:
    // a document type minted for a form that named none, and a form loaded
    // before document types existed. Neither keeps the graph from being
    // clean.
    let minted_document_types: Vec<Value> = graph
        .of_type(DOCUMENT_TYPE)
        .into_iter()
        .filter(|t| prop(t, "minted").and_then(Value::as_bool) == Some(true))
        .map(|t| {
            let mut entry = brief_of(t);
            entry["document_type_id"] = json!(prop_text(t, "document_type_id"));
            entry
        })
        .collect();
    let forms_without_document_type: Vec<Value> = graph
        .of_type(FORM)
        .into_iter()
        .filter(|f| graph.document_type_of(f.id).is_none())
        .map(cite)
        .collect();

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
    report["minted_document_types"] = json!(minted_document_types);
    report["forms_without_document_type"] = json!(forms_without_document_type);
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

/// A term as forms.terms shows it.
fn term_view(term: &Entity) -> Value {
    json!({
        "name": term.name,
        "definition": prop_text(term, "definition"),
        "aliases": aliases(term),
        "carries": prop(term, "carries").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// forms.terms: the namespace's term lists, each term with its aliases.
pub(crate) async fn terms(ctx: &Ctx<'_>, _params: Value) -> Result<Value, RuntimeError> {
    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let all = graph.of_type(FORM_TERM);
    let mut lists = Map::new();
    for (key, list) in TERM_LISTS {
        let terms: Vec<Value> = all
            .iter()
            .filter(|t| prop_text(t, "list").as_deref() == Some(list))
            .map(|t| term_view(t))
            .collect();
        lists.insert(key.to_owned(), Value::Array(terms));
    }
    Ok(json!({ "terms": lists }))
}

/// forms.subjects: the subjects one form revision declares, the revision loaded last unless
/// one is named.
pub(crate) async fn subjects(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let spec = params::text(&args, "form")?;
    let mut revision = params::opt_text(&args, "revision")?;
    let form_id = match (spec.rsplit_once('@'), &revision) {
        (Some((_, in_form)), Some(named)) => {
            return Err(invalid(format!(
                "form {spec} names revision {in_form} and revision names {named}; name the revision once"
            )));
        }
        (Some((form_id, named)), None) => {
            revision = Some(named.to_owned());
            form_id.to_owned()
        }
        (None, _) => spec.clone(),
    };
    let graph = Graph::load(ctx.runtime, ctx.token).await?;
    let mut revisions: Vec<&Entity> = graph
        .of_type(FORM)
        .into_iter()
        .filter(|f| prop_text(f, "form_id").as_deref() == Some(form_id.as_str()))
        .collect();
    revisions.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(prop_text(a, "revision").cmp(&prop_text(b, "revision")))
    });
    let form = match &revision {
        Some(named) => revisions
            .iter()
            .find(|f| prop_text(f, "revision").as_deref() == Some(named.as_str()))
            .copied()
            .ok_or_else(|| invalid(format!("no form {form_id}@{named} is loaded")))?,
        None => revisions
            .last()
            .copied()
            .ok_or_else(|| invalid(format!("no form {form_id} is loaded")))?,
    };
    let key = graph.key(form.id).unwrap_or_default();
    let mut subjects: Vec<&Entity> = graph.subjects_of(&key).into_values().collect();
    subjects.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(json!({
        "form": cite(form),
        "revisions": revisions
            .iter()
            .filter_map(|f| prop_text(f, "revision"))
            .collect::<Vec<_>>(),
        "subjects": subjects.into_iter().map(subject_view).collect::<Vec<_>>(),
    }))
}
