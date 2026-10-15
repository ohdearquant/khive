use super::*;

fn sample_query() -> SparseVector {
    SparseVector {
        indices: vec![0],
        values: vec![1.0],
    }
}

/// STORAGE-AUD-002 / #470: top_k = u32::MAX must be rejected by serde
/// deserialization instead of reaching a backend that would allocate a
/// multi-hundred-GB heap.
#[test]
fn sparse_top_k_u32_max_rejected() {
    let raw = serde_json::json!({
        "query": {"indices": [0], "values": [1.0]},
        "top_k": u32::MAX,
        "namespace": null,
        "kind": null,
    });
    let result: Result<SparseSearchRequest, _> = serde_json::from_value(raw);
    assert!(
        result.is_err(),
        "top_k = u32::MAX must be rejected, got {result:?}"
    );
}

#[test]
fn sparse_top_k_at_max_accepted() {
    let request = SparseSearchRequest {
        query: sample_query(),
        top_k: MAX_SPARSE_SEARCH_TOP_K,
        namespace: None,
        kind: None,
    };
    assert!(request.validate().is_ok());
}

#[test]
fn sparse_top_k_over_max_rejected_direct_construction() {
    let request = SparseSearchRequest {
        query: sample_query(),
        top_k: MAX_SPARSE_SEARCH_TOP_K + 1,
        namespace: None,
        kind: None,
    };
    assert!(request.validate().is_err());
}

#[test]
fn sparse_top_k_zero_still_rejected() {
    let request = SparseSearchRequest {
        query: sample_query(),
        top_k: 0,
        namespace: None,
        kind: None,
    };
    assert!(request.validate().is_err());
}
