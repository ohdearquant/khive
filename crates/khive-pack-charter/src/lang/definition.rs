//! The canonical definition a charter compiles to. It is self-contained: every registry fact
//! evaluation needs (passing outcomes, argument values) is copied in, and the referenced
//! registry entries are pinned by `gate_registry_digest`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::ast::Level;

pub const LANGUAGE: &str = "charter/1";

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Definition {
    pub language: String,
    pub id: String,
    pub version: i64,
    pub title: Option<String>,
    pub subject: String,
    pub roles: Vec<RoleDef>,
    pub triggers: Vec<String>,
    pub phases: Vec<PhaseDef>,
    pub break_glass: Option<BreakGlassDef>,
    pub registry: BTreeMap<String, Value>,
    pub gate_registry_digest: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RoleDef {
    pub name: String,
    pub description: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PhaseDef {
    pub id: String,
    pub ordinal: i64,
    pub owner: Option<String>,
    pub awaits: Vec<Vec<String>>,
    pub actions: Vec<ActionDef>,
    pub performs: Vec<String>,
    pub requires: Vec<RequireDef>,
    pub whens: Vec<WhenDef>,
    pub evidence: Vec<String>,
    pub grants: Vec<GrantDef>,
    pub max_rounds: Option<MaxRounds>,
    /// The route taken when the phase is satisfied and no `when` matched: the phase's own end
    /// clause, or the next phase in declaration order.
    pub next: Route,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ActionDef {
    pub name: String,
    pub args: BTreeMap<String, Value>,
    pub by: Option<String>,
    pub passes: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RequireDef {
    pub phrase: String,
    pub args: BTreeMap<String, Value>,
    pub level: Level,
    pub passes: Vec<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WhenSource {
    Phrase,
    Trigger,
    Action,
    Charter,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WhenDef {
    pub name: String,
    pub source: WhenSource,
    pub args: BTreeMap<String, Value>,
    pub label: String,
    pub route: Route,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct GrantDef {
    pub role: String,
    pub action: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct MaxRounds {
    pub rounds: u32,
    pub then: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Route {
    Goto { phase: String },
    Complete,
    Terminate { reason: Option<String> },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BreakGlassDef {
    pub by: Option<String>,
    pub on: Vec<String>,
    pub reason_min: Option<i64>,
    pub marks_degraded: bool,
}

impl Definition {
    pub fn phase(&self, id: &str) -> Option<&PhaseDef> {
        self.phases.iter().find(|p| p.id == id)
    }
}
