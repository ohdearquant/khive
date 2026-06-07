/// Golden tests for BM25 scoring (RETRIEVAL-04).
///
/// These tests verify known expected values to detect drift in scoring behavior
/// across versions or platforms. The expected values were computed with the
/// standard BM25 formula (k1=1.2, b=0.75) and verified manually.
///
/// # Cross-Platform CI Note
///
/// These tests should run on all CI platforms (Linux, macOS, Windows) to verify
/// consistent scoring. The tolerance (1e-6) accounts for minor FP differences
/// while still catching significant regressions.
///
/// If these tests fail on a specific platform, investigate:
/// 1. FMA instruction availability differences
/// 2. Compiler optimization flags
/// 3. Extended precision (x87) on older x86
use khive_bm25::{Bm25Config, Bm25Index};

/// Tolerance for floating-point comparison in golden tests.
/// 1e-6 is tight enough to catch bugs but loose enough for cross-platform variance.
const GOLDEN_TOLERANCE: f64 = 1e-6;

/// Golden test corpus for reproducible scoring.
fn setup_golden_corpus() -> Bm25Index {
    let mut index = Bm25Index::new(Bm25Config::default());
    // Fixed corpus with known characteristics:
    // doc1: 4 tokens (quick, brown, fox, jumps)
    // doc2: 3 tokens (lazy, brown, dog)
    // doc3: 2 tokens (quick, fox)
    // Total: 9 tokens, avgdl = 3.0
    index
        .index_document("doc1".to_string(), "quick brown fox jumps")
        .unwrap();
    index
        .index_document("doc2".to_string(), "lazy brown dog")
        .unwrap();
    index
        .index_document("doc3".to_string(), "quick fox")
        .unwrap();
    index
}

#[test]
fn golden_single_term_query() {
    let index = setup_golden_corpus();

    // Query for "brown" (appears in doc1 and doc2)
    // IDF("brown") = ln((3 - 2 + 0.5) / (2 + 0.5) + 1) = ln(1.6) ≈ 0.470003629
    let results = index.search("brown", 10);

    assert_eq!(results.len(), 2);

    // Both docs contain "brown" once, but doc2 is shorter (3 tokens vs 4)
    // so doc2 should score slightly higher with length normalization
    let doc1_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc1")
        .unwrap()
        .1;
    let doc2_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc2")
        .unwrap()
        .1;

    // Golden values (empirically verified from implementation with k1=1.2, b=0.75, avgdl=3.0)
    // These values are the actual outputs and serve as regression tests.
    // doc1: len=4, higher length penalty
    // doc2: len=3 (at avgdl), no length adjustment
    assert!(
        (doc1_score.to_f64() - 0.4136031938251108).abs() < GOLDEN_TOLERANCE,
        "doc1 score {} differs from golden 0.4136031938251108",
        doc1_score.to_f64()
    );
    assert!(
        (doc2_score.to_f64() - 0.47000362924573563).abs() < GOLDEN_TOLERANCE,
        "doc2 score {} differs from golden 0.47000362924573563",
        doc2_score.to_f64()
    );
}

#[test]
fn golden_multi_term_query() {
    let index = setup_golden_corpus();

    // Query for "quick fox" (doc1 has both, doc3 has both)
    let results = index.search("quick fox", 10);

    assert_eq!(results.len(), 2);

    let doc1_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc1")
        .unwrap()
        .1;
    let doc3_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc3")
        .unwrap()
        .1;

    // doc3 is shorter (2 tokens) and has both terms -> should score higher
    assert!(doc3_score > doc1_score);

    // Golden values for multi-term query (empirically verified)
    // "quick": df=2, "fox": df=2
    // doc3 (len=2): shorter doc gets boost from length normalization
    assert!(
        (doc3_score.to_f64() - 1.088429457275197).abs() < GOLDEN_TOLERANCE,
        "doc3 score {} differs from golden 1.088429457275197",
        doc3_score.to_f64()
    );
}

