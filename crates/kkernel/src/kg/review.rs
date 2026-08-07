//! `kkernel kg review` — read-only ADR-101 change-set review (ADR-145 D8).
//!
//! The command deliberately has no repository or database input. It parses a
//! strict change-set, reuses `kg commit`'s partial-view validation pass, applies
//! the non-overridable ADR-102 tier floor, and emits the minimal
//! `khive.review.v1` core. Git/GitHub/snapshot metadata is absent because this
//! command cannot establish those identities and ADR-145 forbids inventing
//! optional enrichment.

use std::collections::HashSet;

use anyhow::{bail, Context, Result};
use khive_changeset::{ChangeSet, CreateTarget, DeletePreimage, Op, UpdatePatch};

use super::commit;
use super::types::{
    OutputFormat, ReviewArgs, ReviewCapability, ReviewChangeSet, ReviewFinding, ReviewGate,
    ReviewOperation, ReviewReport, ReviewTierSummary, ReviewValidationSummary, RuleResult,
};

const REVIEW_SCHEMA_VERSION: &str = "khive.review.v1";
const TIER_1: &str = "tier_1";
const TIER_2: &str = "tier_2";

pub(super) fn cmd_review(args: ReviewArgs) -> Result<()> {
    let changeset_text = std::fs::read_to_string(&args.changeset)
        .with_context(|| format!("reading change-set {}", args.changeset.display()))?;
    let changeset = khive_changeset::from_ndjson(&changeset_text).with_context(|| {
        format!(
            "parsing change-set {} as strict ADR-101 NDJSON-delta",
            args.changeset.display()
        )
    })?;

    if !args.rules.exists() {
        bail!(
            "rules file not found: {} — `kg review` requires an explicit TOML rules file",
            args.rules.display()
        );
    }

    let rule_results = commit::run_commit_time_rules(&changeset, &args.rules)
        .context("running read-only commit-time rule pass")?;
    let report = build_report(
        &changeset,
        &rule_results,
        args.reviewer_model_family.as_deref(),
    );

    print_report(&args.format, &report);

    // The report is always emitted first so CI and editor adapters can consume
    // the exact gate state. A failed rule pass or unmet mandatory Tier-2 gate
    // then produces the conventional non-zero process result without exposing
    // any mutation path.
    if !report.review_gate.approval_ready {
        std::process::exit(1);
    }

    Ok(())
}

