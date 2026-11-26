//! How forms nodes are shown in replies.

use khive_storage::Entity;
use serde_json::{json, Value};

use crate::store::{aliases, definition, is_general, label, prop, prop_text, wordings, Graph};
use crate::text::{overlap, words};
use crate::vocab::FIELD;

pub(crate) fn value_reply(value: &Entity) -> Value {
    json!({
        "id": value.id.to_string(),
        "name": value.name,
        "definition": definition(value),
    })
}

/// A field with its definition, value type, aliases, item count, values and,
/// for a general field, the narrower fields it may mean.
pub(crate) fn field_reply(graph: &Graph, field: &Entity) -> Value {
    let mut reply = json!({
        "id": field.id.to_string(),
        "name": field.name,
        "definition": definition(field),
        "value_type": prop_text(field, "value_type"),
        "aliases": aliases(field),
        "items": graph.items_of_field(field.id).len(),
    });
    if is_general(field) {
        reply["general"] = json!(true);
        reply["means"] = Value::Array(
            graph
                .children_of_field(field.id)
                .into_iter()
                .filter_map(|child| graph.get(child))
                .map(|child| json!({"id": child.id.to_string(), "name": child.name}))
                .collect(),
        );
    }
    let values: Vec<Value> = graph
        .values_of_field(field.id)
        .into_iter()
        .filter_map(|value| graph.get(value))
        .map(value_reply)
        .collect();
    if !values.is_empty() {
        reply["values"] = Value::Array(values);
    }
    reply
}

/// Where an item is printed: its form, label and page.
pub(crate) fn usage(item: &Entity) -> Value {
    json!({
        "form": prop_text(item, "form_key"),
        "item": label(item),
        "item_id": item.id.to_string(),
        "page": prop(item, "page"),
    })
}

/// Up to three live, non-general fields of `value_type` nearest to the
/// proposed definition, then name; fields sharing no word are left out.
pub(crate) fn nearest(
    graph: &Graph,
    name: &str,
    definition_text: &str,
    value_type: &str,
) -> Vec<Value> {
    let name_words = words(name);
    let definition_words = words(definition_text);
    let mut scored: Vec<(f64, f64, &Entity)> = graph
        .of_type(FIELD)
        .into_iter()
        .filter(|field| {
            !is_general(field) && prop_text(field, "value_type").as_deref() == Some(value_type)
        })
        .map(|field| {
            let by_definition = definition(field)
                .map(|d| overlap(&definition_words, &words(&d)))
                .unwrap_or(0.0);
            let by_name = wordings(field)
                .iter()
                .map(|w| overlap(&name_words, &words(w)))
                .fold(0.0, f64::max);
            (by_definition, by_name, field)
        })
        .filter(|(d, n, _)| *d > 0.0 || *n > 0.0)
        .collect();
    scored.sort_by(|a, b| {
        b.0.total_cmp(&a.0)
            .then(b.1.total_cmp(&a.1))
            .then(a.2.name.cmp(&b.2.name))
            .then(a.2.id.cmp(&b.2.id))
    });
    scored
        .into_iter()
        .take(3)
        .map(|(by_definition, by_name, field)| {
            let mut reply = field_reply(graph, field);
            reply["score"] = json!({"definition": by_definition, "name": by_name});
            reply
        })
        .collect()
}
