//! Operation-type documents: what a caller defines, the definitions the store keeps, and the
//! checks that refuse a document before anything is written.
//!
//! Each step of a type compiles to its own step type, `<type>/<step>` at the type's version,
//! and the operation type pins each step type by digest. A step's `activated_by` and `unless`
//! become rules beside the type's own, so the frontier reads one rule list.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A refused input: a stable code and a message for the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refusal {
    pub code: &'static str,
    pub message: String,
}

impl Refusal {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}

/// Object keys sorted at every depth, so equal values have equal bytes.
pub(crate) fn canonical_json(value: &Value) -> Vec<u8> {
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                let mut out = Map::new();
                for key in keys {
                    out.insert(key.clone(), sorted(&map[key]));
                }
                Value::Object(out)
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    // A `Value` has string keys only, so writing it to bytes cannot fail.
    serde_json::to_vec(&sorted(value)).unwrap_or_default()
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    format!("blake3:{}", blake3::hash(bytes).to_hex())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Quantifier {
    AllOf,
    AtLeastOne,
}

impl Quantifier {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AllOf => "all_of",
            Self::AtLeastOne => "at_least_one",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Action {
    Activate,
    Skip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RuleStatus {
    Proposed,
    #[default]
    Active,
    Retired,
}

/// One condition of a rule: a fact test or a step's completion.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Term {
    Fact(FactTerm),
    Step(StepTerm),
}

/// A test on the latest value of a fact. Without `slot` the fact is about the operation; with
/// `slot` it is about the identity bound there (for a repeating slot, the identity the step is
/// being asked of). Exactly one of `present`, `equals` and `one_of` is given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FactTerm {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub present: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub equals: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_of: Option<Vec<Value>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepTerm {
    pub name: String,
    pub done: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DocumentInput {
    operation_types: Vec<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TypeInput {
    id: String,
    version: i64,
    #[serde(default)]
    slots: Vec<SlotInput>,
    #[serde(default)]
    steps: Vec<StepInput>,
    #[serde(default)]
    rules: Vec<RuleInput>,
}

fn yes() -> bool {
    true
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SlotInput {
    name: String,
    #[serde(default)]
    repeats: bool,
    #[serde(default = "yes")]
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepInput {
    name: String,
    #[serde(default)]
    act: Option<String>,
    by: String,
    #[serde(default)]
    after: Vec<String>,
    #[serde(default)]
    quantifier: Option<Quantifier>,
    #[serde(default)]
    activated_by: Vec<Vec<Term>>,
    #[serde(default)]
    unless: Vec<Vec<Term>>,
    #[serde(default)]
    outcome: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuleInput {
    step: String,
    action: Action,
    #[serde(default)]
    when: Vec<Vec<Term>>,
    #[serde(default)]
    unless: Vec<Vec<Term>>,
    #[serde(default)]
    status: RuleStatus,
    #[serde(default)]
    owner: Option<String>,
    #[serde(default)]
    citation: Option<String>,
}

/// The stored operation type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct OperationType {
    pub kind: String,
    pub id: String,
    pub version: i64,
    pub slots: Vec<Slot>,
    pub steps: Vec<StepRef>,
    pub rules: Vec<Rule>,
}

impl OperationType {
    pub fn slot(&self, name: &str) -> Option<&Slot> {
        self.slots.iter().find(|slot| slot.name == name)
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Slot {
    pub name: String,
    pub repeats: bool,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepRef {
    pub name: String,
    pub step_type: DefinitionRef,
    pub after: Vec<String>,
    pub outcome: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DefinitionRef {
    pub id: String,
    pub version: i64,
    pub digest: String,
}

/// The stored step type: who acts (`by` names a slot of the type) and, for a repeating slot,
/// whether every identity there must act or one is enough.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepType {
    pub kind: String,
    pub id: String,
    pub version: i64,
    pub name: String,
    pub act: String,
    pub by: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quantifier: Option<Quantifier>,
}

/// A rule asks (`activate`) or skips a step when one of its `when` alternatives holds in full
/// and none of its `unless` alternatives does. Only an `active` rule is read by the frontier.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Rule {
    pub step: String,
    pub action: Action,
    pub when: Vec<Vec<Term>>,
    pub unless: Vec<Vec<Term>>,
    pub status: RuleStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation: Option<String>,
}

/// A compiled definition ready to store.
#[derive(Debug, Clone)]
pub(crate) struct Stored {
    pub kind: &'static str,
    pub id: String,
    pub version: i64,
    pub digest: String,
    pub bytes: Vec<u8>,
    pub source: String,
}

/// One compiled operation type with its step types.
#[derive(Debug, Clone)]
pub(crate) struct Compiled {
    pub operation_type: Stored,
    pub step_types: Vec<Stored>,
    pub definition: OperationType,
}

pub(crate) fn compile_document(document: &Value) -> Result<Vec<Compiled>, Refusal> {
    let input: DocumentInput = serde_json::from_value(document.clone())
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    if input.operation_types.is_empty() {
        return Err(Refusal::new(
            "invalid_document",
            "operation_types lists no type",
        ));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(input.operation_types.len());
    for source in &input.operation_types {
        let compiled = compile_type(source)?;
        let key = (compiled.definition.id.clone(), compiled.definition.version);
        if !seen.insert(key) {
            return Err(Refusal::new(
                "duplicate_type",
                format!(
                    "{} v{} appears twice in the document",
                    compiled.definition.id, compiled.definition.version
                ),
            ));
        }
        out.push(compiled);
    }
    Ok(out)
}

fn check_name(what: &str, name: &str) -> Result<(), Refusal> {
    if name.trim().is_empty() || name.trim() != name {
        return Err(Refusal::new(
            "invalid_name",
            format!("{what} {name:?} must be non-empty with no surrounding space"),
        ));
    }
    if name.contains('/') {
        return Err(Refusal::new(
            "invalid_name",
            format!("{what} {name:?} must not contain '/'"),
        ));
    }
    Ok(())
}

struct Scope<'a> {
    type_id: &'a str,
    slots: BTreeMap<&'a str, &'a SlotInput>,
    steps: BTreeMap<&'a str, &'a StepInput>,
}

impl Scope<'_> {
    fn check_term(&self, governing: &StepInput, term: &Term) -> Result<(), Refusal> {
        match term {
            Term::Fact(fact) => {
                if fact.field.trim().is_empty() {
                    return Err(Refusal::new(
                        "invalid_term",
                        format!("{}: a fact term names no field", self.type_id),
                    ));
                }
                let tests = usize::from(fact.present.is_some())
                    + usize::from(fact.equals.is_some())
                    + usize::from(fact.one_of.is_some());
                if tests != 1 {
                    return Err(Refusal::new(
                        "invalid_term",
                        format!(
                            "{}: fact term on {} must give exactly one of present, equals and one_of",
                            self.type_id, fact.field
                        ),
                    ));
                }
                if fact.one_of.as_ref().is_some_and(Vec::is_empty) {
                    return Err(Refusal::new(
                        "invalid_term",
                        format!(
                            "{}: fact term on {} has an empty one_of",
                            self.type_id, fact.field
                        ),
                    ));
                }
                if let Some(slot) = &fact.slot {
                    let Some(declared) = self.slots.get(slot.as_str()) else {
                        return Err(Refusal::new(
                            "unknown_slot",
                            format!(
                                "{}: fact term names slot {slot}, which the type does not declare",
                                self.type_id
                            ),
                        ));
                    };
                    if declared.repeats && *slot != governing.by {
                        return Err(Refusal::new(
                            "fact_term_outside_expansion",
                            format!(
                                "{}: a rule on step {} tests a fact of repeating slot {slot}, but the step is asked of {}; only the step's own repeating slot names one identity",
                                self.type_id, governing.name, governing.by
                            ),
                        ));
                    }
                }
                Ok(())
            }
            Term::Step(step) => {
                if self.steps.contains_key(step.name.as_str()) {
                    Ok(())
                } else {
                    Err(Refusal::new(
                        "unknown_step",
                        format!(
                            "{}: step term names {}, which the type does not declare",
                            self.type_id, step.name
                        ),
                    ))
                }
            }
        }
    }

    fn check_alternatives(
        &self,
        governing: &StepInput,
        label: &str,
        alternatives: &[Vec<Term>],
    ) -> Result<(), Refusal> {
        for (index, terms) in alternatives.iter().enumerate() {
            if terms.is_empty() {
                return Err(Refusal::new(
                    "empty_premise",
                    format!(
                        "{}: {label} alternative {index} on step {} has no term",
                        self.type_id, governing.name
                    ),
                ));
            }
            for term in terms {
                self.check_term(governing, term)?;
            }
        }
        Ok(())
    }
}

fn step_terms(alternatives: &[Vec<Term>]) -> impl Iterator<Item = &str> {
    alternatives.iter().flatten().filter_map(|term| match term {
        Term::Step(step) => Some(step.name.as_str()),
        Term::Fact(_) => None,
    })
}

/// A cycle in `edges` (node to the nodes it depends on), as the path that closes it.
pub(crate) fn find_cycle(
    order: &[&str],
    edges: &BTreeMap<&str, BTreeSet<&str>>,
) -> Option<Vec<String>> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mark {
        Open,
        Done,
    }
    fn visit<'a>(
        node: &'a str,
        edges: &BTreeMap<&'a str, BTreeSet<&'a str>>,
        marks: &mut BTreeMap<&'a str, Mark>,
        path: &mut Vec<&'a str>,
    ) -> Option<Vec<String>> {
        match marks.get(node) {
            Some(Mark::Done) => return None,
            Some(Mark::Open) => {
                let start = path.iter().position(|n| *n == node).unwrap_or(0);
                let mut cycle: Vec<String> =
                    path[start..].iter().map(|n| (*n).to_owned()).collect();
                cycle.push(node.to_owned());
                return Some(cycle);
            }
            None => {}
        }
        marks.insert(node, Mark::Open);
        path.push(node);
        if let Some(next) = edges.get(node) {
            for dep in next {
                if let Some(cycle) = visit(dep, edges, marks, path) {
                    return Some(cycle);
                }
            }
        }
        path.pop();
        marks.insert(node, Mark::Done);
        None
    }
    let mut marks = BTreeMap::new();
    for node in order {
        let mut path = Vec::new();
        if let Some(cycle) = visit(node, edges, &mut marks, &mut path) {
            return Some(cycle);
        }
    }
    None
}

fn store(kind: &'static str, id: &str, version: i64, value: &Value, source: &Value) -> Stored {
    let bytes = canonical_json(value);
    Stored {
        kind,
        id: id.to_owned(),
        version,
        digest: digest(&bytes),
        bytes,
        source: String::from_utf8(canonical_json(source)).unwrap_or_default(),
    }
}

/// Steps one operation type may declare. The cycle check walks the dependency chain one
/// recursion level per step, so this also bounds its depth.
pub(crate) const MAX_STEPS: usize = 256;

fn compile_type(source: &Value) -> Result<Compiled, Refusal> {
    let input: TypeInput = serde_json::from_value(source.clone())
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    check_name("operation type id", &input.id)?;
    if input.version < 1 {
        return Err(Refusal::new(
            "invalid_version",
            format!(
                "{}: version must be 1 or more, got {}",
                input.id, input.version
            ),
        ));
    }
    if input.steps.is_empty() {
        return Err(Refusal::new(
            "no_steps",
            format!("{} declares no step", input.id),
        ));
    }
    if input.steps.len() > MAX_STEPS {
        return Err(Refusal::new(
            "too_many_steps",
            format!(
                "{} declares {} steps; a type holds at most {MAX_STEPS}",
                input.id,
                input.steps.len()
            ),
        ));
    }
    let mut scope = Scope {
        type_id: &input.id,
        slots: BTreeMap::new(),
        steps: BTreeMap::new(),
    };
    for slot in &input.slots {
        check_name("slot", &slot.name)?;
        if scope.slots.insert(slot.name.as_str(), slot).is_some() {
            return Err(Refusal::new(
                "duplicate_slot",
                format!("{}: slot {} is declared twice", input.id, slot.name),
            ));
        }
    }
    for step in &input.steps {
        check_name("step", &step.name)?;
        if scope.steps.insert(step.name.as_str(), step).is_some() {
            return Err(Refusal::new(
                "duplicate_step",
                format!("{}: step {} is declared twice", input.id, step.name),
            ));
        }
    }
    for step in &input.steps {
        let Some(slot) = scope.slots.get(step.by.as_str()) else {
            return Err(Refusal::new(
                "unknown_slot",
                format!(
                    "{}: step {} is by {}, which the type does not declare",
                    input.id, step.name, step.by
                ),
            ));
        };
        if step.quantifier.is_some() && !slot.repeats {
            return Err(Refusal::new(
                "quantifier_without_repeat",
                format!(
                    "{}: step {} gives a quantifier, but slot {} holds one identity",
                    input.id, step.name, step.by
                ),
            ));
        }
        if let Some(act) = &step.act {
            check_name("act", act)?;
        }
        for after in &step.after {
            if !scope.steps.contains_key(after.as_str()) {
                return Err(Refusal::new(
                    "unknown_step",
                    format!(
                        "{}: step {} comes after {after}, which the type does not declare",
                        input.id, step.name
                    ),
                ));
            }
        }
        scope.check_alternatives(step, "activated_by", &step.activated_by)?;
        scope.check_alternatives(step, "unless", &step.unless)?;
    }
    let outcomes: Vec<&str> = input
        .steps
        .iter()
        .filter(|step| step.outcome)
        .map(|step| step.name.as_str())
        .collect();
    match outcomes.len() {
        0 => {
            return Err(Refusal::new(
                "no_outcome",
                format!("{}: no step is marked outcome", input.id),
            ))
        }
        1 => {}
        _ => {
            return Err(Refusal::new(
                "multiple_outcomes",
                format!(
                    "{}: steps {} are all marked outcome",
                    input.id,
                    outcomes.join(", ")
                ),
            ))
        }
    }

    let mut rules = Vec::new();
    for step in &input.steps {
        if !step.activated_by.is_empty() {
            rules.push(Rule {
                step: step.name.clone(),
                action: Action::Activate,
                when: step.activated_by.clone(),
                unless: Vec::new(),
                status: RuleStatus::Active,
                owner: None,
                citation: None,
            });
        }
        if !step.unless.is_empty() {
            rules.push(Rule {
                step: step.name.clone(),
                action: Action::Skip,
                when: step.unless.clone(),
                unless: Vec::new(),
                status: RuleStatus::Active,
                owner: None,
                citation: None,
            });
        }
    }
    for rule in &input.rules {
        let Some(governing) = scope.steps.get(rule.step.as_str()) else {
            return Err(Refusal::new(
                "unknown_step",
                format!(
                    "{}: a rule names step {}, which the type does not declare",
                    input.id, rule.step
                ),
            ));
        };
        if rule.when.is_empty() {
            return Err(Refusal::new(
                "empty_premise",
                format!("{}: a rule on step {} has no when", input.id, rule.step),
            ));
        }
        scope.check_alternatives(governing, "when", &rule.when)?;
        scope.check_alternatives(governing, "unless", &rule.unless)?;
        rules.push(Rule {
            step: rule.step.clone(),
            action: rule.action,
            when: rule.when.clone(),
            unless: rule.unless.clone(),
            status: rule.status,
            owner: rule.owner.clone(),
            citation: rule.citation.clone(),
        });
    }

    let order: Vec<&str> = input.steps.iter().map(|step| step.name.as_str()).collect();
    let mut after_edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for step in &input.steps {
        after_edges
            .entry(step.name.as_str())
            .or_default()
            .extend(step.after.iter().map(String::as_str));
    }
    if let Some(cycle) = find_cycle(&order, &after_edges) {
        return Err(Refusal::new(
            "after_cycle",
            format!(
                "{}: steps come after each other in a cycle: {}",
                input.id,
                cycle.join(" -> ")
            ),
        ));
    }
    let mut all_edges = after_edges.clone();
    for rule in &rules {
        let entry = all_edges.entry(rule.step.as_str()).or_default();
        entry.extend(step_terms(&rule.when));
        entry.extend(step_terms(&rule.unless));
    }
    if let Some(cycle) = find_cycle(&order, &all_edges) {
        return Err(Refusal::new(
            "rule_cycle",
            format!(
                "{}: a step's rules depend on its own completion through: {}",
                input.id,
                cycle.join(" -> ")
            ),
        ));
    }

    let mut step_types = Vec::with_capacity(input.steps.len());
    let mut step_refs = Vec::with_capacity(input.steps.len());
    for step in &input.steps {
        let slot_repeats = scope
            .slots
            .get(step.by.as_str())
            .is_some_and(|slot| slot.repeats);
        let step_type = StepType {
            kind: "step_type".to_owned(),
            id: format!("{}/{}", input.id, step.name),
            version: input.version,
            name: step.name.clone(),
            act: step.act.clone().unwrap_or_else(|| step.name.clone()),
            by: step.by.clone(),
            quantifier: slot_repeats.then(|| step.quantifier.unwrap_or(Quantifier::AllOf)),
        };
        let value = serde_json::to_value(&step_type)
            .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
        let stored = store("step_type", &step_type.id, input.version, &value, &value);
        step_refs.push(StepRef {
            name: step.name.clone(),
            step_type: DefinitionRef {
                id: stored.id.clone(),
                version: stored.version,
                digest: stored.digest.clone(),
            },
            after: step.after.clone(),
            outcome: step.outcome,
        });
        step_types.push(stored);
    }
    let definition = OperationType {
        kind: "operation_type".to_owned(),
        id: input.id.clone(),
        version: input.version,
        slots: input
            .slots
            .iter()
            .map(|slot| Slot {
                name: slot.name.clone(),
                repeats: slot.repeats,
                required: slot.required,
            })
            .collect(),
        steps: step_refs,
        rules,
    };
    let value = serde_json::to_value(&definition)
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    Ok(Compiled {
        operation_type: store("operation_type", &input.id, input.version, &value, source),
        step_types,
        definition,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn one(document: Value) -> Result<Compiled, Refusal> {
        compile_document(&json!({"operation_types": [document]})).map(|mut all| all.remove(0))
    }

    fn base() -> Value {
        json!({
            "id": "filing",
            "version": 1,
            "slots": [{"name": "applicant", "repeats": true}, {"name": "office"}],
            "steps": [
                {"name": "sign", "by": "applicant"},
                {"name": "issue", "by": "office", "after": ["sign"], "outcome": true}
            ]
        })
    }

    #[test]
    fn a_type_compiles_to_pinned_step_types() {
        let compiled = one(base()).expect("compiles");
        assert_eq!(compiled.step_types.len(), 2);
        assert_eq!(compiled.step_types[0].id, "filing/sign");
        assert_eq!(
            compiled.definition.steps[0].step_type.digest,
            compiled.step_types[0].digest
        );
        assert!(compiled.operation_type.digest.starts_with("blake3:"));
        let again = one(base()).expect("compiles");
        assert_eq!(again.operation_type.digest, compiled.operation_type.digest);
    }

    #[test]
    fn the_checks_refuse_by_code() {
        let cases: Vec<(&str, Value)> = vec![
            (
                "unknown_slot",
                json!({"id": "t", "version": 1, "steps": [{"name": "a", "by": "x", "outcome": true}]}),
            ),
            (
                "no_steps",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}]}),
            ),
            (
                "invalid_name",
                json!({"id": "t/u", "version": 1, "slots": [{"name": "x"}], "steps": [{"name": "a", "by": "x", "outcome": true}]}),
            ),
            (
                "after_cycle",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [
                {"name": "a", "by": "x", "after": ["b"]}, {"name": "b", "by": "x", "after": ["a"], "outcome": true}]}),
            ),
            (
                "rule_cycle",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [
                {"name": "a", "by": "x", "unless": [[{"step": {"name": "b", "done": true}}]]},
                {"name": "b", "by": "x", "after": ["a"], "outcome": true}]}),
            ),
            (
                "fact_term_outside_expansion",
                json!({"id": "t", "version": 1, "slots": [{"name": "x", "repeats": true}, {"name": "y"}], "steps": [
                {"name": "a", "by": "y", "unless": [[{"fact": {"field": "f", "slot": "x", "present": true}}]], "outcome": true}]}),
            ),
            (
                "quantifier_without_repeat",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [
                {"name": "a", "by": "x", "quantifier": "all_of", "outcome": true}]}),
            ),
            (
                "multiple_outcomes",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [
                {"name": "a", "by": "x", "outcome": true}, {"name": "b", "by": "x", "outcome": true}]}),
            ),
            (
                "no_outcome",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [{"name": "a", "by": "x"}]}),
            ),
            (
                "empty_premise",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [{"name": "a", "by": "x", "outcome": true}],
                "rules": [{"step": "a", "action": "skip", "when": []}]}),
            ),
            (
                "invalid_term",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [
                {"name": "a", "by": "x", "unless": [[{"fact": {"field": "f", "present": true, "equals": 1}}]], "outcome": true}]}),
            ),
            (
                "duplicate_step",
                json!({"id": "t", "version": 1, "slots": [{"name": "x"}], "steps": [
                {"name": "a", "by": "x"}, {"name": "a", "by": "x", "outcome": true}]}),
            ),
            (
                "invalid_document",
                json!({"id": "t", "version": 1, "colour": "red", "slots": [{"name": "x"}], "steps": [{"name": "a", "by": "x", "outcome": true}]}),
            ),
        ];
        for (code, document) in cases {
            let refusal = one(document).expect_err(code);
            assert_eq!(refusal.code, code, "{refusal}");
        }
    }

    #[test]
    fn a_step_term_on_an_earlier_step_is_not_a_cycle() {
        let mut document = base();
        document["steps"][1]["unless"] = json!([[{"step": {"name": "sign", "done": false}}]]);
        one(document).expect("a later step may test an earlier one");
    }
}
