//! Configurable ADR-102 classification over an already decoded change-set.

use std::collections::HashSet;

use khive_changeset::{ChangeSet, EntityPatch, Op, UpdatePatch};

use crate::config::TierConfig;
use crate::Rules;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Tier1,
    Tier2,
}

impl Tier {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Tier1 => "tier_1",
            Self::Tier2 => "tier_2",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct TierFinding<'a> {
    pub severity: &'a str,
    pub subject_id: Option<&'a str>,
}

#[derive(Debug, PartialEq, Eq)]
pub struct TierDecision {
    pub tier: Tier,
    pub reason: String,
    pub reason_ids: Vec<String>,
}

impl TierDecision {
    fn seed(tier: Tier, reason_id: &str, reason: &str) -> Self {
        Self {
            tier,
            reason: reason.into(),
            reason_ids: vec![reason_id.into()],
        }
    }

    fn escalate(&mut self, reason_id: &str, reason: &str) {
        self.tier = Tier::Tier2;
        self.reason_ids.push(reason_id.into());
        if !self.reason.is_empty() {
            self.reason.push(' ');
        }
        self.reason.push_str(reason);
    }
}

/// One result per operation, in semantic order. Unscoped or unmapped errors
/// escalate the entire change-set rather than disappearing from classification.
pub fn classify_changeset(
    rules: &Rules,
    changeset: &ChangeSet,
    findings: &[TierFinding<'_>],
) -> Vec<TierDecision> {
    let subjects: Vec<Vec<String>> = changeset.ops.iter().map(operation_subjects).collect();
    let known: HashSet<&str> = subjects.iter().flatten().map(String::as_str).collect();
    let errors: HashSet<&str> = findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .filter_map(|finding| finding.subject_id)
        .collect();
    let global_error = findings.iter().any(|finding| {
        finding.severity == "error" && finding.subject_id.is_none_or(|id| !known.contains(id))
    });
    changeset
        .ops
        .iter()
        .zip(subjects.iter())
        .map(|(op, subjects)| {
            let mut decision = seed_decision(op);
            configure_decision(&mut decision, op, &rules.0.tier);
            if global_error || subjects.iter().any(|id| errors.contains(id.as_str())) {
                decision.escalate(
                    "validation.error_severity_finding",
                    "An error-severity validation finding escalates this operation to Tier 2.",
                );
            }
            decision
        })
        .collect()
}

fn operation_subjects(op: &Op) -> Vec<String> {
    match op {
        Op::Create(create) => vec![create.id.to_string()],
        Op::Link(link) => vec![
            link.id.to_string(),
            link.source.to_string(),
            link.target.to_string(),
        ],
        Op::Update(update) => vec![update.target_id().to_string()],
        Op::Delete(delete) => vec![delete.target_id.to_string()],
        Op::Restore(restore) => vec![restore.target_id().to_string()],
        Op::Merge(merge) => vec![merge.into_id.to_string(), merge.from_id.to_string()],
    }
}

fn seed_decision(op: &Op) -> TierDecision {
    match op {
        Op::Create(_) => TierDecision::seed(
            Tier::Tier1,
            "adr102.create",
            "Create is additive and Tier 1-eligible under the ADR-102 floor.",
        ),
        Op::Link(link) => {
            let mut decision = TierDecision {
                tier: Tier::Tier1,
                reason: String::new(),
                reason_ids: Vec::new(),
            };
            if matches!(
                link.relation.as_str(),
                "supersedes" | "supports" | "refutes"
            ) {
                decision.escalate(
                    "adr102.judgment_relation",
                    &format!(
                        "{} is judgment-bearing and requires Tier 2 review.",
                        link.relation
                    ),
                );
            }
            if link.weight < 0.7 {
                decision.escalate(
                    "adr102.resulting_weight_below_0_7",
                    &format!(
                        "The resulting weight {} is below the ADR-102 floor of 0.7.",
                        link.weight
                    ),
                );
            }
            if decision.reason_ids.is_empty() {
                decision = TierDecision::seed(
                    Tier::Tier1,
                    "adr102.non_judgment_link",
                    "This non-judgment link has weight at least 0.7 and is Tier 1-eligible.",
                );
            }
            decision
        }
        Op::Update(update) => match update.patch() {
            UpdatePatch::Entity(_) => TierDecision::seed(
                Tier::Tier1,
                "adr102.mutable_entity_update",
                "A mutable entity-field update is Tier 1-eligible under the ADR-102 floor.",
            ),
            UpdatePatch::Note(_) => TierDecision::seed(
                Tier::Tier2,
                "adr102.not_tier_1_eligible",
                "Note updates are outside ADR-102's enumerated Tier 1 fast path.",
            ),
            UpdatePatch::Edge(patch) => {
                let mut decision = TierDecision::seed(
                    Tier::Tier2,
                    "adr102.existing_edge_update",
                    "Changing an existing edge relation or weight requires Tier 2 review.",
                );
                if patch.weight.is_some_and(|weight| weight < 0.7) {
                    decision.escalate(
                        "adr102.resulting_weight_below_0_7",
                        "The resulting weight is below the ADR-102 floor of 0.7.",
                    );
                }
                decision
            }
        },
        Op::Delete(_) => TierDecision::seed(
            Tier::Tier2,
            "adr102.delete",
            "Every delete requires Tier 2 review under the ADR-102 floor.",
        ),
        Op::Restore(_) => TierDecision::seed(
            Tier::Tier2,
            "adr102.restore",
            "Restoration requires independently reviewed compensation under ADR-102 D4.",
        ),
        Op::Merge(_) => TierDecision::seed(
            Tier::Tier2,
            "adr102.merge",
            "Every merge requires Tier 2 review under the ADR-102 floor.",
        ),
    }
}

fn configure_decision(decision: &mut TierDecision, op: &Op, config: &TierConfig) {
    if decision.tier == Tier::Tier2 {
        return;
    }
    let operation = match op {
        Op::Create(_) => "create",
        Op::Link(_) => "link",
        Op::Update(_) => "update",
        Op::Delete(_) => "delete",
        Op::Restore(_) => "restore",
        Op::Merge(_) => "merge",
    };
    if !config
        .tier_1_operations
        .iter()
        .any(|value| value == operation)
    {
        decision.escalate(
            "config.operation_not_tier_1",
            &format!("Configured tier_1_operations does not admit {operation}."),
        );
    }
    match op {
        Op::Link(link) => {
            if config.tier_1_relations.as_ref().is_some_and(|relations| {
                !relations
                    .iter()
                    .any(|value| value == link.relation.as_str())
            }) {
                decision.escalate(
                    "config.relation_not_tier_1",
                    &format!(
                        "Configured tier_1_relations does not admit {}.",
                        link.relation
                    ),
                );
            }
            // The fixed 0.7 reason already explains a below-floor weight.
            if config.minimum_weight > 0.7 && link.weight < config.minimum_weight {
                decision.escalate(
                    "config.resulting_weight_below_threshold",
                    &format!(
                        "The resulting weight {} is below the configured minimum {}.",
                        link.weight, config.minimum_weight
                    ),
                );
            }
        }
        Op::Update(update) => {
            if let (UpdatePatch::Entity(patch), Some(allowed)) =
                (update.patch(), config.tier_1_entity_fields.as_ref())
            {
                let excluded: Vec<&str> = entity_fields(patch)
                    .into_iter()
                    .filter(|field| !allowed.iter().any(|value| value.as_str() == *field))
                    .collect();
                if !excluded.is_empty() {
                    decision.escalate(
                        "config.entity_field_not_tier_1",
                        &format!(
                            "Configured tier_1_entity_fields does not admit {}.",
                            excluded.join(", ")
                        ),
                    );
                }
            }
        }
        Op::Create(_) | Op::Delete(_) | Op::Restore(_) | Op::Merge(_) => {}
    }
}

fn entity_fields(patch: &EntityPatch) -> Vec<&'static str> {
    // Exhaustive, so a new patch field cannot bypass a configured field list.
    let EntityPatch {
        name,
        description,
        properties,
        tags,
        entity_type,
    } = patch;
    [
        ("name", name.is_some()),
        ("description", description.is_some()),
        ("properties", properties.is_some()),
        ("tags", tags.is_some()),
        ("entity_type", entity_type.is_some()),
    ]
    .into_iter()
    .filter_map(|(field, touched)| touched.then_some(field))
    .collect()
}
