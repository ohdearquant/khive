//! Operation-type documents: what a caller defines, the definitions the store keeps, and the
//! checks that refuse a document before anything is written.
//!
//! A step type is published on its own (`step_types`) or given inside a type, where it compiles
//! to its own step type, `<type>/<step>` at the type's version. Either way the operation type
//! pins each step type by digest. A step's `activated_by` and `unless` become rules beside the
//! type's own, so the frontier reads one rule list. A step that requires a value another step
//! produces comes after it; that order is derived here and kept in the pinned bytes.

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
    #[serde(default)]
    operation_types: Vec<Value>,
    #[serde(default)]
    step_types: Vec<Value>,
    #[serde(default)]
    source_order: Option<Value>,
}

/// The deployment's order between the places a value can be read from (ADR-222 A2.6): document
/// types and systems loaded in the forms pack, each named once. A namespace holds one, versioned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SourceOrder {
    pub version: i64,
    pub places: Vec<String>,
}

/// What a produced value is: read from a place, computed from other values, or stated by the
/// party that acts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum ProducedClass {
    Read,
    Computed,
    Stated,
}

/// A value a step needs before it can be done: a field, about the identity bound in `slot` or,
/// without one, about the operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Requirement {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    pub value_type: String,
}

/// A value a step produces when it is done.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Product {
    pub field: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slot: Option<String>,
    pub class: ProducedClass,
}

