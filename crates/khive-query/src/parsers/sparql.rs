//! SPARQL-inspired syntax parser producing the same AST as the GQL parser.

use super::lexer::Cursor;
use crate::ast::*;
use crate::error::QueryError;
use crate::language::{read_only_error, DEFAULT_HOP_CAP, SPARQL_WRITE_KEYWORDS};
use std::collections::{BTreeMap, HashMap};

struct Triple {
    subject: String,
    predicate: Predicate,
    object: Object,
}

/// A repeated SPARQL triple is conjunctive, not a last-write-wins map update.
fn insert_string_property_constraint(
    node_props: &mut HashMap<String, HashMap<String, ConditionValue>>,
    subject: String,
    name: String,
    value: String,
) -> Result<(), QueryError> {
    let constraint = ConditionValue::String(value);
    let props = node_props.entry(subject.clone()).or_default();
    if let Some(existing) = props.get(&name) {
        let equivalent = match (existing, &constraint) {
            (ConditionValue::String(left), ConditionValue::String(right))
                if name != "entity_type" =>
            {
                left.eq_ignore_ascii_case(right)
            }
            _ => existing == &constraint,
        };
        if !equivalent {
            return Err(QueryError::Parse {
                position: 0,
                message: format!(
                    "conflicting constraints on '?{subject} :{name}'; repeated predicates must agree"
                ),
            });
        }
    } else {
        props.insert(name, constraint);
    }
    Ok(())
}

enum Predicate {
    Type,
    Relation {
        name: String,
        min_hops: usize,
        max_hops: usize,
    },
}

enum Object {
    Variable(String),
    Kind(String),
    StringLiteral(String),
    NumberLiteral(ConditionValue),
}

struct SparqlParser {
    cursor: Cursor,
}

impl std::ops::Deref for SparqlParser {
    type Target = Cursor;

    fn deref(&self) -> &Self::Target {
        &self.cursor
    }
}

impl std::ops::DerefMut for SparqlParser {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cursor
    }
}

impl SparqlParser {
    fn new(input: &str) -> Self {
        Self {
            cursor: Cursor::new(input),
        }
    }

    fn parse_var(&mut self) -> Result<String, QueryError> {
        self.skip_whitespace();
        self.expect_char('?')?;
        self.parse_ident()
    }

    fn parse_predicate(&mut self) -> Result<Predicate, QueryError> {
        self.skip_whitespace();

        // SPARQL defines `a` as shorthand for rdf:type.
        if self.peek() == Some('a') {
            let start = self.pos;
            self.advance();
            if let Some(c) = self.peek() {
                if c.is_alphanumeric() || c == '_' {
                    self.pos = start;
                } else {
                    return Ok(Predicate::Type);
                }
            } else {
                return Ok(Predicate::Type);
            }
        }

        self.expect_char(':')?;
        let name = self.parse_ident()?;

        self.skip_whitespace();
        let (min_hops, max_hops) = match self.peek() {
            Some('+') => {
                self.advance();
                (1, DEFAULT_HOP_CAP)
            }
            Some('*') => {
                // The CTE cannot emit depth zero; treating `*` as `+` would lose matches.
                return Err(QueryError::Unsupported(
                    "SPARQL '*' (zero-or-more hops) not yet supported; use '+' or '{min,max}'"
                        .into(),
                ));
            }
            Some('{') => {
                self.advance();
                let min = self.parse_number()?;
                self.expect_char(',')?;
                let max = self.parse_number()?;
                self.expect_char('}')?;
                (min, max)
            }
            _ => (1, 1),
        };

        Ok(Predicate::Relation {
            name,
            min_hops,
            max_hops,
        })
    }

