use crate::{Bm25Config, Bm25Index};

#[test]
fn serde_roundtrip_search_works_with_4_postings() {
    let mut index = Bm25Index::default();
    for i in 0..4 {
        index.index_document(format!("doc{i}"), "alpha").unwrap();
    }
    let json = serde_json::to_string(&index).unwrap();
    let restored: Bm25Index = serde_json::from_str(&json).unwrap();
    assert_eq!(
        restored.doc_lengths_f32.len(),
        restored.next_internal_id as usize,
        "doc_lengths_f32 must be rebuilt on deserialization"
    );
    let results = restored.search("alpha", 10);
    assert_eq!(results.len(), 4, "all 4 docs must be found");
}

#[test]
fn serde_roundtrip_search_works_with_8_postings() {
    let mut index = Bm25Index::default();
    for i in 0..8 {
        index.index_document(format!("doc{i}"), "alpha").unwrap();
    }
    let json = serde_json::to_string(&index).unwrap();
    let restored: Bm25Index = serde_json::from_str(&json).unwrap();
    let results = restored.search("alpha", 10);
    assert_eq!(results.len(), 8, "all 8 docs must be found");
}

#[test]
fn remove_old_doc_after_deserialize_and_new_insert_leaves_no_stale_posting() {
    let mut index = Bm25Index::default();
    index.index_document("old_doc", "alpha").unwrap();
    let json = serde_json::to_string(&index).unwrap();
    let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();
    restored.index_document("new_doc", "beta").unwrap();
    for internal_id in restored.doc_lengths.keys() {
        assert!(
            restored.forward_index.contains_key(internal_id),
            "forward_index must cover every live doc after new insert post-serde"
        );
    }
    assert!(restored.remove_document("old_doc"));
    let hits = restored.search("alpha", 10);
    assert!(
        hits.is_empty(),
        "old_doc must not remain searchable after removal"
    );
}

#[test]
fn reindex_with_empty_text_preserves_old_document() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "original content").unwrap();
    index.index_document("doc1", "").unwrap();
    assert!(
        index.contains_document("doc1"),
        "doc must survive a no-op empty reindex"
    );
    let results = index.search("original", 10);
    assert_eq!(
        results.len(),
        1,
        "doc must still be searchable after empty reindex"
    );
}

#[test]
fn budget_check_does_not_overflow() {
    let config = Bm25Config::default().with_memory_budget(1);
    let mut index = Bm25Index::try_new(config).expect("valid config");
    let result = index.index_document("doc1", "hello world");
    assert!(result.is_err(), "budget should be exceeded");
}

#[test]
fn config_nan_k1_rejected_by_try_new() {
    let config = Bm25Config::new(f64::NAN, 0.75);
    assert!(
        Bm25Index::try_new(config).is_err(),
        "NaN k1 must be rejected by try_new"
    );
}

#[test]
fn config_inf_b_rejected_by_try_new() {
    let config = Bm25Config::new(1.2, f64::INFINITY);
    assert!(
        Bm25Index::try_new(config).is_err(),
        "Inf b must be rejected by try_new"
    );
}

#[test]
fn config_nan_k1_rejected_by_try_with_tokenizer() {
    use std::sync::Arc;
    let config = Bm25Config::new(f64::NAN, 0.75);
    let tokenizer = Arc::new(crate::tokenizer::SimpleTokenizer::default());
    assert!(
        Bm25Index::try_with_tokenizer(config, tokenizer).is_err(),
        "NaN k1 must be rejected by try_with_tokenizer"
    );
}

#[test]
fn config_inf_b_rejected_by_try_with_tokenizer() {
    use std::sync::Arc;
    let config = Bm25Config::new(1.2, f64::INFINITY);
    let tokenizer = Arc::new(crate::tokenizer::SimpleTokenizer::default());
    assert!(
        Bm25Index::try_with_tokenizer(config, tokenizer).is_err(),
        "Inf b must be rejected by try_with_tokenizer"
    );
}

#[test]
fn block_size_zero_rejected_on_deserialization() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "hello world").unwrap();
    let json = serde_json::to_string(&index).unwrap();
    let tampered = json.replace(
        &format!("\"block_size\":{}", super::DEFAULT_BLOCK_SIZE),
        "\"block_size\":0",
    );
    let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
    assert!(
        result.is_err(),
        "block_size=0 must be rejected during deserialization"
    );
}

#[test]
fn postings_epoch_max_does_not_collide_with_stale_sentinel() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "hello world").unwrap();
    let json = serde_json::to_string(&index).unwrap();
    let tampered = json.replace(
        &format!("\"postings_epoch\":{}", index.postings_epoch),
        &format!("\"postings_epoch\":{}", u64::MAX),
    );
    let restored: Bm25Index = serde_json::from_str(&tampered).unwrap();
    let results = restored.search("hello", 10);
    assert_eq!(
        results.len(),
        1,
        "search must work with postings_epoch=u64::MAX"
    );
}

#[test]
fn posting_list_sentinel_doc_id_rejected_via_index_serde() {
    let mut index = Bm25Index::default();
    index.index_document("doc1", "hello").unwrap();
    let json = serde_json::to_string(&index).unwrap();
    let tampered = json.replace("[0],\"term_freqs\":[1]", "[4294967295],\"term_freqs\":[1]");
    if tampered == json {
        return;
    }
    let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
    assert!(
        result.is_err(),
        "posting list with u32::MAX doc_id must be rejected"
    );
}

#[test]
fn unsorted_posting_list_rejected_via_index_serde() {
    let mut index = Bm25Index::default();
    index.index_document("doc0", "common term").unwrap();
    index.index_document("doc1", "common word").unwrap();
    let json = serde_json::to_string(&index).unwrap();
    let tampered = json.replace("[0,1],\"term_freqs\"", "[1,0],\"term_freqs\"");
    if tampered == json {
        return;
    }
    let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
    assert!(
        result.is_err(),
        "posting list with unsorted doc_ids must be rejected"
    );
}

