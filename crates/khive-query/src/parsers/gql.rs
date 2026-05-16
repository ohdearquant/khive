//! Hand-written recursive descent parser for GQL subset.
//!
//! Grammar:
//!   query     = 'MATCH' pattern ['WHERE' conditions] 'RETURN' items ['LIMIT' number]
//!   pattern   = node_pat (edge_pat node_pat)*
//!   node_pat  = '(' [var] [':' ident] [props] ')'
//!   edge_pat  = '-[' [var] [':' rels] [range] ']->' | '<-[' ... ']-' | '-[' ... ']-'
//!   rels      = ident ('|' ident)*
//!   range     = '*' number ['..' number]
//!   props     = '{' key ':' value (',' key ':' value)* '}'
//!   conditions = condition ('AND' condition)*
//!   condition  = var '.' prop op value
//!   items     = item (',' item)*
//!   item      = var | var '.' prop

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

    fn parse_props(&mut self) -> Result<HashMap<String, String>, QueryError> {
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
            let val = self.parse_string_literal()?;
            props.insert(key, val);
        }
        Ok(props)
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
                properties,
            });
        }

        // Variable name (optional — skip if next is ':' or '{')
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

        // Kind label
        self.skip_whitespace();
        if self.peek() == Some(':') {
            self.advance();
            kind = Some(self.parse_ident()?.to_lowercase());
        }

        // Properties
        self.skip_whitespace();
        if self.peek() == Some('{') {
            properties = self.parse_props()?;
        }

        self.expect_char(')')?;
        Ok(NodePattern {
            variable,
            kind,
            properties,
        })
    }

    fn parse_edge_pattern(&mut self) -> Result<EdgePattern, QueryError> {
        self.skip_whitespace();

        // Detect direction prefix
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

        // Relation types
        self.skip_whitespace();
        if self.peek() == Some(':') {
            self.advance();
            relations.push(self.parse_ident()?);
            while self.peek() == Some('|') {
                self.advance();
                relations.push(self.parse_ident()?);
            }
        }

        // Variable-length range
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

        // Direction suffix
        let direction = if self.peek() == Some('-') {
            self.advance();
            if self.peek() == Some('>') {
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
            }
        } else {
            direction_start
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
                } else {
                    Err(self.err("expected comparison operator"))
                }
            }
        }
    }

    fn parse_conditions(&mut self) -> Result<Vec<Condition>, QueryError> {
        let mut conditions = Vec::new();
        loop {
            self.skip_whitespace();
            let variable = self.parse_ident()?;
            self.expect_char('.')?;
            let property = self.parse_ident()?;
            let op = self.parse_compare_op()?;
            let value = self.parse_value()?;
            conditions.push(Condition {
                variable,
                property,
                op,
                value,
            });

            self.skip_whitespace();
            if !self.try_keyword("AND") {
                break;
            }
        }
        Ok(conditions)
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
            self.parse_conditions()?
        } else {
            Vec::new()
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

pub fn parse(input: &str) -> Result<GqlQuery, QueryError> {
    let mut parser = Parser::new(input.trim());
    parser.parse_query()
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
        assert_eq!(nodes[0].properties.get("name").unwrap(), "LoRA");

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
        assert_eq!(q.where_clause.len(), 1);
        assert_eq!(q.where_clause[0].variable, "b");
        assert_eq!(q.where_clause[0].property, "name");
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
}
