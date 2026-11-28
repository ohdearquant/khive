//! forms.check: a set of forms and answers against the forms' own rules, and
//! the lines of those forms that no answer gives.

use std::collections::{HashMap, HashSet};

use khive_runtime::RuntimeError;
use khive_storage::Entity;
use khive_types::EdgeRelation;
use serde_json::{json, Value};
use uuid::Uuid;

use super::params::{self, Args};
use super::read::{cite, form_name, names};
use super::views::usage;
use super::Ctx;
use crate::store::{
    invalid, is_general, label, pending_premises, prop, prop_text, quoted_names, scalar, strings,
    type_of, Graph,
};
use crate::text::normalize;
use crate::vocab::{self, FIELD_VALUE, FORM, FORM_ITEM, FORM_RULE, FORM_SECTION};

/// The keys an answer given as a list entry may carry.
const ANSWER_KEYS: [&str; 6] = ["subject", "form", "field", "row", "state", "value"];

/// The printed lines an answer fills: the form and folded subject when it
/// names one, the field, and the folded state of the line. An answer that
/// names no subject fills the lines about none.
type Slot = (Option<Uuid>, Option<String>, Uuid, Option<String>);

/// A printed line of a checked form that asks for a field.
struct Line<'g> {
    item: &'g Entity,
    form: &'g Entity,
    /// The item's own field, or the choice value it prints.
    target: Uuid,
    /// The subject the line is about, as its form declares it.
    subject: Option<String>,
    slot: Slot,
    rows: u64,
    necessity: Option<String>,
    group: Option<String>,
    /// The folded ids of the subjects the line's value selects from.
    selects: Vec<String>,
}

fn number(node: &Entity, key: &str) -> f64 {
    prop(node, key).and_then(Value::as_f64).unwrap_or(f64::MAX)
}

/// The items of the checked forms that ask for a field but are bound to none
/// yet, each with its form.
type Unbound<'g> = Vec<(&'g Entity, &'g Entity)>;

/// The lines of the checked forms that ask for a field, form by form in
/// printed order, and the items that ask for one and are bound to none.
fn lines_of<'g>(graph: &'g Graph, set: &[&'g Entity]) -> (Vec<Line<'g>>, Unbound<'g>) {
    let mut lines = Vec::new();
    let mut unbound = Vec::new();
    for form in set.iter().copied() {
        let subjects = graph.subjects_of(&graph.key(form.id).unwrap_or_default());
        let mut items: Vec<&Entity> = graph
            .items_in(form.id)
            .into_iter()
            .filter_map(|i| graph.get(i))
            .filter(|i| prop_text(i, "item_kind").is_some_and(|k| vocab::item_needs_field(&k)))
            .collect();
        items.sort_by(|a, b| {
            number(a, "page")
                .total_cmp(&number(b, "page"))
                .then(number(a, "order").total_cmp(&number(b, "order")))
                .then(a.id.cmp(&b.id))
        });
        for item in items {
            let Some(target) = graph.out(item.id, EdgeRelation::VariantOf).first().copied() else {
                unbound.push((form, item));
                continue;
            };
            let field = if graph.is(target, FIELD_VALUE) {
                match graph.parent_field(target) {
                    Some(field) => field,
                    None => {
                        unbound.push((form, item));
                        continue;
                    }
                }
            } else {
                target
            };
            let subject = prop_text(item, "about");
            let folded = subject.as_deref().map(normalize);
            let subject_rows = folded
                .as_ref()
                .and_then(|s| subjects.get(s))
                .and_then(|s| prop(s, "repeats"))
                .and_then(Value::as_u64);
            lines.push(Line {
                item,
                form,
                target,
                rows: prop(item, "repeats")
                    .and_then(Value::as_u64)
                    .or(subject_rows)
                    .unwrap_or(1)
                    .max(1),
                slot: (
                    folded.as_ref().map(|_| form.id),
                    folded,
                    field,
                    prop_text(item, "state").map(|s| normalize(&s)),
                ),
                subject,
                necessity: prop_text(item, "necessity"),
                group: prop_text(item, "group"),
                selects: strings(prop(item, "selects"))
                    .iter()
                    .map(|s| normalize(s))
                    .collect(),
            });
        }
    }
    (lines, unbound)
}

