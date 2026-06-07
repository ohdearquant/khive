//! Compatibility shims for the legacy graph traversal module.
//!
//! The graph module was written against an older `khive_db` API that exported
//! `EntityRef`, `Link`, `LinkStore`, and `StorageContext`. These types no longer
//! exist in `khive_db`. This module provides minimal shims so the graph code
//! compiles under the `graph-legacy` feature until the module is ported to the
//! current `khive_storage::GraphStore` API.

use std::collections::BTreeMap;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::error::{Result, RetrievalError};

// ---------------------------------------------------------------------------
// EntityRef
// ---------------------------------------------------------------------------

/// A reference to a graph entity.
///
/// Legacy type — maps to the old `khive_db::EntityRef` API.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum EntityRef {
    /// An externally-identified entity (string key).
    External(String),
}

// ---------------------------------------------------------------------------
// Link
// ---------------------------------------------------------------------------

/// An opaque link identifier.
///
/// Legacy type — shim for the old `khive_db::LinkId`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct LinkId(u64);

impl LinkId {
    /// The nil / zero link ID.
    pub const NIL: Self = Self(0);
}

/// A directed edge between two entities.
///
/// Legacy type — maps to the old `khive_db::Link` API.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Link {
    /// Opaque link identifier.
    pub id: LinkId,
    /// Source entity.
    pub source: EntityRef,
    /// Target entity.
    pub target: EntityRef,
    /// Relation type (e.g. "contains", "references").
    pub relation: String,
    /// Optional edge properties (e.g. `{"weight": 0.9}`).
    pub properties: Option<BTreeMap<String, serde_json::Value>>,
}

impl Link {
    /// Create a new link with no properties.
    pub fn new(
        id: LinkId,
        source: EntityRef,
        target: EntityRef,
        relation: impl Into<String>,
    ) -> Self {
        Self {
            id,
            source,
            target,
            relation: relation.into(),
            properties: None,
        }
    }

    /// Create a new link with serializable properties.
    pub fn with_properties(
        id: LinkId,
        source: EntityRef,
        target: EntityRef,
        relation: impl Into<String>,
        props: serde_json::Value,
    ) -> Self {
        let properties = props
            .as_object()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect());
        Self {
            id,
            source,
            target,
            relation: relation.into(),
            properties,
        }
    }
}

// ---------------------------------------------------------------------------
// StorageContext
// ---------------------------------------------------------------------------

/// Context for storage operations (namespace isolation, etc.).
///
/// Legacy type — maps to the old `khive_db::StorageContext` API.
#[derive(Clone, Debug, Default)]
pub struct StorageContext {
    /// Namespace for multi-tenant isolation.
    pub namespace: String,
}

impl StorageContext {
    /// Create a new storage context with the given namespace.
    pub fn new(namespace: impl Into<String>) -> Self {
        Self {
            namespace: namespace.into(),
        }
    }
}

// ---------------------------------------------------------------------------
// LinkStore
// ---------------------------------------------------------------------------

/// Trait for querying directed graph edges.
///
/// Legacy trait — maps to the old `khive_db::LinkStore` API.
#[async_trait]
pub trait LinkStore: Send + Sync {
    /// Get all outgoing links from an entity.
    async fn outgoing(&self, ctx: &StorageContext, entity: &EntityRef) -> Result<Vec<Link>>;

    /// Get all incoming links to an entity.
    async fn incoming(&self, ctx: &StorageContext, entity: &EntityRef) -> Result<Vec<Link>>;

    /// Create a link between two entities.
    async fn link(
        &self,
        ctx: &StorageContext,
        source: EntityRef,
        target: EntityRef,
        relation: &str,
        properties: Option<serde_json::Value>,
    ) -> Result<Link>;
}

// ---------------------------------------------------------------------------
// MockLinkStore (for tests)
// ---------------------------------------------------------------------------

/// In-memory mock implementation of `LinkStore` for tests.
pub struct MockLinkStore {
    links: parking_lot::Mutex<Vec<Link>>,
    next_id: std::sync::atomic::AtomicU64,
}

impl MockLinkStore {
    /// Create a new empty mock store.
    pub fn new() -> Self {
        Self {
            links: parking_lot::Mutex::new(Vec::new()),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }
}

impl Default for MockLinkStore {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl LinkStore for MockLinkStore {
    async fn outgoing(&self, _ctx: &StorageContext, entity: &EntityRef) -> Result<Vec<Link>> {
        let links = self.links.lock();
        Ok(links
            .iter()
            .filter(|l| &l.source == entity)
            .cloned()
            .collect())
    }

    async fn incoming(&self, _ctx: &StorageContext, entity: &EntityRef) -> Result<Vec<Link>> {
        let links = self.links.lock();
        Ok(links
            .iter()
            .filter(|l| &l.target == entity)
            .cloned()
            .collect())
    }

    async fn link(
        &self,
        _ctx: &StorageContext,
        source: EntityRef,
        target: EntityRef,
        relation: &str,
        properties: Option<serde_json::Value>,
    ) -> Result<Link> {
        let id = self
            .next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let link = if let Some(props) = properties {
            Link::with_properties(LinkId(id), source, target, relation, props)
        } else {
            Link::new(LinkId(id), source, target, relation)
        };
        self.links.lock().push(link.clone());
        Ok(link)
    }
}

/// Create a test storage context.
pub fn test_context() -> StorageContext {
    StorageContext::new("test")
}

// ---------------------------------------------------------------------------
// Error adapter
// ---------------------------------------------------------------------------

/// Adapt a `String` error into a `RetrievalError::GraphTraversal`.
// REASON: used selectively by BFS/DFS/shortest-path helpers under `graph-legacy`; without that
// feature the call sites are compiled out, making this function appear dead to rustc.
#[allow(dead_code)]
pub(crate) fn graph_err(msg: impl std::fmt::Display) -> RetrievalError {
    RetrievalError::GraphTraversal(msg.to_string())
}
