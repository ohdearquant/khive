//! Validation pipeline types for pack-contributed KG rules.
//!
//! Defines the trait surface for `CorpusCheck` (whole-corpus, cross-entity joins)
//! and `StreamingRule` (per-record) shapes. Both return `Vec<Violation>` aggregated
//! into a `ValidationReport`.

use std::collections::BTreeMap;

// ── Rule identity ─────────────────────────────────────────────────────────────

/// Stable rule identifier, namespaced by pack: `"<pack>/<rule-id>"`.
///
/// Built-in rules use no namespace prefix (e.g. `"min-edge-density"`).
/// Pack-contributed rules MUST be namespaced (e.g. `"biology/required-taxa-rank"`).
pub type RuleId = &'static str;

/// Severity of a validation finding.
///
/// - `Error`: causes `kkernel kg validate` to exit with code 1.
/// - `Warning`: reported but does not affect exit code (unless `--strict`).
/// - `Info`: informational; no exit-code effect.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Info,
    Warning,
    Error,
}

// ── Corpus snapshot ───────────────────────────────────────────────────────────

/// Opaque snapshot of the KG corpus passed to `CorpusCheck::check`.
///
/// v1 exposes the bare field set needed for the built-in rules. Pack authors
/// that need richer access should open a design review to extend this surface —
/// do NOT reach through this struct to the storage layer.
#[non_exhaustive]
pub struct GraphSnapshot {
    /// Total entity count in the snapshot.
    pub entity_count: usize,
    /// Total edge count in the snapshot.
    pub edge_count: usize,
}

/// Context passed to all rule implementations.
///
/// Carries configuration overrides from `.khive/kg/rules.toml` merged with
/// pack defaults. Rules read per-rule config from `config[rule_id]`.
#[non_exhaustive]
pub struct ValidationContext<'a> {
    /// The corpus snapshot for whole-corpus rules.
    pub snapshot: &'a GraphSnapshot,
    /// Per-rule config overrides, keyed by rule ID.
    pub config: &'a BTreeMap<&'static str, serde_json::Value>,
}

// ── Violation ─────────────────────────────────────────────────────────────────

/// A single rule violation produced by a rule implementation.
#[non_exhaustive]
pub struct Violation {
    /// The rule that produced this violation.
    pub rule_id: &'static str,
    /// Violation severity (may differ from rule-level severity for pack rules
    /// that emit mixed-severity output within one rule).
    pub severity: Severity,
    /// Human-readable explanation of the violation.
    pub message: String,
    /// Whether the violation can be fixed by `kkernel kg validate --fix`.
    pub fixable: bool,
    /// Optional entity UUID (short-form) that the violation targets.
    pub entity_id: Option<String>,
    /// Optional edge UUID (short-form) that the violation targets.
    pub edge_id: Option<String>,
}

impl Violation {
    /// Construct a non-fixable violation without a specific entity/edge target.
    pub fn new(rule_id: &'static str, severity: Severity, message: impl Into<String>) -> Self {
        Self {
            rule_id,
            severity,
            message: message.into(),
            fixable: false,
            entity_id: None,
            edge_id: None,
        }
    }

    /// Attach an entity identifier to an existing violation.
    pub fn with_entity(mut self, id: impl Into<String>) -> Self {
        self.entity_id = Some(id.into());
        self
    }
}

// ── Rule function type ────────────────────────────────────────────────────────

