use std::collections::BTreeSet;

use khive_runtime::graph_lint::{LintRuleId, LintSeverity};
use khive_runtime::RuntimeError;
use serde_json::Value;

pub(super) struct Selection {
    pub rules: BTreeSet<LintRuleId>,
    pub severity: LintSeverity,
    pub fix: bool,
    pub stats_only: bool,
}

impl Selection {
    pub fn parse(params: Value) -> Result<Self, RuntimeError> {
        let object = params
            .as_object()
            .ok_or_else(|| RuntimeError::InvalidInput("knowledge.lint expects an object".into()))?;
        if let Some(key) = object
            .keys()
            .find(|key| !matches!(key.as_str(), "rules" | "fix" | "severity"))
        {
            return Err(RuntimeError::InvalidInput(format!(
                "unknown knowledge.lint parameter {key:?}"
            )));
        }
        let invalid = |field: &str, expected: &str| {
            RuntimeError::InvalidInput(format!("knowledge.lint {field} must be {expected}"))
        };
        let fix = match object.get("fix") {
            None => false,
            Some(Value::Bool(value)) => *value,
            Some(_) => return Err(invalid("fix", "a boolean")),
        };
        let severity = match object.get("severity") {
            None => LintSeverity::Warn,
            Some(Value::String(value)) => match value.as_str() {
                "error" => LintSeverity::Error,
                "warn" => LintSeverity::Warn,
                "info" => LintSeverity::Info,
                _ => return Err(invalid("severity", "error, warn, or info")),
            },
            Some(_) => return Err(invalid("severity", "error, warn, or info")),
        };
        let mut stats_only = false;
        let rules = match object.get("rules") {
            None => LintRuleId::ALL.into_iter().collect(),
            Some(Value::Array(values)) if !values.is_empty() => {
                let mut rules = BTreeSet::new();
                for value in values {
                    let id = value
                        .as_str()
                        .ok_or_else(|| invalid("rules", "a nonempty array of exact rule IDs"))?;
                    if id == "stats-only" {
                        stats_only = true;
                    } else {
                        let rule = LintRuleId::ALL
                            .into_iter()
                            .find(|rule| rule.as_str() == id)
                            .ok_or_else(|| {
                                RuntimeError::InvalidInput(format!(
                                "unknown lint rule {id:?}; valid rules: {} (or sole stats-only)",
                                LintRuleId::ALL.map(LintRuleId::as_str).join(", ")
                            ))
                            })?;
                        rules.insert(rule);
                    }
                }
                if stats_only && (values.len() != 1 || fix) {
                    return Err(invalid("rules", "sole stats-only without fix=true"));
                }
                rules
            }
            Some(_) => return Err(invalid("rules", "a nonempty array of exact rule IDs")),
        };
        Ok(Self {
            rules,
            severity,
            fix,
            stats_only,
        })
    }
}

#[cfg(test)]
#[path = "params_tests.rs"]
mod tests;