/// One answer as the caller gives it. A list entry is keyed by subject, row
/// and state; an entry of the object form names a field only.
struct Answer {
    subject: Option<String>,
    form: Option<String>,
    field: String,
    row: Option<u64>,
    state: Option<String>,
    value: Value,
    keyed: bool,
}

impl Answer {
    fn view(&self) -> Value {
        json!({
            "subject": self.subject,
            "form": self.form,
            "field": self.field,
            "row": self.row,
            "state": self.state,
        })
    }
}

fn answers(args: &Args) -> Result<Vec<Answer>, RuntimeError> {
    match args.get("answers") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Object(map)) => Ok(map
            .iter()
            .map(|(field, value)| Answer {
                subject: None,
                form: None,
                field: field.clone(),
                row: None,
                state: None,
                value: value.clone(),
                keyed: false,
            })
            .collect()),
        Some(Value::Array(list)) => list
            .iter()
            .map(|entry| {
                let obj = params::entry(entry, "answer")?;
                if let Some(key) = obj.keys().find(|k| !ANSWER_KEYS.contains(&k.as_str())) {
                    return Err(invalid(format!(
                        "an answer takes no key \"{key}\"; it takes {}",
                        ANSWER_KEYS.join(", ")
                    )));
                }
                let row = match obj.get("row") {
                    None | Some(Value::Null) => None,
                    Some(row) => Some(row.as_u64().filter(|r| *r >= 1).ok_or_else(|| {
                        invalid("an answer's row is a whole number, at least 1")
                    })?),
                };
                Ok(Answer {
                    subject: params::opt_text(obj, "subject")?,
                    form: params::opt_text(obj, "form")?,
                    field: params::text(obj, "field")?,
                    row,
                    state: params::opt_text(obj, "state")?,
                    value: obj.get("value").cloned().unwrap_or(Value::Null),
                    keyed: true,
                })
            })
            .collect(),
        Some(_) => Err(invalid(
            "answers is an object of field to answer, or a list of {subject?, form?, field, row?, state?, value}",
        )),
    }
}

