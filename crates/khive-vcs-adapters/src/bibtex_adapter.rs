//! Reader-backed BibTeX import; see `docs/api/bibtex-adapter.md`.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;

use serde::Deserialize;
use serde_bibtex::token::Variable;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::json_adapter::{parse_edge, parse_entity};
use crate::{AdapterError, EdgeRecord, EntityRecord, FormatAdapter};

mod framing;
use framing::BibtexFrames;

#[derive(Clone, Copy)]
struct Limits {
    entry_bytes: usize,
    expanded_bytes: usize,
    macro_bytes: usize,
    depth: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            entry_bytes: 16 * 1024 * 1024,
            expanded_bytes: 16 * 1024 * 1024,
            macro_bytes: 16 * 1024 * 1024,
            depth: 256,
        }
    }
}

fn limit_error(resource: &str, limit: usize) -> AdapterError {
    AdapterError::Parse(format!("BibTeX {resource} exceeds limit {limit}"))
}

/// Source accounting for the BibTeX-only CLI summary.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BibtexImportStats {
    /// Regular entry frames encountered, including malformed regular frames.
    pub entries: usize,
    /// Regular entries skipped after a syntax or value-expansion warning.
    pub skipped: usize,
    /// All warnings, including invalid macros, comments or preambles.
    pub warnings: usize,
}

/// Pure BibTeX transform with bounded raw-entry and macro buffering.
///
/// Converted records remain buffered for whole-import validation and forward crossrefs.
/// See `docs/api/bibtex-adapter.md` for limits, mappings and malformed-tail behavior.
pub struct BibtexFormatAdapter {
    entities: Vec<EntityRecord>,
    edges: Vec<EdgeRecord>,
    warnings: Vec<String>,
    stats: BibtexImportStats,
}

#[derive(Deserialize)]
enum RawToken {
    Variable(String),
    Text(String),
}

#[derive(Deserialize)]
enum RawEntry {
    Regular {
        #[serde(rename = "entry_type")]
        _entry_type: String,
        entry_key: String,
        fields: Vec<(String, Vec<RawToken>)>,
    },
    // Capturing the tokens disables the upstream's unbounded automatic macro expansion.
    Macro(Option<(String, Vec<RawToken>)>),
    Comment(serde::de::IgnoredAny),
    Preamble(serde::de::IgnoredAny),
}

enum ExpansionError {
    Undefined(String),
    Limit,
}

fn expand(
    tokens: Vec<RawToken>,
    macros: &HashMap<Variable<String>, String>,
    budget: &mut usize,
) -> Result<String, ExpansionError> {
    let mut value = String::new();
    for token in tokens {
        let text = match &token {
            RawToken::Text(text) => text,
            RawToken::Variable(name) => {
                let key = Variable::new(name.clone())
                    .map_err(|_| ExpansionError::Undefined(name.clone()))?;
                macros
                    .get(&key)
                    .ok_or_else(|| ExpansionError::Undefined(name.clone()))?
            }
        };
        *budget = budget
            .checked_sub(text.len())
            .ok_or(ExpansionError::Limit)?;
        value.push_str(text);
    }
    Ok(value)
}

impl BibtexFormatAdapter {
    /// Read entries without loading the complete raw source or accessing a database.
    ///
    /// Syntax warnings skip complete malformed frames. IO, UTF-8, resource-limit,
    /// duplicate-key and unresolved-crossref failures refuse the entire source.
    pub fn from_reader(reader: impl BufRead) -> Result<Self, AdapterError> {
        Self::read_with_limits(reader, Limits::default())
    }

    /// Accounting remains available before or after draining either record stream.
    pub fn stats(&self) -> BibtexImportStats {
        self.stats
    }

    fn warn(&mut self, line: usize, offset: u64, regular: bool, reason: impl std::fmt::Display) {
        self.warnings
            .push(format!("BibTeX line {line}, byte {offset}: {reason}"));
        self.stats.warnings += 1;
        self.stats.skipped += usize::from(regular);
    }

