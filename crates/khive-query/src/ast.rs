//! Shared query AST for GQL/SPARQL parsing and SQL compilation.

use std::collections::HashMap;

/// A backend-independent SQL parameter emitted by the compiler.
///
/// See `crates/khive-query/docs/api/ast.md` for value and precision semantics.
#[derive(Clone, Debug)]
pub enum QueryValue {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// A parsed query: pattern, predicates, projections, ordering, offset, and optional limit.
#[derive(Debug, Clone)]
pub struct GqlQuery {
    /// Required first path; single-path queries leave `additional_patterns` empty.
    pub pattern: MatchPattern,
    /// Connected canonical paths following commas in GQL's MATCH clause.
    pub additional_patterns: Vec<MatchPattern>,
    pub where_clause: WhereExpr,
    pub return_items: Vec<ReturnItem>,
    /// Deduplicate requested result values after grouping and before pagination.
    pub distinct: bool,
    /// Caller ordering followed by the compiler's deterministic tie order.
    ///
    /// An empty vector uses identity/traversal order for ordinary queries,
    /// and returned-value order for distinct or grouped queries.
    pub order_by: Vec<OrderItem>,
    /// Number of deterministically ordered matches to skip before returning rows.
    ///
    /// GQL supplies this through `SKIP`; SPARQL currently always sets it to zero.
    pub offset: usize,
    pub limit: Option<usize>,
}

impl GqlQuery {
    /// Iterate every MATCH path in textual order, including the required first path.
    pub fn patterns(&self) -> impl Iterator<Item = &MatchPattern> {
        std::iter::once(&self.pattern).chain(self.additional_patterns.iter())
    }

    /// Mutably iterate every MATCH path in textual order.
    pub fn patterns_mut(&mut self) -> impl Iterator<Item = &mut MatchPattern> {
        std::iter::once(&mut self.pattern).chain(self.additional_patterns.iter_mut())
    }
}

/// A WHERE expression tree that preserves AND/OR grouping.
#[derive(Debug, Clone)]
pub enum WhereExpr {
    /// AND of two sub-expressions.
    And(Box<WhereExpr>, Box<WhereExpr>),
    /// OR of two sub-expressions.
    Or(Box<WhereExpr>, Box<WhereExpr>),
    /// A single scalar condition.
    Condition(Condition),
    /// Absence of one fixed canonical path correlated to outer node bindings.
    /// Additional names are local to this block, not outer scalar conditions.
    NotExists(MatchPattern),
    /// Always-true — used when there is no WHERE clause.
    True,
}

impl WhereExpr {
    /// Iterate outer scalar conditions depth-first, skipping scoped absence paths.
    pub fn conditions(&self) -> impl Iterator<Item = &Condition> {
        let mut stack = vec![self];
        let mut out: Vec<&Condition> = Vec::new();
        while let Some(expr) = stack.pop() {
            match expr {
                WhereExpr::Condition(c) => out.push(c),
                WhereExpr::And(l, r) | WhereExpr::Or(l, r) => {
                    stack.push(r);
                    stack.push(l);
                }
                WhereExpr::True | WhereExpr::NotExists(_) => {}
            }
        }
        out.into_iter()
    }

    /// Apply `f` to each outer scalar condition, skipping scoped absence paths.
    pub fn for_each_condition_mut(&mut self, f: &mut impl FnMut(&mut Condition)) {
        match self {
            WhereExpr::Condition(c) => f(c),
            WhereExpr::And(l, r) | WhereExpr::Or(l, r) => {
                l.for_each_condition_mut(f);
                r.for_each_condition_mut(f);
            }
            WhereExpr::True | WhereExpr::NotExists(_) => {}
        }
    }

    /// Return `true` only for the explicit always-true expression.
    pub fn is_true(&self) -> bool {
        matches!(self, WhereExpr::True)
    }
}

/// A bound variable, property, typed JSON path, or binding count in RETURN.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnItem {
    Variable(String),
    Property(String, String),
    /// A named path below node properties or canonical edge metadata.
    Path(String, PropertyRef),
    /// Count matched rows for a binding; a null zero-hop edge contributes zero.
    Count(String),
}

