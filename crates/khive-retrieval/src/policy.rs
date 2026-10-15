//! Policy integration for access-controlled retrieval.
//!
//! Post-retrieval clearance filter: callers only see documents at or below their
//! ClearanceLevel. See RETRIEVAL-03.

use khive_score::DeterministicScore;
use std::hash::Hash;

#[cfg(feature = "policy")]
use khive_gate::GateContext as PolicyContext;

/// Clearance level for documents.
///
/// Higher values indicate more restricted access.
/// This is a simple hierarchical model; more complex ABAC can be built on top.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum ClearanceLevel {
    /// Public documents, accessible to all.
    #[default]
    Public = 0,
    /// Internal documents, accessible to authenticated users.
    Internal = 1,
    /// Confidential documents, restricted access.
    Confidential = 2,
    /// Secret documents, highly restricted.
    Secret = 3,
}

impl ClearanceLevel {
    /// Check if this clearance level can access a document with the given level.
    ///
    /// A caller can access a document if their clearance is >= the document's level.
    #[inline]
    pub fn can_access(&self, document_level: ClearanceLevel) -> bool {
        *self >= document_level
    }
}

/// Policy context for search operations.
///
/// Encapsulates the caller's clearance level and optional policy engine
/// for more complex access control decisions.
#[derive(Debug, Clone)]
pub struct SearchPolicy {
    /// Caller's clearance level (simple hierarchical model).
    pub caller_clearance: ClearanceLevel,

    /// Optional policy engine for complex ABAC decisions.
    #[cfg(feature = "policy")]
    pub policy_context: Option<PolicyContext>,
}

impl SearchPolicy {
    /// Create a new search policy with the given clearance level.
    pub fn new(caller_clearance: ClearanceLevel) -> Self {
        Self {
            caller_clearance,
            #[cfg(feature = "policy")]
            policy_context: None,
        }
    }

    /// Create a public-level search policy (default).
    pub fn public() -> Self {
        Self::new(ClearanceLevel::Public)
    }

    /// Create an internal-level search policy.
    pub fn internal() -> Self {
        Self::new(ClearanceLevel::Internal)
    }

    /// Create a confidential-level search policy.
    pub fn confidential() -> Self {
        Self::new(ClearanceLevel::Confidential)
    }

    /// Create a secret-level search policy.
    pub fn secret() -> Self {
        Self::new(ClearanceLevel::Secret)
    }

    /// Set the policy context for complex access control.
    #[cfg(feature = "policy")]
    pub fn with_context(mut self, context: PolicyContext) -> Self {
        self.policy_context = Some(context);
        self
    }

    /// Check if the caller can access a document with the given clearance.
    #[inline]
    pub fn can_access(&self, document_clearance: ClearanceLevel) -> bool {
        self.caller_clearance.can_access(document_clearance)
    }
}

impl Default for SearchPolicy {
    fn default() -> Self {
        Self::public()
    }
}

/// Filter search results to only those the caller is authorized to see per `policy`.
pub fn filter_by_policy<Id, F>(
    results: Vec<(Id, DeterministicScore)>,
    policy: &SearchPolicy,
    get_clearance: F,
) -> Vec<(Id, DeterministicScore)>
where
    Id: Clone,
    F: Fn(&Id) -> ClearanceLevel,
{
    results
        .into_iter()
        .filter(|(id, _)| {
            let doc_clearance = get_clearance(id);
            policy.can_access(doc_clearance)
        })
        .collect()
}

/// Filter search results using a custom predicate `is_accessible`.
pub fn filter_by_predicate<Id, F>(
    results: Vec<(Id, DeterministicScore)>,
    is_accessible: F,
) -> Vec<(Id, DeterministicScore)>
where
    Id: Clone,
    F: Fn(&Id) -> bool,
{
    results
        .into_iter()
        .filter(|(id, _)| is_accessible(id))
        .collect()
}

#[cfg(test)]
#[path = "policy/tests.rs"]
mod tests;