fn build_report(
    changeset: &ChangeSet,
    rule_results: &[RuleResult],
    reviewer_model_family: Option<&str>,
) -> ReviewReport {
    let mut findings = flatten_findings(rule_results);
    let coverage_findings = partial_view_coverage_findings(changeset);
    let has_coverage_failure = !coverage_findings.is_empty();
    findings.extend(coverage_findings);
    findings.sort_by(|a, b| {
        severity_rank(&a.severity)
            .cmp(&severity_rank(&b.severity))
            .then_with(|| a.rule_id.cmp(&b.rule_id))
            .then_with(|| a.subject_id.cmp(&b.subject_id))
            .then_with(|| a.message.cmp(&b.message))
    });

    let failed_rules = rule_results.iter().filter(|result| !result.passed).count()
        + usize::from(has_coverage_failure);
    let failed_error_rules = rule_results
        .iter()
        .filter(|result| !result.passed && result.severity == "error")
        .count()
        + usize::from(has_coverage_failure);
    let errors = findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .count();
    let warnings = findings
        .iter()
        .filter(|finding| finding.severity == "warning")
        .count();
    let info = findings
        .iter()
        .filter(|finding| finding.severity == "info")
        .count();
    let validation_passed = failed_error_rules == 0;

    let mut operations: Vec<ReviewOperation> = changeset
        .ops
        .iter()
        .enumerate()
        .map(|(index, op)| summarize_operation(index, op))
        .collect();

    // Findings produced by the existing rule projection identify a subject
    // when they can. An unscoped (or unexpectedly unmapped) error is applied
    // fail-closed to every operation because silently keeping an operation on
    // Tier 1 would violate ADR-102's "any operation carrying an error" floor.
    let known_subjects: HashSet<&str> = operations
        .iter()
        .flat_map(|op| {
            std::iter::once(op.id.as_str()).chain(op.entity_ids.iter().map(String::as_str))
        })
        .collect();
    let error_subjects: HashSet<&str> = findings
        .iter()
        .filter(|finding| finding.severity == "error")
        .filter_map(|finding| finding.subject_id.as_deref())
        .collect();
    let has_global_error = findings.iter().any(|finding| {
        finding.severity == "error"
            && finding
                .subject_id
                .as_deref()
                .is_none_or(|subject| !known_subjects.contains(subject))
    });

    for operation in &mut operations {
        let carries_error = has_global_error
            || error_subjects.contains(operation.id.as_str())
            || operation
                .entity_ids
                .iter()
                .any(|id| error_subjects.contains(id.as_str()));
        if carries_error {
            operation.tier = TIER_2.to_string();
            operation
                .tier_reasons
                .push("validation.error_severity_finding".to_string());
            operation.reason.push_str(
                " An error-severity validation finding escalates this operation to Tier 2.",
            );
        }
    }

    let tier_1 = operations
        .iter()
        .filter(|operation| operation.tier == TIER_1)
        .count();
    let tier_2 = operations.len() - tier_1;
    let requires_independent_review = tier_2 > 0;

    let validation = ReviewValidationSummary {
        scope: "commit_time_partial_view",
        rules_evaluated: rule_results.len() + usize::from(has_coverage_failure),
        failed_rules,
        errors,
        warnings,
        info,
        passed: validation_passed,
    };
    let review_gate = evaluate_review_gate(
        requires_independent_review,
        validation_passed,
        &changeset.envelope.producer_model_family,
        reviewer_model_family,
    );

    ReviewReport {
        schema_version: REVIEW_SCHEMA_VERSION,
        review_kind: "changeset",
        capability: ReviewCapability {
            source: "local_changeset",
            mutability: "read_only",
            no_writes: true,
            git_reads: false,
            khive_reads: false,
            github_writes: false,
            wasm: false,
            persistence: false,
            unavailable_actions: ["stage", "apply", "commit", "push", "publish"],
        },
        change_set: ReviewChangeSet {
            envelope: changeset.envelope.clone(),
            operations,
        },
        tier_summary: ReviewTierSummary {
            operations: changeset.ops.len(),
            tier_1,
            tier_2,
            highest_tier: if tier_2 > 0 { TIER_2 } else { TIER_1 }.to_string(),
            requires_independent_review,
            policy: "adr_102_floor",
        },
        validation,
        findings,
        review_gate,
    }
}

/// `kg commit`'s established rule projection intentionally omits destructive
/// operations and updates. Reusing it is safe only if the review report makes
/// that missing coverage a deterministic, blocking finding. This prevents an
/// entity update from being presented as ADR-102 approval-ready merely because
/// no rule ever saw its patch/preimage.
fn partial_view_coverage_findings(changeset: &ChangeSet) -> Vec<ReviewFinding> {
    changeset
        .ops
        .iter()
        .filter_map(|op| {
            let (op_name, subject_id, subject_kind) = match op {
                Op::Update(update) => {
                    let kind = match update.patch() {
                        UpdatePatch::Entity(_) => "entity",
                        UpdatePatch::Note(_) => "note",
                        UpdatePatch::Edge(_) => "edge",
                    };
                    ("update", update.target_id().to_string(), kind)
                }
                Op::Delete(delete) => {
                    let kind = match &delete.preimage {
                        DeletePreimage::Entity(_) => "entity",
                        DeletePreimage::Note(_) => "note",
                        DeletePreimage::Edge(_) => "edge",
                    };
                    ("delete", delete.target_id.to_string(), kind)
                }
                Op::Merge(merge) => ("merge", merge.into_id.to_string(), "entity"),
                Op::Create(_) | Op::Link(_) => return None,
            };

            Some(ReviewFinding {
                rule_id: "review-rule-coverage".to_string(),
                severity: "error".to_string(),
                message: format!(
                    "The reused commit-time partial-view rules do not evaluate {op_name} operations; this operation cannot be declared validation-complete."
                ),
                subject_id: Some(subject_id),
                subject_name: None,
                subject_kind: Some(subject_kind.to_string()),
                fixable: false,
            })
        })
        .collect()
}