/// Whole-corpus check function type.
///
/// Receives the corpus snapshot and config context; returns all violations
/// produced by the rule in one call.
pub type RuleFn = fn(&ValidationContext<'_>) -> Vec<Violation>;

/// Optional auto-fix function type.
///
/// Receives the context and violations emitted by the corresponding `RuleFn`.
/// Returns a `GraphPatch` (opaque in v1 — see below) that the validator applies
/// before writing NDJSON. Returning `None` leaves the graph unchanged.
///
/// `GraphPatch` is a placeholder type in v1; the auto-fix write path is out of
/// scope for this cluster.
pub type FixFn = fn(&ValidationContext<'_>, &[Violation]) -> Option<GraphPatch>;

/// Opaque graph patch produced by a fix function.
///
/// v1 carries no fields — the auto-fix machinery is stubbed. The type exists
/// so pack authors can write `fix: Some(my_fix as FixFn)` without a
/// compile-time change when the v1 fix path is wired up.
#[non_exhaustive]
pub struct GraphPatch;

// ── ValidationRule ────────────────────────────────────────────────────────────

/// A pack-contributed validation rule.
///
/// Rule IDs must follow the `<pack>/<rule-id>` namespace convention.
/// See `docs/validation.md` for declaration examples and severity override rules.
pub struct ValidationRule {
    /// Stable rule identifier in `<pack>/<rule-id>` format.
    pub id: RuleId,
    /// Default severity; can be overridden in `.khive/kg/rules.toml`.
    pub severity: Severity,
    /// Human-readable description shown in `kkernel kg validate` output.
    pub description: &'static str,
    /// Whole-corpus check function.
    pub check: RuleFn,
    /// Optional auto-fix function. `None` for unfixable rules.
    pub fix: Option<FixFn>,
}

// ── Aggregated report ─────────────────────────────────────────────────────────

/// Aggregated result of running the full rule pipeline.
#[derive(Default)]
pub struct ValidationReport {
    /// Violations grouped by rule ID, sorted canonically by rule ID.
    pub violations_by_rule: BTreeMap<String, Vec<Violation>>,
}

impl ValidationReport {
    /// Add violations for a given rule to the report.
    pub fn add(&mut self, rule_id: &str, violations: Vec<Violation>) {
        self.violations_by_rule
            .entry(rule_id.to_string())
            .or_default()
            .extend(violations);
    }

    /// Total number of violations at `Severity::Error` across all rules.
    pub fn error_count(&self) -> usize {
        self.violations_by_rule
            .values()
            .flat_map(|vs| vs.iter())
            .filter(|v| v.severity == Severity::Error)
            .count()
    }

    /// Total number of violations at `Severity::Warning` across all rules.
    pub fn warning_count(&self) -> usize {
        self.violations_by_rule
            .values()
            .flat_map(|vs| vs.iter())
            .filter(|v| v.severity == Severity::Warning)
            .count()
    }

    /// `true` when no errors were found (the standard exit-0 condition).
    pub fn passed(&self) -> bool {
        self.error_count() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn violation_builder() {
        let v = Violation::new("test/rule", Severity::Warning, "something is off")
            .with_entity("abc123");
        assert_eq!(v.rule_id, "test/rule");
        assert_eq!(v.severity, Severity::Warning);
        assert!(!v.fixable);
        assert_eq!(v.entity_id.as_deref(), Some("abc123"));
    }

    #[test]
    fn report_error_count() {
        let mut report = ValidationReport::default();
        report.add(
            "test/rule",
            vec![
                Violation::new("test/rule", Severity::Error, "bad"),
                Violation::new("test/rule", Severity::Warning, "meh"),
            ],
        );
        assert_eq!(report.error_count(), 1);
        assert_eq!(report.warning_count(), 1);
        assert!(!report.passed());
    }

    #[test]
    fn report_passed_when_no_errors() {
        let mut report = ValidationReport::default();
        report.add(
            "test/rule",
            vec![Violation::new("test/rule", Severity::Warning, "meh")],
        );
        assert!(report.passed());
    }

    #[test]
    fn graph_patch_is_constructible() {
        // Ensure the placeholder type can be named and constructed.
        let _patch = GraphPatch;
    }

    #[test]
    fn validation_rule_fields() {
        fn dummy_check(_ctx: &ValidationContext<'_>) -> Vec<Violation> {
            vec![]
        }
        let rule = ValidationRule {
            id: "bio/taxa",
            severity: Severity::Warning,
            description: "taxa must exist",
            check: dummy_check,
            fix: None,
        };
        assert_eq!(rule.id, "bio/taxa");
        assert!(rule.fix.is_none());
    }
}
