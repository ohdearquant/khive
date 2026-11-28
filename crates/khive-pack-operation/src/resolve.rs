//! Which slot of an operation type each subject of a form fills (ADR-222 Amendment 1, A1.3).
//! The answer is read from the published type, the form's subjects, the namespace's terms and
//! the decisions recorded so far; nothing is written, and nothing is guessed.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use serde_json::{json, Value};

use crate::definition::{fold, OperationType};

/// The namespace's term lists as `forms.terms` returns them: each folded wording, a name or
/// an alias, to the folded name of its term.
#[derive(Debug, Default)]
pub(crate) struct Terms {
    kinds: HashMap<String, String>,
    roles: HashMap<String, String>,
    relations: HashMap<String, String>,
    qualifiers: HashMap<String, String>,
    carrying: HashSet<String>,
}

impl Terms {
    pub(crate) fn from_reply(reply: &Value) -> Self {
        let list = |key: &str| -> HashMap<String, String> {
            let mut map = HashMap::new();
            for term in reply["terms"][key].as_array().into_iter().flatten() {
                let Some(name) = term["name"].as_str() else {
                    continue;
                };
                let canonical = fold(name);
                for alias in term["aliases"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(Value::as_str)
                {
                    map.insert(fold(alias), canonical.clone());
                }
                map.insert(canonical.clone(), canonical);
            }
            map
        };
        let carrying = reply["terms"]["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|term| term["carries"].as_bool() == Some(true))
            .filter_map(|term| term["name"].as_str())
            .map(fold)
            .collect();
        Self {
            kinds: list("kinds"),
            roles: list("roles"),
            relations: list("relations"),
            qualifiers: list("counting_qualifiers"),
            carrying,
        }
    }

    fn list(&self, list: &str) -> Option<&HashMap<String, String>> {
        match list {
            "kind" => Some(&self.kinds),
            "role" => Some(&self.roles),
            "relation" => Some(&self.relations),
            "counting qualifier" => Some(&self.qualifiers),
            _ => None,
        }
    }

    /// Whether a wording names a term of the list (`kind`, `role` or `relation`).
    pub(crate) fn knows(&self, list: &str, wording: &str) -> bool {
        self.list(list)
            .is_some_and(|terms| terms.contains_key(&fold(wording)))
    }

    /// The folded name of the term a wording names, or the folded wording when it names none.
    fn canonical(&self, list: &str, wording: &str) -> String {
        let folded = fold(wording);
        self.list(list)
            .and_then(|terms| terms.get(&folded))
            .cloned()
            .unwrap_or(folded)
    }

    fn is_counting(&self, qualifier: &str) -> bool {
        self.qualifiers.contains_key(qualifier)
    }
}

/// A subject as `forms.subjects` returns it.
#[derive(Debug, Clone)]
pub(crate) struct Subject {
    pub id: String,
    kind: Option<String>,
    role: Option<String>,
    qualifier: Option<String>,
    relations: Vec<(String, String)>,
}

impl Subject {
    pub(crate) fn from_view(view: &Value) -> Option<Self> {
        let text = |key: &str| view[key].as_str().map(str::to_owned);
        let relations = view["relations"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                Some((
                    r["relation"].as_str()?.to_owned(),
                    r["to"].as_str()?.to_owned(),
                ))
            })
            .collect();
        Some(Self {
            id: text("id")?,
            kind: text("kind"),
            role: text("role"),
            qualifier: text("qualifier"),
            relations,
        })
    }
}

/// A recorded decision: these subjects fill `slot`, or, with no slot, are not of this type.
#[derive(Debug, Clone)]
pub(crate) struct Decision {
    pub id: String,
    pub subjects: Vec<String>,
    pub slot: Option<String>,
}

/// A slot with its terms folded to the names of the terms they name.
struct SlotTerms {
    name: String,
    repeats: bool,
    kind: Option<String>,
    role: Option<String>,
    qualifier: Option<String>,
    defined_by: Option<(String, String)>,
}

#[derive(Debug, Clone)]
enum State {
    Pending,
    /// The subject fills the slot itself.
    Own {
        slot: String,
        by: &'static str,
        decision: Option<String>,
    },
    NotThisType {
        decision: String,
    },
    /// The subject is carried by another subject and goes with that subject's slot.
    Carried {
        slot: String,
        by: String,
    },
    Undecided {
        candidates: BTreeSet<String>,
        lapsed: Option<String>,
    },
    NoSlot {
        lapsed: Option<String>,
    },
}

enum Match {
    None,
    One(String),
    Several(BTreeSet<String>),
}

/// What resolve answers: one entry per subject, in the order given, and the decisions whose
/// subject the form no longer declares.
pub(crate) struct Resolution {
    pub subjects: Vec<Value>,
    pub lapsed: Vec<Value>,
}

pub(crate) fn resolve(
    type_: &OperationType,
    terms: &Terms,
    subjects: &[Subject],
    decisions: &[Decision],
) -> Resolution {
    let slots: Vec<SlotTerms> = type_
        .slots
        .iter()
        .map(|slot| SlotTerms {
            name: slot.name.clone(),
            repeats: slot.repeats,
            kind: slot.kind.as_deref().map(|k| terms.canonical("kind", k)),
            role: slot.role.as_deref().map(|r| terms.canonical("role", r)),
            qualifier: slot
                .qualifier
                .as_deref()
                .map(|q| terms.canonical("counting qualifier", q)),
            defined_by: slot
                .defined_by
                .as_ref()
                .map(|d| (terms.canonical("relation", &d.relation), d.slot.clone())),
        })
        .collect();
    let index: HashMap<String, usize> = subjects
        .iter()
        .enumerate()
        .map(|(i, subject)| (fold(&subject.id), i))
        .collect();
    let mut states = vec![State::Pending; subjects.len()];

    // Rung 1: for each subject, the newest decision that names it.
    let mut newest: BTreeMap<String, (&Decision, &str)> = BTreeMap::new();
    for decision in decisions.iter().rev() {
        for subject in &decision.subjects {
            newest
                .entry(fold(subject))
                .or_insert((decision, subject.as_str()));
        }
    }
    let mut lapsed = Vec::new();
    let mut held_back = Vec::new();
    for (folded, (decision, named)) in &newest {
        let Some(&i) = index.get(folded) else {
            lapsed.push(json!({"decision": decision.id, "subject": named}));
            continue;
        };
        states[i] = match &decision.slot {
            None => State::NotThisType {
                decision: decision.id.clone(),
            },
            Some(slot) if slots.iter().any(|s| s.name == *slot) => State::Own {
                slot: slot.clone(),
                by: "decision",
                decision: Some(decision.id.clone()),
            },
            // The decided slot is gone from this version: no rung binds the subject in its
            // place, because the decision was written for a reason the rungs do not see.
            Some(_) => {
                held_back.push((i, decision.id.clone()));
                State::NoSlot {
                    lapsed: Some(decision.id.clone()),
                }
            }
        };
    }

    // Rung 2: the subject's kind, role and qualifier.
    let found: Vec<(usize, Match)> = subjects
        .iter()
        .enumerate()
        .filter(|(i, _)| matches!(states[*i], State::Pending))
        .map(|(i, subject)| (i, role_match(&slots, terms, subject)))
        .collect();
    settle(&slots, &mut states, found, "role");

    // Rungs 3 and 4 repeat until no subject changes; a relation to a slot is tried before a
    // carrying relation.
    loop {
        let found: Vec<(usize, Match)> = subjects
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(states[*i], State::Pending))
            .map(|(i, subject)| (i, relation_match(&slots, terms, subject, &index, &states)))
            .filter(|(_, found)| !matches!(found, Match::None))
            .collect();
        if !found.is_empty() {
            settle(&slots, &mut states, found, "relation");
            continue;
        }
        let carried: Vec<(usize, State)> = subjects
            .iter()
            .enumerate()
            .filter(|(i, _)| matches!(states[*i], State::Pending))
            .filter_map(|(i, subject)| carried_by(terms, subject, &index, &states).map(|s| (i, s)))
            .collect();
        if carried.is_empty() {
            break;
        }
        for (i, state) in carried {
            states[i] = state;
        }
    }