fn flatten_findings(rule_results: &[RuleResult]) -> Vec<ReviewFinding> {
    let mut findings = Vec::new();
    for result in rule_results.iter().filter(|result| !result.passed) {
        if result.violations.is_empty() {
            findings.push(ReviewFinding {
                rule_id: result.id.clone(),
                severity: result.severity.to_string(),
                message: "Rule failed without violation detail".to_string(),
                subject_id: None,
                subject_name: None,
                subject_kind: None,
                fixable: false,
            });
            continue;
        }

        findings.extend(result.violations.iter().map(|violation| ReviewFinding {
            rule_id: violation.rule_id.clone(),
            severity: violation.severity.to_string(),
            message: violation.message.clone(),
            subject_id: violation.entity_id.clone(),
            subject_name: violation.entity_name.clone(),
            subject_kind: violation.entity_kind.clone(),
            fixable: violation.fixable,
        }));
    }
    findings
}

fn severity_rank(severity: &str) -> u8 {
    match severity {
        "error" => 0,
        "warning" => 1,
        "info" => 2,
        _ => 3,
    }
}

fn summarize_operation(index: usize, op: &Op) -> ReviewOperation {
    match op {
        Op::Create(create) => match &create.target {
            CreateTarget::Entity(fields) => ReviewOperation {
                index,
                id: create.id.to_string(),
                op: "create".to_string(),
                target: "entity".to_string(),
                tier: TIER_1.to_string(),
                summary: format!("Create {} entity “{}”", fields.entity_kind, fields.name),
                reason: "Create is additive and Tier 1-eligible under the ADR-102 floor."
                    .to_string(),
                tier_reasons: vec!["adr102.create".to_string()],
                entity_ids: vec![create.id.to_string()],
                before: None,
                after: Some(json_value(&create.target)),
            },
            CreateTarget::Note(fields) => ReviewOperation {
                index,
                id: create.id.to_string(),
                op: "create".to_string(),
                target: "note".to_string(),
                tier: TIER_1.to_string(),
                summary: format!("Create {} note {}", fields.note_kind, create.id),
                reason: "Create is additive and Tier 1-eligible under the ADR-102 floor."
                    .to_string(),
                tier_reasons: vec!["adr102.create".to_string()],
                entity_ids: Vec::new(),
                before: None,
                after: Some(json_value(&create.target)),
            },
        },
        Op::Link(link) => {
            let mut tier = TIER_1;
            let mut reasons = Vec::new();
            let mut explanations = Vec::new();
            if matches!(
                link.relation.as_str(),
                "supersedes" | "supports" | "refutes"
            ) {
                tier = TIER_2;
                reasons.push("adr102.judgment_relation".to_string());
                explanations.push(format!(
                    "{} is judgment-bearing and requires Tier 2 review.",
                    link.relation
                ));
            }
            if link.weight < 0.7 {
                tier = TIER_2;
                reasons.push("adr102.resulting_weight_below_0_7".to_string());
                explanations.push(format!(
                    "The resulting weight {} is below the ADR-102 floor of 0.7.",
                    link.weight
                ));
            }
            if reasons.is_empty() {
                reasons.push("adr102.non_judgment_link".to_string());
                explanations.push(
                    "This non-judgment link has weight at least 0.7 and is Tier 1-eligible."
                        .to_string(),
                );
            }
            ReviewOperation {
                index,
                id: link.id.to_string(),
                op: "link".to_string(),
                target: "edge".to_string(),
                tier: tier.to_string(),
                summary: format!("Link {} {} {}", link.source, link.relation, link.target),
                reason: explanations.join(" "),
                tier_reasons: reasons,
                entity_ids: vec![link.source.to_string(), link.target.to_string()],
                before: None,
                after: Some(serde_json::json!({
                    "relation": link.relation,
                    "weight": link.weight,
                    "properties": link.properties,
                })),
            }
        }
        Op::Update(update) => {
            let target_id = update.target_id().to_string();
            match update.patch() {
                UpdatePatch::Entity(_) => ReviewOperation {
                    index,
                    id: target_id.clone(),
                    op: "update".to_string(),
                    target: "entity".to_string(),
                    tier: TIER_1.to_string(),
                    summary: format!("Update entity {target_id}"),
                    reason:
                        "A mutable entity-field update is Tier 1-eligible under the ADR-102 floor."
                            .to_string(),
                    tier_reasons: vec!["adr102.mutable_entity_update".to_string()],
                    entity_ids: vec![target_id],
                    before: Some(json_value(update.preimage())),
                    after: Some(json_value(update.patch())),
                },
                UpdatePatch::Note(_) => ReviewOperation {
                    index,
                    id: target_id.clone(),
                    op: "update".to_string(),
                    target: "note".to_string(),
                    tier: TIER_2.to_string(),
                    summary: format!("Update note {target_id}"),
                    reason: "Note updates are outside ADR-102's enumerated Tier 1 fast path."
                        .to_string(),
                    tier_reasons: vec!["adr102.not_tier_1_eligible".to_string()],
                    entity_ids: Vec::new(),
                    before: Some(json_value(update.preimage())),
                    after: Some(json_value(update.patch())),
                },
                UpdatePatch::Edge(edge_patch) => {
                    let mut reasons = vec!["adr102.existing_edge_update".to_string()];
                    let mut explanation =
                        "Changing an existing edge relation or weight requires Tier 2 review."
                            .to_string();
                    if edge_patch.weight.is_some_and(|weight| weight < 0.7) {
                        reasons.push("adr102.resulting_weight_below_0_7".to_string());
                        explanation
                            .push_str(" The resulting weight is below the ADR-102 floor of 0.7.");
                    }
                    ReviewOperation {
                        index,
                        id: target_id.clone(),
                        op: "update".to_string(),
                        target: "edge".to_string(),
                        tier: TIER_2.to_string(),
                        summary: format!("Update edge {target_id}"),
                        reason: explanation,
                        tier_reasons: reasons,
                        entity_ids: Vec::new(),
                        before: Some(json_value(update.preimage())),
                        after: Some(json_value(update.patch())),
                    }
                }
            }
        }
        Op::Delete(delete) => {
            let target = match &delete.preimage {
                DeletePreimage::Entity(_) => "entity",
                DeletePreimage::Note(_) => "note",
                DeletePreimage::Edge(_) => "edge",
            };
            ReviewOperation {
                index,
                id: delete.target_id.to_string(),
                op: "delete".to_string(),
                target: target.to_string(),
                tier: TIER_2.to_string(),
                summary: format!("Delete {target} {}", delete.target_id),
                reason: "Every delete requires Tier 2 review under the ADR-102 floor.".to_string(),
                tier_reasons: vec!["adr102.delete".to_string()],
                entity_ids: if target == "entity" {
                    vec![delete.target_id.to_string()]
                } else {
                    Vec::new()
                },
                before: Some(json_value(&delete.preimage)),
                after: None,
            }
        }
        Op::Merge(merge) => ReviewOperation {
            index,
            id: merge.into_id.to_string(),
            op: "merge".to_string(),
            target: "entity".to_string(),
            tier: TIER_2.to_string(),
            summary: format!("Merge entity {} into {}", merge.from_id, merge.into_id),
            reason: "Every merge requires Tier 2 review under the ADR-102 floor.".to_string(),
            tier_reasons: vec!["adr102.merge".to_string()],
            entity_ids: vec![merge.into_id.to_string(), merge.from_id.to_string()],
            before: Some(json_value(&merge.preimage)),
            after: None,
        },
    }
}

