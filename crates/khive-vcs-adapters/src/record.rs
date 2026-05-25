// Copyright 2026 khive contributors. Licensed under Apache-2.0.
//
//! ADR-020 §2 record shapes for adapter output.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Entity record shape (ADR-020 §2) produced by adapters.
///
/// Adapters produce these; the standard `khive kg import` pipeline validates
/// and loads them into `working.db`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRecord {
    pub id: Uuid,
    pub kind: String,
    pub name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub properties: serde_json::Value,
    #[serde(default)]
    pub tags: Vec<String>,
}

/// Edge record shape (ADR-020 §2) produced by adapters.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EdgeRecord {
    pub edge_id: Uuid,
    pub source: String,
    pub target: String,
    pub relation: String,
    #[serde(default = "default_weight")]
    pub weight: f64,
    #[serde(default)]
    pub properties: serde_json::Value,
}

fn default_weight() -> f64 {
    0.7
}
