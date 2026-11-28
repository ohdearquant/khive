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
use crate::vocab::{self, MAX_ROWS, MUSTS, NECESSITIES, TERM_LISTS};

/// One term a document loads into one of its lists.
pub(super) struct TermSpec {
    pub(super) list: &'static str,
    pub(super) name: String,
    pub(super) definition: Option<String>,
    pub(super) aliases: Vec<String>,
}

impl TermSpec {
    /// The term's name and its aliases, folded.
    pub(super) fn wordings(&self) -> Vec<String> {
        std::iter::once(&self.name)
            .chain(&self.aliases)
            .map(|w| normalize(w))
            .collect()
    }
}

/// The terms of a document's `terms` object, list by list, and the relations
/// it names under `carrying_relations`.
pub(super) fn term_specs(doc: &Args) -> Result<(Vec<TermSpec>, Vec<String>), RuntimeError> {
    let Some(terms) = params::object(doc, "terms")? else {
        return Ok((Vec::new(), Vec::new()));
    };
    if let Some(key) = terms
        .keys()
        .find(|key| *key != "carrying_relations" && !TERM_LISTS.iter().any(|(name, _)| name == key))
    {
        return Err(invalid(format!(
            "terms has no list \"{key}\"; its lists are kinds, roles, relations, purposes, counting_qualifiers and carrying_relations"
        )));
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (key, list) in TERM_LISTS {
        for entry in params::list(terms, key)? {
            let (name, definition, aliases) = match entry {
                Value::String(name) if !name.trim().is_empty() => {
                    (name.trim().to_owned(), None, Vec::new())
                }
                Value::Object(obj) => (
                    params::text(obj, "name")?,
                    params::opt_text(obj, "definition")?,
                    params::string_list(obj, "aliases")?,
                ),
                _ => {
                    return Err(invalid(format!(
                        "each entry of terms.{key} is a name or {{name, definition?, aliases?}}"
                    )))
                }
            };
            let spec = TermSpec {
                list,
                name,
                definition,
                aliases,
            };
            for wording in spec.wordings() {
                if !seen.insert((list, wording.clone())) {
                    return Err(invalid(format!(
                        "terms.{key} gives \"{wording}\" twice, as a name or an alias"
                    )));
                }
            }
            out.push(spec);
        }
    }
    let mut carrying = Vec::new();
    for entry in params::list(terms, "carrying_relations")? {
        match entry {
            Value::String(name) if !name.trim().is_empty() => carrying.push(name.trim().to_owned()),
            _ => {
                return Err(invalid(
                    "each entry of terms.carrying_relations is the name of a relation",
                ))
            }
        }
    }
    Ok((out, carrying))
}

/// Each loaded term by its list and a folded wording (its name or an alias),
/// to its canonical name.
pub(super) type KnownTerms = HashMap<(String, String), String>;

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
/// relation points at a subject the document does not declare, and write each
/// term a subject names by an alias under the term's canonical name. `known`
/// holds every term loaded before or by the document.
pub(super) fn check_subjects(
    specs: &mut [SubjectSpec],
    known: &KnownTerms,
) -> Result<(), RuntimeError> {
    let declared: HashSet<String> = specs.iter().map(|s| normalize(&s.id)).collect();
    let canonical =
        |list: &str, term: &str| known.get(&(list.to_owned(), normalize(term))).cloned();
    for spec in specs.iter_mut() {
        for (list, term) in [("kind", &mut spec.kind), ("role", &mut spec.role)] {
            let Some(name) = canonical(list, term) else {
                return Err(invalid(format!(
                    "subject \"{}\" has {list} \"{term}\", which is not a loaded {list}; list it under terms",
                    spec.id
                )));
            };
            *term = name;
        }
        // A qualifier that is a loaded counting qualifier is stored under its
        // canonical name; any other qualifier is kept as the document gives it.
        if let Some(qualifier) = spec.qualifier.as_mut() {
            if let Some(name) = canonical("counting_qualifier", qualifier) {
                *qualifier = name;
            }
        }
        for (relation, to) in spec.relations.iter_mut() {
            let Some(name) = canonical("relation", relation) else {
                return Err(invalid(format!(
                    "subject \"{}\" names the relation \"{relation}\", which is not a loaded relation; list it under terms",
                    spec.id
                )));
            };
            *relation = name;
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

/// A subject as the lines of its form name it: the id as declared, the rows
/// it repeats when it does, and the subject it names under each relation.
#[derive(Clone)]
pub(super) struct DeclaredSubject {
    id: String,
    rows: Option<u64>,
    relations: Vec<(String, String)>,
}

/// The subjects a form declares, by folded id.
pub(super) type Declared = HashMap<String, DeclaredSubject>;

pub(super) fn declared_in(specs: &[SubjectSpec]) -> Declared {
    specs
        .iter()
        .map(|s| {
            (
                normalize(&s.id),
                DeclaredSubject {
                    id: s.id.clone(),
                    rows: s.repeats,
                    relations: s.relations.clone(),
                },
            )
        })
        .collect()
}

pub(super) fn declared_on(subjects: &HashMap<String, &Entity>) -> Declared {
    subjects
        .iter()
        .map(|(folded, subject)| {
            let relations = prop(subject, "relations")
                .and_then(Value::as_array)
                .map(|list| {
                    list.iter()
                        .filter_map(|r| {
                            Some((
                                r.get("relation")?.as_str()?.to_owned(),
                                r.get("to")?.as_str()?.to_owned(),
                            ))
                        })
                        .collect()
                })
                .unwrap_or_default();
            (
                folded.clone(),
                DeclaredSubject {
                    id: prop_text(subject, "subject_id").unwrap_or_else(|| subject.name.clone()),
                    rows: prop(subject, "repeats").and_then(Value::as_u64),
                    relations,
                },
            )
        })
        .collect()
}

/// The line properties of a printed item beyond its label, page and order:
/// `about`, `filled_by`, `known_to`, `state`, `repeats`, `necessity`,
/// `group` and `selects`, checked against the subjects its form declares.
///
/// `known_to` names the subject that already holds the line's value, apart
/// from the one who writes it: a declared subject, or `{relation}` for the
/// subject the line's own subject names under that relation (an account
/// held at an institution is known to that institution).
///
/// On a form that declares subjects, every item that asks for a field names
/// the subject it is about and the subject who writes it, both declared, and
/// is required unless it states another necessity. On a form that declares
/// none, an item naming a subject is refused, and a necessity is kept only
/// when the item states one.
///
/// A stating line names the subject it is about like any line, and has no
/// writer and no necessity. A line that requires an attached document has a
/// writer and a necessity as a line that asks for a field does.
pub(super) fn line_props(
    obj: &Args,
    label: &str,
    item_kind: &str,
    relation: &str,
    filled_by: Option<&str>,
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
    let requires = relation == "requires";
    // A line someone writes: one that asks for a field, or one that takes an
    // attached document.
    let written = (asks && relation != "states") || requires;
    let with_subjects = !declared.is_empty();

    let subject = |id: &str, role: &str| -> Result<DeclaredSubject, RuntimeError> {
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
    let mut about_subject = None;
    match &about {
        Some(id) => {
            let declared = subject(id, "the subject it is about")?;
            props.insert("about".to_owned(), json!(declared.id));
            about_subject = Some(declared);
        }
        None if asks && with_subjects => {
            return Err(invalid(format!(
                "item \"{label}\" names no subject it is about; its form declares subjects, so every line that asks for a field names one (about)"
            )));
        }
        None => {}
    }
    if written && with_subjects {
        let writer = filled_by.ok_or_else(|| {
            invalid(format!(
                "item \"{label}\" names no subject who writes it (filled_by)"
            ))
        })?;
        let declared = subject(writer, "the subject who writes it (filled_by)")?;
        props.insert("filled_by".to_owned(), json!(declared.id));
    }
    match obj.get("known_to") {
        None | Some(Value::Null) => {}
        Some(_) if !asks => {
            return Err(invalid(format!(
                "item \"{label}\" asks for no field, so no subject holds its value (known_to)"
            )));
        }
        Some(Value::String(id)) => {
            let holder = subject(id, "the subject that already holds its value (known_to)")?;
            props.insert("known_to".to_owned(), json!(holder.id));
        }
        Some(via @ Value::Object(_)) => {
            let relation = params::text(params::entry(via, "known_to")?, "relation")?;
            let Some(about) = &about_subject else {
                return Err(invalid(format!(
                    "item \"{label}\" takes known_to through the relation \"{relation}\", but names no subject it is about"
                )));
            };
            let targets: Vec<&String> = about
                .relations
                .iter()
                .filter(|(r, _)| normalize(r) == normalize(&relation))
                .map(|(_, to)| to)
                .collect();
            let to = match targets.as_slice() {
                [to] => *to,
                [] => {
                    return Err(invalid(format!(
                        "item \"{label}\" takes known_to through the relation \"{relation}\", and its subject \"{}\" names no subject under that relation",
                        about.id
                    )));
                }
                _ => {
                    return Err(invalid(format!(
                        "item \"{label}\" takes known_to through the relation \"{relation}\", and its subject \"{}\" names more than one subject under it; name the subject instead",
                        about.id
                    )));
                }
            };
            let holder = subject(to, "the subject that already holds its value (known_to)")?;
            props.insert("known_to".to_owned(), json!(holder.id));
            props.insert("known_to_via".to_owned(), json!(relation));
        }
        Some(_) => {
            return Err(invalid(format!(
                "item \"{label}\": known_to is the id of a subject, or {{relation}} naming the subject its own subject holds under that relation"
            )));
        }
    }
    let mut selected = Vec::new();
    for id in &selects {
        selected.push(subject(id, "a subject its value selects")?.id);
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
        Some(_) if !asks && !requires => {
            return Err(invalid(format!(
                "item \"{label}\" asks for no field, so it states no necessity"
            )));
        }
        Some(necessity) => Some(necessity),
        None if written && with_subjects => Some("required".to_owned()),
        None => None,
    };
    let rows = own_rows
        .or(about_subject.as_ref().and_then(|s| s.rows))
        .unwrap_or(1);
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

/// One end of a constraint: the line that asks a subject's field, in a state.
pub(super) struct End {
    pub(super) subject: Option<String>,
    pub(super) field: String,
    pub(super) state: Option<String>,
}

impl End {
    pub(super) fn describe(&self) -> String {
        let whose = self
            .subject
            .as_ref()
            .map(|s| format!(" of \"{s}\""))
            .unwrap_or_default();
        let state = self
            .state
            .as_ref()
            .map(|s| format!(" ({s})"))
            .unwrap_or_default();
        format!("\"{}\"{whose}{state}", self.field)
    }
}

/// A constraint a form declares across two of its lines.
pub(super) struct ConstraintSpec {
    pub(super) must: String,
    pub(super) ends: [End; 2],
    text: Option<String>,
}

/// The constraints of a document, each `{must, lines: [two ends], text?}`.
pub(super) fn constraint_specs(doc: &Args) -> Result<Vec<ConstraintSpec>, RuntimeError> {
    let mut out = Vec::new();
    for constraint in params::list(doc, "constraints")? {
        let obj = params::entry(constraint, "constraint")?;
        let must = params::one_of(params::text(obj, "must")?, "must", &MUSTS)?;
        let mut ends = Vec::new();
        for end in params::list(obj, "lines")? {
            let end = params::entry(end, "constraint line")?;
            ends.push(End {
                subject: params::opt_text(end, "subject")?,
                field: params::text(end, "field")?,
                state: params::opt_text(end, "state")?,
            });
        }
        let ends: [End; 2] = ends.try_into().map_err(|ends: Vec<End>| {
            invalid(format!(
                "a constraint holds two lines to {must}; this one names {}",
                ends.len()
            ))
        })?;
        let fold = |end: &End| {
            (
                end.subject.as_deref().map(normalize),
                normalize(&end.field),
                end.state.as_deref().map(normalize),
            )
        };
        if fold(&ends[0]) == fold(&ends[1]) {
            return Err(invalid(format!(
                "a constraint holds two different lines to {must}; this one names {} twice",
                ends[0].describe()
            )));
        }
        out.push(ConstraintSpec {
            text: params::opt_text(obj, "text")?,
            must,
            ends,
        });
    }
    Ok(out)
}

/// The constraints as a form node keeps them.
pub(super) fn constraints_prop(specs: &[ConstraintSpec]) -> Value {
    Value::Array(
        specs
            .iter()
            .map(|spec| {
                let lines: Vec<Value> = spec
                    .ends
                    .iter()
                    .map(|end| {
                        let mut line = Map::new();
                        if let Some(subject) = &end.subject {
                            line.insert("subject".to_owned(), json!(subject));
                        }
                        line.insert("field".to_owned(), json!(end.field));
                        if let Some(state) = &end.state {
                            line.insert("state".to_owned(), json!(state));
                        }
                        Value::Object(line)
                    })
                    .collect();
                let mut constraint = Map::new();
                constraint.insert("must".to_owned(), json!(spec.must));
                constraint.insert("lines".to_owned(), Value::Array(lines));
                if let Some(text) = &spec.text {
                    constraint.insert("text".to_owned(), json!(text));
                }
                Value::Object(constraint)
            })
            .collect(),
    )
}