fn json_value<T: serde::Serialize>(value: &T) -> serde_json::Value {
    serde_json::to_value(value).expect("typed change-set values must serialize")
}

fn evaluate_review_gate(
    required: bool,
    validation_passed: bool,
    producer_model_family: &str,
    reviewer_model_family: Option<&str>,
) -> ReviewGate {
    let reviewer = reviewer_model_family.map(str::to_string);

    let (eligible, status, reason) = if !required {
        (
            true,
            if validation_passed {
                "not_required"
            } else {
                "blocked_by_findings"
            },
            if validation_passed {
                "No operation requires independent Tier 2 review."
            } else {
                "Error-severity validation findings block approval."
            },
        )
    } else if producer_model_family.trim().is_empty() {
        (
            false,
            "invalid_producer_model_family",
            "Tier 2 review requires a non-empty producer model family in the ADR-101 envelope.",
        )
    } else if reviewer_model_family.is_none_or(|family| family.trim().is_empty()) {
        (
            false,
            "reviewer_required",
            "Tier 2 review requires --reviewer-model-family from an independent model family.",
        )
    } else if reviewer_model_family == Some(producer_model_family) {
        (
            false,
            "ineligible_same_model_family",
            "ADR-102 refuses same-family review; choose a reviewer model family different from the producer.",
        )
    } else if !validation_passed {
        (
            true,
            "blocked_by_findings",
            "The reviewer family is independent, but error-severity validation findings block approval.",
        )
    } else {
        (
            true,
            "eligible",
            "The reviewer model family differs from the producer model family.",
        )
    };

    ReviewGate {
        required,
        producer_model_family: producer_model_family.to_string(),
        reviewer_model_family: reviewer,
        eligible,
        approval_ready: eligible && validation_passed,
        status: status.to_string(),
        reason: reason.to_string(),
        persisted: false,
    }
}

