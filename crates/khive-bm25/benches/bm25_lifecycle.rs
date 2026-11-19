use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use khive_bm25::{Bm25Index, SimpleTokenizer, Tokenizer};
use std::hint::black_box;
use std::time::{Duration, Instant};

const WORDS_PER_DOCUMENT: usize = 200;
const VOCABULARY: [&str; 16] = [
    "alpha", "beta", "gamma", "delta", "river", "forest", "copper", "silver", "vector", "matrix",
    "kernel", "corpus", "search", "index", "quantum", "photon",
];
const INSERTED_ID: &str = "after-insert";
const INSERTED_TEXT: &str = "freshdocument freshdocument freshdocument";

fn corpus(size: usize) -> Bm25Index {
    let mut index = Bm25Index::default();
    for document in 0..size {
        let text = (0..WORDS_PER_DOCUMENT)
            .map(|word| VOCABULARY[(42 + document * 7 + word * 5) % VOCABULARY.len()])
            .collect::<Vec<_>>()
            .join(" ");
        index
            .index_document(format!("document-{document}"), &text)
            .expect("generated document fits the default index");
    }
    let stats = index.stats();
    assert_eq!(stats.doc_count, size);
    assert_eq!(stats.total_tokens, size * WORDS_PER_DOCUMENT);
    assert_eq!(stats.avg_doc_length, WORDS_PER_DOCUMENT as f64);
    assert_eq!(stats.unique_terms, VOCABULARY.len());
    index
}

fn after_insert(base: &Bm25Index) -> Bm25Index {
    let mut index = base.clone();
    index
        .index_document(INSERTED_ID, INSERTED_TEXT)
        .expect("single insert succeeds");
    assert_eq!(index.doc_count(), 1_001);
    index
}

fn bench_clear(c: &mut Criterion) {
    let base = corpus(1_000);
    c.bench_function("clear/1000", |b| {
        b.iter_custom(|iterations| {
            let mut measured = Duration::ZERO;
            for _ in 0..iterations {
                let mut index = base.clone();
                let start = Instant::now();
                black_box(&mut index).clear();
                measured += start.elapsed();

                let stats = index.stats();
                assert_eq!(stats.doc_count, 0);
                assert_eq!(stats.total_tokens, 0);
                assert_eq!(stats.unique_terms, 0);
                assert!(index.search("alpha", 10).is_empty());
                index
                    .index_document(INSERTED_ID, INSERTED_TEXT)
                    .expect("cleared index remains reusable");
                assert_eq!(index.doc_count(), 1);
                assert_eq!(index.stats().total_tokens, 3);
                assert_eq!(index.search("freshdocument", 10)[0].0.as_ref(), INSERTED_ID);
            }
            measured
        });
    });
}

fn bench_ensure_guards(c: &mut Criterion) {
    let base = corpus(1_000);
    // Inserts already maintain both caches. These public calls take their
    // populated-cache guard path; they do not materialize a cold index.
    let mut probe = after_insert(&base);
    probe.ensure_forward_index();
    probe.ensure_doc_lengths_vec();
    assert_eq!(probe.search("freshdocument", 10)[0].0.as_ref(), INSERTED_ID);
    assert!(probe.remove_document(INSERTED_ID));
    assert_eq!(probe.doc_count(), base.doc_count());
    assert_eq!(probe.stats().total_tokens, base.stats().total_tokens);
    drop(probe);

    let mut group = c.benchmark_group("ensure_after_insert");
    group.bench_function("forward_index_guard/1000_plus_1", |b| {
        b.iter_batched_ref(
            || after_insert(&base),
            |index| {
                black_box(index).ensure_forward_index();
            },
            BatchSize::PerIteration,
        );
    });
    group.bench_function("doc_lengths_guard/1000_plus_1", |b| {
        b.iter_batched_ref(
            || after_insert(&base),
            |index| {
                black_box(index).ensure_doc_lengths_vec();
            },
            BatchSize::PerIteration,
        );
    });
    group.finish();
}

fn bench_stats(c: &mut Criterion) {
    let mut group = c.benchmark_group("stats");
    for size in [100, 500, 1_000] {
        let index = corpus(size);
        group.bench_with_input(BenchmarkId::from_parameter(size), &index, |b, index| {
            b.iter(|| black_box(black_box(index).stats()));
        });
    }
    group.finish();
}

fn bench_tokenizer(c: &mut Criterion) {
    let tokenizer = SimpleTokenizer::default();
    let mut group = c.benchmark_group("tokenize_unicode_chars");
    for chars in [200, 2_000, 20_000] {
        let text: String = "Alpha, café 東京42 beta! 123 kernel "
            .chars()
            .cycle()
            .take(chars)
            .collect();
        assert_eq!(text.chars().count(), chars);
        assert!(!text.is_ascii());
        assert!(!tokenizer.tokenize(&text).is_empty());
        group.throughput(Throughput::Elements(chars as u64));
        group.bench_with_input(BenchmarkId::from_parameter(chars), &text, |b, text| {
            b.iter_batched(
                || (),
                |()| black_box(tokenizer.tokenize(black_box(text.as_str()))),
                BatchSize::PerIteration,
            );
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_clear,
    bench_ensure_guards,
    bench_stats,
    bench_tokenizer
);
criterion_main!(benches);
