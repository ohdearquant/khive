//! SPARQL-inspired syntax parser producing the same AST as the GQL parser.

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

        self.skip_whitespace();
        if self.pos < self.input.len() {
            return Err(self.err(format!(
                "unexpected trailing input: '{}'",
                self.input[self.pos..].iter().collect::<String>()
            )));
        }

        // Reconstruct graph pattern from triples.
        triples_to_ast(triples, return_items, limit)
    }
}

/// Reconstruct GQL-style AST from SPARQL triples, chaining edges into a path pattern.
fn triples_to_ast(
    triples: Vec<Triple>,
    return_items: Vec<String>,
    limit: Option<usize>,
) -> Result<GqlQuery, QueryError> {
    let return_items: Vec<ReturnItem> =
        return_items.into_iter().map(ReturnItem::Variable).collect();
    let mut node_kinds: HashMap<String, String> = HashMap::new();
    let mut node_props: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut edges: Vec<(String, String, String, usize, usize)> = Vec::new(); // (src, tgt, rel, min, max)
    let mut where_cond_list: Vec<Condition> = Vec::new();

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
                    where_cond_list.push(Condition {
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

    // Fold the flat condition list into a left-associative AND tree.
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
    let mut first_props = node_props.get(first_var).cloned().unwrap_or_default();
    let first_entity_type = first_props.remove("entity_type");
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
        let tgt_entity_type = tgt_props.remove("entity_type");
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
        limit,
    })
}

/// Parse a SPARQL query string into a [`GqlQuery`] AST. Errors on invalid or unsupported syntax.
pub fn parse(input: &str) -> Result<GqlQuery, QueryError> {
    reject_sparql_write(input.trim())?;
    let mut parser = SparqlParser::new(input.trim());
    parser.parse_query()
}

/// Reject SPARQL update operations with an explicit, actionable error.
///
/// SPARQL 1.1 Update defines several write operations that must never reach
/// the compiler. Check for them before parsing so the rejection is deliberate
/// ("the query verb is read-only") rather than a generic "expected SELECT".
///
/// We skip the optional prologue (PREFIX / BASE declarations) and line comments
/// before inspecting the leading keyword so that `WITH <g> DELETE …` and
/// prologue-prefixed updates (`PREFIX ex: <…> INSERT DATA { … }`) are also
/// caught. Read-only forms (SELECT / ASK / CONSTRUCT / DESCRIBE) are untouched.
fn reject_sparql_write(input: &str) -> Result<(), QueryError> {
    let keyword = leading_keyword(input);
    match keyword.as_str() {
        "INSERT" | "DELETE" | "WITH" | "LOAD" | "CLEAR" | "CREATE" | "DROP" | "COPY" | "MOVE"
        | "ADD" => Err(QueryError::Unsupported(
            "the query verb is read-only; \
             to mutate the graph use: create, update, link, merge, delete"
                .into(),
        )),
        _ => Ok(()),
    }
}

