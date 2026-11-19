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