    // A subject whose decided slot is gone is offered what rungs 2 and 3 would offer.
    for (i, decision) in held_back {
        let mut candidates = BTreeSet::new();
        for found in [
            role_match(&slots, terms, &subjects[i]),
            relation_match(&slots, terms, &subjects[i], &index, &states),
        ] {
            match found {
                Match::None => {}
                Match::One(slot) => {
                    candidates.insert(slot);
                }
                Match::Several(slots) => candidates.extend(slots),
            }
        }
        if !candidates.is_empty() {
            states[i] = State::Undecided {
                candidates,
                lapsed: Some(decision),
            };
        }
    }

    let views = subjects
        .iter()
        .zip(&states)
        .map(|(subject, state)| view(&subject.id, state))
        .collect();
    Resolution {
        subjects: views,
        lapsed,
    }
}

/// Rung 2. A subject with no qualifier takes the slot of its kind and role that has none; a
/// qualifier equal to a slot's takes that slot; a counting qualifier no slot carries takes
/// the slot with none. Any other qualifier, and a subject with no qualifier where every slot
/// of its kind and role has one, offers the slots of its kind and role.
fn role_match(slots: &[SlotTerms], terms: &Terms, subject: &Subject) -> Match {
    let (Some(kind), Some(role)) = (&subject.kind, &subject.role) else {
        return Match::None;
    };
    let (kind, role) = (terms.canonical("kind", kind), terms.canonical("role", role));
    let same: Vec<&SlotTerms> = slots
        .iter()
        .filter(|s| s.kind.as_deref() == Some(&kind) && s.role.as_deref() == Some(&role))
        .collect();
    if same.is_empty() {
        return Match::None;
    }
    let unqualified = || same.iter().find(|s| s.qualifier.is_none());
    let hit = match subject.qualifier.as_deref() {
        None => unqualified(),
        Some(qualifier) => {
            let qualifier = terms.canonical("counting qualifier", qualifier);
            same.iter()
                .find(|s| s.qualifier.as_deref() == Some(&qualifier))
                .or_else(|| unqualified().filter(|_| terms.is_counting(&qualifier)))
        }
    };
    match hit {
        Some(slot) => Match::One(slot.name.clone()),
        None => Match::Several(same.iter().map(|s| s.name.clone()).collect()),
    }
}

