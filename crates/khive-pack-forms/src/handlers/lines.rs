//! Subjects and lines: who or what a printed line is about, who writes it,
//! how often it repeats and how necessary it is, read from a document and
//! checked against the subjects its form declares, with the term lists that
//! name subject kinds, roles and relations.

use std::collections::{HashMap, HashSet};

use khive_runtime::RuntimeError;
use khive_storage::Entity;
use serde_json::{json, Map, Value};

use super::params::{self, Args};
use crate::store::{invalid, prop, prop_text};
use crate::text::normalize;
use crate::vocab::{self, MAX_ROWS, NECESSITIES, TERM_LISTS};

/// One term a document loads into one of its lists.
pub(super) struct TermSpec {
    pub(super) list: &'static str,
    pub(super) name: String,
    pub(super) definition: Option<String>,
}

/// The terms of a document's `terms` object, list by list.
pub(super) fn term_specs(doc: &Args) -> Result<Vec<TermSpec>, RuntimeError> {
    let Some(terms) = params::object(doc, "terms")? else {
        return Ok(Vec::new());
    };
    if let Some(key) = terms
        .keys()
        .find(|key| !TERM_LISTS.iter().any(|(name, _)| name == key))
    {
        return Err(invalid(format!(
            "terms has no list \"{key}\"; its lists are kinds, roles and relations"
        )));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (key, list) in TERM_LISTS {
        for entry in params::list(terms, key)? {
            let (name, definition) = match entry {
                Value::String(name) if !name.trim().is_empty() => (name.trim().to_owned(), None),
                Value::Object(obj) => (
                    params::text(obj, "name")?,
                    params::opt_text(obj, "definition")?,
                ),
                _ => {
                    return Err(invalid(format!(
                        "each entry of terms.{key} is a name or {{name, definition?}}"
                    )))
                }
            };
            if !seen.insert((list, normalize(&name))) {
                return Err(invalid(format!("terms.{key} lists \"{name}\" twice")));
            }
            out.push(TermSpec {
                list,
                name,
                definition,
            });
        }
    }
    Ok(out)
}

/// A subject a document declares on its form.
pub(super) struct SubjectSpec {
    pub(super) id: String,
    kind: String,
    role: String,
    qualifier: Option<String>,
    repeats: Option<u64>,
    relations: Vec<(String, String)>,
}

fn repeats(obj: &Args, what: &str) -> Result<Option<u64>, RuntimeError> {
    match obj.get("repeats") {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value
            .as_u64()
            .filter(|rows| (1..=MAX_ROWS).contains(rows))
            .map(Some)
            .ok_or_else(|| {
                invalid(format!(
                    "{what}: repeats is a whole number of rows, from 1 to {MAX_ROWS}"
                ))
            }),
    }
}

/// The subjects of a document, in the order it declares them.
pub(super) fn subject_specs(doc: &Args) -> Result<Vec<SubjectSpec>, RuntimeError> {
    let mut out: Vec<SubjectSpec> = Vec::new();
    for subject in params::list(doc, "subjects")? {
        let obj = params::entry(subject, "subject")?;
        let id = params::text(obj, "id")?;
        if out.iter().any(|s| normalize(&s.id) == normalize(&id)) {
            return Err(invalid(format!(
                "the document declares subject \"{id}\" twice"
            )));
        }
        let mut relations = Vec::new();
        for relation in params::list(obj, "relations")? {
            let relation = params::entry(relation, "subject relation")?;
            relations.push((
                params::text(relation, "relation")?,
                params::text(relation, "to")?,
            ));
        }
        out.push(SubjectSpec {
            kind: params::text(obj, "kind")?,
            role: params::text(obj, "role")?,
            qualifier: params::opt_text(obj, "qualifier")?,
            repeats: repeats(obj, &format!("subject \"{id}\""))?,
            relations,
            id,
        });
    }
    Ok(out)
}

/// Refuse a subject whose kind, role or relation is not a loaded term, or whose
/// relation points at a subject the document does not declare. `known` holds
/// (list, folded term) for every term loaded before or by the document.
pub(super) fn check_subjects(
    specs: &[SubjectSpec],
    known: &HashSet<(String, String)>,
) -> Result<(), RuntimeError> {
    let declared: HashSet<String> = specs.iter().map(|s| normalize(&s.id)).collect();
    let loaded = |list: &str, term: &str| known.contains(&(list.to_owned(), normalize(term)));
    for spec in specs {
        for (list, term) in [("kind", &spec.kind), ("role", &spec.role)] {
            if !loaded(list, term) {
                return Err(invalid(format!(
                    "subject \"{}\" has {list} \"{term}\", which is not a loaded {list}; list it under terms",
                    spec.id
                )));
            }
        }
        for (relation, to) in &spec.relations {
            if !loaded("relation", relation) {
                return Err(invalid(format!(
                    "subject \"{}\" names the relation \"{relation}\", which is not a loaded relation; list it under terms",
                    spec.id
                )));
            }
            if !declared.contains(&normalize(to)) {
                return Err(invalid(format!(
                    "subject \"{}\" names \"{to}\" under the relation \"{relation}\", and the document does not declare \"{to}\"",
                    spec.id
                )));
            }
            if normalize(to) == normalize(&spec.id) {
                return Err(invalid(format!(
                    "subject \"{}\" names itself under the relation \"{relation}\"",
                    spec.id
                )));
            }
        }
    }
    Ok(())
}

/// The properties of a subject node.
pub(super) fn subject_props(spec: &SubjectSpec, form_key: &str) -> Map<String, Value> {
    let mut props = Map::new();
    props.insert("subject_id".to_owned(), json!(spec.id));
    props.insert("kind".to_owned(), json!(spec.kind));
    props.insert("role".to_owned(), json!(spec.role));
    props.insert("form_key".to_owned(), json!(form_key));
    if let Some(qualifier) = &spec.qualifier {
        props.insert("qualifier".to_owned(), json!(qualifier));
    }
    if let Some(rows) = spec.repeats {
        props.insert("repeats".to_owned(), json!(rows));
    }
    if !spec.relations.is_empty() {
        props.insert(
            "relations".to_owned(),
            Value::Array(
                spec.relations
                    .iter()
                    .map(|(relation, to)| json!({"relation": relation, "to": to}))
                    .collect(),
            ),
        );
    }
    props
}

/// The subjects a form declares, by folded id: the id as declared and the
/// rows the subject repeats, when it does.
pub(super) type Declared = HashMap<String, (String, Option<u64>)>;

pub(super) fn declared_in(specs: &[SubjectSpec]) -> Declared {
    specs
        .iter()
        .map(|s| (normalize(&s.id), (s.id.clone(), s.repeats)))
        .collect()
}

pub(super) fn declared_on(subjects: &HashMap<String, &Entity>) -> Declared {
    subjects
        .iter()
        .map(|(folded, subject)| {
            (
                folded.clone(),
                (
                    prop_text(subject, "subject_id").unwrap_or_else(|| subject.name.clone()),
                    prop(subject, "repeats").and_then(Value::as_u64),
                ),
            )
        })
        .collect()
}

/// The line properties of a printed item beyond its label, page and order:
/// `about`, `filled_by`, `state`, `repeats`, `necessity`, `group` and
/// `selects`, checked against the subjects its form declares.
///
/// On a form that declares subjects, every item that asks for a field names
/// the subject it is about and the subject who writes it, both declared, and
/// is required unless it states another necessity. On a form that declares
/// none, an item naming a subject is refused, and a necessity is kept only
/// when the item states one.
pub(super) fn line_props(
    obj: &Args,
    label: &str,
    item_kind: &str,
    filled_by: &str,
    declared: &Declared,
) -> Result<Map<String, Value>, RuntimeError> {
    let about = params::opt_text(obj, "about")?;
    let state = params::opt_text(obj, "state")?;
    let own_rows = repeats(obj, &format!("item \"{label}\""))?;
    let necessity = params::opt_text(obj, "necessity")?
        .map(|n| params::one_of(n, "necessity", &NECESSITIES))
        .transpose()?;
    let group = params::opt_text(obj, "group")?;
    let selects = params::string_list(obj, "selects")?;
    let asks = vocab::item_needs_field(item_kind);
    let with_subjects = !declared.is_empty();

    let subject = |id: &str, role: &str| -> Result<(String, Option<u64>), RuntimeError> {
        declared.get(&normalize(id)).cloned().ok_or_else(|| {
            if with_subjects {
                invalid(format!(
                    "item \"{label}\" names \"{id}\" as {role}, which its form does not declare"
                ))
            } else {
                invalid(format!(
                    "item \"{label}\" names \"{id}\" as {role}, but its form declares no subjects"
                ))
            }
        })
    };

    let mut props = Map::new();
    let mut subject_rows = None;
    match &about {
        Some(id) => {
            let (declared_id, rows) = subject(id, "the subject it is about")?;
            subject_rows = rows;
            props.insert("about".to_owned(), json!(declared_id));
        }
        None if asks && with_subjects => {
            return Err(invalid(format!(
                "item \"{label}\" names no subject it is about; its form declares subjects, so every line that asks for a field names one (about)"
            )));
        }
        None => {}
    }
    if asks && with_subjects {
        let (declared_id, _) = subject(filled_by, "the subject who writes it (filled_by)")?;
        props.insert("filled_by".to_owned(), json!(declared_id));
    }
    let mut selected = Vec::new();
    for id in &selects {
        selected.push(subject(id, "a subject its value selects")?.0);
    }
    if !selected.is_empty() {
        props.insert("selects".to_owned(), json!(selected));
    }
    if let Some(state) = state {
        props.insert("state".to_owned(), json!(state));
    }
    if let Some(rows) = own_rows {
        props.insert("repeats".to_owned(), json!(rows));
    }

    let necessity = match necessity {
        Some(_) if !asks => {
            return Err(invalid(format!(
                "item \"{label}\" asks for no field, so it states no necessity"
            )));
        }
        Some(necessity) => Some(necessity),
        None if asks && with_subjects => Some("required".to_owned()),
        None => None,
    };
    let rows = own_rows.or(subject_rows).unwrap_or(1);
    match (necessity.as_deref(), &group) {
        (Some("one_of"), None) => {
            return Err(invalid(format!(
                "item \"{label}\" is one_of a group; name the group"
            )));
        }
        (Some("one_of"), Some(_)) => {}
        (_, Some(_)) => {
            return Err(invalid(format!(
                "item \"{label}\" names a group, which only a one_of line has"
            )));
        }
        (Some("at_least_one_row"), None) if rows < 2 => {
            return Err(invalid(format!(
                "item \"{label}\" needs at least one row, but neither it nor its subject repeats"
            )));
        }
        _ => {}
    }
    if let Some(necessity) = necessity {
        props.insert("necessity".to_owned(), json!(necessity));
    }
    if let Some(group) = group {
        props.insert("group".to_owned(), json!(group));
    }
    Ok(props)
}