/// The slot and row an answer fills and the lines that print it, or why it
/// fits none.
fn place(
    graph: &Graph,
    set: &[&Entity],
    lines: &[Line<'_>],
    answer: &Answer,
    field: &Entity,
) -> Result<(Slot, u64, Vec<usize>), String> {
    if !answer.keyed {
        let slot: Slot = (None, None, field.id, None);
        let printed: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, l)| l.slot == slot)
            .map(|(i, _)| i)
            .collect();
        // An answer that names no subject fills only lines about none. Where
        // the checked forms ask for its field only of subjects, it fills
        // nothing, holds no premise, and says so.
        let mut whose: Vec<String> = lines
            .iter()
            .filter(|l| l.slot.2 == field.id)
            .filter_map(|l| l.subject.clone())
            .collect();
        whose.sort();
        whose.dedup();
        if printed.is_empty() && !whose.is_empty() {
            return Err(format!(
                "the checked forms ask for \"{}\" only of subjects ({}); answer it as a list entry that names the subject",
                field.name,
                whose
                    .iter()
                    .map(|s| format!("\"{s}\""))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        return Ok((slot, 1, printed));
    }
    let whose = match &answer.subject {
        Some(subject) => format!("of \"{subject}\""),
        None => "of no subject".to_owned(),
    };
    let (form, subject) = match &answer.subject {
        None => (None, None),
        Some(subject) => {
            let folded = normalize(subject);
            let mut forms: Vec<&Entity> = set
                .iter()
                .copied()
                .filter(|f| {
                    graph
                        .subjects_of(&graph.key(f.id).unwrap_or_default())
                        .contains_key(&folded)
                })
                .collect();
            if let Some(spec) = &answer.form {
                let named = graph
                    .find_form(spec)
                    .ok()
                    .filter(|f| set.iter().any(|s| s.id == f.id))
                    .ok_or_else(|| format!("form \"{spec}\" is not in the checked set"))?;
                forms.retain(|f| f.id == named.id);
            }
            match forms.as_slice() {
                [] => {
                    return Err(format!(
                        "no such subject: no checked form declares \"{subject}\""
                    ))
                }
                [one] => (Some(one.id), Some(folded)),
                many => {
                    return Err(format!(
                        "subject \"{subject}\" is declared by {}; name the form",
                        many.iter()
                            .map(|f| form_name(f))
                            .collect::<Vec<_>>()
                            .join(" and ")
                    ))
                }
            }
        }
    };
    let asking: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.slot.0 == form && l.slot.1 == subject && l.slot.2 == field.id)
        .map(|(i, _)| i)
        .collect();
    if asking.is_empty() {
        return Err(format!(
            "no line of the checked forms asks for \"{}\" {whose}",
            field.name
        ));
    }
    let state = answer.state.as_deref().map(normalize);
    let printed: Vec<usize> = asking
        .iter()
        .copied()
        .filter(|i| lines[*i].slot.3 == state)
        .collect();
    if printed.is_empty() {
        let state_of = |s: Option<String>| {
            s.map(|s| format!("\"{s}\""))
                .unwrap_or_else(|| "no state".to_owned())
        };
        let mut states: Vec<String> = asking
            .iter()
            .map(|i| state_of(prop_text(lines[*i].item, "state")))
            .collect();
        states.sort();
        states.dedup();
        return Err(format!(
            "the lines asking for \"{}\" {whose} are in {}; the answer names {}",
            field.name,
            states.join(", "),
            state_of(answer.state.clone())
        ));
    }
    let rows = printed.iter().map(|i| lines[*i].rows).max().unwrap_or(1);
    let row = match answer.row {
        Some(row) if row > rows => {
            return Err(format!(
                "row {row}, but the line asking for \"{}\" {whose} has {rows} row(s)",
                field.name
            ))
        }
        Some(row) => row,
        None if rows > 1 => {
            return Err(format!(
                "the line asking for \"{}\" {whose} repeats {rows} times; name a row",
                field.name
            ))
        }
        None => 1,
    };
    Ok(((form, subject, field.id, state), row, printed))
}

/// What the answers give: fields and values for rule premises, and slots by
/// row for printed lines.
#[derive(Default)]
struct Given {
    fields: HashSet<Uuid>,
    values: HashSet<Uuid>,
    rows: HashSet<(Slot, u64)>,
    choices: HashSet<(Slot, u64, Uuid)>,
}

impl Given {
    /// Whether any row of a line is answered: for an option, by its value.
    fn line_filled(&self, graph: &Graph, line: &Line<'_>) -> bool {
        (1..=line.rows).any(|row| {
            if graph.is(line.target, FIELD_VALUE) {
                self.choices
                    .contains(&(line.slot.clone(), row, line.target))
            } else {
                self.rows.contains(&(line.slot.clone(), row))
            }
        })
    }
}

/// An item that prints a line is answered only through its own slot: an item
/// about a subject by an answer for that subject, an item about none by an
/// answer that names none. Any other item, by any answer for its field or
/// value.
fn item_answered(
    graph: &Graph,
    item: Uuid,
    given: &Given,
    lines: &HashMap<Uuid, &Line<'_>>,
) -> bool {
    match lines.get(&item) {
        Some(line) => given.line_filled(graph, line),
        None => graph
            .out(item, EdgeRelation::VariantOf)
            .iter()
            .any(|t| given.fields.contains(t) || given.values.contains(t)),
    }
}

fn met(
    graph: &Graph,
    target: Uuid,
    given: &Given,
    lines: &HashMap<Uuid, &Line<'_>>,
    forms: &HashSet<Uuid>,
) -> bool {
    match graph.get(target).map(type_of) {
        Some(FORM) => forms.contains(&target),
        Some(FORM_ITEM) => item_answered(graph, target, given, lines),
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
                    .any(|item| item_answered(graph, *item, given, lines))
            } else {
                asking
                    .iter()
                    .all(|item| item_answered(graph, *item, given, lines))
            }
        }
        // A subject a rule brings into play is not a finding of its own: its
        // required lines are reported one by one as missing.
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

/// Whether the answers ask for a line: every node on its way that a printed
/// rule enables (the item, each section holding it, its subject) is enabled
/// by at least one rule whose premise holds.
fn asked(
    graph: &Graph,
    line: &Line<'_>,
    gates: &HashMap<Uuid, bool>,
    subjects: &HashMap<Uuid, HashMap<String, Uuid>>,
) -> bool {
    asked_item(
        graph,
        line.form.id,
        line.item.id,
        line.slot.1.as_ref(),
        gates,
        subjects,
    )
}

/// `asked` for an item of a form, about a folded subject or none.
fn asked_item(
    graph: &Graph,
    form: Uuid,
    item: Uuid,
    subject: Option<&String>,
    gates: &HashMap<Uuid, bool>,
    subjects: &HashMap<Uuid, HashMap<String, Uuid>>,
) -> bool {
    let mut on_the_way = vec![item];
    let mut node = item;
    while let Some(section) = graph
        .incoming(node, EdgeRelation::Contains)
        .into_iter()
        .find(|p| graph.is(*p, FORM_SECTION) && !on_the_way.contains(p))
    {
        on_the_way.push(section);
        node = section;
    }
    if let Some(subject) = subject.and_then(|s| subjects.get(&form)?.get(s)) {
        on_the_way.push(*subject);
    }
    on_the_way
        .iter()
        .all(|node| gates.get(node).copied().unwrap_or(true))
}

/// What a missing finding names: one row of a line (none when it does not
/// repeat), any row of a repeating line, or any line of a group.
enum Need {
    Row(Option<u64>),
    AnyRow,
    Group,
}

fn missing(graph: &Graph, lines: &[Line<'_>], members: &[usize], need: Need) -> Value {
    let first = &lines[members[0]];
    let printed: Vec<Value> = members
        .iter()
        .map(|i| {
            let item = lines[*i].item;
            json!({
                "id": item.id.to_string(),
                "label": label(item),
                "page": prop(item, "page"),
                "order": prop(item, "order"),
            })
        })
        .collect();
    let labels = members
        .iter()
        .map(|i| format!("\"{}\"", label(lines[*i].item)))
        .collect::<Vec<_>>()
        .join(", ");
    let form = form_name(first.form);
    let row = match need {
        Need::Group => {
            let group = first.group.clone().unwrap_or_default();
            return json!({
                "kind": "missing",
                "form": cite(first.form),
                "group": group,
                "lines": printed,
                "message": format!("{form} asks for one line of the group \"{group}\" ({labels}); no answer gives any of them"),
            });
        }
        Need::Row(row) => row,
        Need::AnyRow => None,
    };
    let field = graph
        .get(first.slot.2)
        .map(|f| f.name.clone())
        .unwrap_or_default();
    let state = prop_text(first.item, "state");
    let what = match (&need, row) {
        (Need::AnyRow, _) => "at least one row of ".to_owned(),
        (_, Some(row)) => format!("row {row} of "),
        _ => String::new(),
    };
    let whose = first
        .subject
        .as_ref()
        .map(|s| format!(" of \"{s}\""))
        .unwrap_or_default();
    let in_state = state
        .as_ref()
        .map(|s| format!(" ({s})"))
        .unwrap_or_default();
    json!({
        "kind": "missing",
        "form": cite(first.form),
        "subject": first.subject,
        "field": field,
        "state": state,
        "row": row,
        "lines": printed,
        "message": format!("{form} asks for {what}\"{field}\"{whose}{in_state} on {labels}; no answer gives it"),
    })
}

/// An answer that fits a line: how it shows itself, its field, the slot and
/// row it fills, the lines that print that slot, and the values it chooses
/// when its field is a choice.
struct Placed {
    view: Value,
    field: Uuid,
    slot: Slot,
    row: u64,
    printed: Vec<usize>,
    values: Vec<Uuid>,
}

/// What the answers in play give.
fn given_of(placed: &[Placed], in_play: &[bool]) -> Given {
    let mut given = Given::default();
    for (answer, _) in placed.iter().zip(in_play).filter(|(_, on)| **on) {
        given.fields.insert(answer.field);
        given.rows.insert((answer.slot.clone(), answer.row));
        for value in &answer.values {
            given.values.insert(*value);
            given
                .choices
                .insert((answer.slot.clone(), answer.row, *value));
        }
    }
    given
}

/// Whether every premise of a rule is given: a value by an answer choosing
/// it, a field by any answer for it.
fn premises_hold(graph: &Graph, rule: &Entity, given: &Given) -> bool {
    graph.out(rule.id, EdgeRelation::DependsOn).iter().all(|p| {
        if graph.is(*p, FIELD_VALUE) {
            given.values.contains(p)
        } else {
            given.fields.contains(p)
        }
    })
}

/// Each node a rule of the checked forms enables, and whether a rule that
/// enables it holds. A rule with a premise still undecided holds for nothing,
/// so what it enables is not asked meanwhile.
fn rule_gates(graph: &Graph, forms: &HashSet<Uuid>, given: &Given) -> HashMap<Uuid, bool> {
    let mut gates: HashMap<Uuid, bool> = HashMap::new();
    for rule in graph.of_type(FORM_RULE) {
        if !graph.form_of(rule).is_some_and(|f| forms.contains(&f.id)) {
            continue;
        }
        let holds = pending_premises(rule).is_empty() && premises_hold(graph, rule, given);
        for target in graph.out(rule.id, EdgeRelation::Enables) {
            *gates.entry(target).or_insert(false) |= holds;
        }
    }
    gates
}

/// A line a form prints and asks of someone that is bound to no field yet,
/// so no answer can fill it.
fn unbound_line(form: &Entity, item: &Entity) -> Value {
    let pending = prop_text(item, "field_pending");
    let next = match &pending {
        Some(wording) => format!("\"{wording}\" waits for forms.same or forms.split"),
        None => "bind it to a field with forms.same".to_owned(),
    };
    json!({
        "kind": "line_without_field",
        "form": cite(form),
        "subject": prop_text(item, "about"),
        "line": {
            "id": item.id.to_string(),
            "label": label(item),
            "page": prop(item, "page"),
            "order": prop(item, "order"),
        },
        "field_pending": pending,
        "message": format!(
            "{} prints \"{}\", which asks for no field yet, so no answer can fill it; {next}",
            form_name(form),
            label(item)
        ),
    })
}

/// forms.check: a set of forms and answers against the forms' own rules.
pub(crate) async fn check(ctx: &Ctx<'_>, params: Value) -> Result<Value, RuntimeError> {
    let args = params::args(params)?;
    let specs = params::string_list(&args, "forms")?;
    if specs.is_empty() {
        return Err(invalid("forms must name at least one form"));
    }
    let answers = answers(&args)?;
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

    let (lines, unbound) = lines_of(&graph, &set);
    let by_item: HashMap<Uuid, &Line<'_>> = lines.iter().map(|l| (l.item.id, l)).collect();
    // The subjects each checked form declares, by folded id.
    let subjects: HashMap<Uuid, HashMap<String, Uuid>> = set
        .iter()
        .map(|f| {
            let declared = graph
                .subjects_of(&graph.key(f.id).unwrap_or_default())
                .into_iter()
                .map(|(id, subject)| (id, subject.id))
                .collect();
            (f.id, declared)
        })
        .collect();
    // Each answer that fits a line, before the rules decide which lines are
    // asked.
    let mut placed: Vec<Placed> = Vec::new();
    let mut unresolved = Vec::new();
    let mut unplaced = Vec::new();
    for answer in &answers {
        let key = &answer.field;
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
        let texts: Vec<String> = match &answer.value {
            Value::Array(list) => list.iter().filter_map(scalar).collect(),
            other => scalar(other).into_iter().collect(),
        };
        if texts.is_empty() {
            unresolved.push(
                json!({"answer": key, "reason": "an answer is text, a number or a list of them"}),
            );
            continue;
        }
        let (slot, row, printed) = match place(&graph, &set, &lines, answer, field) {
            Ok(placed) => placed,
            Err(reason) => {
                let mut view = answer.view();
                view["reason"] = json!(reason);
                unplaced.push(view);
                continue;
            }
        };
        // A line whose value names another subject of its form takes only the
        // subjects it lists.
        if let Some(line) = printed
            .iter()
            .map(|i| &lines[*i])
            .find(|l| !l.selects.is_empty())
        {
            let outside: Vec<&String> = texts
                .iter()
                .filter(|t| !line.selects.contains(&normalize(t)))
                .collect();
            if !outside.is_empty() {
                let allowed = strings(prop(line.item, "selects"));
                findings.push(json!({
                    "kind": "subject_outside_selection",
                    "answer": outside,
                    "allowed": allowed,
                    "asked_on": [usage(line.item)],
                    "message": format!(
                        "{} is not a subject the line \"{}\" selects from ({})",
                        outside.iter().map(|t| format!("\"{t}\"")).collect::<Vec<_>>().join(", "),
                        label(line.item),
                        allowed.iter().map(|s| format!("\"{s}\"")).collect::<Vec<_>>().join(", ")
                    ),
                }));
                continue;
            }
        }
        // A choice field is answered only through a value it lists: an answer
        // outside its values is reported below and holds no premise or line.
        let mut entry = Placed {
            view: answer.view(),
            field: field.id,
            slot,
            row,
            printed,
            values: Vec::new(),
        };
        if prop_text(field, "value_type").as_deref() != Some("choice") {
            placed.push(entry);
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
                Some(value) => entry.values.push(value),
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
        if !entry.values.is_empty() {
            placed.push(entry);
        }
    }

    // Rules decide which lines are asked, and an answer counts, as a premise
    // and as a filled line, only while a line it fills is asked. Leaving an
    // answer out can only turn rules off, so each pass keeps fewer answers
    // and the loop ends within one pass per answer.
    let mut in_play = vec![true; placed.len()];
    let given = loop {
        let given = given_of(&placed, &in_play);
        let gates = rule_gates(&graph, &in_set, &given);
        let next: Vec<bool> = placed
            .iter()
            .map(|answer| {
                answer.printed.is_empty()
                    || answer
                        .printed
                        .iter()
                        .any(|i| asked(&graph, &lines[*i], &gates, &subjects))
            })
            .collect();
        if next == in_play {
            break given;
        }
        in_play = next;
    };
    let answers_not_asked: Vec<Value> = placed
        .iter()
        .zip(&in_play)
        .filter(|(_, on)| !**on)
        .map(|(answer, _)| {
            let mut view = answer.view.clone();
            view["reason"] =
                json!("the lines it fills are not asked: no rule that brings them into play holds");
            view
        })
        .collect();
    // Each choice in play: the slot and row it fills, its field and its value.
    let chosen: Vec<(Slot, u64, Uuid, Uuid)> = placed
        .iter()
        .zip(&in_play)
        .filter(|(_, on)| **on)
        .flat_map(|(answer, _)| {
            answer
                .values
                .iter()
                .map(move |value| (answer.slot.clone(), answer.row, answer.field, *value))
        })
        .collect();
    for (i, (first_slot, first_row, first_field, first)) in chosen.iter().enumerate() {
        for (second_slot, second_row, second_field, second) in &chosen[i + 1..] {
            // Values exclude each other within one subject, row and state.
            if (first_slot.0, &first_slot.1, &first_slot.3, first_row)
                != (second_slot.0, &second_slot.1, &second_slot.3, second_row)
            {
                continue;
            }
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

    // Each node a rule of the set enables, and whether a rule enabling it holds.
    let gates = rule_gates(&graph, &in_set, &given);
    let mut unchecked = Vec::new();
    for rule in graph.of_type(FORM_RULE) {
        let Some(form) = graph.form_of(rule) else {
            continue;
        };
        if !in_set.contains(&form.id) {
            continue;
        }
        let conclusions = graph.out(rule.id, EdgeRelation::Enables);
        let pending = pending_premises(rule);
        if !pending.is_empty() {
            // Its bound premises are only part of the printed rule, so it is
            // not evaluated until every premise names a field, and what it
            // enables is not asked meanwhile.
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
        if premises.is_empty() || !premises_hold(&graph, rule, &given) {
            continue;
        }
        let unmet: Vec<&Entity> = conclusions
            .into_iter()
            .filter(|c| !met(&graph, *c, &given, &by_item, &in_set))
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

    // The lines a necessity makes open, grouped by the slot one answer fills.
    let mut not_asked = 0usize;
    let mut slots: Vec<(Slot, Vec<usize>)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if !matches!(
            line.necessity.as_deref(),
            Some("required" | "one_of" | "at_least_one_row")
        ) {
            continue;
        }
        if !asked(&graph, line, &gates, &subjects) {
            not_asked += 1;
            continue;
        }
        match slots.iter_mut().find(|(slot, _)| *slot == line.slot) {
            Some((_, members)) => members.push(i),
            None => slots.push((line.slot.clone(), vec![i])),
        }
    }
    let mut groups: Vec<((Uuid, String), Vec<usize>, bool)> = Vec::new();
    for (slot, members) in &slots {
        let rows = members.iter().map(|i| lines[*i].rows).max().unwrap_or(1);
        let filled: Vec<u64> = (1..=rows)
            .filter(|row| given.rows.contains(&(slot.clone(), *row)))
            .collect();
        let needs = |n: &str| {
            members
                .iter()
                .any(|i| lines[*i].necessity.as_deref() == Some(n))
        };
        if needs("required") {
            for row in (1..=rows).filter(|row| !filled.contains(row)) {
                let row = (rows > 1).then_some(row);
                findings.push(missing(&graph, &lines, members, Need::Row(row)));
            }
        } else if needs("at_least_one_row") && filled.is_empty() {
            findings.push(missing(&graph, &lines, members, Need::AnyRow));
        }
        // A one_of line belongs to its group whatever else shares its slot.
        for i in members {
            let line = &lines[*i];
            let Some(group) = &line.group else {
                continue;
            };
            let key = (line.form.id, normalize(group));
            match groups.iter_mut().find(|(k, _, _)| *k == key) {
                Some((_, list, any)) => {
                    list.push(*i);
                    *any |= !filled.is_empty();
                }
                None => groups.push((key, vec![*i], !filled.is_empty())),
            }
        }
    }
    for (_, members, any) in &groups {
        if !any {
            findings.push(missing(&graph, &lines, members, Need::Group));
        }
    }
    // A line asked of someone that no field backs yet stays open.
    for (form, item) in &unbound {
        if !matches!(
            prop_text(item, "necessity").as_deref(),
            Some("required" | "one_of" | "at_least_one_row")
        ) {
            continue;
        }
        let about = prop_text(item, "about").map(|s| normalize(&s));
        if !asked_item(&graph, form.id, item.id, about.as_ref(), &gates, &subjects) {
            not_asked += 1;
            continue;
        }
        findings.push(unbound_line(form, item));
    }

    Ok(json!({
        "forms": set.iter().map(|f| cite(f)).collect::<Vec<_>>(),
        "consistent": findings.is_empty()
            && unresolved.is_empty()
            && unplaced.is_empty()
            && answers_not_asked.is_empty()
            && unchecked.is_empty(),
        "findings": findings,
        "unresolved_answers": unresolved,
        "unplaced_answers": unplaced,
        "answers_not_asked": answers_not_asked,
        "rules_unchecked": unchecked,
        "lines_not_asked": not_asked,
    }))
}
