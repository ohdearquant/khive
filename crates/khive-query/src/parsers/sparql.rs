//! SPARQL-inspired syntax parser.
//!
//! Parses a practical subset of SPARQL into the same AST as the GQL parser,
//! so the SQL compiler works unchanged.
//!
//! Supported:
//!   SELECT ?a ?b WHERE { ?a a :concept . ?a :extends+ ?b . } LIMIT 10
//!   SELECT ?a WHERE { ?a :name "LoRA" . ?a :extends{1,3} ?b . } LIMIT 5
//!
//! Grammar:
//!   query     = SELECT vars WHERE '{' triples '}' [LIMIT number]
//!   vars      = var+
//!   var       = '?' ident
//!   triples   = triple ('.' triple)* '.'?
//!   triple    = var predicate object
//!   predicate = 'a' | ':' ident [path_mod]
//!   path_mod  = '+' | '*' | '?' | '{' min ',' max '}'
//!   object    = var | ':' ident | string_literal | number

use crate::ast::*;
use crate::error::QueryError;
use std::collections::HashMap;

struct Triple {
    subject: String,
    predicate: Predicate,
    object: Object,
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
    NumberLiteral(f64),
}

struct SparqlParser {
    input: Vec<char>,
    pos: usize,
}

impl SparqlParser {
    fn new(input: &str) -> Self {
        Self {
            input: input.chars().collect(),
            pos: 0,
        }
    }

    fn err(&self, msg: impl Into<String>) -> QueryError {
        QueryError::Parse {
            position: self.pos,
            message: msg.into(),
        }
    }

    fn peek(&self) -> Option<char> {
        self.input.get(self.pos).copied()
    }

    fn advance(&mut self) -> Option<char> {
        let c = self.input.get(self.pos).copied();
        if c.is_some() {
            self.pos += 1;
        }
        c
    }

    fn skip_whitespace(&mut self) {
        while let Some(c) = self.peek() {
            if c.is_whitespace() {
                self.advance();
            } else {
                break;
            }
        }
    }

    fn expect_char(&mut self, expected: char) -> Result<(), QueryError> {
        self.skip_whitespace();
        match self.advance() {
            Some(c) if c == expected => Ok(()),
            Some(c) => Err(self.err(format!("expected '{expected}', got '{c}'"))),
            None => Err(self.err(format!("expected '{expected}', got end of input"))),
        }
    }

