use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion};
use khive_request::parse_request;

// ── DSL inputs ────────────────────────────────────────────────────────────────

const SINGLE_SIMPLE: &str = r#"verb(arg="value")"#;

const SINGLE_COMPLEX: &str = r#"memory.remember(content="Long content string that represents a real-world memory payload", salience=0.85, decay_factor=0.0, memory_type="semantic", source_id="550e8400-e29b-41d4-a716-446655440000")"#;

const BATCH_3: &str = r#"[create(kind="concept", name="LoRA"), search(kind="entity", query="attention"), get(id="550e8400-e29b-41d4-a716-446655440000")]"#;

fn make_batch(n: usize) -> String {
    let ops: Vec<String> = (0..n)
        .map(|i| format!(r#"create(kind="concept", name="concept_{i}", description="desc_{i}")"#))
        .collect();
    format!("[{}]", ops.join(", "))
}

const CHAIN_2: &str = r#"create(kind="concept", name="LoRA") | link(source_id=$prev.id, target_id="550e8400-e29b-41d4-a716-446655440000", relation="extends")"#;

fn make_chain(n: usize) -> String {
    let mut ops: Vec<String> = vec![r#"create(kind="concept", name="seed")"#.to_owned()];
    for i in 1..n {
        ops.push(format!(
            r#"create(kind="concept", name="step_{i}", description=$prev.id)"#
        ));
    }
    ops.join(" | ")
}

const JSON_FORM: &str = r#"[{"tool":"create","args":{"kind":"concept","name":"LoRA"}},{"tool":"search","args":{"kind":"entity","query":"attention mechanism"}},{"tool":"get","args":{"id":"550e8400-e29b-41d4-a716-446655440000"}}]"#;

// ── Benchmarks ────────────────────────────────────────────────────────────────

fn bench_parse_single(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse/single");
    g.sample_size(200);

    g.bench_function("simple", |b| {
        b.iter(|| parse_request(black_box(SINGLE_SIMPLE)))
    });

    g.bench_function("complex", |b| {
        b.iter(|| parse_request(black_box(SINGLE_COMPLEX)))
    });

    g.finish();
}

fn bench_parse_batch(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse/batch");
    g.sample_size(100);

    g.bench_function("3", |b| b.iter(|| parse_request(black_box(BATCH_3))));

    {
        let input = make_batch(10);
        g.bench_with_input(BenchmarkId::from_parameter(10), &input, |b, s| {
            b.iter(|| parse_request(black_box(s.as_str())))
        });
    }

    g.finish();
}

fn bench_parse_chain(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse/chain");
    g.sample_size(100);

    g.bench_function("2", |b| b.iter(|| parse_request(black_box(CHAIN_2))));

    {
        let input = make_chain(5);
        g.bench_with_input(BenchmarkId::from_parameter(5), &input, |b, s| {
            b.iter(|| parse_request(black_box(s.as_str())))
        });
    }

    g.finish();
}

fn bench_parse_json_form(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse/json_form");
    g.sample_size(100);

    g.bench_function("3_ops", |b| b.iter(|| parse_request(black_box(JSON_FORM))));

    g.finish();
}

criterion_group!(
    benches,
    bench_parse_single,
    bench_parse_batch,
    bench_parse_chain,
    bench_parse_json_form,
);
criterion_main!(benches);
