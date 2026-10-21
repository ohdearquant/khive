//! The phrase registry: subjects, triggers, phrases and actions a charter may name.
//!
//! Entries carry no code. A phrase is an observation kind with a closed outcome set, the
//! outcomes that satisfy a `require`, an enforcement ceiling and an argument schema. The core
//! vocabulary is compiled into the pack; further vocabularies are published as data.

use std::collections::BTreeMap;

use serde_json::{json, Value};

use super::ast::{EntryKind, Level, VocabularySource};
use super::Diagnostic;

pub const CORE_VOCABULARY: &str = include_str!("../../vocab/core.vocab");

/// Outcomes a `perform <charter>` step records in its parent run.
pub const CHILD_RUN_OUTCOMES: &[&str] = &["completed", "terminated", "cancelled"];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ParamType {
    String,
    Integer,
    Duration,
    Bool,
    Word,
}

impl ParamType {
    fn parse(word: &str) -> Option<Self> {
        match word {
            "string" => Some(Self::String),
            "integer" => Some(Self::Integer),
            "duration" => Some(Self::Duration),
            "bool" => Some(Self::Bool),
            "word" => Some(Self::Word),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Self::String => "string",
            Self::Integer => "integer",
            Self::Duration => "duration",
            Self::Bool => "bool",
            Self::Word => "word",
        }
    }
}

#[derive(Clone, Debug)]
pub struct ParamDef {
    pub name: String,
    pub ty: ParamType,
    pub optional: bool,
}

#[derive(Clone, Debug)]
pub struct EntryDef {
    pub kind: EntryKind,
    pub name: String,
    pub description: Option<String>,
    pub outcomes: Vec<String>,
    pub passes: Vec<String>,
    pub ceiling: Level,
    pub params: Vec<ParamDef>,
    /// For a subject kind: the triggers whose observation starts a new generation.
    pub new_generation: Vec<String>,
    pub vocabulary: String,
    pub vocabulary_version: i64,
}

impl EntryDef {
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind.as_str(),
            "name": self.name,
            "description": self.description,
            "outcomes": self.outcomes,
            "passes": self.passes,
            "ceiling": self.ceiling.as_str(),
            "params": self.params.iter().map(|p| json!({
                "name": p.name,
                "type": p.ty.as_str(),
                "optional": p.optional,
            })).collect::<Vec<_>>(),
            "new_generation": self.new_generation,
            "vocabulary": self.vocabulary,
            "vocabulary_version": self.vocabulary_version,
        })
    }
}

#[derive(Clone, Debug, Default)]
pub struct Registry {
    entries: BTreeMap<String, EntryDef>,
}

impl Registry {
    /// The compiled core vocabulary. A parse failure here is a build defect, covered by tests.
    pub fn core() -> Self {
        let mut registry = Self::default();
        let source = super::parse_vocabulary_source(CORE_VOCABULARY)
            .expect("the compiled core vocabulary parses");
        registry
            .add(&source)
            .expect("the compiled core vocabulary is consistent");
        registry
    }

    pub fn get(&self, name: &str) -> Option<&EntryDef> {
        self.entries.get(name)
    }

    pub fn entries(&self) -> impl Iterator<Item = &EntryDef> {
        self.entries.values()
    }

    pub fn names_of(&self, kind: &EntryKind) -> Vec<&str> {
        self.entries
            .values()
            .filter(|e| &e.kind == kind)
            .map(|e| e.name.as_str())
            .collect()
    }