/// Return the first meaningful query keyword, skipping SPARQL/GQL line comments
/// and the optional SPARQL prologue (zero or more PREFIX / BASE declarations).
/// Used by both the per-parser write guard and the public-path guard in
/// `language::parse_auto`.
pub(crate) fn leading_keyword(input: &str) -> String {
    let mut rest = input;
    loop {
        // Skip leading ASCII whitespace.
        rest = rest.trim_start();

        // Skip a line comment: # … \n
        if rest.starts_with('#') {
            rest = match rest.find('\n') {
                Some(pos) => &rest[pos + 1..],
                None => "",
            };
            continue;
        }

        // Peel off one PREFIX or BASE declaration and loop.
        let upper: String = rest
            .chars()
            .take(6)
            .flat_map(|c| c.to_uppercase())
            .collect();
        if upper.starts_with("PREFIX") || upper.starts_with("BASE") {
            // Advance past the keyword.
            let skip = if upper.starts_with("PREFIX") { 6 } else { 4 };
            rest = &rest[skip..];
            // Skip to end of the IRI in angle brackets, if present.
            if let Some(close) = rest.find('>') {
                rest = &rest[close + 1..];
            }
            continue;
        }

        // The next whitespace-delimited token is the operative keyword.
        return rest.split_whitespace().next().unwrap_or("").to_uppercase();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_two_node() {
        let q = parse("SELECT ?a ?b WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10").unwrap();
        assert_eq!(
            q.return_items,
            vec![
                ReturnItem::Variable("a".into()),
                ReturnItem::Variable("b".into())
            ]
        );
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

    #[test]
    fn rejects_trailing_input_after_limit() {
        let err = parse("SELECT ?a WHERE { ?a a :concept . ?a :extends ?b . } LIMIT 10 GARBAGE")
            .unwrap_err();
        assert!(
            err.to_string().contains("unexpected trailing input"),
            "expected trailing-input parse error, got {err}"
        );
    }

    #[test]
    fn rejects_trailing_input_without_limit() {
        let err = parse("SELECT ?a WHERE { ?a a :concept . ?a :extends ?b . } and then some")
            .unwrap_err();
        assert!(
            err.to_string().contains("unexpected trailing input"),
            "expected trailing-input parse error, got {err}"
        );
    }

    // --- Read-only invariant regression tests (#16) ---

    #[test]
    fn sparql_insert_data_rejected_with_readonly_message() {
        let err = parse("INSERT DATA { <a> :extends <b> }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "INSERT DATA must return Unsupported; got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("read-only"),
            "error must mention 'read-only'; got: {msg}"
        );
        assert!(
            msg.contains("create") && msg.contains("update") && msg.contains("delete"),
            "error must name the mutation verbs; got: {msg}"
        );
    }

    #[test]
    fn sparql_delete_data_rejected_with_readonly_message() {
        let err = parse("DELETE DATA { <a> :extends <b> }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "DELETE DATA must return Unsupported; got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "got: {msg}");
    }

    #[test]
    fn sparql_delete_where_rejected_with_readonly_message() {
        let err = parse("DELETE WHERE { ?s :extends ?o }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "DELETE WHERE must return Unsupported; got {err:?}"
        );
    }

    #[test]
    fn sparql_load_rejected_with_readonly_message() {
        let err = parse("LOAD <http://example.org/graph>").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "LOAD must return Unsupported; got {err:?}"
        );
    }

    #[test]
    fn sparql_select_still_compiles_after_write_guard() {
        // Positive control: a valid SELECT must still parse correctly.
        let q = parse("SELECT ?a WHERE { ?a :extends ?b . }").unwrap();
        assert!(!q.pattern.elements.is_empty(), "valid SELECT must parse");
    }

    #[test]
    fn sparql_with_delete_where_rejected() {
        // WITH starts a SPARQL Update graph-management operation; the write
        // guard rejects it via the WITH keyword in its write set.
        let err = parse("WITH <http://g> DELETE { ?s ?p ?o } WHERE { ?s ?p ?o }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "WITH … DELETE … must return Unsupported; got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("read-only"),
            "error must mention 'read-only'; got: {msg}"
        );
        assert!(
            msg.contains("create") && msg.contains("update") && msg.contains("delete"),
            "error must name the mutation verbs; got: {msg}"
        );
    }

    #[test]
    fn sparql_prefixed_insert_data_rejected() {
        // A prologue-prefixed INSERT DATA must also be caught.
        let err =
            parse("PREFIX ex: <http://example.org/> INSERT DATA { ex:a ex:b ex:c }").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "prefixed INSERT DATA must return Unsupported; got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "got: {msg}");
    }

    #[test]
    fn sparql_clear_graph_rejected() {
        let err = parse("CLEAR GRAPH <http://example.org/graph>").unwrap_err();
        assert!(
            matches!(err, QueryError::Unsupported(_)),
            "CLEAR GRAPH must return Unsupported; got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "got: {msg}");
    }

    #[test]
    fn sparql_prefixed_select_write_guard_passes_through() {
        // Positive control: the write guard must NOT trip on a prefixed SELECT.
        // The underlying parser does not support PREFIX prologues, so this
        // returns a Parse error — but it must NOT return Unsupported.
        let err =
            parse("PREFIX ex: <http://example.org/> SELECT ?s WHERE { ?s ?p ?o }").unwrap_err();
        assert!(
            !matches!(err, QueryError::Unsupported(_)),
            "prefixed SELECT must not be rejected by the write guard; got {err:?}"
        );
    }
}