impl ReturnItem {
    /// Returns the variable name bound to this return item.
    pub fn variable(&self) -> &str {
        match self {
            Self::Variable(v) | Self::Property(v, _) | Self::Path(v, _) | Self::Count(v) => v,
        }
    }
}

/// The MATCH pattern of a GQL query, as an alternating sequence of node and edge elements.
#[derive(Debug, Clone)]
pub struct MatchPattern {
    pub elements: Vec<PatternElement>,
}

impl MatchPattern {
    /// Iterate over the `NodePattern` elements in this MATCH pattern.
    pub fn nodes(&self) -> impl Iterator<Item = &NodePattern> {
        self.elements.iter().filter_map(|e| match e {
            PatternElement::Node(n) => Some(n),
            _ => None,
        })
    }

    /// Iterate over the `EdgePattern` elements in this MATCH pattern.
    pub fn edges(&self) -> impl Iterator<Item = &EdgePattern> {
        self.elements.iter().filter_map(|e| match e {
            PatternElement::Edge(e) => Some(e),
            _ => None,
        })
    }

    /// Returns whether any edge needs traversal rather than exactly one fixed hop.
    pub fn has_variable_length(&self) -> bool {
        self.edges().any(|e| e.min_hops != 1 || e.max_hops != 1)
    }
}

/// A single element in the MATCH pattern -- either a node or an edge.
#[derive(Debug, Clone)]
pub enum PatternElement {
    Node(NodePattern),
    Edge(EdgePattern),
}

/// A node binding with optional kind, governed subtype, and property filters.
#[derive(Debug, Clone)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub kind: Option<String>,
    /// Governed subtype compiled against the dedicated `entity_type` column.
    pub entity_type: Option<String>,
    pub properties: HashMap<String, ConditionValue>,
}

/// An edge binding with relation alternatives, direction, and inclusive hop bounds.
#[derive(Debug, Clone)]
pub struct EdgePattern {
    pub variable: Option<String>,
    pub relations: Vec<String>,
    pub direction: EdgeDirection,
    pub min_hops: usize,
    pub max_hops: usize,
}

/// Traversal direction for an edge in the MATCH pattern.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    /// Outgoing only — `(a)-->(b)`.
    Out,
    /// Incoming only — `(a)<--(b)`.
    In,
    /// Either direction — `(a)--(b)`.
    Both,
}

/// A field or JSON path referenced by a WHERE condition or ORDER BY term.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PropertyRef {
    /// A dedicated field on the bound substrate, such as `name` or `created_at`.
    Field(String),
    /// A path below the bound record's JSON `properties` object.
    JsonPath(Vec<String>),
    /// A path below a canonical edge's JSON `metadata` object.
    MetadataPath(Vec<String>),
}

/// One validated field or JSON path in GQL's `ORDER BY` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderItem {
    /// Binding whose value supplies this ordering term.
    pub variable: String,
    /// Dedicated scalar field or nonempty explicit JSON path.
    pub property: PropertyRef,
    /// Ascending or descending SQLite value order, without coercion.
    pub direction: OrderDirection,
}

/// Direction of a caller-supplied ordering term.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderDirection {
    /// Ascending order, with null values first.
    Asc,
    /// Descending order, with null values last.
    Desc,
}

/// A predicate in the WHERE clause: `variable.field op value` or
/// `variable.properties.path op value`.
#[derive(Debug, Clone)]
pub struct Condition {
    pub variable: String,
    pub property: PropertyRef,
    pub op: CompareOp,
    pub value: ConditionValue,
}

/// Comparison operator used in WHERE clause conditions.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareOp {
    Eq,
    Neq,
    Gt,
    Lt,
    Gte,
    Lte,
    Like,
    Contains,
    StartsWith,
    In,
    IsNotNull,
    IsNull,
}

/// A typed condition value; integer and decimal forms remain distinct.
///
/// See `crates/khive-query/docs/api/ast.md` for binding and precision details.
#[derive(Debug, Clone, PartialEq)]
pub enum ConditionValue {
    String(String),
    /// Exact `i64` literal whose source lexeme had no decimal point.
    Integer(i64),
    /// A float literal (decimal point present in the source lexeme).
    Number(f64),
    Bool(bool),
    /// A list literal used by the `IN` operator.
    List(Vec<ConditionValue>),
    /// The operand marker for the operand-free `IS NOT NULL` operator.
    Null,
}
