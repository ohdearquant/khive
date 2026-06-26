use super::simd::*;
use super::*;

/// Reference scalar BM25 score for a single posting.
fn scalar_bm25(tf: u8, doc_len: f32, idf: f32, k1p1: f32, base: f32, dl_fac: f32) -> f32 {
    let tf = tf as f32;
    let num = tf * k1p1;
    let denom = tf + base + dl_fac * doc_len;
    idf * (num / denom)
}

/// Compute reference scores for an arbitrary-length batch using scalar code.
fn reference_scores(
    tfs: &[u8],
    dls: &[f32],
    idf: f32,
    k1p1: f32,
    base: f32,
    dl_fac: f32,
) -> Vec<f32> {
    tfs.iter()
        .zip(dls.iter())
        .map(|(&tf, &dl)| scalar_bm25(tf, dl, idf, k1p1, base, dl_fac))
        .collect()
}

// Test parameters (standard BM25 with k1=1.2, b=0.75, avgdl=10.0)
const TEST_IDF: f32 = 1.5;
const TEST_K1P1: f32 = 2.2; // k1 + 1 = 1.2 + 1
const TEST_BASE: f32 = 0.3; // k1 * (1 - b) = 1.2 * 0.25
const TEST_DL_FAC: f32 = 0.09; // k1 * b / avgdl = 1.2 * 0.75 / 10.0

// -----------------------------------------------------------------------
// Test 1: scalar_4 vs reference (parity check)
// -----------------------------------------------------------------------