    fn read_with_limits(reader: impl BufRead, limits: Limits) -> Result<Self, AdapterError> {
        let mut frames = BibtexFrames::new(reader, limits);
        let mut adapter = Self {
            entities: Vec::new(),
            edges: Vec::new(),
            warnings: Vec::new(),
            stats: BibtexImportStats::default(),
        };
        let mut macros = HashMap::<Variable<String>, String>::new();
        let mut macro_bytes = 0usize;
        let mut keys = HashMap::<String, Uuid>::new();
        let mut seen_keys = HashSet::new();
        let mut crossrefs = Vec::new();
        while let Some(frame) = frames.next_frame()? {
            adapter.stats.entries += usize::from(frame.regular);
            if !frame.complete {
                adapter.warn(
                    frame.line,
                    frame.offset,
                    frame.regular,
                    "unfinished entry; remaining tail skipped",
                );
                break;
            }
            let parsed: Vec<RawEntry> = match serde_bibtex::from_str(&frame.source) {
                Ok(parsed) => parsed,
                Err(error) => {
                    adapter.warn(frame.line, frame.offset, frame.regular, error);
                    continue;
                }
            };
            for entry in parsed {
                match entry {
                    RawEntry::Macro(Some((name, tokens))) => {
                        let key = Variable::new(name.clone())
                            .map_err(|error| AdapterError::Parse(error.to_string()))?;
                        let old_bytes = macros
                            .get_key_value(&key)
                            .map_or(0, |(stored, value)| stored.as_ref().len() + value.len());
                        let available = limits
                            .macro_bytes
                            .checked_sub(macro_bytes - old_bytes)
                            .and_then(|n| n.checked_sub(name.len()))
                            .ok_or_else(|| limit_error("macro state", limits.macro_bytes))?;
                        let mut budget = available;
                        match expand(tokens, &macros, &mut budget) {
                            Ok(value) => {
                                macro_bytes = macro_bytes - old_bytes + name.len() + value.len();
                                macros.remove(&key);
                                macros.insert(key, value);
                            }
                            Err(ExpansionError::Undefined(name)) => {
                                adapter.warn(
                                    frame.line,
                                    frame.offset,
                                    false,
                                    format_args!("undefined macro {name:?}; definition skipped"),
                                );
                            }
                            Err(ExpansionError::Limit) => {
                                return Err(limit_error("macro state", limits.macro_bytes));
                            }
                        }
                    }
                    RawEntry::Regular {
                        entry_key, fields, ..
                    } => {
                        if !seen_keys.insert(entry_key.clone()) {
                            return Err(AdapterError::Parse(format!(
                                "duplicate BibTeX citation key {entry_key:?}"
                            )));
                        }
                        let mut expanded = HashMap::new();
                        let mut budget = limits.expanded_bytes;
                        let mut undefined = None;
                        for (name, tokens) in fields {
                            match expand(tokens, &macros, &mut budget) {
                                Ok(value) => {
                                    expanded.insert(name.to_ascii_lowercase(), value);
                                }
                                Err(ExpansionError::Undefined(name)) => {
                                    undefined = Some(name);
                                    break;
                                }
                                Err(ExpansionError::Limit) => {
                                    return Err(limit_error(
                                        "expanded entry",
                                        limits.expanded_bytes,
                                    ));
                                }
                            }
                        }
                        if let Some(name) = undefined {
                            adapter.warn(
                                frame.line,
                                frame.offset,
                                true,
                                format_args!(
                                    "entry {entry_key:?} has undefined macro {name:?}; skipped"
                                ),
                            );
                            continue;
                        }
                        let entity = map_entity(adapter.entities.len(), &entry_key, &expanded)?;
                        if let Some(target) = expanded.get("crossref") {
                            crossrefs.push((entity.id, entry_key.clone(), target.clone()));
                        }
                        keys.insert(entry_key, entity.id);
                        adapter.entities.push(entity);
                    }
                    RawEntry::Macro(None) | RawEntry::Comment(_) | RawEntry::Preamble(_) => {}
                }
            }
        }
        for (source, key, target) in crossrefs {
            let target_id = keys.get(&target).ok_or_else(|| {
                AdapterError::Parse(format!(
                    "BibTeX entry {key:?} crossref {target:?} does not identify an imported entry"
                ))
            })?;
            let object = Map::from_iter([
                ("source".into(), json!(source)),
                ("target".into(), json!(target_id)),
                ("relation".into(), json!("depends_on")),
            ]);
            adapter
                .edges
                .push(parse_edge(adapter.edges.len(), object, &mut Vec::new())?);
        }
        Ok(adapter)
    }
}