    fn try_keyword(&mut self, kw: &str) -> bool {
        self.skip_whitespace();
        let start = self.pos;
        let kw_upper = kw.to_uppercase();
        for expected_char in kw_upper.chars() {
            match self.advance() {
                Some(c) if c.to_uppercase().next() == Some(expected_char) => {}
                _ => {
                    self.pos = start;
                    return false;
                }
            }
        }
        if let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                self.pos = start;
                return false;
            }
        }
        true
    }

    fn expect_keyword(&mut self, kw: &str) -> Result<(), QueryError> {
        if !self.try_keyword(kw) {
            Err(self.err(format!("expected keyword '{kw}'")))
        } else {
            Ok(())
        }
    }

    fn parse_ident(&mut self) -> Result<String, QueryError> {
        self.skip_whitespace();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_alphanumeric() || c == '_' {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected identifier"));
        }
        Ok(self.input[start..self.pos].iter().collect())
    }

    fn parse_var(&mut self) -> Result<String, QueryError> {
        self.skip_whitespace();
        self.expect_char('?')?;
        self.parse_ident()
    }

    fn parse_number(&mut self) -> Result<usize, QueryError> {
        self.skip_whitespace();
        let start = self.pos;
        while let Some(c) = self.peek() {
            if c.is_ascii_digit() {
                self.advance();
            } else {
                break;
            }
        }
        if self.pos == start {
            return Err(self.err("expected number"));
        }
        let s: String = self.input[start..self.pos].iter().collect();
        s.parse()
            .map_err(|_| self.err(format!("invalid number: {s}")))
    }

    fn parse_string_literal(&mut self) -> Result<String, QueryError> {
        self.skip_whitespace();
        let quote = match self.advance() {
            Some(c @ ('\'' | '"')) => c,
            _ => return Err(self.err("expected string literal")),
        };
        let start = self.pos;
        while let Some(c) = self.advance() {
            if c == quote {
                return Ok(self.input[start..self.pos - 1].iter().collect());
            }
        }
        Err(self.err("unterminated string literal"))
    }

    fn parse_predicate(&mut self) -> Result<Predicate, QueryError> {
        self.skip_whitespace();

        // 'a' is shorthand for rdf:type
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

        // Path modifiers
        self.skip_whitespace();
        let (min_hops, max_hops) = match self.peek() {
            Some('+') => {
                self.advance();
                (1, 5)
            }
            Some('*') => {
                // SPARQL `*` means zero-or-more, but our recursive CTE seed
                // starts at depth 1 and cannot emit a depth-0 row that maps
                // the start node to itself. Reject explicitly until depth-0
                // is implemented — silently treating `*` as `+` would drop
                // valid matches.
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
                let n: f64 = s
                    .parse()
                    .map_err(|_| self.err(format!("invalid number: {s}")))?;
                Ok(Object::NumberLiteral(n))
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
                // Dot separator (optional before closing brace)
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

        let limit = if self.try_keyword("LIMIT") {
            Some(self.parse_number()?)
        } else {
            None
        };

        // Reconstruct graph pattern from triples.
        triples_to_ast(triples, return_items, limit)
    }
}

/// Reconstruct GQL-style AST from SPARQL triples.
///
/// Classifies triples into:
/// - Kind filters: `?a a :concept` → node kind
/// - Property filters: `?a :name "LoRA"` → node property
/// - Edge patterns: `?a :extends ?b` → directed edge between nodes
///
/// Then chains edge triples into a path pattern.
fn triples_to_ast(
    triples: Vec<Triple>,
    return_items: Vec<String>,
    limit: Option<usize>,
) -> Result<GqlQuery, QueryError> {
    let mut node_kinds: HashMap<String, String> = HashMap::new();
    let mut node_props: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut edges: Vec<(String, String, String, usize, usize)> = Vec::new(); // (src, tgt, rel, min, max)
    let mut where_conditions: Vec<Condition> = Vec::new();

    for triple in triples {
        match triple.predicate {
            Predicate::Type => {
                if let Object::Kind(kind) = triple.object {
                    node_kinds.insert(triple.subject, kind);
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
                Object::StringLiteral(val) => {
                    node_props
                        .entry(triple.subject)
                        .or_default()
                        .insert(name, val);
                }
                Object::NumberLiteral(val) => {
                    where_conditions.push(Condition {
                        variable: triple.subject,
                        property: name,
                        op: CompareOp::Eq,
                        value: ConditionValue::Number(val),
                    });
                }
                Object::Kind(val) => {
                    node_props
                        .entry(triple.subject)
                        .or_default()
                        .insert(name, val);
                }
            },
        }
    }

    if edges.is_empty() {
        return Err(QueryError::Parse {
            message: "no edge patterns found — need at least one :relation between variables"
                .into(),
            position: 0,
        });
    }

    // Chain edges into a path. Find the start node (appears as source but
    // not as target of any other edge).
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
        // Cycle — pick first source
        edges[0].0.clone()
    };

    // Walk the chain
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

    // SPARQL triples are conjunctive — every edge must be reachable from the
    // single start through the path walk. If any edge wasn't consumed, the
    // pattern is branched or disconnected and we cannot represent it in the
    // current single-path AST.
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

    // Collect all variables that appear in the path. Node-only constraints
    // on variables outside the path (kind filters, property filters) would be
    // silently dropped — reject instead.
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

    // Build AST pattern: alternating Node-Edge-Node
    let mut elements: Vec<PatternElement> = Vec::new();

    let first_var = &ordered_edges[0].0;
    elements.push(PatternElement::Node(NodePattern {
        variable: Some(first_var.clone()),
        kind: node_kinds.get(first_var).cloned(),
        properties: node_props.get(first_var).cloned().unwrap_or_default(),
    }));

    for (_, tgt, rel, min_hops, max_hops) in &ordered_edges {
        elements.push(PatternElement::Edge(EdgePattern {
            variable: None,
            relations: vec![rel.clone()],
            direction: EdgeDirection::Out,
            min_hops: *min_hops,
            max_hops: *max_hops,
        }));
        elements.push(PatternElement::Node(NodePattern {
            variable: Some(tgt.clone()),
            kind: node_kinds.get(tgt).cloned(),
            properties: node_props.get(tgt).cloned().unwrap_or_default(),
        }));
    }

    Ok(GqlQuery {
        pattern: MatchPattern { elements },
        where_clause: where_conditions,
        return_items,
        limit,
    })
}

pub fn parse(input: &str) -> Result<GqlQuery, QueryError> {
    let mut parser = SparqlParser::new(input.trim());
    parser.parse_query()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_two_node() {
        let q = parse("SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10").unwrap();
        assert_eq!(q.return_items, vec!["a", "b"]);
        assert_eq!(q.limit, Some(10));

        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(nodes[0].kind.as_deref(), Some("concept"));
        assert_eq!(nodes[0].variable.as_deref(), Some("a"));
    }

    #[test]
    fn variable_length_plus() {
        let q = parse("SELECT ?b WHERE { ?a :name 'LoRA' . ?a :extends+ ?b . }").unwrap();
        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(nodes[0].properties.get("name").unwrap(), "LoRA");

        let edges: Vec<_> = q.pattern.edges().collect();
        assert_eq!(edges[0].min_hops, 1);
        assert_eq!(edges[0].max_hops, 5);
    }

    #[test]
    fn explicit_range() {
        let q = parse("SELECT ?a ?b WHERE { ?a :extends{1,3} ?b . }").unwrap();
        let edges: Vec<_> = q.pattern.edges().collect();
        assert_eq!(edges[0].min_hops, 1);
        assert_eq!(edges[0].max_hops, 3);
    }

    #[test]
    fn three_node_chain() {
        let q =
            parse("SELECT ?a ?c WHERE { ?a :extends ?b . ?b :introduced_by ?c . ?c a :paper . }")
                .unwrap();
        assert_eq!(q.pattern.elements.len(), 5); // 3 nodes + 2 edges
        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(nodes.len(), 3);
        assert_eq!(nodes[2].kind.as_deref(), Some("paper"));
    }

    #[test]
    fn property_filter() {
        let q =
            parse("SELECT ?a WHERE { ?a a :concept . ?a :domain 'attention' . ?a :extends+ ?b . }")
                .unwrap();
        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(nodes[0].properties.get("domain").unwrap(), "attention");
    }

    #[test]
    fn disconnected_triples_rejected() {
        // Two separate edges with no shared variable — silently dropping the
        // second triple would change query semantics, so reject.
        let err = parse("SELECT ?a ?d WHERE { ?a :extends ?b . ?c :implements ?d . }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn branched_triples_rejected() {
        // `?a` has two outbound edges — branching, not a single path.
        let err =
            parse("SELECT ?a ?b ?c WHERE { ?a :extends ?b . ?a :implements ?c . }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported, got {err:?}"
        );
    }

    #[test]
    fn disconnected_kind_constraint_rejected() {
        // `?c a :concept` constrains a variable not on the edge path — must
        // not be silently dropped.
        let err = parse("SELECT ?a WHERE { ?a :extends ?b . ?c a :concept . }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported for disconnected kind constraint, got {err:?}"
        );
    }

    #[test]
    fn disconnected_property_constraint_rejected() {
        // `?c :name "LoRA"` constrains a variable not on the edge path.
        let err = parse("SELECT ?a WHERE { ?a :extends ?b . ?c :name 'LoRA' . }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "expected Unsupported for disconnected property constraint, got {err:?}"
        );
    }
}
