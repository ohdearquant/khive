use std::hint::black_box;
use std::time::Instant;

use super::{Bm25Index, SearchContext};
use crate::config::Bm25Config;

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

fn build_index(doc_count: usize, seed: u64) -> (Bm25Index, Vec<String>, ZipfSampler) {
    let vocab = build_vocab(2_048);
    let zipf = ZipfSampler::new(vocab.len(), 1.07);
    let mut rng = XorShift64::new(seed);
    let mut index = Bm25Index::new(Bm25Config::default());

    for doc_idx in 0..doc_count {
        let len = 24 + rng.gen_range(40);
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

    (index, vocab, zipf)
}

fn build_queries(
    vocab: &[String],
    zipf: &ZipfSampler,
    rng: &mut XorShift64,
    count: usize,
    terms_per_query: usize,
) -> Vec<String> {
    let mut queries = Vec::with_capacity(count);
    for _ in 0..count {
        let mut query = String::new();
        for idx in 0..terms_per_query {
            if idx > 0 {
                query.push(' ');
            }
            query.push_str(&vocab[zipf.sample(rng)]);
        }
        queries.push(query);
    }
    queries
}

/// Benchmark: WAND vs brute-force on Zipf-distributed corpora.
///
/// Note: `search_with_context` routes to WAND only when total query postings
/// exceed `SMALL_QUERY_POSTINGS_THRESHOLD` (256). For very rare terms or
/// small corpora, the brute-force path may be taken instead, so speedup
/// numbers should be interpreted accordingly.
#[test]
#[ignore = "benchmark; run with `cargo test bench_wand -- --ignored --nocapture`"]
fn bench_bm25_wand_vs_bruteforce_zipf_matrix() {
    let corpus_sizes = [10_000usize, 50_000, 100_000];
    let query_lengths = [1usize, 2, 3];

    for &doc_count in &corpus_sizes {
        let (index, vocab, zipf) = build_index(doc_count, 0xFACE_FEED ^ doc_count as u64);

        println!("\nCorpus: {doc_count} docs");
        println!("query_terms | brute_force_ms | bmw_ms | speedup_x");
        println!("------------|----------------|--------|----------");

        for &terms_per_query in &query_lengths {
            let mut rng = XorShift64::new(0xDEAD_BEEF ^ ((doc_count as u64) << terms_per_query));
            let queries = build_queries(&vocab, &zipf, &mut rng, 64, terms_per_query);

            let mut brute_ctx = SearchContext::with_capacity(512);
            let brute_start = Instant::now();
            for query in &queries {
                black_box(index.search_brute_force(query, 10, &mut brute_ctx));
            }
            let brute_ms = brute_start.elapsed().as_secs_f64() * 1000.0;

            let mut wand_ctx = SearchContext::with_capacity(512);
            let wand_start = Instant::now();
            for query in &queries {
                black_box(index.search_with_context(query, 10, &mut wand_ctx));
            }
            let wand_ms = wand_start.elapsed().as_secs_f64() * 1000.0;

            let speedup = if wand_ms > 0.0 {
                brute_ms / wand_ms
            } else {
                f64::INFINITY
            };

            println!("{terms_per_query:>11} | {brute_ms:>14.3} | {wand_ms:>6.3} | {speedup:>8.2}");
        }
    }
}
