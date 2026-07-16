//! Hand-written recursive descent parser for GQL subset.

use crate::ast::*;
use crate::error::QueryError;
use std::collections::HashMap;

struct Parser {
    input: Vec<char>,
    pos: usize,
}

impl Parser {
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

    fn parse_value(&mut self) -> Result<ConditionValue, QueryError> {
        self.skip_whitespace();
        match self.peek() {
            Some('\'' | '"') => Ok(ConditionValue::String(self.parse_string_literal()?)),
            Some(c) if c.is_ascii_digit() || c == '-' => {
                let start = self.pos;
                if c == '-' {
                    self.advance();
                }
                // Enforce digits on both sides; `f64::parse` would accept `1.` and `-.5`.
                let int_start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                    self.advance();
                }
                if self.pos == int_start {
                    return Err(self.err("expected digit after '-'"));
                }
                let mut has_frac = false;
                if self.peek() == Some('.') {
                    has_frac = true;
                    self.advance();
                    let frac_start = self.pos;
                    while matches!(self.peek(), Some(c) if c.is_ascii_digit()) {
                        self.advance();
                    }
                    if self.pos == frac_start {
                        return Err(self.err(
                            "float literal must have digits after '.' (e.g. '1.0', not '1.')",
                        ));
                    }
                }
                let s: String = self.input[start..self.pos].iter().collect();
                // Preserve exact i64 values instead of rounding through f64 past 2^53.
                if !has_frac {
                    let n: i64 = s.parse().map_err(|_| {
                        self.err(format!(
                            "integer literal '{s}' out of supported range \
                             ({} to {})",
                            i64::MIN,
                            i64::MAX
                        ))
                    })?;
                    return Ok(ConditionValue::Integer(n));
                }
                let n: f64 = s
                    .parse()
                    .map_err(|_| self.err(format!("invalid number: {s}")))?;
                if !n.is_finite() {
                    return Err(self.err(format!("float literal '{s}' is not finite")));
                }
                Ok(ConditionValue::Number(n))
            }
            _ => {
                let ident = self.parse_ident()?;
                match ident.to_lowercase().as_str() {
                    "true" => Ok(ConditionValue::Bool(true)),
                    "false" => Ok(ConditionValue::Bool(false)),
                    _ => Err(self.err(format!("unexpected value: {ident}"))),
                }
            }
        }
    }

    fn parse_props(&mut self) -> Result<HashMap<String, ConditionValue>, QueryError> {
        self.expect_char('{')?;
        let mut props = HashMap::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some('}') {
                self.advance();
                break;
            }
            if !props.is_empty() {
                self.expect_char(',')?;
            }
            let key = self.parse_ident()?;
            self.expect_char(':')?;
            let val = self.parse_value()?;
            if props.insert(key.clone(), val).is_some() {
                return Err(self.err(format!("duplicate property '{key}'")));
            }
        }
        Ok(props)
    }

    fn parse_list_literal(&mut self) -> Result<Vec<ConditionValue>, QueryError> {
        self.expect_char('[')?;
        let mut values = Vec::new();
        loop {
            self.skip_whitespace();
            if self.peek() == Some(']') {
                self.advance();
                return Ok(values);
            }
            if !values.is_empty() {
                self.expect_char(',')?;
            }
            values.push(self.parse_value()?);
        }
    }

    fn parse_node_pattern(&mut self) -> Result<NodePattern, QueryError> {
        self.expect_char('(')?;
        self.skip_whitespace();

        let mut variable = None;
        let mut kind = None;
        let mut properties = HashMap::new();

        if self.peek() == Some(')') {
            self.advance();
            return Ok(NodePattern {
                variable,
                kind,
                entity_type: None,
                properties,
            });
        }

        if let Some(c) = self.peek() {
            if c.is_alphabetic() || c == '_' {
                let start = self.pos;
                let ident = self.parse_ident()?;
                self.skip_whitespace();
                if self.peek() == Some(':') || self.peek() == Some(')') || self.peek() == Some('{')
                {
                    variable = Some(ident);
                } else {
                    self.pos = start;
                }
            }
        }

        self.skip_whitespace();
        if self.peek() == Some(':') {
            self.advance();
            kind = Some(self.parse_ident()?.to_lowercase());
        }

        self.skip_whitespace();
        if self.peek() == Some('{') {
            properties = self.parse_props()?;
        }

        // `entity_type` has a governed column and must not become arbitrary JSON.
        let entity_type = match properties.remove("entity_type") {
            Some(ConditionValue::String(s)) => Some(s),
            Some(_) => return Err(self.err("entity_type must be a string literal")),
            None => None,
        };

        self.expect_char(')')?;
        Ok(NodePattern {
            variable,
            kind,
            entity_type,
            properties,
        })
    }

    fn parse_edge_pattern(&mut self) -> Result<EdgePattern, QueryError> {
        self.skip_whitespace();

        let direction_start = if self.peek() == Some('<') {
            self.advance(); // '<'
            self.expect_char('-')?;
            EdgeDirection::In
        } else if self.peek() == Some('-') {
            self.advance(); // '-'
            EdgeDirection::Out // tentative — could be Both
        } else {
            return Err(self.err("expected edge pattern (- or <-)"));
        };

        self.expect_char('[')?;

        let mut variable = None;
        let mut relations = Vec::new();
        let mut min_hops: usize = 1;
        let mut max_hops: usize = 1;

        self.skip_whitespace();
        if self.peek() != Some(']') && self.peek() != Some(':') && self.peek() != Some('*') {
            variable = Some(self.parse_ident()?);
        }

        self.skip_whitespace();
        if self.peek() == Some(':') {
            self.advance();
            relations.push(self.parse_ident()?);
            while self.peek() == Some('|') {
                self.advance();
                relations.push(self.parse_ident()?);
            }
        }

        self.skip_whitespace();
        if self.peek() == Some('*') {
            self.advance();
            self.skip_whitespace();
            if self.peek().map(|c| c.is_ascii_digit()).unwrap_or(false) {
                min_hops = self.parse_number()?;
                self.skip_whitespace();
                if self.peek() == Some('.') {
                    self.advance();
                    self.expect_char('.')?;
                    max_hops = self.parse_number()?;
                } else {
                    max_hops = min_hops;
                }
            } else {
                min_hops = 1;
                max_hops = 5; // default unbounded cap
            }
        }

        self.expect_char(']')?;

        // Require the closing dash so direction is never inferred from malformed syntax.
        self.expect_char('-')?;
        let direction = if self.peek() == Some('>') {
            self.advance();
            if direction_start == EdgeDirection::In {
                EdgeDirection::Both
            } else {
                EdgeDirection::Out
            }
        } else {
            if direction_start == EdgeDirection::In {
                EdgeDirection::In
            } else {
                EdgeDirection::Both
            }
        };

        Ok(EdgePattern {
            variable,
            relations,
            direction,
            min_hops,
            max_hops,
        })
    }

    fn parse_pattern(&mut self) -> Result<MatchPattern, QueryError> {
        let mut elements = Vec::new();

        elements.push(PatternElement::Node(self.parse_node_pattern()?));

        loop {
            self.skip_whitespace();
            match self.peek() {
                Some('-') | Some('<') => {
                    elements.push(PatternElement::Edge(self.parse_edge_pattern()?));
                    elements.push(PatternElement::Node(self.parse_node_pattern()?));
                }
                _ => break,
            }
        }

        Ok(MatchPattern { elements })
    }

    fn parse_compare_op(&mut self) -> Result<CompareOp, QueryError> {
        self.skip_whitespace();
        match self.peek() {
            Some('=') => {
                self.advance();
                Ok(CompareOp::Eq)
            }
            Some('!') => {
                self.advance();
                self.expect_char('=')?;
                Ok(CompareOp::Neq)
            }
            Some('>') => {
                self.advance();
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(CompareOp::Gte)
                } else {
                    Ok(CompareOp::Gt)
                }
            }
            Some('<') => {
                self.advance();
                if self.peek() == Some('=') {
                    self.advance();
                    Ok(CompareOp::Lte)
                } else {
                    Ok(CompareOp::Lt)
                }
            }
            _ => {
                if self.try_keyword("LIKE") {
                    Ok(CompareOp::Like)
                } else if self.try_keyword("CONTAINS") {
                    Ok(CompareOp::Contains)
                } else if self.try_keyword("STARTS") {
                    self.expect_keyword("WITH")?;
                    Ok(CompareOp::StartsWith)
                } else if self.try_keyword("IN") {
                    Ok(CompareOp::In)
                } else if self.try_keyword("IS") {
                    self.expect_keyword("NOT")?;
                    self.expect_keyword("NULL")?;
                    Ok(CompareOp::IsNotNull)
                } else {
                    Err(self.err("expected comparison operator"))
                }
            }
        }
    }

    fn parse_condition(&mut self) -> Result<Condition, QueryError> {
        self.skip_whitespace();
        let variable = self.parse_ident()?;
        self.expect_char('.')?;
        let property = self.parse_ident()?;
        let op = self.parse_compare_op()?;
        let value = match op {
            CompareOp::In => ConditionValue::List(self.parse_list_literal()?),
            CompareOp::IsNotNull => ConditionValue::Null,
            CompareOp::Contains | CompareOp::StartsWith => {
                let value = self.parse_value()?;
                if !matches!(value, ConditionValue::String(_)) {
                    return Err(self.err("CONTAINS and STARTS WITH require a string literal"));
                }
                value
            }
            _ => self.parse_value()?,
        };
        Ok(Condition {
            variable,
            property,
            op,
            value,
        })
    }

    /// Parses one AND-chain.
    fn parse_and_expr(&mut self) -> Result<WhereExpr, QueryError> {
        let first = WhereExpr::Condition(self.parse_condition()?);
        let mut acc = first;
        loop {
            self.skip_whitespace();
            if !self.try_keyword("AND") {
                break;
            }
            let rhs = WhereExpr::Condition(self.parse_condition()?);
            acc = WhereExpr::And(Box::new(acc), Box::new(rhs));
        }
        Ok(acc)
    }

    /// Parses OR-separated AND-chains, giving AND higher precedence.
    fn parse_where_expr(&mut self) -> Result<WhereExpr, QueryError> {
        let first = self.parse_and_expr()?;
        let mut acc = first;
        loop {
            self.skip_whitespace();
            if !self.try_keyword("OR") {
                break;
            }
            let rhs = self.parse_and_expr()?;
            acc = WhereExpr::Or(Box::new(acc), Box::new(rhs));
        }
        Ok(acc)
    }

    fn parse_return_items(&mut self) -> Result<Vec<ReturnItem>, QueryError> {
        let mut items = Vec::new();
        items.push(self.parse_return_item()?);
        loop {
            self.skip_whitespace();
            if self.peek() == Some(',') {
                self.advance();
                items.push(self.parse_return_item()?);
            } else {
                break;
            }
        }
        Ok(items)
    }

    fn parse_return_item(&mut self) -> Result<ReturnItem, QueryError> {
        let ident = self.parse_ident()?;
        if self.peek() == Some('.') {
            self.advance();
            let prop = self.parse_ident()?;
            Ok(ReturnItem::Property(ident, prop))
        } else {
            Ok(ReturnItem::Variable(ident))
        }
    }

    fn parse_query(&mut self) -> Result<GqlQuery, QueryError> {
        self.expect_keyword("MATCH")?;
        let pattern = self.parse_pattern()?;

        let where_clause = if self.try_keyword("WHERE") {
            self.parse_where_expr()?
        } else {
            WhereExpr::True
        };

        self.expect_keyword("RETURN")?;
        let return_items = self.parse_return_items()?;

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

        Ok(GqlQuery {
            pattern,
            where_clause,
            return_items,
            limit,
        })
    }
}