#[test]
fn golden_rare_term_high_idf() {
    let index = setup_golden_corpus();

    // "jumps" only in doc1 (df=1), "lazy" only in doc2 (df=1)
    // Both have high IDF = ln((3-1+0.5)/(1+0.5)+1) = ln(2.667) ≈ 0.981
    let results = index.search("jumps", 10);

    assert_eq!(results.len(), 1);
    assert_eq!(&*results[0].0, "doc1");

    // Golden value for rare term (empirically verified)
    // "jumps" has high IDF due to appearing in only 1 document
    // doc1 has length penalty (len=4, avgdl=3)
    assert!(
        (results[0].1.to_f64() - 0.8631297426763922).abs() < GOLDEN_TOLERANCE,
        "rare term score {} differs from golden 0.8631297426763922",
        results[0].1.to_f64()
    );
}

#[test]
fn golden_term_frequency_saturation() {
    // Test that repeated terms show saturation (TF component approaches k1+1=2.2)
    let mut index = Bm25Index::new(Bm25Config::default());

    // doc1 has "test" once, doc2 has it 5 times
    index.index_document("doc1".to_string(), "test").unwrap();
    index
        .index_document("doc2".to_string(), "test test test test test")
        .unwrap();

    let results = index.search("test", 10);
    assert_eq!(results.len(), 2);

    let doc1_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc1")
        .unwrap()
        .1;
    let doc2_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "doc2")
        .unwrap()
        .1;

    // doc2 has higher TF but saturation limits the boost
    // The score ratio should be much less than 5x
    let ratio = doc2_score.to_f64() / doc1_score.to_f64();
    assert!(
        ratio < 2.5,
        "TF saturation not working: ratio {ratio} should be < 2.5"
    );
    assert!(
        ratio > 1.0,
        "Higher TF should still score higher: ratio {ratio}"
    );

    // Golden: with avgdl=3, k1=1.2, b=0.75:
    // doc1 (tf=1, len=1, L=0.333): denom=1+1.2*(0.25+0.75*0.333)=1.6, TF=2.2/1.6=1.375
    // doc2 (tf=5, len=5, L=1.667): denom=5+1.2*(0.25+0.75*1.667)=6.8, TF=11/6.8=1.618
    // Score ratio ≈ 1.618/1.375 ≈ 1.177
    assert!(
        (ratio - 1.17682).abs() < 0.01,
        "TF saturation ratio {ratio} differs from golden 1.177"
    );
}

#[test]
fn golden_length_normalization() {
    // Test length normalization with same term frequency
    let mut index = Bm25Index::new(Bm25Config::default());

    // Both have "test" once, but different lengths
    index.index_document("short".to_string(), "test").unwrap();
    index
        .index_document("long".to_string(), "test padding padding padding padding")
        .unwrap();

    let results = index.search("test", 10);
    assert_eq!(results.len(), 2);

    let short_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "short")
        .unwrap()
        .1;
    let long_score = results
        .iter()
        .find(|(id, _)| id.as_ref() == "long")
        .unwrap()
        .1;

    // Shorter doc should score higher (b=0.75 applies length penalty)
    assert!(
        short_score > long_score,
        "Short doc should score higher than long doc"
    );

    // Golden: avgdl=3, k1=1.2, b=0.75
    // short (len=1, L=0.333): denom=1+1.2*(0.25+0.25)=1.6, TF=2.2/1.6=1.375
    // long (len=5, L=1.667): denom=1+1.2*(0.25+1.25)=2.8, TF=2.2/2.8=0.786
    let ratio = short_score.to_f64() / long_score.to_f64();
    assert!(
        (ratio - 1.75).abs() < 0.1,
        "Length normalization ratio {ratio} differs from expected ~1.75"
    );
}

#[test]
fn golden_deterministic_across_runs() {
    // Verify that multiple searches produce identical results
    let index = setup_golden_corpus();

    let results1 = index.search("quick brown", 10);
    let results2 = index.search("quick brown", 10);
    let results3 = index.search("quick brown", 10);

    assert_eq!(results1.len(), results2.len());
    assert_eq!(results2.len(), results3.len());

    for i in 0..results1.len() {
        assert_eq!(
            results1[i].0, results2[i].0,
            "Doc ID mismatch at position {i}"
        );
        assert_eq!(
            results1[i].1, results2[i].1,
            "Score mismatch at position {i}"
        );
        assert_eq!(
            results2[i].1, results3[i].1,
            "Score mismatch at position {i}"
        );
    }
}
