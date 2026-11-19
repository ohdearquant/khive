#[path = "additional_support/tokenize.rs"]
mod tokenize;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_text::{
    contains_cjk_f32, is_meaningful_text_query,
    tokenizer::{IdentifierTokenizer, KeywordTokenizer},
};
use std::hint::black_box;
use std::time::Instant;

const IDENTIFIERS: &str = "fine_tuning camelCase HTTP2Client ";
const IDENTIFIER_TOKENS: [&str; 10] = [
    "fine_tuning",
    "fine",
    "tuning",
    "camelcase",
    "camel",
    "case",
    "http2client",
    "http",
    "2",
    "client",
];

fn tokenizer_benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tokenize_additional");
    for target in tokenize::SIZES {
        let keyword_chars =
            target.div_ceil(tokenize::WORDS.chars().count()) * tokenize::WORDS.chars().count();
        group.throughput(Throughput::Elements(keyword_chars as u64));
        let mut fixture = None;
        group.bench_function(BenchmarkId::new("keyword", keyword_chars), |b| {
            let (text, expected) = fixture.get_or_insert_with(|| {
                let (text, _) = tokenize::repeated(tokenize::WORDS, target);
                let expected = vec![text.trim_end().to_string()];
                assert_eq!(text.chars().count(), keyword_chars);
                (text, expected)
            });
            tokenize::run(b, &KeywordTokenizer, text, expected);
        });

        let identifier_chars =
            target.div_ceil(IDENTIFIERS.chars().count()) * IDENTIFIERS.chars().count();
        group.throughput(Throughput::Elements(identifier_chars as u64));
        let mut fixture = None;
        group.bench_function(BenchmarkId::new("identifier", identifier_chars), |b| {
            let (text, expected) = fixture.get_or_insert_with(|| {
                let (text, repetitions) = tokenize::repeated(IDENTIFIERS, target);
                let expected = (0..repetitions)
                    .flat_map(|_| IDENTIFIER_TOKENS.map(str::to_string))
                    .collect::<Vec<_>>();
                assert_eq!(expected.len(), repetitions * IDENTIFIER_TOKENS.len());
                assert_eq!(text.chars().count(), identifier_chars);
                (text, expected)
            });
            tokenize::run(b, &IdentifierTokenizer::default(), text, expected);
        });
    }
    group.finish();
}

// Exercise the compatibility entry point requested by this benchmark, without
// implying that it implements a second query policy.
#[allow(deprecated)]
fn legacy_meaningful(query: &str) -> bool {
    khive_text::is_meaningful_query(query)
}

fn median_call_ns(function: fn(&str) -> bool, input: &str, expected: bool) -> u128 {
    let mut samples = [0u128; 101];
    for sample in &mut samples {
        let started = Instant::now();
        let actual = black_box(function(black_box(input)));
        *sample = started.elapsed().as_nanos();
        assert_eq!(actual, expected);
    }
    samples.sort_unstable();
    samples[samples.len() / 2]
}

type LanguageCase = (&'static str, fn(&str) -> bool, &'static str, bool);

fn language_benches(c: &mut Criterion) {
    let cases: [LanguageCase; 4] = [
        (
            "legacy_meaningful/normal",
            legacy_meaningful,
            "transformer attention mechanism",
            true,
        ),
        (
            "legacy_meaningful/gibberish",
            legacy_meaningful,
            "aaaaaaaaa",
            false,
        ),
        (
            "contains_cjk_f32/latin",
            contains_cjk_f32,
            "retrieval index search",
            false,
        ),
        (
            "contains_cjk_f32/mixed",
            contains_cjk_f32,
            "检索系统 retrieval",
            true,
        ),
    ];
    let mut group = c.benchmark_group("language_additional");
    for (name, function, input, expected) in cases {
        let mut reported = false;
        group.bench_function(name, |b| {
            assert_eq!(function(input), expected);
            if name.starts_with("legacy_meaningful/") {
                assert_eq!(function(input), is_meaningful_text_query(input));
            }
            if !reported {
                let median = median_call_ns(function, input, expected);
                println!(
                    "scenario=language_additional/{name} samples=101 statistic=single_call_median median_ns={median} chars={} utf8_bytes={} clock_overhead=included",
                    input.chars().count(), input.len()
                );
                reported = true;
            }
            b.iter(|| black_box(function(black_box(input))));
            assert_eq!(function(input), expected);
        });
    }
    group.finish();
}

criterion_group!(benches, tokenizer_benches, language_benches);
criterion_main!(benches);
