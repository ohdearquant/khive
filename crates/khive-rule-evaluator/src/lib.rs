//! Pure configurable graph validation over a captured NDJSON view.
//!
//! Hosts own I/O, pack metadata and clock capture. This crate performs no I/O
//! and never reads an ambient clock; full and partial views share one evaluator.

#![forbid(unsafe_code)]

mod config;
mod evaluate;

use chrono::{DateTime, Utc};
use khive_types::EdgeEndpointRule;
use serde::Serialize;

pub use config::{
    CitationDateLintConfig, DanglingRefsConfig, DirectionRuleConfig,
    EdgeDirectionConventionsConfig, EdgeEndpointTypesConfig, NamingConventionsConfig,
    NamingConventionsOverride, RuleConfig, Rules, RulesError,
};
pub use evaluate::{
    check_citation_date_lint, check_dangling_refs, check_edge_direction_conventions,
    check_edge_endpoint_types, check_naming_conventions, collect_edge_ids, collect_ids,
    edge_source_id, edge_target_id, evaluate, record_prefix,
};

/// Immutable input strings, preserving record and substrate scan order.
#[derive(Clone, Copy, Debug)]
pub struct NdjsonState<'a> {
    pub entities: &'a str,
    pub edges: &'a str,
    pub notes: &'a str,
}

/// Whether the input is a complete dataset or a projected change-set.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvaluationMode {
    FullDataset,
    /// Skip only the built-in dangling-reference finding, retaining config errors.
    PartialView,
}

/// Host-supplied metadata and a single explicit validation-time instant.
#[derive(Clone, Copy, Debug)]
pub struct EvaluationContext<'a> {
    pub mode: EvaluationMode,
    pub pack_edge_rules: &'a [EdgeEndpointRule],
    pub now: DateTime<Utc>,
}

/// Result for a single validation rule in a `kkernel kg validate` run.
#[derive(Debug, Serialize)]
pub struct RuleResult {
    pub id: String,
    pub severity: &'static str,
    pub passed: bool,
    pub violations: Vec<Violation>,
}

/// A single rule violation with location metadata and a fixability flag.
#[derive(Debug, Serialize)]
pub struct Violation {
    pub entity_id: Option<String>,
    pub entity_name: Option<String>,
    pub entity_kind: Option<String>,
    pub rule_id: String,
    pub severity: &'static str,
    pub message: String,
    pub fixable: bool,
}
