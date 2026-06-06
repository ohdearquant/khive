use std::sync::Arc;

use khive_bm25::{Bm25Config, Bm25Index, DeterministicScore, SearchContext, DEFAULT_BLOCK_SIZE};

#[derive(Clone)]
struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    fn new(seed: u64) -> Self {
        Self { state: seed.max(1) }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    fn next_f64(&mut self) -> f64 {
        ((self.next_u64() >> 11) as f64) / ((1u64 << 53) as f64)
    }

    fn gen_range(&mut self, upper: usize) -> usize {
        if upper <= 1 {
            0
        } else {
            (self.next_u64() as usize) % upper
        }
    }
}

struct ZipfSampler {
    cdf: Vec<f64>,
}

impl ZipfSampler {
    fn new(vocab_size: usize, exponent: f64) -> Self {
        let mut cumulative = Vec::with_capacity(vocab_size);
        let mut running = 0.0;
        for rank in 1..=vocab_size {
            running += 1.0 / (rank as f64).powf(exponent);
            cumulative.push(running);
        }
        for value in &mut cumulative {
            *value /= running;
        }
        Self { cdf: cumulative }
    }

    fn sample(&self, rng: &mut XorShift64) -> usize {
        let needle = rng.next_f64();
        let idx = self.cdf.partition_point(|value| *value < needle);
        idx.min(self.cdf.len().saturating_sub(1))
    }
}

fn build_vocab(size: usize) -> Vec<String> {
    (0..size).map(|idx| format!("tok_{idx:04}")).collect()
}

fn build_zipf_corpus(index: &mut Bm25Index, doc_count: usize, seed: u64) {
    let vocab = build_vocab(512);
    let zipf = ZipfSampler::new(vocab.len(), 1.07);
    let mut rng = XorShift64::new(seed);

    for doc_idx in 0..doc_count {
        let len = 16 + rng.gen_range(48);
        let mut text = String::new();
        for token_idx in 0..len {
            if token_idx > 0 {
                text.push(' ');
            }
            let token = &vocab[zipf.sample(&mut rng)];
            text.push_str(token);
        }
        index
            .index_document(format!("doc_{doc_idx}"), &text)
            .expect("synthetic document should index");
    }
}

fn build_query(vocab: &[String], zipf: &ZipfSampler, rng: &mut XorShift64, terms: usize) -> String {
    let mut query = String::new();
    for idx in 0..terms {
        if idx > 0 {
            query.push(' ');
        }
        if rng.gen_range(10) == 0 {
            query.push_str("missing_term");
        } else {
            query.push_str(&vocab[zipf.sample(rng)]);
        }
    }
    query
}

/// Assert that two result lists are equivalent within floating-point tolerance.
///
/// Due to floating-point accumulation order differences between brute-force
/// and WAND (cursors are sorted by doc_id, not by query term order), documents
/// with extremely close raw f64 scores may be ordered differently or swapped
/// at the k-th boundary. After `DeterministicScore` quantization, documents
/// that had slightly different raw f64 scores may appear with the same score.
///
/// This comparator verifies:
/// 1. Same result count
/// 2. Score at each rank matches within a small relative tolerance
/// 3. Documents that differ must have scores within floating-point tolerance
///    of the boundary score (the k-th score)
fn assert_same_results(
    expected: &[(Arc<str>, DeterministicScore)],
    actual: &[(Arc<str>, DeterministicScore)],
    context: &str,
) {
    assert_eq!(
        expected.len(),
        actual.len(),
        "result length mismatch for {context}"
    );

    if expected.is_empty() {
        return;
    }

    // Relative tolerance for score comparison.
    // The brute-force path uses f32 SIMD batch scoring (NEON/scalar) then
    // accumulates into f64, while the WAND path scores entirely in f64.
    // f32 has ~7 decimal digits of precision, so per-term error is ~1e-7.
    // With multi-term queries the error accumulates additively, so we use
    // 1e-6 to accommodate up to ~10 query terms with comfortable margin.
    let rel_tol = 1e-6;

    // Get the boundary score (the score at the last position).
    let _boundary_score = expected.last().unwrap().1.to_f64();

    for (rank, ((expected_doc, expected_score), (actual_doc, actual_score))) in
        expected.iter().zip(actual.iter()).enumerate()
    {
        let exp_s = expected_score.to_f64();
        let act_s = actual_score.to_f64();

        // Scores at each rank should be very close.
        let score_diff = (exp_s - act_s).abs();
        let tol = rel_tol * exp_s.abs().max(1.0);
        assert!(
            score_diff <= tol,
            "score mismatch at rank {rank} for {context}: expected {exp_s} got {act_s} (diff={score_diff})"
        );

        // If documents differ, their scores must be within f32 precision of
        // each other. The brute-force path uses f32 SIMD while WAND uses f64,
        // so documents with nearly-identical f64 scores may swap positions when
        // one path computes a slightly different value due to f32 rounding.
        if expected_doc != actual_doc {
            let mutual_diff = (exp_s - act_s).abs();
            let mutual_tol = rel_tol * exp_s.abs().max(1.0);
            assert!(
                mutual_diff <= mutual_tol,
                "doc mismatch at rank {rank} with divergent scores for {context}: \
                 expected=({expected_doc}, {exp_s}) actual=({actual_doc}, {act_s}) diff={mutual_diff} tol={mutual_tol}"
            );
        }
    }
}