    fn parse_object(&mut self) -> Result<Object, QueryError> {
        self.skip_whitespace();
        match self.peek() {
            Some('?') => Ok(Object::Variable(self.parse_var()?)),
            Some(':') => {
                self.advance();
                Ok(Object::Kind(self.parse_ident()?.to_lowercase()))
            }
            Some('\'' | '"') => Ok(Object::StringLiteral(self.parse_string_literal()?)),
            Some(c) if c.is_ascii_digit() || c == '-' => {
                let start = self.pos;
                if c == '-' {
                    self.advance();
                }
                while let Some(c) = self.peek() {
                    if c.is_ascii_digit() || c == '.' {
                        self.advance();
                    } else {
                        break;
                    }
                }
                let s: String = self.input[start..self.pos].iter().collect();
                // Keep representable integer spellings exact without changing the float fallback.
                if !s.contains('.') {
                    if let Ok(n) = s.parse::<i64>() {
                        return Ok(Object::NumberLiteral(ConditionValue::Integer(n)));
                    }
                }
                let n: f64 = s
                    .parse()
                    .map_err(|_| self.err(format!("invalid number: {s}")))?;
                if !n.is_finite() {
                    return Err(self.err(format!("float literal '{s}' is not finite")));
                }
                Ok(Object::NumberLiteral(ConditionValue::Number(n)))
            }
            _ => Err(self.err("expected variable (?x), kind (:concept), string, or number")),
        }
    }

    fn parse_triple(&mut self) -> Result<Triple, QueryError> {
        let subject = self.parse_var()?;
        let predicate = self.parse_predicate()?;
        let object = self.parse_object()?;
        Ok(Triple {
            subject,
            predicate,
            object,
        })
    }

    fn parse_query(&mut self) -> Result<GqlQuery, QueryError> {
        self.expect_keyword("SELECT")?;

        let mut return_items = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some('?') {
                return_items.push(self.parse_var()?);
            } else {
                break;
            }
        }
        if return_items.is_empty() {
            return Err(self.err("SELECT requires at least one variable"));
        }

        self.expect_keyword("WHERE")?;
        self.expect_char('{')?;

        let mut triples = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some('}') {
                self.advance();
                break;
            }
            if !triples.is_empty() {
                self.skip_whitespace();
                if self.peek() == Some('.') {
                    self.advance();
                    self.skip_whitespace();
                    if self.peek() == Some('}') {
                        self.advance();
                        break;
                    }
                }
            }
            triples.push(self.parse_triple()?);
        }

        let limit = self.parse_limit()?;

        self.expect_end()?;

        triples_to_ast(triples, return_items, limit)
    }
}

