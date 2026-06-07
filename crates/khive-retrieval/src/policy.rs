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
mod tests {
    use super::*;

    #[test]
    fn test_clearance_level_ordering() {
        assert!(ClearanceLevel::Secret > ClearanceLevel::Confidential);
        assert!(ClearanceLevel::Confidential > ClearanceLevel::Internal);
        assert!(ClearanceLevel::Internal > ClearanceLevel::Public);
    }

    #[test]
    fn test_clearance_can_access() {
        let secret = ClearanceLevel::Secret;
        let public = ClearanceLevel::Public;

        // Secret can access everything
        assert!(secret.can_access(ClearanceLevel::Secret));
        assert!(secret.can_access(ClearanceLevel::Confidential));
        assert!(secret.can_access(ClearanceLevel::Internal));
        assert!(secret.can_access(ClearanceLevel::Public));

        // Public can only access public
        assert!(public.can_access(ClearanceLevel::Public));
        assert!(!public.can_access(ClearanceLevel::Internal));
        assert!(!public.can_access(ClearanceLevel::Confidential));
        assert!(!public.can_access(ClearanceLevel::Secret));
    }

    #[test]
    fn test_search_policy_constructors() {
        let policy = SearchPolicy::public();
        assert_eq!(policy.caller_clearance, ClearanceLevel::Public);

        let policy = SearchPolicy::secret();
        assert_eq!(policy.caller_clearance, ClearanceLevel::Secret);
    }

    // =========================================================================
    // RETRIEVAL-03: Policy Integration Tests
    // =========================================================================

    #[test]
    fn test_filter_by_policy_hides_secret_from_public() {
        let results = vec![
            ("doc_public", DeterministicScore::from_f64(0.9)),
            ("doc_secret", DeterministicScore::from_f64(0.95)),
            ("doc_internal", DeterministicScore::from_f64(0.8)),
        ];

        let policy = SearchPolicy::public();

        // Clearance lookup function
        let get_clearance = |id: &&str| -> ClearanceLevel {
            match *id {
                "doc_public" => ClearanceLevel::Public,
                "doc_internal" => ClearanceLevel::Internal,
                "doc_secret" => ClearanceLevel::Secret,
                _ => ClearanceLevel::Public,
            }
        };

        let filtered = filter_by_policy(results, &policy, get_clearance);

        // Public caller should only see public documents
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].0, "doc_public");
    }

    #[test]
    fn test_filter_by_policy_secret_sees_all() {
        let results = vec![
            ("doc_public", DeterministicScore::from_f64(0.9)),
            ("doc_secret", DeterministicScore::from_f64(0.95)),
            ("doc_confidential", DeterministicScore::from_f64(0.8)),
        ];

        let policy = SearchPolicy::secret();

        let get_clearance = |id: &&str| -> ClearanceLevel {
            match *id {
                "doc_public" => ClearanceLevel::Public,
                "doc_confidential" => ClearanceLevel::Confidential,
                "doc_secret" => ClearanceLevel::Secret,
                _ => ClearanceLevel::Public,
            }
        };

        let filtered = filter_by_policy(results, &policy, get_clearance);

        // Secret caller should see all documents
        assert_eq!(filtered.len(), 3);
    }

    #[test]
    fn test_filter_by_policy_internal_sees_public_and_internal() {
        let results = vec![
            ("doc_public", DeterministicScore::from_f64(0.9)),
            ("doc_secret", DeterministicScore::from_f64(0.95)),
            ("doc_internal", DeterministicScore::from_f64(0.8)),
        ];

        let policy = SearchPolicy::internal();

        let get_clearance = |id: &&str| -> ClearanceLevel {
            match *id {
                "doc_public" => ClearanceLevel::Public,
                "doc_internal" => ClearanceLevel::Internal,
                "doc_secret" => ClearanceLevel::Secret,
                _ => ClearanceLevel::Public,
            }
        };

        let filtered = filter_by_policy(results, &policy, get_clearance);

        // Internal caller should see public and internal
        assert_eq!(filtered.len(), 2);
        assert!(filtered.iter().any(|(id, _)| *id == "doc_public"));
        assert!(filtered.iter().any(|(id, _)| *id == "doc_internal"));
        assert!(!filtered.iter().any(|(id, _)| *id == "doc_secret"));
    }

    #[test]
    fn test_filter_by_policy_preserves_order() {
        let results = vec![
            ("doc1", DeterministicScore::from_f64(0.9)),
            ("doc2", DeterministicScore::from_f64(0.8)),
            ("doc3", DeterministicScore::from_f64(0.7)),
        ];

        let policy = SearchPolicy::public();
        let get_clearance = |_: &&str| ClearanceLevel::Public;

        let filtered = filter_by_policy(results, &policy, get_clearance);

        // Order should be preserved
        assert_eq!(filtered[0].0, "doc1");
        assert_eq!(filtered[1].0, "doc2");
        assert_eq!(filtered[2].0, "doc3");
    }

    #[test]
    fn test_filter_by_predicate() {
        let results = vec![
            ("allowed", DeterministicScore::from_f64(0.9)),
            ("denied", DeterministicScore::from_f64(0.8)),
            ("allowed2", DeterministicScore::from_f64(0.7)),
        ];

        let filtered = filter_by_predicate(results, |id| id.starts_with("allowed"));

        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].0, "allowed");
        assert_eq!(filtered[1].0, "allowed2");
    }
}