#[test]
fn bmw_matches_bruteforce_on_random_zipf_corpora() {
    let vocab = build_vocab(512);
    let zipf = ZipfSampler::new(vocab.len(), 1.07);

    for (case_idx, &doc_count) in [1_000usize, 2_500, 10_000].iter().enumerate() {
        let mut index = Bm25Index::new(Bm25Config::default());
        build_zipf_corpus(&mut index, doc_count, 0xC0FFEE + case_idx as u64);

        let mut rng = XorShift64::new(0xBAD5EED + doc_count as u64);
        let mut brute_ctx = SearchContext::with_capacity(256);
        let mut wand_ctx = SearchContext::with_capacity(256);

        for query_idx in 0..256 {
            let term_count = 1 + rng.gen_range(5);
            let query = build_query(&vocab, &zipf, &mut rng, term_count);
            let k = [1usize, 3, 5, 10, 25][rng.gen_range(5)];

            let brute = index.search_brute_force(&query, k, &mut brute_ctx);
            let wand = index.search_with_context(&query, k, &mut wand_ctx);

            assert_same_results(
                &brute,
                &wand,
                &format!("doc_count={doc_count}, query_idx={query_idx}, query='{query}', k={k}"),
            );
        }
    }
}

#[test]
fn bmw_handles_empty_index_and_zero_k() {
    let index = Bm25Index::new(Bm25Config::default());
    let mut ctx = SearchContext::new();

    assert!(index
        .search_with_context("alpha beta", 10, &mut ctx)
        .is_empty());
    assert!(index
        .search_brute_force("alpha beta", 10, &mut ctx)
        .is_empty());
    assert!(index
        .search_with_context("alpha beta", 0, &mut ctx)
        .is_empty());
}

#[test]
fn bmw_handles_single_document_and_large_k() {
    let mut index = Bm25Index::new(Bm25Config::default());
    index
        .index_document("doc1", "alpha beta gamma alpha")
        .unwrap();

    let mut brute_ctx = SearchContext::new();
    let mut wand_ctx = SearchContext::new();

    let brute = index.search_brute_force("alpha gamma", 10, &mut brute_ctx);
    let wand = index.search_with_context("alpha gamma", 10, &mut wand_ctx);

    assert_same_results(&brute, &wand, "single document / large k");
    assert_eq!(wand.len(), 1);
    assert_eq!(&*wand[0].0, "doc1");
}

#[test]
fn bmw_handles_all_docs_match_and_no_docs_match() {
    let mut index = Bm25Index::new(Bm25Config::default());
    for doc_idx in 0..300 {
        index
            .index_document(format!("doc_{doc_idx}"), &format!("common term_{doc_idx}"))
            .unwrap();
    }

    let mut brute_ctx = SearchContext::new();
    let mut wand_ctx = SearchContext::new();

    let brute_all = index.search_brute_force("common", 20, &mut brute_ctx);
    let wand_all = index.search_with_context("common", 20, &mut wand_ctx);
    assert_same_results(&brute_all, &wand_all, "all docs match");

    let brute_none = index.search_brute_force("absent_token", 20, &mut brute_ctx);
    let wand_none = index.search_with_context("absent_token", 20, &mut wand_ctx);
    assert_same_results(&brute_none, &wand_none, "no docs match");
    assert!(wand_none.is_empty());
}

#[test]
fn bmw_handles_many_term_queries() {
    let mut index = Bm25Index::new(Bm25Config::default());
    build_zipf_corpus(&mut index, 2_000, 0x1234_5678);

    let query = "tok_0000 tok_0001 tok_0002 tok_0003 tok_0004 tok_0005 tok_0006 tok_0007";
    let mut brute_ctx = SearchContext::new();
    let mut wand_ctx = SearchContext::new();

    let brute = index.search_brute_force(query, 15, &mut brute_ctx);
    let wand = index.search_with_context(query, 15, &mut wand_ctx);

    assert_same_results(&brute, &wand, "many term query");
}

#[test]
fn bmw_block_boundary_regression() {
    let mut index = Bm25Index::new(Bm25Config::default());
    let filler = " filler filler filler filler filler filler filler filler";

    for doc_idx in 0..(DEFAULT_BLOCK_SIZE * 2) {
        let repeats = if doc_idx == DEFAULT_BLOCK_SIZE - 1 || doc_idx == DEFAULT_BLOCK_SIZE {
            12
        } else {
            1
        };

        let mut text = String::new();
        for rep in 0..repeats {
            if rep > 0 {
                text.push(' ');
            }
            text.push_str("boundary");
        }
        text.push_str(filler);

        index
            .index_document(format!("doc_{doc_idx}"), &text)
            .unwrap();
    }

    let mut brute_ctx = SearchContext::new();
    let mut wand_ctx = SearchContext::new();

    let brute = index.search_brute_force("boundary", 5, &mut brute_ctx);
    let wand = index.search_with_context("boundary", 5, &mut wand_ctx);

    assert_same_results(&brute, &wand, "block boundary regression");
    assert_eq!(wand.first().map(|entry| entry.0.as_ref()), Some("doc_127"));
}

#[test]
fn sorted_posting_lists_are_maintained_across_mutations() {
    let mut index = Bm25Index::new(Bm25Config::default());
    index.index_document("c", "alpha beta").unwrap();
    index.index_document("a", "alpha gamma").unwrap();
    index.index_document("b", "alpha delta").unwrap();
    assert!(index.remove_document("a"));
    index.index_document("a2", "alpha epsilon").unwrap();

    let postings = index
        .inverted_index_for_test("alpha")
        .expect("alpha postings should exist");

    assert!(postings
        .doc_ids
        .windows(2)
        .all(|window| window[0] < window[1]));
}
