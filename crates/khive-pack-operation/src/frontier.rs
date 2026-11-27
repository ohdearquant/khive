//! The frontier: what an operation owes now, read from its records and its pinned type.
//!
//! A step is asked of each identity in its `by` slot when that slot repeats, and once
//! otherwise. Rules decide per identity: a skip rule that holds skips it, and when a step has
//! activate rules, an identity none of them holds for is not asked. A step nobody is asked
//! is bypassed, and a step after it waits on what the bypassed step waited on. A step is done
//! when every asked identity acted (`all_of`), when one did (`at_least_one`), or, for a slot
//! of one identity, when an act naming no identity or the identity bound there is recorded.

use std::collections::{BTreeMap, HashMap, HashSet};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::definition::{Action, OperationType, Quantifier, RuleStatus, StepType, Term};

/// Who or what fills a slot. Two identities are the same when kind and key are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub kind: String,
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
}

impl Identity {
    pub fn reference(&self) -> String {
        format!("{}:{}", self.kind, self.key)
    }
}

/// A recorded item, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum Payload {
    Binding {
        slot: String,
        identity: Identity,
    },
    Fact {
        field: String,
        value: Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<Identity>,
    },
    Act {
        step: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        identity: Option<Identity>,
    },
}

impl Payload {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Binding { .. } => "binding",
            Self::Fact { .. } => "fact",
            Self::Act { .. } => "act",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GroupBy {
    Supplier,
    By,
}

#[derive(Default)]
struct State {
    bindings: BTreeMap<String, Vec<Identity>>,
    facts: HashMap<(String, Option<String>), Value>,
    acts: HashMap<String, HashSet<Option<String>>>,
}

impl State {
    fn apply(records: &[Payload]) -> Self {
        let mut state = Self::default();
        for record in records {
            match record {
                Payload::Binding { slot, identity } => {
                    let bound = state.bindings.entry(slot.clone()).or_default();
                    if !bound
                        .iter()
                        .any(|held| held.reference() == identity.reference())
                    {
                        bound.push(identity.clone());
                    }
                }
                Payload::Fact {
                    field,
                    value,
                    identity,
                } => {
                    state.facts.insert(
                        (field.clone(), identity.as_ref().map(Identity::reference)),
                        value.clone(),
                    );
                }
                Payload::Act { step, identity } => {
                    state
                        .acts
                        .entry(step.clone())
                        .or_default()
                        .insert(identity.as_ref().map(Identity::reference));
                }
            }
        }
        state
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Asked {
    Yes,
    Skipped { rule: usize, alternative: usize },
    NotActivated,
}

struct Unit {
    identity: Option<Identity>,
    asked: Asked,
    acted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Status {
    Done,
    Owed,
    Blocked,
    Bypassed,
    Held,
}

impl Status {
    fn as_str(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Owed => "owed",
            Self::Blocked => "blocked",
            Self::Bypassed => "bypassed",
            Self::Held => "held",
        }
    }
}

struct StepEval {
    status: Status,
    units: Vec<Unit>,
    effective_after: Vec<String>,
    optional_unbound: bool,
}

enum Subject {
    Operation,
    Identity(String),
    Unbound,
}

struct Context<'a> {
    state: &'a State,
    done: &'a HashMap<String, bool>,
}

impl Context<'_> {
    fn holds(&self, term: &Term, by: &str, unit: Option<&Identity>) -> bool {
        match term {
            Term::Step(step) => self.done.get(&step.name).copied().unwrap_or(false) == step.done,
            Term::Fact(fact) => {
                let subject = match &fact.slot {
                    None => Subject::Operation,
                    Some(slot) if slot == by => match unit {
                        Some(identity) => Subject::Identity(identity.reference()),
                        None => Subject::Unbound,
                    },
                    Some(slot) => match self
                        .state
                        .bindings
                        .get(slot)
                        .and_then(|bound| bound.first())
                    {
                        Some(identity) => Subject::Identity(identity.reference()),
                        None => Subject::Unbound,
                    },
                };
                let value = match subject {
                    Subject::Operation => self.state.facts.get(&(fact.field.clone(), None)),
                    Subject::Identity(reference) => {
                        self.state.facts.get(&(fact.field.clone(), Some(reference)))
                    }
                    Subject::Unbound => None,
                };
                if let Some(present) = fact.present {
                    value.is_some() == present
                } else if let Some(expected) = &fact.equals {
                    value == Some(expected)
                } else if let Some(options) = &fact.one_of {
                    value.is_some_and(|value| options.contains(value))
                } else {
                    false
                }
            }
        }
    }