/// Reconstructs the single-path AST representable by the compiler.
fn triples_to_ast(
    triples: Vec<Triple>,
    return_items: Vec<String>,
    limit: Option<usize>,
) -> Result<GqlQuery, QueryError> {
    let return_items: Vec<ReturnItem> =
        return_items.into_iter().map(ReturnItem::Variable).collect();
    let mut substrate_kinds: BTreeMap<String, String> = BTreeMap::new();
    let mut granular_kinds: BTreeMap<String, String> = BTreeMap::new();
    let mut node_props: HashMap<String, HashMap<String, ConditionValue>> = HashMap::new();
    let mut edges: Vec<(String, String, String, usize, usize)> = Vec::new(); // (src, tgt, rel, min, max)
    let mut where_cond_list: Vec<Condition> = Vec::new();

    for triple in triples {
        match triple.predicate {
            Predicate::Type => {
                if let Object::Kind(kind) = triple.object {
                    let kinds = if matches!(kind.as_str(), "entity" | "note" | "event" | "edge") {
                        &mut substrate_kinds
                    } else {
                        &mut granular_kinds
                    };
                    if let Some(existing) = kinds.get(&triple.subject) {
                        if existing != &kind {
                            return Err(QueryError::Parse {
                                position: 0,
                                message: format!(
                                    "conflicting kind constraints on '?{}'; repeated kinds must agree",
                                    triple.subject
                                ),
                            });
                        }
                    } else {
                        kinds.insert(triple.subject, kind);
                    }
                } else {
                    return Err(QueryError::Parse {
                        message: "'a' predicate requires a kind object (:concept, :paper, etc.)"
                            .into(),
                        position: 0,
                    });
                }
            }
            Predicate::Relation {
                name,
                min_hops,
                max_hops,
            } => match triple.object {
                Object::Variable(target) => {
                    edges.push((triple.subject, target, name, min_hops, max_hops));
                }
                _ if min_hops != 1 || max_hops != 1 => {
                    return Err(QueryError::Unsupported(
                        "SPARQL property predicates require one-hop bounds (1,1); use a variable object for path traversal".into(),
                    ));
                }
                Object::StringLiteral(val) => {
                    insert_string_property_constraint(&mut node_props, triple.subject, name, val)?;
                }
                Object::NumberLiteral(val) => {
                    where_cond_list.push(Condition {
                        variable: triple.subject,
                        property: PropertyRef::JsonPath(vec![name]),
                        op: CompareOp::Eq,
                        value: val,
                    });
                }
                Object::Kind(val) => {
                    insert_string_property_constraint(&mut node_props, triple.subject, name, val)?;
                }
            },
        }
    }

    // The AST has one kind slot, so retain the substrate there and filter its granular kind.
    let mut node_kinds = substrate_kinds;
    for (variable, kind) in granular_kinds {
        match node_kinds.entry(variable) {
            std::collections::btree_map::Entry::Occupied(entry) => {
                where_cond_list.push(Condition {
                    variable: entry.key().clone(),
                    property: PropertyRef::Field("kind".into()),
                    op: CompareOp::Eq,
                    value: ConditionValue::String(kind),
                });
            }
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(kind);
            }
        }
    }

    // SPARQL triples are conjunctive, so fold conditions with AND.
    let where_conditions = where_cond_list
        .into_iter()
        .fold(WhereExpr::True, |acc, cond| {
            let leaf = WhereExpr::Condition(cond);
            match acc {
                WhereExpr::True => leaf,
                other => WhereExpr::And(Box::new(other), Box::new(leaf)),
            }
        });

    if edges.is_empty() {
        return Err(QueryError::Parse {
            message: "no edge patterns found — need at least one :relation between variables"
                .into(),
            position: 0,
        });
    }

    // A path starts at a source that is not another edge's target.
    let targets: std::collections::HashSet<&str> =
        edges.iter().map(|(_, t, _, _, _)| t.as_str()).collect();
    let sources: std::collections::HashSet<&str> =
        edges.iter().map(|(s, _, _, _, _)| s.as_str()).collect();

    let start_candidates: Vec<&str> = sources
        .iter()
        .filter(|s| !targets.contains(*s))
        .copied()
        .collect();

    if start_candidates.len() > 1 {
        return Err(QueryError::Unsupported(
            "SPARQL WHERE block has multiple disconnected components; \
             only single-path patterns are supported"
                .into(),
        ));
    }

    let start = if start_candidates.len() == 1 {
        start_candidates[0].to_string()
    } else {
        // Cycles have no unique source; validation later rejects repeated bindings.
        edges[0].0.clone()
    };

    let mut ordered_edges: Vec<(String, String, String, usize, usize)> = Vec::new();
    let mut current = start.clone();
    let mut used: Vec<bool> = vec![false; edges.len()];

    for _ in 0..edges.len() {
        let mut found = false;
        for (i, (src, tgt, rel, min, max)) in edges.iter().enumerate() {
            if !used[i] && src == &current {
                ordered_edges.push((src.clone(), tgt.clone(), rel.clone(), *min, *max));
                current = tgt.clone();
                used[i] = true;
                found = true;
                break;
            }
        }
        if !found {
            break;
        }
    }

    // Reject unconsumed conjuncts rather than silently dropping a branch or component.
    if used.iter().any(|consumed| !consumed) {
        return Err(QueryError::Unsupported(
            "SPARQL WHERE block is branched or disconnected; \
             only single-path patterns are supported"
                .into(),
        ));
    }

    if ordered_edges.is_empty() {
        return Err(QueryError::Parse {
            message: "could not chain edge patterns into a path".into(),
            position: 0,
        });
    }

    // Reject constraints outside the path rather than silently dropping them.
    let mut path_vars: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for (src, tgt, _, _, _) in &ordered_edges {
        path_vars.insert(src.as_str());
        path_vars.insert(tgt.as_str());
    }
    for var in node_kinds.keys() {
        if !path_vars.contains(var.as_str()) {
            return Err(QueryError::Unsupported(format!(
                "SPARQL variable '?{var}' has constraints (kind/property) but is not \
                 connected to the edge path; disconnected node constraints are not \
                 supported"
            )));
        }
    }
    for var in node_props.keys() {
        if !path_vars.contains(var.as_str()) {
            return Err(QueryError::Unsupported(format!(
                "SPARQL variable '?{var}' has constraints (kind/property) but is not \
                 connected to the edge path; disconnected node constraints are not \
                 supported"
            )));
        }
    }

    let mut elements: Vec<PatternElement> = Vec::new();

    let first_var = &ordered_edges[0].0;
    let mut first_props = node_props.get(first_var).cloned().unwrap_or_default();
    let first_entity_type = extract_entity_type(&mut first_props)?;
    elements.push(PatternElement::Node(NodePattern {
        variable: Some(first_var.clone()),
        kind: node_kinds.get(first_var).cloned(),
        entity_type: first_entity_type,
        properties: first_props,
    }));

    for (_, tgt, rel, min_hops, max_hops) in &ordered_edges {
        elements.push(PatternElement::Edge(EdgePattern {
            variable: None,
            relations: vec![rel.clone()],
            direction: EdgeDirection::Out,
            min_hops: *min_hops,
            max_hops: *max_hops,
        }));
        let mut tgt_props = node_props.get(tgt).cloned().unwrap_or_default();
        let tgt_entity_type = extract_entity_type(&mut tgt_props)?;
        elements.push(PatternElement::Node(NodePattern {
            variable: Some(tgt.clone()),
            kind: node_kinds.get(tgt).cloned(),
            entity_type: tgt_entity_type,
            properties: tgt_props,
        }));
    }

    Ok(GqlQuery {
        pattern: MatchPattern { elements },
        where_clause: where_conditions,
        return_items,
        // SPARQL OFFSET is intentionally outside the current dialect. Issue #1601
        // adds paging to the GQL surface without silently widening SPARQL syntax.
        offset: 0,
        limit,
    })
}

