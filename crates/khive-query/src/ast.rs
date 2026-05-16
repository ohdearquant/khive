//! GQL abstract syntax tree.

use std::collections::HashMap;

#[derive(Debug, Clone)]
pub struct GqlQuery {
    pub pattern: MatchPattern,
    pub where_clause: Vec<Condition>,
    pub return_items: Vec<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct MatchPattern {
    pub elements: Vec<PatternElement>,
}

impl MatchPattern {
    pub fn nodes(&self) -> impl Iterator<Item = &NodePattern> {
        self.elements.iter().filter_map(|e| match e {
            PatternElement::Node(n) => Some(n),
            _ => None,
        })
    }

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

#[derive(Debug, Clone)]
pub enum PatternElement {
    Node(NodePattern),
    Edge(EdgePattern),
}

#[derive(Debug, Clone)]
pub struct NodePattern {
    pub variable: Option<String>,
    pub kind: Option<String>,
    pub properties: HashMap<String, String>,
}

#[derive(Debug, Clone)]
pub struct EdgePattern {
    pub variable: Option<String>,
    pub relations: Vec<String>,
    pub direction: EdgeDirection,
    pub min_hops: usize,
    pub max_hops: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EdgeDirection {
    Out,
    In,
    Both,
}

#[derive(Debug, Clone)]
pub struct Condition {
    pub variable: String,
    pub property: String,
    pub op: CompareOp,
    pub value: ConditionValue,
}

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

#[derive(Debug, Clone)]
pub enum ConditionValue {
    String(String),
    Number(f64),
    Bool(bool),
}
