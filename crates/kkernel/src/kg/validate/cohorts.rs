use std::collections::BTreeMap;

use anyhow::{bail, Result};

use super::{FixedCounts, RuleResult, ValidationReport, ValidationSummary};

pub(super) struct RuleCohorts {
    structural: Vec<RuleResult>,
    configurable: Vec<RuleResult>,
    summary: ValidationSummary,
}

impl RuleCohorts {
    pub(super) fn new(
        structural: Vec<RuleResult>,
        configurable: Vec<RuleResult>,
        summary: ValidationSummary,
    ) -> Self {
        Self {
            structural,
            configurable,
            summary,
        }
    }

    pub(super) fn rules(&self) -> impl Iterator<Item = &RuleResult> {
        self.structural.iter().chain(&self.configurable)
    }

    pub(super) fn ensure_unique_ids(&self, run: &str) -> Result<()> {
        for (source, rules) in [
            ("structural", self.structural.as_slice()),
            ("configurable", self.configurable.as_slice()),
        ] {
            index_rules(rules, run, source)?;
        }
        Ok(())
    }

    pub(super) fn fixed_counts_after(&self, rechecked: &Self) -> Result<FixedCounts> {
        let mut fixed = FixedCounts::default();
        for (source, before_rules, after_rules) in [
            (
                "structural",
                self.structural.as_slice(),
                rechecked.structural.as_slice(),
            ),
            (
                "configurable",
                self.configurable.as_slice(),
                rechecked.configurable.as_slice(),
            ),
        ] {
            let before_index = index_rules(before_rules, "original", source)?;
            let after_index = index_rules(after_rules, "final", source)?;
            for id in before_index.keys() {
                if !after_index.contains_key(*id) {
                    bail!(
                        "--fix rule identity error: {source} rule {id:?} is missing from final run"
                    );
                }
            }
            for id in after_index.keys() {
                if !before_index.contains_key(*id) {
                    bail!(
                        "--fix rule identity error: {source} rule {id:?} is missing from original run"
                    );
                }
            }
            for (&id, &(_, before)) in &before_index {
                let after = after_index[id].1;
                if before.severity != after.severity {
                    bail!(
                        "--fix rule identity error: {source} rule {id:?} changed severity from {:?} to {:?}",
                        before.severity,
                        after.severity,
                    );
                }
                if before.id == "sort-order"
                    && before.violations.iter().any(|violation| violation.fixable)
                    && !before.passed
                    && after.passed
                {
                    match before.severity {
                        "error" => fixed.errors += 1,
                        "warning" => fixed.warnings += 1,
                        "info" => fixed.info += 1,
                        _ => {}
                    }
                }
            }
        }
        Ok(fixed)
    }

    pub(super) fn into_report(mut self) -> ValidationReport {
        self.structural.extend(self.configurable);
        ValidationReport {
            rules: self.structural,
            summary: self.summary,
        }
    }
}

fn index_rules<'a>(
    rules: &'a [RuleResult],
    run: &str,
    source: &str,
) -> Result<BTreeMap<&'a str, (usize, &'a RuleResult)>> {
    let mut index = BTreeMap::new();
    for (position, rule) in rules.iter().enumerate() {
        if let Some((first, _)) = index.insert(rule.id.as_str(), (position, rule)) {
            bail!(
                "--fix rule identity error: duplicate rule id {:?}: {run} {source}[{first}] and {run} {source}[{position}]",
                rule.id,
            );
        }
    }
    Ok(index)
}
