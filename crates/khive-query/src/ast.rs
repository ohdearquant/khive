//! GQL abstract syntax tree.

use std::collections::HashMap;

/// A SQL parameter value emitted by the query compiler.
#[derive(Clone, Debug)]
pub enum QueryValue {
    Null,
    Integer(i64),
    Float(f64),
    Text(String),
    Blob(Vec<u8>),
}

/// Top-level GQL query node produced by the parser.
#[derive(Debug, Clone)]
pub struct GqlQuery {
    pub pattern: MatchPattern,
    pub where_clause: WhereExpr,
    pub return_items: Vec<ReturnItem>,
    pub limit: Option<usize>,
}

/// A WHERE expression tree supporting AND, OR, and leaf conditions.
#[derive(Debug, Clone)]
pub enum WhereExpr {
    /// AND of two sub-expressions.
    And(Box<WhereExpr>, Box<WhereExpr>),
    /// OR of two sub-expressions.
    Or(Box<WhereExpr>, Box<WhereExpr>),
    /// A single scalar condition.
    Condition(Condition),
    /// Always-true — used when there is no WHERE clause.
    True,
}

impl WhereExpr {
    /// Iterate all leaf conditions in the expression tree (depth-first).
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
                WhereExpr::True => {}
            }
        }
        out.into_iter()
    }

    /// Mutable walk — applies `f` to every leaf condition.
    pub fn for_each_condition_mut(&mut self, f: &mut impl FnMut(&mut Condition)) {
        match self {
            WhereExpr::Condition(c) => f(c),
            WhereExpr::And(l, r) | WhereExpr::Or(l, r) => {
                l.for_each_condition_mut(f);
                r.for_each_condition_mut(f);
            }
            WhereExpr::True => {}
        }
    }

    /// Return `true` when the expression has no conditions (is always-true).
    pub fn is_true(&self) -> bool {
        matches!(self, WhereExpr::True)
    }
}

/// A single item in the RETURN clause — either a bound variable or a property projection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReturnItem {
    Variable(String),
    Property(String, String),
}

impl ReturnItem {
    /// Returns the variable name bound to this return item.
    pub fn variable(&self) -> &str {
        match self {
            Self::Variable(v) | Self::Property(v, _) => v,
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

    pub fn has_variable_length(&self) -> bool {
        self.edges().any(|e| e.max_hops > 1)
    }
}

/// A single element in the MATCH pattern -- either a node or an edge.
#[derive(Debug, Clone)]
pub enum PatternElement {
    Node(NodePattern),
    Edge(EdgePattern),
}

/// A node binding in the MATCH pattern with optional kind, entity_type, and property filters.
#[derive(Debug, Clone)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub kind: Option<String>,
    /// Governed subtype within the kind (e.g. "researcher" within "person").
    /// Compiled to `entity_type = ?` — a direct column, not a property extraction.
    pub entity_type: Option<String>,
    pub properties: HashMap<String, String>,
}

/// An edge binding in the MATCH pattern with optional relation filters, direction, and hop bounds.
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

/// A scalar comparison in the WHERE clause: `variable.property op value`.
#[derive(Debug, Clone)]
pub struct Condition {
    pub variable: String,
    pub property: String,
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
}

/// Right-hand side value in a WHERE condition.
#[derive(Debug, Clone)]
pub enum ConditionValue {
    String(String),
    Number(f64),
    Bool(bool),
}