#[test]
fn test_score_batch_4_matches_scalar() {
    let tfs: [u8; 4] = [1, 3, 5, 10];
    let dls: [f32; 4] = [8.0, 12.0, 5.0, 20.0];

    let batch = score_batch_4(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    for i in 0..4 {
        assert!(
            (batch[i] - reference[i]).abs() < 1e-6,
            "batch_4[{i}] = {}, expected {} (delta {})",
            batch[i],
            reference[i],
            (batch[i] - reference[i]).abs()
        );
    }
}

// -----------------------------------------------------------------------
// Test 2: x86_64 AVX2 8-wide vs scalar parity
// -----------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[test]
fn test_avx2_matches_scalar_basic() {
    if !is_x86_feature_detected!("avx2") {
        eprintln!("AVX2 not available, skipping test");
        return;
    }

    let tfs: [u8; 8] = [1, 2, 3, 5, 8, 13, 21, 34];
    let dls: [f32; 8] = [5.0, 10.0, 15.0, 20.0, 25.0, 30.0, 35.0, 40.0];

    // SAFETY: The test returns early unless AVX2 is detected, and the
    // fixed-size arrays provide all lanes consumed by the helper.
    let avx2_result =
        unsafe { score_batch_avx2(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC) };
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    for i in 0..8 {
        assert!(
            (avx2_result[i] - reference[i]).abs() < 1e-6,
            "avx2[{i}] = {}, expected {} (delta {})",
            avx2_result[i],
            reference[i],
            (avx2_result[i] - reference[i]).abs()
        );
    }
}

// -----------------------------------------------------------------------
// Test 3: AVX2+FMA vs scalar (slightly relaxed tolerance due to FMA rounding)
// -----------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[test]
fn test_avx2_fma_matches_scalar() {
    if !is_x86_feature_detected!("avx2") || !is_x86_feature_detected!("fma") {
        eprintln!("AVX2+FMA not available, skipping test");
        return;
    }

    let tfs: [u8; 8] = [0, 1, 127, 255, 42, 7, 99, 200];
    let dls: [f32; 8] = [1.0, 2.0, 100.0, 0.5, 10.0, 50.0, 3.0, 1000.0];

    // SAFETY: The test returns early unless AVX2+FMA is detected, and the
    // fixed-size arrays provide all lanes consumed by the helper.
    let fma_result =
        unsafe { score_batch_avx2_fma(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC) };
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    // FMA has single rounding vs two roundings in mul+add, so allow slightly
    // more tolerance (1 ULP of f32 ~ 1.19e-7, we allow ~10 ULPs).
    for i in 0..8 {
        let tol = reference[i].abs() * 1e-6 + 1e-7;
        assert!(
            (fma_result[i] - reference[i]).abs() < tol,
            "fma[{i}] = {}, expected {} (delta {}, tol {})",
            fma_result[i],
            reference[i],
            (fma_result[i] - reference[i]).abs(),
            tol
        );
    }
}

// -----------------------------------------------------------------------
// Test 4: x86_64 dispatch function selects correctly and produces correct results
// -----------------------------------------------------------------------

#[cfg(target_arch = "x86_64")]
#[test]
fn test_dispatch_score_batch_8() {
    let score_fn = select_score_batch_8();

    let tfs: [u8; 8] = [3, 7, 1, 15, 0, 255, 128, 50];
    let dls: [f32; 8] = [10.0, 5.0, 20.0, 8.0, 100.0, 1.0, 15.0, 30.0];

    // SAFETY: `select_score_batch_8` only returns a target-feature helper
    // after matching runtime CPU detection; otherwise it returns scalar.
    let dispatched = unsafe { score_fn(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC) };
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    for i in 0..8 {
        let tol = reference[i].abs() * 1e-5 + 1e-7;
        assert!(
            (dispatched[i] - reference[i]).abs() < tol,
            "dispatch[{i}] = {}, expected {} (delta {})",
            dispatched[i],
            reference[i],
            (dispatched[i] - reference[i]).abs()
        );
    }
}

// -----------------------------------------------------------------------
// Test 5: Edge case -- tf=0 produces zero score
// -----------------------------------------------------------------------

#[test]
fn test_tf_zero_produces_zero_score() {
    let tfs_4: [u8; 4] = [0, 0, 0, 0];
    let dls_4: [f32; 4] = [10.0, 20.0, 5.0, 1.0];
    let result = score_batch_4(&tfs_4, &dls_4, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
    for val in &result {
        assert!(
            val.abs() < 1e-10,
            "tf=0 should produce ~0 score, got {}",
            val
        );
    }

    #[cfg(target_arch = "x86_64")]
    if is_x86_feature_detected!("avx2") {
        let tfs_8: [u8; 8] = [0; 8];
        let dls_8: [f32; 8] = [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0];
        // SAFETY: This branch only runs when AVX2 is detected, and the
        // fixed-size arrays provide all lanes consumed by the helper.
        let result = unsafe {
            score_batch_avx2(&tfs_8, &dls_8, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC)
        };
        for val in &result {
            assert!(
                val.abs() < 1e-10,
                "avx2 tf=0 should produce ~0 score, got {}",
                val
            );
        }
    }
}

// -----------------------------------------------------------------------
// Test 6: Edge case -- very large doc_length
// -----------------------------------------------------------------------

#[test]
fn test_large_doc_length() {
    let tfs: [u8; 4] = [5, 10, 20, 50];
    let dls: [f32; 4] = [1e6, 1e6, 1e6, 1e6];
    let result = score_batch_4(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    for i in 0..4 {
        // Very large doc_length pushes scores toward zero but they should
        // still be positive and match scalar.
        assert!(result[i] > 0.0, "score should be positive");
        assert!(
            (result[i] - reference[i]).abs() < 1e-6,
            "large dl mismatch at [{i}]: {} vs {}",
            result[i],
            reference[i]
        );
    }
}

// -----------------------------------------------------------------------
// Test 7: Edge case -- max tf (255)
// -----------------------------------------------------------------------

#[test]
fn test_max_tf() {
    let tfs: [u8; 4] = [255, 255, 255, 255];
    let dls: [f32; 4] = [10.0, 10.0, 10.0, 10.0];
    let result = score_batch_4(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    for i in 0..4 {
        assert!(
            (result[i] - reference[i]).abs() < 1e-5,
            "max tf mismatch at [{i}]: {} vs {}",
            result[i],
            reference[i]
        );
    }
}

// -----------------------------------------------------------------------
// Test 8: Integration test -- brute-force search with various posting lengths
// Exercises batch sizes 1, 7, 8, 16, 100 by indexing documents.
// -----------------------------------------------------------------------

#[test]
fn test_brute_force_search_various_sizes() {
    use crate::{Bm25Config, Bm25Index};

    let mut index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");

    // Index enough documents to exercise different batch sizes.
    // The word "alpha" appears in all 100 docs, giving a posting list of 100.
    // The word "beta" appears in 16 docs.
    // The word "gamma" appears in 8 docs.
    // The word "delta" appears in 7 docs.
    // The word "epsilon" appears in 1 doc.
    for i in 0..100 {
        let mut text = format!("alpha doc{i}");
        if i < 16 {
            text.push_str(" beta");
        }
        if i < 8 {
            text.push_str(" gamma");
        }
        if i < 7 {
            text.push_str(" delta");
        }
        if i == 0 {
            text.push_str(" epsilon");
        }
        index.index_document(format!("doc{i}"), &text).unwrap();
    }

    // Each query exercises a different posting list length through brute-force.
    let mut ctx = SearchContext::new();
    for query in &["alpha", "beta", "gamma", "delta", "epsilon"] {
        let results = index.search_with_context(query, 10, &mut ctx);
        assert!(!results.is_empty(), "query '{query}' should return results");
        // All scores should be positive.
        for (doc_id, score) in &results {
            assert!(
                score.to_f64() > 0.0,
                "query '{query}', doc '{doc_id}': score should be positive"
            );
        }
    }

    // Multi-term query exercises score accumulation across terms.
    let results = index.search_with_context("alpha beta gamma", 5, &mut ctx);
    assert!(!results.is_empty());
    // The first result should be a doc that contains all three terms.
    let (top_doc, _) = &results[0];
    let top_id: usize = top_doc.strip_prefix("doc").unwrap().parse().unwrap();
    assert!(
        top_id < 8,
        "top result should be a doc with all 3 terms (doc0-doc7), got doc{top_id}"
    );
}

// -----------------------------------------------------------------------
// Test 9: scalar_8 matches reference (non-SIMD path)
// -----------------------------------------------------------------------

#[cfg(not(target_arch = "aarch64"))]
#[test]
fn test_score_batch_scalar_8_matches_reference() {
    let tfs: [u8; 8] = [1, 5, 10, 20, 50, 100, 200, 255];
    let dls: [f32; 8] = [3.0, 7.0, 15.0, 25.0, 50.0, 100.0, 200.0, 500.0];

    let result = score_batch_scalar_8(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);
    let reference = reference_scores(&tfs, &dls, TEST_IDF, TEST_K1P1, TEST_BASE, TEST_DL_FAC);

    for i in 0..8 {
        assert!(
            (result[i] - reference[i]).abs() < 1e-7,
            "scalar_8[{i}] = {}, expected {}",
            result[i],
            reference[i]
        );
    }
}

// -----------------------------------------------------------------------
// Test 10: Empty posting list handled correctly (no panic)
// -----------------------------------------------------------------------

#[test]
fn test_empty_posting_list_search() {
    use crate::{Bm25Config, Bm25Index};

    let mut index = Bm25Index::try_new(Bm25Config::default()).expect("valid config");
    index.index_document("doc1", "hello world").unwrap();

    // Search for a term not in the index.
    let results = index.search("nonexistent", 10);
    assert!(results.is_empty());
}