fn map_entity(
    index: usize,
    key: &str,
    fields: &HashMap<String, String>,
) -> Result<EntityRecord, AdapterError> {
    let nonempty = |field: &str| fields.get(field).filter(|value| !value.trim().is_empty());
    let mut properties = Map::new();
    for (from, to) in [("author", "authors"), ("year", "year"), ("doi", "doi")] {
        if let Some(value) = fields.get(from) {
            properties.insert(to.into(), json!(value));
        }
    }
    if let Some(venue) = nonempty("journal").or_else(|| nonempty("booktitle")) {
        properties.insert("venue".into(), json!(venue));
    }
    let source =
        if nonempty("archiveprefix").is_some_and(|prefix| prefix.eq_ignore_ascii_case("arxiv")) {
            nonempty("eprint").map(|value| format!("arxiv:{value}"))
        } else {
            None
        }
        .or_else(|| nonempty("url").map(|value| format!("url:{value}")));
    if let Some(source) = source {
        properties.insert("source".into(), json!(source));
    }
    let mut object = Map::from_iter([
        ("kind".into(), json!("document")),
        ("entity_type".into(), json!("paper")),
        (
            "name".into(),
            json!(nonempty("title").map(String::as_str).unwrap_or(key)),
        ),
        ("properties".into(), Value::Object(properties)),
    ]);
    if let Some(abstract_) = fields.get("abstract") {
        object.insert("description".into(), json!(abstract_));
    }
    parse_entity(index, object, &mut Vec::new(), &[])
}

impl FormatAdapter for BibtexFormatAdapter {
    fn name(&self) -> &str {
        "bibtex"
    }

    fn entities(&mut self) -> impl Iterator<Item = Result<EntityRecord, AdapterError>> {
        self.entities.drain(..).map(Ok)
    }

    fn edges(&mut self) -> impl Iterator<Item = Result<EdgeRecord, AdapterError>> {
        self.edges.drain(..).map(Ok)
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expanded_value_budget_is_shared_across_fields_and_checked_before_append() {
        let source = b"@string{v={1234}} @book{k,title=v,year=v}";
        let limits = Limits {
            expanded_bytes: 8,
            ..Limits::default()
        };
        assert!(BibtexFormatAdapter::read_with_limits(source.as_slice(), limits).is_ok());
        let error = BibtexFormatAdapter::read_with_limits(
            source.as_slice(),
            Limits {
                expanded_bytes: 7,
                ..limits
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("expanded entry"));
        let mut budget = 3;
        let mut macros = HashMap::new();
        macros.insert(Variable::new("v".to_string()).unwrap(), "1234".to_string());
        assert!(matches!(
            expand(vec![RawToken::Variable("v".into())], &macros, &mut budget),
            Err(ExpansionError::Limit)
        ));
    }

    #[test]
    fn macro_budget_counts_names_and_values_and_releases_replaced_entries() {
        let source = b"@string{v={1234}} @string{V={5678}} @book{k,title=v}";
        let limits = Limits {
            macro_bytes: 5,
            ..Limits::default()
        };
        let mut adapter = BibtexFormatAdapter::read_with_limits(source.as_slice(), limits).unwrap();
        assert_eq!(adapter.entities().next().unwrap().unwrap().name, "5678");
        let error = BibtexFormatAdapter::read_with_limits(
            source.as_slice(),
            Limits {
                macro_bytes: 4,
                ..limits
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("macro state"));
        let expansion = b"@string{a={1234}} @string{b=a#a#a#a}";
        let error = BibtexFormatAdapter::read_with_limits(
            expansion.as_slice(),
            Limits {
                macro_bytes: 20,
                ..limits
            },
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("macro state"));
    }

    #[test]
    fn framed_valid_syntax_decodes_with_the_upstream_parser() {
        let source =
            b"@comment(% {)} text) @string{v={one}} @book(k,title={nested {value}},year=2026)";
        let mut frames = BibtexFrames::new(source.as_slice(), Limits::default());
        let mut count = 0;
        while let Some(frame) = frames.next_frame().unwrap() {
            assert!(frame.complete);
            let parsed: Vec<RawEntry> = serde_bibtex::from_str(&frame.source).unwrap();
            assert_eq!(parsed.len(), 1);
            count += 1;
        }
        assert_eq!(count, 3);
    }
}