/// Parses the supported read-only GQL subset into a [`GqlQuery`].
///
/// # Errors
///
/// Returns [`QueryError`] for invalid, write-shaped, or unsupported syntax.
/// See `crates/khive-query/docs/api/parsing.md` for grammar and literal rules.
pub fn parse(input: &str) -> Result<GqlQuery, QueryError> {
    reject_gql_write(input.trim())?;
    let mut parser = Parser::new(input.trim());
    parser.parse_query()
}

/// Deliberately rejects GQL/Cypher writes before the read grammar runs.
fn reject_gql_write(input: &str) -> Result<(), QueryError> {
    let first = input.split_whitespace().next().unwrap_or("").to_uppercase();
    match first.as_str() {
        "CREATE" | "DELETE" | "DETACH" | "SET" | "REMOVE" | "MERGE" | "INSERT" | "UPDATE" => {
            Err(QueryError::Unsupported(
                "the query verb is read-only; \
                 to mutate the graph use: create, update, link, merge, delete"
                    .into(),
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_two_node_pattern() {
        let q = parse("MATCH (a:concept)-[e:introduced_by]->(b:paper) RETURN a, e, b").unwrap();
        assert_eq!(q.pattern.elements.len(), 3);
        assert_eq!(
            q.return_items,
            vec![
                ReturnItem::Variable("a".into()),
                ReturnItem::Variable("e".into()),
                ReturnItem::Variable("b".into()),
            ]
        );

        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(nodes[0].kind.as_deref(), Some("concept"));
        assert_eq!(nodes[1].kind.as_deref(), Some("paper"));

        let edges: Vec<_> = q.pattern.edges().collect();
        assert_eq!(edges[0].relations, vec!["introduced_by"]);
        assert_eq!(edges[0].direction, EdgeDirection::Out);
    }

    #[test]
    fn variable_length_with_multiple_relations() {
        let q = parse("MATCH (a {name: 'LoRA'})-[:extends|variant_of*1..3]->(b) RETURN b LIMIT 20")
            .unwrap();
        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(
            nodes[0].properties.get("name").unwrap(),
            &ConditionValue::String("LoRA".into())
        );

        let edges: Vec<_> = q.pattern.edges().collect();
        assert_eq!(edges[0].relations, vec!["extends", "variant_of"]);
        assert_eq!(edges[0].min_hops, 1);
        assert_eq!(edges[0].max_hops, 3);
        assert_eq!(q.limit, Some(20));
    }

    #[test]
    fn where_clause() {
        let q = parse(
            "MATCH (a)-[e:implements]->(b:project) WHERE b.name = 'lattice-inference' RETURN a LIMIT 10"
        ).unwrap();
        let conds: Vec<_> = q.where_clause.conditions().collect();
        assert_eq!(conds.len(), 1);
        assert_eq!(conds[0].variable, "b");
        assert_eq!(conds[0].property, "name");
    }

    #[test]
    fn where_clause_and() {
        let q = parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' AND b.kind = 'concept' RETURN a, b"
        ).unwrap();
        let conds: Vec<_> = q.where_clause.conditions().collect();
        assert_eq!(conds.len(), 2, "AND should produce two leaf conditions");
        assert!(
            matches!(&q.where_clause, WhereExpr::And(_, _)),
            "should be And node"
        );
    }

    #[test]
    fn where_clause_or() {
        let q = parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'LoRA' OR a.name = 'QLoRA' RETURN a",
        )
        .unwrap();
        let conds: Vec<_> = q.where_clause.conditions().collect();
        assert_eq!(conds.len(), 2, "OR should produce two leaf conditions");
        assert!(
            matches!(&q.where_clause, WhereExpr::Or(_, _)),
            "should be Or node"
        );
    }

    #[test]
    fn where_clause_and_or() {
        let q = parse(
            "MATCH (a:concept)-[e:extends]->(b) WHERE a.name = 'X' AND a.kind = 'concept' OR b.kind = 'project' RETURN a"
        ).unwrap();
        let conds: Vec<_> = q.where_clause.conditions().collect();
        assert_eq!(conds.len(), 3);
        assert!(
            matches!(&q.where_clause, WhereExpr::Or(_, _)),
            "top-level should be Or"
        );
    }

    #[test]
    fn where_clause_extended_operators() {
        let q = parse(
            "MATCH (n:entity) WHERE n.name CONTAINS '%_' AND n.name STARTS WITH 'pre' \
             AND n.kind IN ['concept', 'document'] AND n.domain IS NOT NULL RETURN n",
        )
        .unwrap();
        let conds: Vec<_> = q.where_clause.conditions().collect();
        assert_eq!(conds.len(), 4);
        assert_eq!(conds[0].op, CompareOp::Contains);
        assert_eq!(conds[0].value, ConditionValue::String("%_".into()));
        assert_eq!(conds[1].op, CompareOp::StartsWith);
        assert_eq!(conds[2].op, CompareOp::In);
        assert_eq!(
            conds[2].value,
            ConditionValue::List(vec![
                ConditionValue::String("concept".into()),
                ConditionValue::String("document".into()),
            ])
        );
        assert_eq!(conds[3].op, CompareOp::IsNotNull);
        assert_eq!(conds[3].value, ConditionValue::Null);
    }

    #[test]
    fn where_in_accepts_scalar_list_and_empty_list() {
        let q = parse("MATCH (n) WHERE n.value IN ['x', 1, 2.5, true] OR n.value IN [] RETURN n")
            .unwrap();
        let conds: Vec<_> = q.where_clause.conditions().collect();
        assert_eq!(
            conds[0].value,
            ConditionValue::List(vec![
                ConditionValue::String("x".into()),
                ConditionValue::Integer(1),
                ConditionValue::Number(2.5),
                ConditionValue::Bool(true),
            ])
        );
        assert_eq!(conds[1].value, ConditionValue::List(vec![]));
    }

    #[test]
    fn contains_and_starts_with_require_strings() {
        let contains = parse("MATCH (n) WHERE n.name CONTAINS 1 RETURN n").unwrap_err();
        assert!(contains.to_string().contains("require a string literal"));

        let starts = parse("MATCH (n) WHERE n.name STARTS WITH true RETURN n").unwrap_err();
        assert!(starts.to_string().contains("require a string literal"));
    }

    #[test]
    fn inbound_edge() {
        let q = parse("MATCH (a:paper)<-[e:introduced_by]-(b:concept) RETURN a, b").unwrap();
        let edges: Vec<_> = q.pattern.edges().collect();
        assert_eq!(edges[0].direction, EdgeDirection::In);
    }

    #[test]
    fn undirected_edge() {
        let q = parse("MATCH (a)-[e:competes_with]-(b) RETURN a, b").unwrap();
        let edges: Vec<_> = q.pattern.edges().collect();
        assert_eq!(edges[0].direction, EdgeDirection::Both);
    }

    #[test]
    fn three_node_chain() {
        let q = parse(
            "MATCH (a:concept)-[:introduced_by]->(p:paper)-[:introduced_by]->(c:concept) RETURN a, c"
        ).unwrap();
        assert_eq!(q.pattern.elements.len(), 5);
        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(nodes.len(), 3);
    }

    #[test]
    fn node_pattern_entity_type_lifted_from_properties() {
        let q = parse("MATCH (n:document {entity_type: 'paper'}) RETURN n").unwrap();
        let nodes: Vec<_> = q.pattern.nodes().collect();
        assert_eq!(
            nodes[0].entity_type.as_deref(),
            Some("paper"),
            "entity_type must be lifted into NodePattern.entity_type"
        );
        assert!(
            !nodes[0].properties.contains_key("entity_type"),
            "entity_type must be removed from the properties map after lifting"
        );
    }

    #[test]
    fn gql_create_rejected_with_readonly_message() {
        let err = parse("CREATE (n:concept {name: 'X'}) RETURN n").unwrap_err();
        assert!(
            matches!(err, crate::error::QueryError::Unsupported(_)),
            "GQL CREATE must return Unsupported; got {err:?}"
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
    fn gql_delete_rejected_with_readonly_message() {
        let err = parse("DELETE (n) WHERE n.kind = 'concept'").unwrap_err();
        assert!(
            matches!(err, crate::error::QueryError::Unsupported(_)),
            "GQL DELETE must return Unsupported; got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("read-only"), "got: {msg}");
    }

    #[test]
    fn gql_set_rejected_with_readonly_message() {
        let err = parse("SET (n:concept) RETURN n").unwrap_err();
        assert!(
            matches!(err, crate::error::QueryError::Unsupported(_)),
            "GQL SET must return Unsupported; got {err:?}"
        );
    }

    #[test]
    fn gql_merge_rejected_with_readonly_message() {
        let err = parse("MERGE (n:concept {name: 'X'}) RETURN n").unwrap_err();
        assert!(
            matches!(err, crate::error::QueryError::Unsupported(_)),
            "GQL MERGE must return Unsupported; got {err:?}"
        );
    }

    #[test]
    fn gql_match_still_compiles_after_write_guard() {
        let q = parse("MATCH (a:concept)-[:extends]->(b) RETURN a").unwrap();
        assert!(!q.pattern.elements.is_empty(), "valid MATCH must parse");
    }

    #[test]
    fn gql_detach_delete_rejected() {
        let err = parse("DETACH DELETE (n)").unwrap_err();
        assert!(
            matches!(err, crate::error::QueryError::Unsupported(_)),
            "DETACH DELETE must return Unsupported; got {err:?}"
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
}