/// A value one step produces as `field` and the step that carries this supply requires as
/// `as`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Supply {
    pub from: String,
    pub field: String,
    #[serde(rename = "as")]
    pub as_field: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinInput {
    id: String,
    version: i64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepTypeInput {
    id: String,
    version: i64,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    act: Option<String>,
    by: String,
    #[serde(default)]
    quantifier: Option<Quantifier>,
    #[serde(default)]
    requires: Vec<Requirement>,
    #[serde(default)]
    produces: Vec<Product>,
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
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    role: Option<String>,
    #[serde(default)]
    qualifier: Option<String>,
    #[serde(default)]
    defined_by: Option<SlotRelation>,
}

/// A slot given by a relation to another slot: what stands in `relation` to the identity
/// in `slot`, such as the party that owns what is in slot `parcel`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SlotRelation {
    pub relation: String,
    pub slot: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StepInput {
    name: String,
    #[serde(default)]
    act: Option<String>,
    #[serde(default)]
    by: Option<String>,
    #[serde(default)]
    step_type: Option<PinInput>,
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
    #[serde(default)]
    requires: Vec<Requirement>,
    #[serde(default)]
    produces: Vec<Product>,
    #[serde(default)]
    supplies: Vec<Supply>,
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
    /// The kind and role a form subject must carry to fill this slot, with the qualifier
    /// that tells two slots of one role apart. Absent from the stored bytes when not given,
    /// so a type published before slots carried them keeps its digest.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub qualifier: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub defined_by: Option<SlotRelation>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StepRef {
    pub name: String,
    pub step_type: DefinitionRef,
    pub after: Vec<String>,
    pub outcome: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub supplies: Vec<Supply>,
    /// The steps this one comes after because it requires what they produce or is supplied
    /// from them, besides those in `after`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub derived_after: Vec<String>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires: Vec<Requirement>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub produces: Vec<Product>,
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

/// Published step types by (id, version), with the digest each is held under.
pub(crate) type Available = BTreeMap<(String, i64), (StepType, String)>;

/// What one document publishes: the step types it gives on their own, then its operation
/// types with the step types given inside them, and the namespace's source order if it gives
/// one.
#[derive(Debug)]
pub(crate) struct Document {
    pub step_types: Vec<Stored>,
    pub operation_types: Vec<Compiled>,
    pub source_order: Option<(Stored, SourceOrder)>,
}

/// The step types a document names by id and version, those it gives and those its steps
/// pin, so the caller can read the published ones before compiling. A malformed entry is left
/// to [`compile_document`] to refuse.
pub(crate) fn step_type_refs(document: &Value) -> BTreeSet<(String, i64)> {
    let pair = |value: &Value| {
        Some((
            value.get("id")?.as_str()?.to_owned(),
            value.get("version")?.as_i64()?,
        ))
    };
    let list = |key: &str| {
        document
            .get(key)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
    };
    let mut refs: BTreeSet<(String, i64)> = list("step_types").filter_map(pair).collect();
    for type_ in list("operation_types") {
        let steps = type_
            .get("steps")
            .and_then(Value::as_array)
            .into_iter()
            .flatten();
        refs.extend(
            steps
                .filter_map(|step| step.get("step_type"))
                .filter_map(pair),
        );
    }
    refs
}

/// Compile a document against the step types already published in its namespace.
pub(crate) fn compile_document(
    document: &Value,
    published: &Available,
) -> Result<Document, Refusal> {
    let input: DocumentInput = serde_json::from_value(document.clone())
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    if input.operation_types.is_empty()
        && input.step_types.is_empty()
        && input.source_order.is_none()
    {
        return Err(Refusal::new(
            "invalid_document",
            "the document lists no operation type, no step type and no source order",
        ));
    }
    let source_order = input
        .source_order
        .as_ref()
        .map(compile_source_order)
        .transpose()?;
    let mut available = published.clone();
    let mut step_types = Vec::with_capacity(input.step_types.len());
    let mut seen_steps = BTreeSet::new();
    for source in &input.step_types {
        let (stored, step_type) = compile_step_type(source)?;
        let key = (stored.id.clone(), stored.version);
        if !seen_steps.insert(key.clone()) {
            return Err(Refusal::new(
                "duplicate_type",
                format!(
                    "step type {} v{} appears twice in the document",
                    stored.id, stored.version
                ),
            ));
        }
        if let Some((_, held)) = published.get(&key) {
            if *held != stored.digest {
                return Err(Refusal::new(
                    "version_conflict",
                    format!(
                        "{} v{} is published with a different definition ({held}); publish a new version",
                        stored.id, stored.version
                    ),
                ));
            }
        }
        available.insert(key, (step_type, stored.digest.clone()));
        step_types.push(stored);
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(input.operation_types.len());
    for source in &input.operation_types {
        let compiled = compile_type(source, &available)?;
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
    Ok(Document {
        step_types,
        operation_types: out,
        source_order,
    })
}

/// A source order: a version and the places in order, none named twice. Names are compared as
/// the forms pack compares them, so two spellings of one place are refused as a duplicate; that
/// each place is loaded is checked against the forms pack before anything is written.
fn compile_source_order(source: &Value) -> Result<(Stored, SourceOrder), Refusal> {
    let order: SourceOrder = serde_json::from_value(source.clone())
        .map_err(|e| Refusal::new("invalid_document", format!("source_order: {e}")))?;
    if order.version < 1 {
        return Err(Refusal::new(
            "invalid_version",
            format!(
                "source_order: version must be 1 or more, got {}",
                order.version
            ),
        ));
    }
    if order.places.is_empty() {
        return Err(Refusal::new(
            "invalid_document",
            "source_order lists no place",
        ));
    }
    let mut seen = BTreeSet::new();
    for place in &order.places {
        // A place is a forms pack name, not an operation id, so check_name's '/' rule is not
        // applied to it.
        if place.trim().is_empty() || place.trim() != place {
            return Err(Refusal::new(
                "invalid_name",
                format!("place {place:?} must be non-empty with no surrounding space"),
            ));
        }
        if !seen.insert(fold(place)) {
            return Err(Refusal::new(
                "duplicate_place",
                format!("source_order names the place \"{place}\" twice"),
            ));
        }
    }
    let value = serde_json::to_value(&order)
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    let stored = store(
        "source_order",
        "source_order",
        order.version,
        &value,
        source,
    );
    Ok((stored, order))
}

/// A step type published on its own. Its `by` and the slots its values are about are slot
/// names; an operation type that pins it must declare them.
fn compile_step_type(source: &Value) -> Result<(Stored, StepType), Refusal> {
    let input: StepTypeInput = serde_json::from_value(source.clone())
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    check_name("step type id", &input.id)?;
    if input.version < 1 {
        return Err(Refusal::new(
            "invalid_version",
            format!(
                "{}: version must be 1 or more, got {}",
                input.id, input.version
            ),
        ));
    }
    let name = input.name.unwrap_or_else(|| input.id.clone());
    check_name("step", &name)?;
    check_name("slot", &input.by)?;
    let act = input.act.unwrap_or_else(|| name.clone());
    check_name("act", &act)?;
    check_values(&input.id, &name, &input.requires, &input.produces)?;
    let step_type = StepType {
        kind: "step_type".to_owned(),
        id: input.id,
        version: input.version,
        name,
        act,
        by: input.by,
        quantifier: input.quantifier,
        requires: input.requires,
        produces: input.produces,
    };
    let value = serde_json::to_value(&step_type)
        .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
    let stored = store(
        "step_type",
        &step_type.id,
        step_type.version,
        &value,
        source,
    );
    Ok((stored, step_type))
}

/// The values a step requires and produces: each names a field, a requirement names its value
/// type, and no (field, slot) is required, or produced, twice.
fn check_values(
    owner: &str,
    step: &str,
    requires: &[Requirement],
    produces: &[Product],
) -> Result<(), Refusal> {
    let about = |slot: &Option<String>| slot.as_deref().unwrap_or("the operation").to_owned();
    let mut needed = BTreeSet::new();
    for need in requires {
        if need.field.trim().is_empty() || need.value_type.trim().is_empty() {
            return Err(Refusal::new(
                "invalid_document",
                format!("{owner}: step {step} requires a value with no field or no value_type"),
            ));
        }
        if !needed.insert((need.field.as_str(), need.slot.as_deref())) {
            return Err(Refusal::new(
                "invalid_document",
                format!(
                    "{owner}: step {step} requires {} about {} twice",
                    need.field,
                    about(&need.slot)
                ),
            ));
        }
    }
    let mut made = BTreeSet::new();
    for product in produces {
        if product.field.trim().is_empty() {
            return Err(Refusal::new(
                "invalid_document",
                format!("{owner}: step {step} produces a value with no field"),
            ));
        }
        if !made.insert((product.field.as_str(), product.slot.as_deref())) {
            return Err(Refusal::new(
                "invalid_document",
                format!(
                    "{owner}: step {step} produces {} about {} twice",
                    product.field,
                    about(&product.slot)
                ),
            ));
        }
    }
    Ok(())
}

/// One step of a type as compiled: given in the type, or a published step type it pins.
struct StepSpec {
    name: String,
    act: Option<String>,
    by: String,
    after: Vec<String>,
    quantifier: Option<Quantifier>,
    activated_by: Vec<Vec<Term>>,
    unless: Vec<Vec<Term>>,
    outcome: bool,
    requires: Vec<Requirement>,
    produces: Vec<Product>,
    supplies: Vec<Supply>,
    pinned: Option<DefinitionRef>,
}

fn step_spec(type_id: &str, step: StepInput, available: &Available) -> Result<StepSpec, Refusal> {
    let mut spec = StepSpec {
        name: step.name,
        act: step.act,
        by: String::new(),
        after: step.after,
        quantifier: step.quantifier,
        activated_by: step.activated_by,
        unless: step.unless,
        outcome: step.outcome,
        requires: step.requires,
        produces: step.produces,
        supplies: step.supplies,
        pinned: None,
    };
    match (step.step_type, step.by) {
        (Some(pin), by) => {
            if by.is_some()
                || spec.act.is_some()
                || spec.quantifier.is_some()
                || !spec.requires.is_empty()
                || !spec.produces.is_empty()
            {
                return Err(Refusal::new(
                    "pinned_step_shape",
                    format!(
                        "{type_id}: step {} pins step type {} v{}, which says who acts and what it requires and produces; drop by, act, quantifier, requires and produces",
                        spec.name, pin.id, pin.version
                    ),
                ));
            }
            check_name("step type id", &pin.id)?;
            let Some((step_type, digest)) = available.get(&(pin.id.clone(), pin.version)) else {
                return Err(Refusal::new(
                    "unknown_step_type",
                    format!(
                        "{type_id}: step {} pins step type {} v{}, which is neither published in this namespace nor given in the document",
                        spec.name, pin.id, pin.version
                    ),
                ));
            };
            spec.act = Some(step_type.act.clone());
            spec.by = step_type.by.clone();
            spec.quantifier = step_type.quantifier;
            spec.requires = step_type.requires.clone();
            spec.produces = step_type.produces.clone();
            spec.pinned = Some(DefinitionRef {
                id: pin.id,
                version: pin.version,
                digest: digest.clone(),
            });
        }
        (None, Some(by)) => spec.by = by,
        (None, None) => {
            return Err(Refusal::new(
                "invalid_document",
                format!(
                    "{type_id}: step {} names neither by nor step_type",
                    spec.name
                ),
            ))
        }
    }
    Ok(spec)
}

/// A supply names a step of the type that produces `field` about a slot this step requires
/// `as` about. A step may require `as` about more than one slot; the requirement supplied is
/// the one whose slot the producer produces `field` about.
fn check_supply(
    type_id: &str,
    step: &StepSpec,
    supply: &Supply,
    scope: &Scope<'_>,
) -> Result<(), Refusal> {
    let Some(from) = scope.steps.get(supply.from.as_str()) else {
        return Err(Refusal::new(
            "unknown_step",
            format!(
                "{type_id}: step {} is supplied from {}, which the type does not declare",
                step.name, supply.from
            ),
        ));
    };
    if from.name == step.name {
        return Err(Refusal::new(
            "derived_cycle",
            format!(
                "{type_id}: step {} is supplied from itself: {} -> {}",
                step.name, step.name, step.name
            ),
        ));
    }
    let needs: Vec<&Requirement> = step
        .requires
        .iter()
        .filter(|need| need.field == supply.as_field)
        .collect();
    if needs.is_empty() {
        return Err(Refusal::new(
            "unknown_supply",
            format!(
                "{type_id}: step {} is supplied a value as {}, which it does not require",
                step.name, supply.as_field
            ),
        ));
    }
    let made: Vec<&Product> = from
        .produces
        .iter()
        .filter(|made| made.field == supply.field)
        .collect();
    if made.is_empty() {
        return Err(Refusal::new(
            "unknown_supply",
            format!(
                "{type_id}: step {} is supplied {} from {}, which does not produce it",
                step.name, supply.field, supply.from
            ),
        ));
    }
    if !needs
        .iter()
        .any(|need| made.iter().any(|made| made.slot == need.slot))
    {
        let about: Vec<&str> = needs
            .iter()
            .map(|need| need.slot.as_deref().unwrap_or("the operation"))
            .collect();
        return Err(Refusal::new(
            "supply_slot_mismatch",
            format!(
                "{type_id}: step {} requires {} about {}, but {} produces {} about another slot",
                step.name,
                supply.as_field,
                about.join(" or "),
                supply.from,
                supply.field
            ),
        ));
    }
    Ok(())
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
    steps: BTreeMap<&'a str, &'a StepSpec>,
}

impl Scope<'_> {
    fn check_term(&self, governing: &StepSpec, term: &Term) -> Result<(), Refusal> {
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
        governing: &StepSpec,
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

/// A slot is given by kind and role, with an optional qualifier, or by a relation to
/// another slot, or by neither; never by both.
fn check_slot_shape(
    type_id: &str,
    slot: &SlotInput,
    slots: &BTreeMap<&str, &SlotInput>,
) -> Result<(), Refusal> {
    for (what, term) in [
        ("slot kind", &slot.kind),
        ("slot role", &slot.role),
        ("slot qualifier", &slot.qualifier),
    ] {
        if let Some(term) = term {
            check_name(what, term)?;
        }
    }
    let shape = |message: String| Err(Refusal::new("slot_shape", message));
    if slot.role.is_some() != slot.kind.is_some() {
        return shape(format!(
            "{type_id}: slot {} gives a kind or a role alone; give both",
            slot.name
        ));
    }
    if slot.qualifier.is_some() && slot.role.is_none() {
        return shape(format!(
            "{type_id}: slot {} gives a qualifier without a kind and a role",
            slot.name
        ));
    }
    let Some(relation) = &slot.defined_by else {
        return Ok(());
    };
    if slot.role.is_some() {
        return shape(format!(
            "{type_id}: slot {} is given both by a role and by a relation; give one",
            slot.name
        ));
    }
    check_name("slot relation", &relation.relation)?;
    if relation.slot == slot.name {
        return shape(format!(
            "{type_id}: slot {} is defined by a relation to itself",
            slot.name
        ));
    }
    if !slots.contains_key(relation.slot.as_str()) {
        return Err(Refusal::new(
            "unknown_slot",
            format!(
                "{type_id}: slot {} is defined by a relation to slot {}, which the type does not declare",
                slot.name, relation.slot
            ),
        ));
    }
    Ok(())
}

/// A qualifier as compared: lower case, with runs of space folded to one.
pub(crate) fn fold(text: &str) -> String {
    text.split_whitespace()
        .map(str::to_lowercase)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Visit each field name a document gives: the field of every requirement and product of its
/// step types and of the steps of its operation types, and the `field` and `as` of every
/// supply. `visit` is told whether the name is a supply's.
fn each_field_name(document: &mut Value, visit: &mut dyn FnMut(&mut Value, bool)) {
    let mut visit_step = |step: &mut Value| {
        for (list, keys) in [
            ("requires", &["field"][..]),
            ("produces", &["field"][..]),
            ("supplies", &["field", "as"][..]),
        ] {
            let entries = step
                .get_mut(list)
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten();
            for entry in entries {
                for key in keys {
                    if let Some(name) = entry.get_mut(*key) {
                        visit(name, list == "supplies");
                    }
                }
            }
        }
    };
    let step_types = document
        .get_mut("step_types")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten();
    for step_type in step_types {
        visit_step(step_type);
    }
    let types = document
        .get_mut("operation_types")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten();
    for type_ in types {
        let steps = type_
            .get_mut("steps")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten();
        for step in steps {
            visit_step(step);
        }
    }
}

/// The field names a document's requirements and products give, as written.
pub(crate) fn named_fields(document: &Value) -> BTreeSet<String> {
    let mut document = document.clone();
    let mut named = BTreeSet::new();
    each_field_name(&mut document, &mut |name, supply| {
        if let (false, Some(name)) = (supply, name.as_str()) {
            named.insert(name.to_owned());
        }
    });
    named
}

/// Write each field name of a document, a supply's included, as the canonical name in
/// `canonical` (keyed by folded name) that it folds to, so that a requirement, a product and a
/// supply naming one field in another case or spacing compare equal and the stored bytes carry
/// the canonical name. A name that folds to none is left as written.
pub(crate) fn write_canonical_field_names(
    document: &mut Value,
    canonical: &BTreeMap<String, String>,
) {
    each_field_name(document, &mut |name, _| {
        if let Some(canonical) = name
            .as_str()
            .and_then(|written| canonical.get(&fold(written)))
        {
            *name = Value::String(canonical.clone());
        }
    });
}

/// The first slot, in name order, whose chain of `defined_by` relations returns to it.
fn slot_relation_cycle(slots: &BTreeMap<&str, &SlotInput>) -> Option<String> {
    for start in slots.keys() {
        let mut seen = BTreeSet::new();
        let mut at = *start;
        while let Some(next) = slots
            .get(at)
            .and_then(|slot| slot.defined_by.as_ref())
            .map(|relation| relation.slot.as_str())
        {
            if next == *start {
                return Some((*start).to_owned());
            }
            if !seen.insert(next) {
                break;
            }
            at = next;
        }
    }
    None
}

fn compile_type(source: &Value, available: &Available) -> Result<Compiled, Refusal> {
    let mut input: TypeInput = serde_json::from_value(source.clone())
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
    let specs = std::mem::take(&mut input.steps)
        .into_iter()
        .map(|step| step_spec(&input.id, step, available))
        .collect::<Result<Vec<_>, _>>()?;
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
    let mut by_role: BTreeMap<(&str, &str, String), &str> = BTreeMap::new();
    for slot in &input.slots {
        check_slot_shape(&input.id, slot, &scope.slots)?;
        if let (Some(kind), Some(role)) = (&slot.kind, &slot.role) {
            let qualifier = slot.qualifier.as_deref().map(fold).unwrap_or_default();
            if let Some(first) = by_role.insert(
                (kind.as_str(), role.as_str(), qualifier),
                slot.name.as_str(),
            ) {
                return Err(Refusal::new(
                    "slot_shape",
                    format!(
                        "{}: slots {first} and {} give the same kind, role and qualifier; a party that appears more than once is one slot that repeats",
                        input.id, slot.name
                    ),
                ));
            }
        }
    }
    if let Some(slot) = slot_relation_cycle(&scope.slots) {
        return Err(Refusal::new(
            "slot_cycle",
            format!(
                "{}: slot {slot} is defined by a relation that leads back to itself",
                input.id
            ),
        ));
    }
    for step in &specs {
        check_name("step", &step.name)?;
        if scope.steps.insert(step.name.as_str(), step).is_some() {
            return Err(Refusal::new(
                "duplicate_step",
                format!("{}: step {} is declared twice", input.id, step.name),
            ));
        }
    }
    for step in &specs {
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
        if step.pinned.is_some() && slot.repeats && step.quantifier.is_none() {
            return Err(Refusal::new(
                "quantifier_missing",
                format!(
                    "{}: step {} is asked of repeating slot {}, but its step type gives no quantifier",
                    input.id, step.name, step.by
                ),
            ));
        }
        check_values(&input.id, &step.name, &step.requires, &step.produces)?;
        for about in step
            .requires
            .iter()
            .map(|need| &need.slot)
            .chain(step.produces.iter().map(|made| &made.slot))
            .flatten()
        {
            if !scope.slots.contains_key(about.as_str()) {
                return Err(Refusal::new(
                    "unknown_slot",
                    format!(
                        "{}: step {} requires or produces a value about slot {about}, which the type does not declare",
                        input.id, step.name
                    ),
                ));
            }
        }
        for supply in &step.supplies {
            check_supply(&input.id, step, supply, &scope)?;
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
    let outcomes: Vec<&str> = specs
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
    for step in &specs {
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

    let order: Vec<&str> = specs.iter().map(|step| step.name.as_str()).collect();
    let mut after_edges: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for step in &specs {
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
    // A step that requires a value another step produces comes after it, as does a step
    // supplied from another; `after` stays for steps between which nothing flows.
    let mut derived: BTreeMap<&str, BTreeSet<&str>> = BTreeMap::new();
    for step in &specs {
        let entry = derived.entry(step.name.as_str()).or_default();
        for need in &step.requires {
            for producer in &specs {
                if producer.name != step.name
                    && producer
                        .produces
                        .iter()
                        .any(|made| made.field == need.field && made.slot == need.slot)
                {
                    entry.insert(producer.name.as_str());
                }
            }
        }
        entry.extend(step.supplies.iter().map(|supply| supply.from.as_str()));
        for after in &step.after {
            entry.remove(after.as_str());
        }
    }
    // Cycles are refused in a fixed order (ADR-222 A2.3): through `after` alone, then through
    // `after` and rules, then through every edge with the derived ones added.
    let mut ruled = after_edges.clone();
    for rule in &rules {
        let entry = ruled.entry(rule.step.as_str()).or_default();
        entry.extend(step_terms(&rule.when));
        entry.extend(step_terms(&rule.unless));
    }
    if let Some(cycle) = find_cycle(&order, &ruled) {
        return Err(Refusal::new(
            "rule_cycle",
            format!(
                "{}: a step's rules depend on its own completion through: {}",
                input.id,
                cycle.join(" -> ")
            ),
        ));
    }
    let mut all_edges = ruled;
    for (step, needs) in &derived {
        all_edges
            .entry(*step)
            .or_default()
            .extend(needs.iter().copied());
    }
    if let Some(cycle) = find_cycle(&order, &all_edges) {
        return Err(Refusal::new(
            "derived_cycle",
            format!(
                "{}: steps wait on each other's values in a cycle: {}",
                input.id,
                cycle.join(" -> ")
            ),
        ));
    }

    let mut step_types = Vec::with_capacity(specs.len());
    let mut step_refs = Vec::with_capacity(specs.len());
    for step in &specs {
        let pinned = match &step.pinned {
            Some(pinned) => pinned.clone(),
            None => {
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
                    requires: step.requires.clone(),
                    produces: step.produces.clone(),
                };
                let value = serde_json::to_value(&step_type)
                    .map_err(|e| Refusal::new("invalid_document", e.to_string()))?;
                let stored = store("step_type", &step_type.id, input.version, &value, &value);
                let pinned = DefinitionRef {
                    id: stored.id.clone(),
                    version: stored.version,
                    digest: stored.digest.clone(),
                };
                step_types.push(stored);
                pinned
            }
        };
        step_refs.push(StepRef {
            name: step.name.clone(),
            step_type: pinned,
            after: step.after.clone(),
            outcome: step.outcome,
            supplies: step.supplies.clone(),
            derived_after: derived
                .get(step.name.as_str())
                .map(|needs| needs.iter().map(|need| (*need).to_owned()).collect())
                .unwrap_or_default(),
        });
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
                kind: slot.kind.clone(),
                role: slot.role.clone(),
                qualifier: slot.qualifier.clone(),
                defined_by: slot.defined_by.clone(),
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
        compile_document(&json!({"operation_types": [document]}), &Available::new())
            .map(|mut all| all.operation_types.remove(0))
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

    fn sign_v1() -> Value {
        json!({"id": "sign", "version": 1, "by": "applicant", "quantifier": "all_of"})
    }

    /// A type over the base slots whose first step pins `pin` and whose second is the outcome.
    fn pinning(id: &str, pin: Value) -> Value {
        json!({
            "id": id,
            "version": 1,
            "slots": [{"name": "applicant", "repeats": true}, {"name": "office"}],
            "steps": [
                {"name": "sign", "step_type": pin},
                {"name": "issue", "by": "office", "after": ["sign"], "outcome": true}
            ]
        })
    }

    fn document(step_types: Value, operation_types: Value) -> Result<Document, Refusal> {
        compile_document(
            &json!({"step_types": step_types, "operation_types": operation_types}),
            &Available::new(),
        )
    }

    #[test]
    fn a_step_type_given_alone_is_pinned_by_two_types_and_stored_once() {
        let pin = json!({"id": "sign", "version": 1});
        let compiled = document(
            json!([sign_v1()]),
            json!([pinning("filing", pin.clone()), pinning("renewal", pin)]),
        )
        .expect("compiles");
        assert_eq!(compiled.step_types.len(), 1);
        let shared = &compiled.step_types[0];
        assert_eq!((shared.id.as_str(), shared.version), ("sign", 1));
        for type_ in &compiled.operation_types {
            let sign = &type_.definition.steps[0];
            assert_eq!(sign.step_type.id, "sign");
            assert_eq!(sign.step_type.digest, shared.digest);
            // Only the inline outcome step is compiled into the type's own step types.
            let own: Vec<&str> = type_.step_types.iter().map(|s| s.id.as_str()).collect();
            assert_eq!(own, vec![format!("{}/issue", type_.definition.id)]);
        }
        let alone = document(json!([sign_v1()]), json!([])).expect("a step type alone");
        assert_eq!(alone.step_types[0].digest, shared.digest);
        assert!(alone.operation_types.is_empty());
    }

    #[test]
    fn a_published_step_type_is_pinned_without_being_given_again() {
        let (stored, step_type) = compile_step_type(&sign_v1()).expect("step type");
        let mut published = Available::new();
        published.insert(("sign".to_owned(), 1), (step_type, stored.digest.clone()));
        let pin = json!({"id": "sign", "version": 1});
        let compiled = compile_document(
            &json!({"operation_types": [pinning("filing", pin)]}),
            &published,
        )
        .expect("compiles against the published step type");
        assert!(compiled.step_types.is_empty());
        assert_eq!(
            compiled.operation_types[0].definition.steps[0]
                .step_type
                .digest,
            stored.digest
        );
        // The same definition given again is accepted; it is the store that writes nothing.
        compile_document(&json!({"step_types": [sign_v1()]}), &published)
            .expect("the same definition under the same version");
        let mut other = sign_v1();
        other["quantifier"] = json!("at_least_one");
        let refusal = compile_document(&json!({"step_types": [other]}), &published)
            .expect_err("a different definition under a held version");
        assert_eq!(refusal.code, "version_conflict", "{refusal}");
    }

    #[test]
    fn the_step_type_checks_refuse_by_code() {
        let pin = json!({"id": "sign", "version": 1});
        let mut with_by = pinning("t", pin.clone());
        with_by["steps"][0]["by"] = json!("applicant");
        let mut with_requires = pinning("t", pin.clone());
        with_requires["steps"][0]["requires"] = json!([{"field": "f", "value_type": "text"}]);
        let witness = json!({"id": "witness", "version": 1, "by": "witness"});
        let about_nobody = json!({"id": "sign", "version": 1, "by": "applicant", "quantifier": "all_of",
            "requires": [{"field": "f", "slot": "nobody", "value_type": "text"}]});
        let cases: Vec<(&str, Value, Value)> = vec![
            (
                "unknown_step_type",
                json!([]),
                json!([pinning("t", json!({"id": "sign", "version": 2}))]),
            ),
            ("pinned_step_shape", json!([sign_v1()]), json!([with_by])),
            (
                "pinned_step_shape",
                json!([sign_v1()]),
                json!([with_requires]),
            ),
            (
                "quantifier_missing",
                json!([{"id": "sign", "version": 1, "by": "applicant"}]),
                json!([pinning("t", pin.clone())]),
            ),
            ("duplicate_type", json!([sign_v1(), sign_v1()]), json!([])),
            (
                "unknown_slot",
                json!([witness]),
                json!([pinning("t", json!({"id": "witness", "version": 1}))]),
            ),
            (
                "unknown_slot",
                json!([about_nobody]),
                json!([pinning("t", pin)]),
            ),
            (
                "invalid_version",
                json!([{"id": "sign", "version": 0, "by": "applicant"}]),
                json!([]),
            ),
            (
                "invalid_document",
                json!([{"id": "sign", "version": 1, "by": "applicant", "after": []}]),
                json!([]),
            ),
        ];
        for (code, step_types, operation_types) in cases {
            let refusal = document(step_types, operation_types).expect_err(code);
            assert_eq!(refusal.code, code, "{refusal}");
        }
        let refusal = compile_document(&json!({}), &Available::new()).expect_err("empty");
        assert_eq!(refusal.code, "invalid_document", "{refusal}");
    }

    /// Steps a and b over one slot, b the outcome, with the given fields merged into each.
    fn flowing(a: Value, b: Value) -> Value {
        let mut steps =
            json!([{"name": "a", "by": "x"}, {"name": "b", "by": "x", "outcome": true}]);
        for (step, extra) in [(0, a), (1, b)] {
            if let Value::Object(extra) = extra {
                steps[step].as_object_mut().expect("step").extend(extra);
            }
        }
        json!({"id": "t", "version": 1, "slots": [{"name": "x", "repeats": true}, {"name": "y"}], "steps": steps})
    }

    #[test]
    fn the_derived_order_checks_refuse_by_code() {
        let needs = |field: &str| json!([{"field": field, "value_type": "text"}]);
        let makes = |field: &str| json!([{"field": field, "class": "stated"}]);
        let cases: Vec<(&str, Value, &[&str])> = vec![
            (
                "derived_cycle",
                flowing(
                    json!({"requires": needs("p"), "produces": makes("q")}),
                    json!({"requires": needs("q"), "produces": makes("p")}),
                ),
                &["a -> b", "b -> a"],
            ),
            (
                "derived_cycle",
                flowing(
                    json!({"requires": needs("p"), "supplies": [{"from": "a", "field": "p", "as": "p"}], "produces": makes("p")}),
                    json!({}),
                ),
                &["a -> a"],
            ),
            (
                "unknown_step",
                flowing(
                    json!({}),
                    json!({"requires": needs("p"), "supplies": [{"from": "ghost", "field": "p", "as": "p"}]}),
                ),
                &["ghost"],
            ),
            (
                "unknown_supply",
                flowing(
                    json!({"produces": makes("p")}),
                    json!({"requires": needs("p"), "supplies": [{"from": "a", "field": "p", "as": "other"}]}),
                ),
                &["does not require"],
            ),
            (
                "unknown_supply",
                flowing(
                    json!({"produces": makes("p")}),
                    json!({"requires": needs("r"), "supplies": [{"from": "a", "field": "q", "as": "r"}]}),
                ),
                &["does not produce"],
            ),
            (
                "supply_slot_mismatch",
                flowing(
                    json!({"produces": [{"field": "p", "slot": "y", "class": "read"}]}),
                    json!({"requires": [{"field": "r", "slot": "x", "value_type": "text"}],
                        "supplies": [{"from": "a", "field": "p", "as": "r"}]}),
                ),
                &["about x"],
            ),
            (
                "after_cycle",
                flowing(
                    json!({"after": ["b"], "requires": needs("p"), "produces": makes("q")}),
                    json!({"after": ["a"], "requires": needs("q"), "produces": makes("p")}),
                ),
                &[],
            ),
            (
                "rule_cycle",
                flowing(
                    json!({"unless": [[{"step": {"name": "b", "done": true}}]], "requires": needs("p")}),
                    json!({"after": ["a"], "produces": makes("p")}),
                ),
                &[],
            ),
        ];
        for (code, document, named) in cases {
            let refusal = one(document).expect_err(code);
            assert_eq!(refusal.code, code, "{refusal}");
            for part in named {
                assert!(refusal.message.contains(part), "{part} in {refusal}");
            }
        }
    }

    #[test]
    fn a_step_comes_after_what_it_needs_and_the_order_is_in_the_bytes() {
        let produces = json!([{"field": "p", "class": "computed"}]);
        let requires = json!([{"field": "p", "value_type": "text"}]);
        let derived = one(flowing(
            json!({"produces": produces.clone()}),
            json!({"requires": requires.clone()}),
        ))
        .expect("compiles");
        assert_eq!(derived.definition.steps[1].derived_after, vec!["a"]);
        assert!(derived.definition.steps[0].derived_after.is_empty());
        let stored: Value =
            serde_json::from_slice(&derived.operation_type.bytes).expect("stored bytes");
        assert_eq!(stored["steps"][1]["derived_after"], json!(["a"]));

        let supplied = one(flowing(
            json!({"produces": produces.clone()}),
            json!({"requires": json!([{"field": "r", "value_type": "text"}]),
                "supplies": [{"from": "a", "field": "p", "as": "r"}]}),
        ))
        .expect("compiles");
        assert_eq!(supplied.definition.steps[1].derived_after, vec!["a"]);

        // An `after` already naming the producer leaves nothing to derive.
        let ordered = one(flowing(
            json!({"produces": produces}),
            json!({"after": ["a"], "requires": requires}),
        ))
        .expect("compiles");
        assert!(ordered.definition.steps[1].derived_after.is_empty());
        let stored: Value =
            serde_json::from_slice(&ordered.operation_type.bytes).expect("stored bytes");
        assert!(stored["steps"][1].get("derived_after").is_none());

        // A step that requires one field about two slots is supplied the requirement whose
        // slot the producer makes the field about.
        let two_slots = json!([{"field": "r", "slot": "x", "value_type": "text"},
            {"field": "r", "slot": "y", "value_type": "text"}]);
        let supplied = one(flowing(
            json!({"produces": [{"field": "p", "slot": "y", "class": "read"}]}),
            json!({"requires": two_slots.clone(), "supplies": [{"from": "a", "field": "p", "as": "r"}]}),
        ))
        .expect("compiles");
        assert_eq!(supplied.definition.steps[1].derived_after, vec!["a"]);
        let refusal = one(flowing(
            json!({"produces": [{"field": "p", "class": "read"}]}),
            json!({"requires": two_slots, "supplies": [{"from": "a", "field": "p", "as": "r"}]}),
        ))
        .expect_err("no requirement about the operation");
        assert_eq!(refusal.code, "supply_slot_mismatch", "{refusal}");
        assert!(refusal.message.contains("about x or y"), "{refusal}");
    }

    #[test]
    fn field_names_are_written_canonically_before_the_type_is_stored() {
        let mut document = json!({"operation_types": [flowing(
            json!({"produces": [{"field": "Case  Number", "class": "stated"}]}),
            json!({"requires": [{"field": "CASE number", "value_type": "text"}]}),
        )]});
        assert_eq!(
            named_fields(&document),
            BTreeSet::from(["CASE number".to_owned(), "Case  Number".to_owned()])
        );
        let compiled = |document: &Value| {
            compile_document(document, &Available::new())
                .expect("compiles")
                .operation_types
                .into_iter()
                .next()
                .expect("one type")
        };
        // As written, the two names compare unequal and nothing is derived.
        assert!(compiled(&document).definition.steps[1]
            .derived_after
            .is_empty());

        let canonical = BTreeMap::from([("case number".to_owned(), "case number".to_owned())]);
        write_canonical_field_names(&mut document, &canonical);
        let type_ = compiled(&document);
        assert_eq!(type_.definition.steps[1].derived_after, vec!["a"]);
        let stored: Value = serde_json::from_slice(&type_.operation_type.bytes).expect("bytes");
        assert_eq!(stored["steps"][1]["derived_after"], json!(["a"]));
        let steps: Vec<Value> = type_
            .step_types
            .iter()
            .map(|step| serde_json::from_slice(&step.bytes).expect("step bytes"))
            .collect();
        assert_eq!(steps[0]["produces"][0]["field"], "case number");
        assert_eq!(steps[1]["requires"][0]["field"], "case number");
        assert!(
            !type_.operation_type.source.contains("CASE"),
            "{}",
            type_.operation_type.source
        );
    }

    fn ordering(order: Value) -> Result<Document, Refusal> {
        compile_document(&json!({"source_order": order}), &Available::new())
    }

    #[test]
    fn a_source_order_given_alone_is_stored_in_its_order() {
        let compiled =
            ordering(json!({"version": 2, "places": ["W-2", "1099-INT"]})).expect("compiles");
        assert!(compiled.step_types.is_empty() && compiled.operation_types.is_empty());
        let (stored, order) = compiled.source_order.expect("a source order");
        assert_eq!((stored.kind, stored.version), ("source_order", 2));
        assert_eq!(order.places, vec!["W-2", "1099-INT"]);
        let bytes: Value = serde_json::from_slice(&stored.bytes).expect("stored bytes");
        assert_eq!(bytes, json!({"version": 2, "places": ["W-2", "1099-INT"]}));
        // The order is the deployment's: the same places in another order are another digest.
        let (swapped, _) = ordering(json!({"version": 2, "places": ["1099-INT", "W-2"]}))
            .expect("compiles")
            .source_order
            .expect("a source order");
        assert_ne!(swapped.digest, stored.digest);
    }

    #[test]
    fn the_source_order_checks_refuse_by_code() {
        let cases = [
            ("invalid_version", json!({"version": 0, "places": ["W-2"]})),
            ("invalid_document", json!({"version": 1, "places": []})),
            ("invalid_document", json!({"version": 1})),
            (
                "invalid_document",
                json!({"version": 1, "places": ["W-2"], "id": "x"}),
            ),
            ("invalid_name", json!({"version": 1, "places": [" W-2"]})),
            (
                "duplicate_place",
                json!({"version": 1, "places": ["W-2", "1099", "W-2"]}),
            ),
            (
                "duplicate_place",
                json!({"version": 1, "places": ["W-2", "w-2"]}),
            ),
        ];
        for (code, order) in cases {
            let refusal = ordering(order.clone()).expect_err(code);
            assert_eq!(refusal.code, code, "{order}: {refusal}");
        }
        let refusal = compile_document(&json!({}), &Available::new()).expect_err("empty");
        assert_eq!(refusal.code, "invalid_document");
    }
}
