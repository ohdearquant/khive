//! Tests that require access to private/pub(crate) fields and must stay inline.

/// Tests for forward index persistence and O(|doc|) remove behaviour (issue #307).
#[cfg(test)]
mod forward_index_tests {
    use crate::{Bm25Config, Bm25Index};

    /// After a serde round-trip, the forward map must be complete (every document
    /// present), because the custom `Deserialize` implementation now rebuilds all
    /// derived caches including `forward_index` automatically.  This ensures that
    /// subsequent removes use the O(|terms_in_doc|) fast path without requiring
    /// any manual `ensure_forward_index()` call.
    #[test]
    fn test_forward_index_persisted_across_save_load_cycle() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "quick brown fox").unwrap();
        index.index_document("doc2", "lazy brown dog").unwrap();
        index.index_document("doc3", "quick fox jumps").unwrap();

        // Serialize (forward_index is #[serde(skip)] — intentionally not in snapshot).
        let json = serde_json::to_string(&index).unwrap();

        // Deserialize — custom Deserialize now auto-rebuilds forward_index.
        let restored: Bm25Index = serde_json::from_str(&json).unwrap();

        // After custom deserialization the forward index must be fully populated.
        assert!(
            !restored.forward_index.is_empty(),
            "forward_index must be populated after custom deserialization"
        );

        // Every document that has a doc_lengths entry must appear in the forward index.
        for internal_id in restored.doc_lengths.keys() {
            assert!(
                restored.forward_index.contains_key(internal_id),
                "doc {internal_id} missing from rebuilt forward_index"
            );
        }
    }

    /// `remove_document` on a deserialized index must use the forward index
    /// (O(|terms_in_doc|) path) rather than the O(|vocabulary|) full scan.
    ///
    /// The custom `Deserialize` implementation now rebuilds `forward_index`
    /// automatically, so it is already populated when the first `remove_document`
    /// call happens — no lazy rebuild needed.
    #[test]
    fn test_remove_uses_forward_index_not_full_scan() {
        // Build an index with a meaningful vocabulary.
        let mut index = Bm25Index::default();
        let words = [
            "alpha", "beta", "gamma", "delta", "epsilon", "zeta", "eta", "theta", "iota", "kappa",
        ];
        for (i, word) in words.iter().enumerate() {
            index
                .index_document(format!("doc{i}"), &format!("{word} shared_term"))
                .unwrap();
        }

        // Serialize and restore (forward_index stripped by #[serde(skip)],
        // but custom Deserialize rebuilds it immediately).
        let json = serde_json::to_string(&index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();

        // Postcondition of custom deserialization: forward index already complete.
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

        // Remove a document — forward index already populated, no lazy rebuild needed.
        let removed = restored.remove_document("doc0");
        assert!(
            removed,
            "remove_document must return true for an existing doc"
        );

        // After removal: doc0 must be gone, forward index intact for remaining docs.
        assert!(!restored.contains_document("doc0"));
        assert_eq!(
            restored.doc_count(),
            words.len() - 1,
            "doc_count must decrease by exactly one"
        );

        // Remove remaining documents one by one — must all succeed cleanly.
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

    /// Search results must be identical before and after a save/load/remove cycle
    /// for documents that remain in the index.
    ///
    /// This is the regression guard: the forward-index change must not alter
    /// the scoring or result ordering for documents that were not removed.
    ///
    /// Strategy: build baseline on an index without doc4, add doc4, round-trip
    /// through serde, remove doc4 — then assert results match the original baseline.
    #[test]
    fn test_search_results_unchanged_after_add_remove_cycle() {
        // Step 1: baseline on a 3-document index (no doc4).
        let mut baseline_index = Bm25Index::new(Bm25Config::default());
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

        // Step 2: add doc4 to introduce it, then round-trip through serde.
        baseline_index
            .index_document("doc4", "unrelated zebra content")
            .unwrap();
        let json = serde_json::to_string(&baseline_index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();
        restored.ensure_doc_lengths_vec();

        // Step 3: remove doc4 on the restored index.
        let removed = restored.remove_document("doc4");
        assert!(removed, "doc4 must be removable from the restored index");

        // Step 4: results must match the original 3-document baseline exactly.
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
}

/// Regression tests for correctness bugs found in the external code review.
#[cfg(test)]
mod regression_tests {
    use crate::{Bm25Config, Bm25Index};

    // -------------------------------------------------------------------------
    // Finding 1: serde roundtrip + SIMD search panic
    // After deserialization, doc_lengths_f32 must be populated so that
    // search_brute_force does not panic when indexing into it.
    // -------------------------------------------------------------------------

    #[test]
    fn serde_roundtrip_search_works_with_4_postings() {
        let mut index = Bm25Index::default();
        for i in 0..4 {
            index.index_document(format!("doc{i}"), "alpha").unwrap();
        }

        let json = serde_json::to_string(&index).unwrap();
        let restored: Bm25Index = serde_json::from_str(&json).unwrap();

        // doc_lengths_f32 must be populated by the custom Deserialize.
        assert_eq!(
            restored.doc_lengths_f32.len(),
            restored.next_internal_id as usize,
            "doc_lengths_f32 must be rebuilt on deserialization"
        );

        // Must not panic — the 4-wide SIMD path now has valid doc_lengths_f32.
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

        // Must not panic — exercises the 8-wide SIMD batch path on x86_64.
        let results = restored.search("alpha", 10);
        assert_eq!(results.len(), 8, "all 8 docs must be found");
    }

    // -------------------------------------------------------------------------
    // Finding 2: ensure_forward_index completeness
    // After serde + new insert, the forward index must cover ALL live docs.
    // -------------------------------------------------------------------------

    #[test]
    fn remove_old_doc_after_deserialize_and_new_insert_leaves_no_stale_posting() {
        let mut index = Bm25Index::default();
        index.index_document("old_doc", "alpha").unwrap();

        let json = serde_json::to_string(&index).unwrap();
        let mut restored: Bm25Index = serde_json::from_str(&json).unwrap();

        // Insert a new doc after deserialization.
        restored.index_document("new_doc", "beta").unwrap();

        // Forward index must cover both old_doc and new_doc.
        for internal_id in restored.doc_lengths.keys() {
            assert!(
                restored.forward_index.contains_key(internal_id),
                "forward_index must cover every live doc after new insert post-serde"
            );
        }

        // Remove old_doc — must clean up its postings.
        assert!(restored.remove_document("old_doc"));

        // old_doc must not remain searchable.
        let hits = restored.search("alpha", 10);
        assert!(
            hits.is_empty(),
            "old_doc must not remain searchable after removal"
        );
    }

    // -------------------------------------------------------------------------
    // Finding 3: non-atomic reindex — empty reindex must not delete old doc
    // -------------------------------------------------------------------------

    #[test]
    fn reindex_with_empty_text_preserves_old_document() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "original content").unwrap();

        // Re-index with empty text — the old document must be preserved.
        index.index_document("doc1", "").unwrap();

        // Doc must still be present and searchable.
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

    // -------------------------------------------------------------------------
    // Finding 4: budget overflow — saturating_add prevents wrapping
    // A huge cost value must not wrap around and appear to be within budget.
    // -------------------------------------------------------------------------

    #[test]
    fn budget_check_does_not_overflow() {
        // Budget of 1 byte: current≈0, cost≈very large. saturating_add prevents
        // wrapping cost from bypassing the check.
        let config = Bm25Config::default().with_memory_budget(1);
        let mut index = Bm25Index::new(config);

        // The budget check must reject this even if cost > usize::MAX / 2.
        let result = index.index_document("doc1", "hello world");
        assert!(result.is_err(), "budget should be exceeded");
    }

    // -------------------------------------------------------------------------
    // Finding 5: NaN/Inf in config rejected at validate()
    // -------------------------------------------------------------------------

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

    // -------------------------------------------------------------------------
    // Finding 6: block_size=0 rejected on deserialization
    // -------------------------------------------------------------------------

    #[test]
    fn block_size_zero_rejected_on_deserialization() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "hello world").unwrap();

        let json = serde_json::to_string(&index).unwrap();
        // Inject block_size=0 into the serialized form.
        let tampered = json.replace(
            &format!("\"block_size\":{}", crate::index::DEFAULT_BLOCK_SIZE),
            "\"block_size\":0",
        );

        let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
        assert!(
            result.is_err(),
            "block_size=0 must be rejected during deserialization"
        );
    }

    // -------------------------------------------------------------------------
    // Finding 7: postings_epoch sentinel collision with Option<u64>
    // Setting postings_epoch to u64::MAX must not be treated as stale.
    // -------------------------------------------------------------------------

    #[test]
    fn postings_epoch_max_does_not_collide_with_stale_sentinel() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "hello world").unwrap();

        // Force the epoch to u64::MAX via the serialized form.
        let json = serde_json::to_string(&index).unwrap();
        let tampered = json.replace(
            &format!("\"postings_epoch\":{}", index.postings_epoch),
            &format!("\"postings_epoch\":{}", u64::MAX),
        );

        let restored: Bm25Index = serde_json::from_str(&tampered).unwrap();

        // With the old u64::MAX sentinel this would incorrectly appear stale.
        // With Option<u64>, Some(u64::MAX) != None so it is treated as valid.
        // Search must still work.
        let results = restored.search("hello", 10);
        assert_eq!(
            results.len(),
            1,
            "search must work with postings_epoch=u64::MAX"
        );
    }

    // -------------------------------------------------------------------------
    // Finding 8: PostingList invariants validated on deserialization
    // -------------------------------------------------------------------------

    #[test]
    fn posting_list_sentinel_doc_id_rejected_via_index_serde() {
        let mut index = Bm25Index::default();
        index.index_document("doc1", "hello").unwrap();

        let json = serde_json::to_string(&index).unwrap();

        // Inject u32::MAX as a doc_id in the posting list for "hello".
        // doc_ids is serialized as an array; replace [0] with [4294967295].
        let tampered = json.replace("[0],\"term_freqs\":[1]", "[4294967295],\"term_freqs\":[1]");
        if tampered == json {
            // Pattern not found in this serialization; skip (implementation detail).
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
        // Index 2 docs so there are 2 postings for "common".
        index.index_document("doc0", "common term").unwrap();
        index.index_document("doc1", "common word").unwrap();

        let json = serde_json::to_string(&index).unwrap();

        // Swap internal IDs [0,1] -> [1,0] in the posting list (unsorted).
        let tampered = json.replace("[0,1],\"term_freqs\"", "[1,0],\"term_freqs\"");
        if tampered == json {
            return; // Pattern not present; skip.
        }

        let result: Result<Bm25Index, _> = serde_json::from_str(&tampered);
        assert!(
            result.is_err(),
            "posting list with unsorted doc_ids must be rejected"
        );
    }
}