/// Lifts string-valued `entity_type` into its governed node field.
fn extract_entity_type(
    props: &mut HashMap<String, ConditionValue>,
) -> Result<Option<String>, QueryError> {
    match props.remove("entity_type") {
        Some(ConditionValue::String(s)) => Ok(Some(s)),
        Some(_) => Err(QueryError::Parse {
            message: "entity_type must be a string literal".into(),
            position: 0,
        }),
        None => Ok(None),
    }
}

/// Parses the supported read-only SPARQL subset into a [`GqlQuery`].
///
/// # Errors
///
/// Returns [`QueryError`] for invalid, write-shaped, disconnected, or unsupported syntax.
/// See `crates/khive-query/docs/api/parsing.md` for the supported subset.
pub fn parse(input: &str) -> Result<GqlQuery, QueryError> {
    reject_sparql_write(input.trim())?;
    let mut parser = SparqlParser::new(input.trim());
    parser.parse_query()
}

/// Rejects SPARQL Update after skipping comments and an optional prologue.
fn reject_sparql_write(input: &str) -> Result<(), QueryError> {
    let keyword = leading_keyword(input);
    if SPARQL_WRITE_KEYWORDS.contains(&keyword.as_str()) {
        Err(read_only_error(QueryError::Unsupported))
    } else {
        Ok(())
    }
}

/// Returns the operative keyword after comments and repeated PREFIX/BASE declarations.
pub(crate) fn leading_keyword(input: &str) -> String {
    let mut rest = input;
    loop {
        rest = rest.trim_start();

        if rest.starts_with('#') {
            rest = match rest.find('\n') {
                Some(pos) => &rest[pos + 1..],
                None => "",
            };
            continue;
        }

        let upper: String = rest
            .chars()
            .take(6)
            .flat_map(|c| c.to_uppercase())
            .collect();
        if upper.starts_with("PREFIX") || upper.starts_with("BASE") {
            let skip = if upper.starts_with("PREFIX") { 6 } else { 4 };
            rest = &rest[skip..];
            if let Some(close) = rest.find('>') {
                rest = &rest[close + 1..];
            }
            continue;
        }

        return rest.split_whitespace().next().unwrap_or("").to_uppercase();
    }
}

#[cfg(test)]
#[path = "sparql_tests.rs"]
mod tests;