fn print_report(format: &OutputFormat, report: &ReviewReport) {
    match format {
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(report).expect("serialize khive.review.v1")
        ),
        OutputFormat::Text => {
            println!(
                "KG review ({}, kind={})",
                report.schema_version, report.review_kind
            );
            println!(
                "Producer: {} [{}]",
                report.change_set.envelope.producer,
                report.change_set.envelope.producer_model_family
            );
            println!(
                "Operations: {} ({} Tier 1, {} Tier 2)",
                report.tier_summary.operations,
                report.tier_summary.tier_1,
                report.tier_summary.tier_2
            );
            for operation in &report.change_set.operations {
                println!(
                    "  {}. [{}] {}",
                    operation.index, operation.tier, operation.summary
                );
                println!("     {}", operation.reason);
            }
            println!(
                "Validation [{}]: {} error(s), {} warning(s), {} info ({})",
                report.validation.scope,
                report.validation.errors,
                report.validation.warnings,
                report.validation.info,
                if report.validation.passed {
                    "passed"
                } else {
                    "failed"
                }
            );
            for finding in &report.findings {
                println!(
                    "  [{}] {}: {}",
                    finding.severity, finding.rule_id, finding.message
                );
            }
            println!(
                "Review gate: {} — {}",
                report.review_gate.status, report.review_gate.reason
            );
            println!("Capabilities: local change-set · read-only · no writes · WASM unavailable");
        }
        OutputFormat::Github => {
            for finding in &report.findings {
                let level = match finding.severity.as_str() {
                    "error" => "error",
                    "warning" => "warning",
                    _ => "notice",
                };
                println!(
                    "::{level} title={}::{}",
                    escape_github_property(&finding.rule_id),
                    escape_github_message(&finding.message)
                );
            }
            if !report.review_gate.approval_ready {
                println!(
                    "::error title=review-gate::{}",
                    escape_github_message(&report.review_gate.reason)
                );
            }
            println!(
                "::notice title={}::{} operation(s)%2C {} Tier 2%2C gate {}",
                report.schema_version,
                report.tier_summary.operations,
                report.tier_summary.tier_2,
                escape_github_message(&report.review_gate.status)
            );
        }
    }
}

fn escape_github_message(value: &str) -> String {
    value
        .replace('%', "%25")
        .replace('\r', "%0D")
        .replace('\n', "%0A")
}

fn escape_github_property(value: &str) -> String {
    escape_github_message(value)
        .replace(':', "%3A")
        .replace(',', "%2C")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(op: serde_json::Value) -> ChangeSet {
        let envelope = serde_json::json!({
            "schema_version": 1,
            "producer": "lambda:atlas",
            "producer_model_family": "family:atlas",
            "staged_at": 1_000_000_u64,
        });
        khive_changeset::from_ndjson(&format!("{}\n{}\n", envelope, op)).unwrap()
    }

    #[test]
    fn low_weight_non_judgment_link_is_tier_2() {
        let changeset = parse(serde_json::json!({
            "op": "link",
            "id": "00000000-0000-4000-8000-000000000001",
            "namespace": "local",
            "source": "00000000-0000-4000-8000-000000000002",
            "target": "00000000-0000-4000-8000-000000000003",
            "relation": "extends",
            "weight": 0.69,
            "properties": {},
        }));

        let operation = summarize_operation(0, &changeset.ops[0]);
        assert_eq!(operation.tier, TIER_2);
        assert!(operation
            .tier_reasons
            .contains(&"adr102.resulting_weight_below_0_7".to_string()));
    }

    #[test]
    fn same_family_gate_is_ineligible() {
        let gate = evaluate_review_gate(true, true, "family:atlas", Some("family:atlas"));
        assert!(!gate.eligible);
        assert!(!gate.approval_ready);
        assert_eq!(gate.status, "ineligible_same_model_family");
    }
}
