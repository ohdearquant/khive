//! Hand-written recursive descent parser for GQL subset.

use super::lexer::Cursor;
use crate::ast::*;
use crate::error::QueryError;
use crate::language::{read_only_error, DEFAULT_HOP_CAP, GQL_WRITE_KEYWORDS};
use std::collections::HashMap;

struct Parser {
    cursor: Cursor,
}

impl std::ops::Deref for Parser {
    type Target = Cursor;

    fn deref(&self) -> &Self::Target {
        &self.cursor
    }
}

impl std::ops::DerefMut for Parser {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.cursor
    }
}

impl Parser {
    fn new(input: &str) -> Self {
        Self {
            cursor: Cursor::new(input),
        }
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
                self.skip_ascii_digits();
                if self.pos == int_start {
                    return Err(self.err("expected digit after '-'"));
                }
                let mut has_frac = false;
                if self.peek() == Some('.') {
                    has_frac = true;
                    self.advance();
                    let frac_start = self.pos;
                    self.skip_ascii_digits();
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
            if !self.peek().is_some_and(|c| c.is_alphabetic() || c == '_') {
                return Err(self.err("edge variable must start with a letter or '_'"));
            }
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
                max_hops = DEFAULT_HOP_CAP; // default unbounded cap
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
                    if self.try_keyword("NOT") {
                        self.expect_keyword("NULL")?;
                        Ok(CompareOp::IsNotNull)
                    } else {
                        self.expect_keyword("NULL")?;
                        Ok(CompareOp::IsNull)
                    }
                } else {
                    Err(self.err("expected comparison operator"))
                }
            }
        }
    }

    fn parse_property_ref(&mut self) -> Result<PropertyRef, QueryError> {
        let root = self.parse_ident()?;
        self.skip_whitespace();
        if self.peek() != Some('.') {
            return Ok(PropertyRef::Field(root));
        }
        if root != "properties" {
            return Err(self.err(format!(
                "invalid WHERE property path root '{root}'; nested paths must start with 'properties'"
            )));
        }

        let mut path = Vec::new();
        while self.peek() == Some('.') {
            self.advance();
            path.push(self.parse_ident()?);
            self.skip_whitespace();
        }
        Ok(PropertyRef::JsonPath(path))
    }

    fn parse_condition(&mut self) -> Result<Condition, QueryError> {
        self.skip_whitespace();
        let variable = self.parse_ident()?;
        self.expect_char('.')?;
        let property = self.parse_property_ref()?;
        let op = self.parse_compare_op()?;
        let value = match op {
            CompareOp::In => ConditionValue::List(self.parse_list_literal()?),
            CompareOp::IsNotNull | CompareOp::IsNull => ConditionValue::Null,
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
        loop {
            items.push(self.parse_return_item()?);
            if self.try_keyword("AS") {
                return Err(QueryError::Unsupported(
                    "RETURN aliases (`AS`) are not supported; use unaliased variable or \
                     variable.property projections"
                        .into(),
                ));
            }

            self.skip_whitespace();
            if self.peek() == Some(',') {
                self.advance();
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
        self.skip_whitespace();
        if self.peek() == Some(',') {
            return Err(QueryError::Unsupported(
                "comma-separated MATCH patterns are not supported; use one alternating path per \
                 query; for parallel-edge audits, return a.id, e.id, e.relation, b.id and group \
                 by endpoint IDs client-side"
                    .into(),
            ));
        }

        let where_clause = if self.try_keyword("WHERE") {
            self.parse_where_expr()?
        } else {
            WhereExpr::True
        };

        self.expect_keyword("RETURN")?;
        let return_items = self.parse_return_items()?;

        let offset = if self.try_keyword("SKIP") {
            self.parse_number()?
        } else {
            0
        };

        let limit = self.parse_limit()?;

        self.expect_end()?;

        Ok(GqlQuery {
            pattern,
            where_clause,
            return_items,
            offset,
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
    if GQL_WRITE_KEYWORDS.contains(&first.as_str()) {
        Err(read_only_error(QueryError::Unsupported))
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[path = "gql_tests.rs"]
mod tests;