    fn first_holding(
        &self,
        alternatives: &[Vec<Term>],
        by: &str,
        unit: Option<&Identity>,
    ) -> Option<usize> {
        alternatives
            .iter()
            .position(|terms| terms.iter().all(|term| self.holds(term, by, unit)))
    }
}

/// Steps in an order where every step comes after the steps it waits on and the steps its
/// rules test. Publication refused every cycle, so every step is placed.
fn evaluation_order(definition: &OperationType) -> Result<Vec<String>, String> {
    let mut deps: BTreeMap<&str, HashSet<&str>> = BTreeMap::new();
    for step in &definition.steps {
        deps.entry(step.name.as_str())
            .or_default()
            .extend(step.after.iter().map(String::as_str));
    }
    for rule in &definition.rules {
        let entry = deps.entry(rule.step.as_str()).or_default();
        for term in rule.when.iter().chain(rule.unless.iter()).flatten() {
            if let Term::Step(step) = term {
                entry.insert(step.name.as_str());
            }
        }
    }
    let mut placed: Vec<String> = Vec::with_capacity(definition.steps.len());
    let mut placed_set: HashSet<&str> = HashSet::new();
    while placed.len() < definition.steps.len() {
        let next = definition.steps.iter().find(|step| {
            !placed_set.contains(step.name.as_str())
                && deps
                    .get(step.name.as_str())
                    .is_none_or(|needs| needs.iter().all(|need| placed_set.contains(need)))
        });
        let Some(next) = next else {
            return Err(format!(
                "operation type {} v{} has a step cycle",
                definition.id, definition.version
            ));
        };
        placed_set.insert(next.name.as_str());
        placed.push(next.name.clone());
    }
    Ok(placed)
}

fn unit_supplier(by: &str, identity: Option<&Identity>) -> Value {
    match identity {
        Some(identity) => json!({"slot": by, "identity": identity}),
        None => json!({"slot": by}),
    }
}

/// Read the frontier of one operation from its records, already cut to the as-of time and in
/// the order they apply.
pub(crate) fn evaluate(
    definition: &OperationType,
    step_types: &HashMap<String, StepType>,
    records: &[Payload],
    opened_by: &str,
    group_by: GroupBy,
) -> Result<Value, String> {
    let state = State::apply(records);
    let order = evaluation_order(definition)?;
    let mut done: HashMap<String, bool> = HashMap::new();
    let mut evals: HashMap<String, StepEval> = HashMap::new();
    for name in &order {
        let step = definition
            .steps
            .iter()
            .find(|step| &step.name == name)
            .ok_or_else(|| format!("step {name} is missing from its type"))?;
        let step_type = step_types
            .get(name)
            .ok_or_else(|| format!("step type for {name} is missing"))?;
        let slot = definition
            .slot(&step_type.by)
            .ok_or_else(|| format!("step {name} is by undeclared slot {}", step_type.by))?;
        let bound = state
            .bindings
            .get(&step_type.by)
            .cloned()
            .unwrap_or_default();
        let unbound = bound.is_empty();
        let identities: Vec<Option<Identity>> = if slot.repeats {
            bound.into_iter().map(Some).collect()
        } else {
            vec![bound.into_iter().next()]
        };
        let rules: Vec<(usize, &crate::definition::Rule)> = definition
            .rules
            .iter()
            .enumerate()
            .filter(|(_, rule)| rule.step == *name && rule.status == RuleStatus::Active)
            .collect();
        let has_activate = rules
            .iter()
            .any(|(_, rule)| rule.action == Action::Activate);
        let acts = state.acts.get(name);
        let context = Context {
            state: &state,
            done: &done,
        };
        let units: Vec<Unit> = identities
            .into_iter()
            .map(|identity| {
                let fires = |rule: &crate::definition::Rule| {
                    let alternative =
                        context.first_holding(&rule.when, &step_type.by, identity.as_ref())?;
                    context
                        .first_holding(&rule.unless, &step_type.by, identity.as_ref())
                        .is_none()
                        .then_some(alternative)
                };
                let skipped = rules
                    .iter()
                    .filter(|(_, rule)| rule.action == Action::Skip)
                    .find_map(|(index, rule)| fires(rule).map(|alternative| (*index, alternative)));
                let asked = if let Some((rule, alternative)) = skipped {
                    Asked::Skipped { rule, alternative }
                } else if has_activate
                    && !rules
                        .iter()
                        .filter(|(_, rule)| rule.action == Action::Activate)
                        .any(|(_, rule)| fires(rule).is_some())
                {
                    Asked::NotActivated
                } else {
                    Asked::Yes
                };
                let acted = match (&identity, slot.repeats) {
                    (Some(identity), true) => {
                        acts.is_some_and(|acts| acts.contains(&Some(identity.reference())))
                    }
                    (Some(identity), false) => acts.is_some_and(|acts| {
                        acts.contains(&None) || acts.contains(&Some(identity.reference()))
                    }),
                    (None, _) => acts.is_some_and(|acts| !acts.is_empty()),
                };
                Unit {
                    identity,
                    asked,
                    acted,
                }
            })
            .collect();
        let asked: Vec<&Unit> = units
            .iter()
            .filter(|unit| unit.asked == Asked::Yes)
            .collect();
        let is_done = match step_type.quantifier {
            Some(Quantifier::AllOf) => !asked.is_empty() && asked.iter().all(|unit| unit.acted),
            Some(Quantifier::AtLeastOne) => asked.iter().any(|unit| unit.acted),
            None => !asked.is_empty() && asked.iter().any(|unit| unit.acted),
        };
        let mut effective_after: Vec<String> = Vec::new();
        for after in &step.after {
            let Some(prior) = evals.get(after) else {
                return Err(format!("step {name} comes after unplaced step {after}"));
            };
            let inherited: Vec<String> = if prior.status == Status::Bypassed {
                prior.effective_after.clone()
            } else {
                vec![after.clone()]
            };
            for name in inherited {
                if !effective_after.contains(&name) {
                    effective_after.push(name);
                }
            }
        }
        let ready = effective_after.iter().all(|after| {
            evals
                .get(after)
                .is_some_and(|prior| prior.status == Status::Done)
        });
        let optional_unbound = unbound && !slot.required && !is_done;
        let status = if is_done {
            Status::Done
        } else if optional_unbound || (!units.is_empty() && asked.is_empty()) {
            Status::Bypassed
        } else if unbound {
            Status::Held
        } else if ready {
            Status::Owed
        } else {
            Status::Blocked
        };
        done.insert(name.clone(), status == Status::Done);
        evals.insert(
            name.clone(),
            StepEval {
                status,
                units,
                effective_after,
                optional_unbound,
            },
        );
    }

    let opener = json!({"opener": opened_by});
    let suppliers_of = |name: &str| -> Vec<Value> {
        let Some(eval) = evals.get(name) else {
            return Vec::new();
        };
        if eval.status == Status::Held {
            return vec![opener.clone()];
        }
        let by = step_types
            .get(name)
            .map(|t| t.by.as_str())
            .unwrap_or_default();
        eval.units
            .iter()
            .filter(|unit| unit.asked == Asked::Yes && !unit.acted)
            .map(|unit| unit_supplier(by, unit.identity.as_ref()))
            .collect()
    };

    let outcome = definition
        .steps
        .iter()
        .find(|step| step.outcome)
        .map(|step| step.name.clone());
    let complete = outcome.as_ref().is_some_and(|name| {
        evals
            .get(name)
            .is_some_and(|eval| eval.status == Status::Done)
    });

    let mut items: Vec<Value> = Vec::new();
    let mut held: Vec<(String, Vec<String>)> = Vec::new();
    let mut not_asked: Vec<Value> = Vec::new();
    let mut steps_view: Vec<Value> = Vec::new();
    for step in &definition.steps {
        let (Some(eval), Some(step_type)) = (evals.get(&step.name), step_types.get(&step.name))
        else {
            continue;
        };
        steps_view.push(json!({"step": step.name, "status": eval.status.as_str()}));
        if eval.optional_unbound {
            not_asked.push(json!({
                "step": step.name,
                "reason": "optional_slot_unbound",
                "slot": step_type.by,
            }));
        }
        for unit in &eval.units {
            let mut entry = match unit.asked {
                Asked::Yes => continue,
                Asked::Skipped { rule, alternative } => json!({
                    "step": step.name,
                    "reason": "skipped",
                    "rule": rule,
                    "alternative": alternative,
                }),
                Asked::NotActivated => json!({"step": step.name, "reason": "not_activated"}),
            };
            if let Some(identity) = &unit.identity {
                entry["identity"] = json!(identity);
            }
            not_asked.push(entry);
        }
        if complete {
            continue;
        }
        match eval.status {
            Status::Held => match held.iter_mut().find(|(slot, _)| *slot == step_type.by) {
                Some((_, steps)) => steps.push(step.name.clone()),
                None => held.push((step_type.by.clone(), vec![step.name.clone()])),
            },
            Status::Owed | Status::Blocked => {
                let waits_on: Vec<Value> = eval
                    .effective_after
                    .iter()
                    .filter(|after| {
                        evals
                            .get(*after)
                            .is_some_and(|prior| prior.status != Status::Done)
                    })
                    .map(|after| json!({"step": after, "suppliers": suppliers_of(after)}))
                    .collect();
                for unit in eval
                    .units
                    .iter()
                    .filter(|unit| unit.asked == Asked::Yes && !unit.acted)
                {
                    let owed = eval.status == Status::Owed;
                    let mut item = json!({
                        "kind": if owed { "act_owed" } else { "act_blocked" },
                        "step": step.name,
                        "act": step_type.act,
                        "by": step_type.by,
                        "supplier": unit_supplier(&step_type.by, unit.identity.as_ref()),
                    });
                    if let Some(identity) = &unit.identity {
                        item["identity"] = json!(identity);
                    }
                    if let Some(quantifier) = step_type.quantifier {
                        item["quantifier"] = json!(quantifier.as_str());
                    }
                    if !owed {
                        item["waits_on"] = json!(waits_on);
                    }
                    items.push(item);
                }
            }
            Status::Done | Status::Bypassed => {}
        }
    }
    for (slot, steps) in held {
        items.push(json!({
            "kind": "slot_unbound",
            "slot": slot,
            "supplier": opener,
            "holds_steps": steps,
        }));
    }

    let group_key = |supplier: &Value| -> Value {
        match group_by {
            GroupBy::Supplier => supplier.clone(),
            GroupBy::By => match supplier.get("slot") {
                Some(slot) => json!({"slot": slot}),
                None => supplier.clone(),
            },
        }
    };
    let mut groups: Vec<(Value, Vec<Value>)> = Vec::new();
    for item in &items {
        let mut keys: Vec<Value> = Vec::new();
        if item["kind"] == "act_blocked" {
            for wait in item["waits_on"].as_array().into_iter().flatten() {
                for supplier in wait["suppliers"].as_array().into_iter().flatten() {
                    keys.push(group_key(supplier));
                }
            }
        } else {
            keys.push(group_key(&item["supplier"]));
        }
        let mut seen: Vec<Value> = Vec::new();
        for key in keys {
            if seen.contains(&key) {
                continue;
            }
            seen.push(key.clone());
            match groups.iter_mut().find(|(held, _)| *held == key) {
                Some((_, members)) => members.push(item.clone()),
                None => groups.push((key, vec![item.clone()])),
            }
        }
    }

    Ok(json!({
        "complete": complete,
        "outcome_step": outcome,
        "items": items,
        "groups": groups
            .into_iter()
            .map(|(supplier, items)| json!({"supplier": supplier, "items": items}))
            .collect::<Vec<_>>(),
        "not_asked": {"count": not_asked.len(), "steps": not_asked},
        "steps": steps_view,
    }))
}