    /// Resolve a vocabulary into entries and add them. A name already held by a different
    /// vocabulary is refused; a newer version of the same vocabulary replaces its own entries.
    pub fn add(&mut self, source: &VocabularySource) -> Result<Vec<EntryDef>, Vec<Diagnostic>> {
        let mut errors = Vec::new();
        let mut resolved: BTreeMap<String, EntryDef> = BTreeMap::new();
        for entry in &source.entries {
            if resolved.contains_key(&entry.name) {
                errors.push(Diagnostic::new(
                    entry.line,
                    "duplicate_entry",
                    format!("{:?} is declared twice in this vocabulary", entry.name),
                ));
                continue;
            }
            if let Some(held) = self.entries.get(&entry.name) {
                if held.vocabulary != source.name {
                    errors.push(Diagnostic::new(
                        entry.line,
                        "entry_conflict",
                        format!(
                            "{:?} is already declared by vocabulary {:?}",
                            entry.name, held.vocabulary
                        ),
                    ));
                    continue;
                }
                if held.vocabulary_version > source.version {
                    errors.push(Diagnostic::new(
                        entry.line,
                        "stale_vocabulary",
                        format!(
                            "vocabulary {:?} v{} is older than the held v{}",
                            source.name, source.version, held.vocabulary_version
                        ),
                    ));
                    continue;
                }
            }
            let (default_outcomes, default_passes): (&[&str], &[&str]) = match entry.kind {
                EntryKind::Subject => (&[], &[]),
                EntryKind::Trigger => (&["observed"], &["observed"]),
                EntryKind::Phrase => (&["pass", "fail"], &["pass"]),
                EntryKind::Action => (&["done", "failed", "skipped"], &["done", "skipped"]),
            };
            let outcomes: Vec<String> = entry
                .outcomes
                .clone()
                .unwrap_or_else(|| default_outcomes.iter().map(|s| s.to_string()).collect());
            let passes: Vec<String> = match &entry.passes {
                Some(p) => p.clone(),
                None if entry.outcomes.is_some() && entry.kind == EntryKind::Phrase => outcomes
                    .iter()
                    .filter(|o| o.as_str() == "pass")
                    .cloned()
                    .collect(),
                None if entry.outcomes.is_some() && entry.kind == EntryKind::Trigger => {
                    outcomes.clone()
                }
                None if entry.outcomes.is_some() && entry.kind == EntryKind::Action => outcomes
                    .iter()
                    .filter(|o| matches!(o.as_str(), "done" | "skipped"))
                    .cloned()
                    .collect(),
                None => default_passes.iter().map(|s| s.to_string()).collect(),
            };
            if entry.kind == EntryKind::Subject
                && (entry.outcomes.is_some()
                    || entry.passes.is_some()
                    || entry.ceiling.is_some()
                    || !entry.params.is_empty())
            {
                errors.push(Diagnostic::new(
                    entry.line,
                    "bad_entry",
                    "a subject kind takes only a description and new_generation",
                ));
                continue;
            }
            if entry.kind != EntryKind::Subject && entry.new_generation.is_some() {
                errors.push(Diagnostic::new(
                    entry.line,
                    "bad_entry",
                    format!(
                        "{:?}: only a subject kind declares new_generation",
                        entry.name
                    ),
                ));
                continue;
            }
            let mut seen = std::collections::BTreeSet::new();
            if outcomes.iter().any(|o| !seen.insert(o.clone())) {
                errors.push(Diagnostic::new(
                    entry.line,
                    "duplicate_outcome",
                    format!("{:?} lists an outcome twice", entry.name),
                ));
                continue;
            }
            if let Some(stray) = passes.iter().find(|p| !outcomes.contains(p)) {
                errors.push(Diagnostic::new(
                    entry.line,
                    "bad_passes",
                    format!(
                        "passing outcome {stray:?} is not one of {:?}'s outcomes",
                        entry.name
                    ),
                ));
                continue;
            }
            let mut params = Vec::new();
            let mut names = std::collections::BTreeSet::new();
            let mut bad = false;
            for p in &entry.params {
                if !names.insert(p.name.clone()) {
                    errors.push(Diagnostic::new(
                        entry.line,
                        "duplicate_param",
                        format!("{:?} declares parameter {:?} twice", entry.name, p.name),
                    ));
                    bad = true;
                    continue;
                }
                let Some(ty) = ParamType::parse(&p.ty) else {
                    bad = true;
                    continue;
                };
                params.push(ParamDef {
                    name: p.name.clone(),
                    ty,
                    optional: p.optional,
                });
            }
            if bad {
                continue;
            }
            resolved.insert(
                entry.name.clone(),
                EntryDef {
                    kind: entry.kind.clone(),
                    name: entry.name.clone(),
                    description: entry.description.clone(),
                    outcomes,
                    passes,
                    ceiling: match entry.kind {
                        EntryKind::Phrase => entry.ceiling.unwrap_or(Level::Advisory),
                        _ => entry.ceiling.unwrap_or(Level::Hard),
                    },
                    params,
                    new_generation: entry.new_generation.clone().unwrap_or_default(),
                    vocabulary: source.name.clone(),
                    vocabulary_version: source.version,
                },
            );
        }
        for entry in &source.entries {
            for name in entry.new_generation.iter().flatten() {
                let trigger = resolved.get(name).or_else(|| {
                    self.entries
                        .get(name)
                        .filter(|held| held.vocabulary != source.name)
                });
                if trigger.is_none_or(|t| t.kind != EntryKind::Trigger) {
                    errors.push(Diagnostic::new(
                        entry.line,
                        "unknown_trigger",
                        format!(
                            "{:?} starts a new generation on {name:?}, which is not a registered \
                             trigger",
                            entry.name
                        ),
                    ));
                }
            }
        }
        if !errors.is_empty() {
            return Err(errors);
        }
        self.entries
            .retain(|_, held| held.vocabulary != source.name);
        let added: Vec<EntryDef> = resolved.values().cloned().collect();
        self.entries.extend(resolved);
        Ok(added)
    }
}