#[test]
fn bm25_index_clone_recovers_idf_cache_from_poisoned_lock() {
    let mut index = Bm25Index::default();
    // Index enough documents so the IDF cache gets populated.
    for i in 0..5 {
        index
            .index_document(format!("doc{i}"), "rust systems programming")
            .unwrap();
    }
    // Warm the IDF cache by running a search.
    let _ = index.search("rust", 10);
    assert!(
        !index.is_idf_cache_empty(),
        "IDF cache must be populated before poisoning"
    );

    // Poison the IDF cache lock by panicking inside its write-lock scope.
    let _ = std::panic::catch_unwind(|| {
        let _guard = index.idf_cache.by_df.write().unwrap();
        panic!("intentional idf cache poison");
    });
    assert!(
        index.idf_cache.by_df.read().is_err(),
        "IDF cache lock must be poisoned"
    );

    // Clone must preserve populated IDF cache, not collapse to empty.
    let cloned = index.clone();
    assert!(
        !cloned.is_idf_cache_empty(),
        "cloned index must retain non-empty IDF cache after lock was poisoned"
    );

    // Search results must be non-empty (scores non-zero) — not silently zeroed.
    let results = cloned.search("rust", 10);
    assert_eq!(
        results.len(),
        5,
        "all documents must be found in clone of poisoned index"
    );
    assert!(
        results.iter().all(|(_id, score)| score.to_f64() > 0.0),
        "BM25 scores must be non-zero; a collapsed IDF cache would produce score=0"
    );
}

#[test]
fn bm25_index_clone_recovers_block_max_state_from_poisoned_lock() {
    let mut index = Bm25Index::default();
    for i in 0..4 {
        index
            .index_document(format!("doc{i}"), "search engine indexing")
            .unwrap();
    }
    // Force block-max metadata to be built.
    index.ensure_block_max_metadata();

    // Poison the block_max_state lock.
    let _ = std::panic::catch_unwind(|| {
        let _guard = index.block_max_state.write().unwrap();
        panic!("intentional block_max poison");
    });
    assert!(
        index.block_max_state.read().is_err(),
        "block_max_state lock must be poisoned"
    );

    // Clone must succeed and the clone must be searchable.
    let cloned = index.clone();
    let results = cloned.search("search", 10);
    assert_eq!(
        results.len(),
        4,
        "all documents must be found in clone after block_max_state lock was poisoned"
    );
}

/// A schema-valid single-live-doc wire snapshot: doc "doc0" -> internal id 0,
/// one posting for term "alpha". Tests corrupt one field at a time from this base.
fn valid_single_doc_json() -> serde_json::Value {
    serde_json::json!({
        "inverted_index": {
            "alpha": { "doc_ids": [0], "term_freqs": [1] }
        },
        "doc_lengths": { "0": 1 },
        "id_to_internal": { "doc0": 0 },
        "internal_to_id": ["doc0"],
        "next_internal_id": 1,
        "total_tokens": 1,
        "postings_epoch": 0,
        "block_size": 128,
        "config": { "k1": 1.2, "b": 0.75, "memory_budget": null }
    })
}

#[test]
fn deserialize_rejects_doc_length_id_at_u32_max() {
    let json = serde_json::json!({
        "inverted_index": {},
        "doc_lengths": { "4294967295": 1 },
        "id_to_internal": {},
        "internal_to_id": [],
        "next_internal_id": 0,
        "total_tokens": 1,
        "postings_epoch": 0,
        "block_size": 128,
        "config": { "k1": 1.2, "b": 0.75, "memory_budget": null }
    });
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("invalid live doc_id"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_live_doc_id_at_or_above_next_internal_id() {
    let mut json = valid_single_doc_json();
    // doc_lengths key 0 is fine, but shift next_internal_id below it.
    json["next_internal_id"] = serde_json::json!(0);
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("invalid live doc_id"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_posting_id_at_or_above_next_internal_id() {
    let mut json = valid_single_doc_json();
    json["doc_lengths"] = serde_json::json!({ "0": 1, "5": 1 });
    json["inverted_index"] = serde_json::json!({
        "alpha": { "doc_ids": [0, 5], "term_freqs": [1, 1] }
    });
    json["total_tokens"] = serde_json::json!(2);
    // next_internal_id stays 1, so doc_id 5 is out of range for both doc_lengths and postings.
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("invalid live doc_id")
            || err.to_string().contains("non-live doc_id"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_missing_internal_to_id_for_live_doc() {
    let mut json = valid_single_doc_json();
    json["internal_to_id"] = serde_json::json!([]);
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("missing internal_to_id"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_mismatched_internal_to_id_reverse_mapping() {
    let mut json = valid_single_doc_json();
    json["internal_to_id"] = serde_json::json!(["wrong"]);
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("does not map")
            || err.to_string().contains("does not reverse-map"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_stale_id_to_internal_value() {
    let mut json = valid_single_doc_json();
    // "stale" points at internal id 7, which is neither live nor in range.
    json["id_to_internal"] = serde_json::json!({ "doc0": 0, "stale": 7 });
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("non-live doc_id"),
        "unexpected error: {err}"
    );
}

#[test]
fn deserialize_rejects_bad_total_tokens() {
    let mut json = valid_single_doc_json();
    json["total_tokens"] = serde_json::json!(4);
    let err = serde_json::from_value::<Bm25Index>(json).unwrap_err();
    assert!(
        err.to_string().contains("does not match doc_lengths sum"),
        "unexpected error: {err}"
    );
}
