#[path = "additional_support/tokenize.rs"]
mod tokenize;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use khive_text::tokenizer::UnicodeWordTokenizer;

fn benches(c: &mut Criterion) {
    let mut group = c.benchmark_group("tokenize_additional");
    for target in tokenize::SIZES {
        let chars =
            target.div_ceil(tokenize::WORDS.chars().count()) * tokenize::WORDS.chars().count();
        group.throughput(Throughput::Elements(chars as u64));
        let mut fixture = None;
        group.bench_function(BenchmarkId::new("unicode_word", chars), |b| {
            let (text, expected) = fixture.get_or_insert_with(|| {
                let (text, repetitions) = tokenize::repeated(tokenize::WORDS, target);
                let expected = (0..repetitions)
                    .flat_map(|_| ["café", "naïve", "résumé", "κόσμος"].map(str::to_string))
                    .collect::<Vec<_>>();
                assert_eq!(expected.len(), 4 * repetitions);
                assert_eq!(text.chars().count(), chars);
                assert!(text.len() > chars, "UTF-8 bytes are not scalar characters");
                (text, expected)
            });
            tokenize::run(b, &UnicodeWordTokenizer, text, expected);
        });
    }
    group.finish();
}

criterion_group!(workloads, benches);
criterion_main!(workloads);