/// Rung 3: a slot defined by relation R to slot S, where the subject carries R to a subject
/// that fills S itself.
fn relation_match(
    slots: &[SlotTerms],
    terms: &Terms,
    subject: &Subject,
    index: &HashMap<String, usize>,
    states: &[State],
) -> Match {
    let mut found = BTreeSet::new();
    for (relation, to) in &subject.relations {
        let relation = terms.canonical("relation", relation);
        let Some(State::Own { slot: target, .. }) = index.get(&fold(to)).map(|&j| &states[j])
        else {
            continue;
        };
        for slot in slots {
            if slot.defined_by.as_ref() == Some(&(relation.clone(), target.clone())) {
                found.insert(slot.name.clone());
            }
        }
    }
    match found.len() {
        0 => Match::None,
        1 => Match::One(found.into_iter().next().unwrap_or_default()),
        _ => Match::Several(found),
    }
}

/// Rung 4: the subject carries a carrying relation to a subject that has a slot.
fn carried_by(
    terms: &Terms,
    subject: &Subject,
    index: &HashMap<String, usize>,
    states: &[State],
) -> Option<State> {
    let mut found: BTreeMap<String, String> = BTreeMap::new();
    for (relation, to) in &subject.relations {
        if !terms
            .carrying
            .contains(&terms.canonical("relation", relation))
        {
            continue;
        }
        let Some(&j) = index.get(&fold(to)) else {
            continue;
        };
        match &states[j] {
            State::Own { slot, .. } | State::Carried { slot, .. } => {
                found.entry(slot.clone()).or_insert_with(|| to.clone());
            }
            _ => {}
        }
    }
    match found.len() {
        0 => None,
        1 => found
            .into_iter()
            .next()
            .map(|(slot, by)| State::Carried { slot, by }),
        _ => Some(State::Undecided {
            candidates: found.into_keys().collect(),
            lapsed: None,
        }),
    }
}

/// Apply one rung's matches. A subject offered several slots, and every subject that reaches
/// a slot that does not repeat along with another subject or after one already fills it, is
/// undecided.
fn settle(slots: &[SlotTerms], states: &mut [State], found: Vec<(usize, Match)>, by: &'static str) {
    let held: HashSet<String> = states
        .iter()
        .filter_map(|state| match state {
            State::Own { slot, .. } => Some(slot.clone()),
            _ => None,
        })
        .collect();
    let mut reach: HashMap<String, usize> = HashMap::new();
    for (_, found) in &found {
        if let Match::One(slot) = found {
            *reach.entry(slot.clone()).or_default() += 1;
        }
    }
    for (i, found) in found {
        states[i] = match found {
            Match::None => continue,
            Match::Several(candidates) => State::Undecided {
                candidates,
                lapsed: None,
            },
            Match::One(slot) => {
                let repeats = slots.iter().any(|s| s.name == slot && s.repeats);
                if !repeats && (reach[&slot] > 1 || held.contains(&slot)) {
                    State::Undecided {
                        candidates: BTreeSet::from([slot]),
                        lapsed: None,
                    }
                } else {
                    State::Own {
                        slot,
                        by,
                        decision: None,
                    }
                }
            }
        };
    }
}

fn view(subject: &str, state: &State) -> Value {
    match state {
        State::Own { slot, by, decision } => {
            let mut view = json!({"subject": subject, "resolved_by": by, "slot": slot});
            if let Some(decision) = decision {
                view["decision"] = json!(decision);
            }
            view
        }
        State::NotThisType { decision } => json!({
            "subject": subject,
            "resolved_by": "decision",
            "slot": null,
            "not_this_type": true,
            "decision": decision,
        }),
        State::Carried { slot, by } => json!({
            "subject": subject,
            "resolved_by": "carried",
            "slot": slot,
            "carried_by": by,
        }),
        State::Undecided { candidates, lapsed } => {
            let mut view = json!({
                "subject": subject,
                "resolved_by": "undecided",
                "slot": null,
                "candidates": candidates,
            });
            if let Some(decision) = lapsed {
                view["lapsed"] = json!(decision);
            }
            view
        }
        State::Pending | State::NoSlot { lapsed: None } => {
            json!({"subject": subject, "resolved_by": "no_slot", "slot": null})
        }
        State::NoSlot {
            lapsed: Some(decision),
        } => json!({
            "subject": subject,
            "resolved_by": "no_slot",
            "slot": null,
            "lapsed": decision,
        }),
    }
}

#[cfg(test)]
#[path = "resolve_tests.rs"]
mod tests;
