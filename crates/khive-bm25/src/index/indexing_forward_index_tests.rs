use crate::{Bm25Config, Bm25Index};

#[test]
fn test_forward_index_persisted_across_save_load_cycle() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "quick brown fox").unwrap();
    index.index_document("doc2", "lazy brown dog").unwrap();
    index.index_document("doc3", "quick fox jumps").unwrap();

    let json = serde_json::to_string(&index).unwrap();
    let restored: Bm25Index = serde_json::from_str(&json).unwrap();

    assert!(
        !restored.forward_index.is_empty(),
        "forward_index must be populated after custom deserialization"
    );

    for internal_id in restored.doc_lengths.keys() {
        assert!(
            restored.forward_index.contains_key(internal_id),
            "doc {internal_id} missing from rebuilt forward_index"
        );
    }
}

#[test]
fn test_remove_uses_forward_index_not_full_scan() {
    let mut index = Bm25Index::default();
    let words = [
        "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
    ];
    for (i, word) in words.iter().enumerate() {
        index
            .index_document(format!("doc{i}"), &format!("{word} shared_term"))
            .unwrap();
    }

    let json = serde_json::to_string(&index).unwrap();
    let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();

    assert!(
        !restored.forward_index.is_empty(),
        "custom Deserialize must rebuild forward_index immediately"
    );
    for internal_id in restored.doc_lengths.keys() {
        assert!(
            restored.forward_index.contains_key(internal_id),
            "doc {internal_id} missing from rebuilt forward_index after deserialization"
        );
    }

    let removed = restored.remove_document("doc0");
    assert!(
        removed,
        "remove_document must return true for an existing doc"
    );

    assert!(!restored.contains_document("doc0"));
    assert_eq!(
        restored.doc_count(),
        words.len() - 1,
        "doc_count must decrease by exactly one"
    );

    for i in 1..words.len() {
        let doc_id = format!("doc{i}");
        let ok = restored.remove_document(&doc_id);
        assert!(ok, "remove_document must succeed for {doc_id}");
    }
    assert_eq!(restored.doc_count(), 0);
    assert!(
        restored.inverted_index.is_empty(),
        "inverted index must be empty after all removes"
    );
}

#[test]
fn test_search_results_unchanged_after_add_remove_cycle() {
    let mut baseline_index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");
    baseline_index
        .index_document("doc1", "quick brown fox")
        .unwrap();
    baseline_index
        .index_document("doc2", "lazy brown dog")
        .unwrap();
    baseline_index
        .index_document("doc3", "quick fox jumps")
        .unwrap();
    let baseline = baseline_index.search("quick brown fox", 10);

    baseline_index
        .index_document("doc4", "unrelated zebra content")
        .unwrap();
    let json = serde_json::to_string(&baseline_index).unwrap();
    let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();
    restored.ensure_doc_lengths_vec();

    let removed = restored.remove_document("doc4");
    assert!(removed, "doc4 must be removable from the restored index");

    let after = restored.search("quick brown fox", 10);

    assert_eq!(
        baseline.len(),
        after.len(),
        "result count must match the original 3-doc baseline after remove cycle"
    );
    for (base, post) in baseline.iter().zip(after.iter()) {
        assert_eq!(
            base.0, post.0,
            "doc_id ordering must be preserved after remove cycle"
        );
        assert_eq!(
            base.1, post.1,
            "BM25 scores must be identical after remove cycle"
        );
    }
}
