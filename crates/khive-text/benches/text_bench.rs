use criterion::{black_box, criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion};
use khive_text::{
    analyzer::StandardAnalyzer,
    filter::{LowercaseFilter, MinLengthFilter, StopWordFilter},
    lang::{contains_cjk, is_meaningful_query, ScriptProfile},
    preset,
    tokenizer::{CjkCharTokenizer, WhitespaceTokenizer},
    Analyzer, Tokenizer,
};

// ── Fixture strings ───────────────────────────────────────────────────────────

const SHORT: &str = "attention mechanism transformer model";

const MEDIUM: &str = "The Transformer architecture relies on self-attention mechanisms that \
    allow the model to weigh the importance of different words in a sequence. \
    Unlike RNNs, Transformers process all tokens in parallel, enabling faster \
    training on modern hardware. The key insight is the scaled dot-product \
    attention formula: softmax(QK^T / sqrt(d_k)) * V.";

const LONG: &str = "Large language models such as GPT-4, Claude, and Llama-3 are built on \
    the Transformer architecture first introduced in 'Attention Is All You Need' by \
    Vaswani et al. (2017). The core innovation is replacing recurrence with multi-head \
    self-attention, which allows each token to attend to every other token in the sequence \
    in O(n^2) time. Positional encodings inject order information into the otherwise \
    permutation-invariant attention mechanism. Pre-training on large corpora followed by \
    instruction fine-tuning and RLHF alignment has proven to be the dominant paradigm for \
    building capable, general-purpose language assistants. Parameter-efficient fine-tuning \
    techniques such as LoRA, QLoRA, and prefix-tuning reduce the cost of adapting pre-trained \
    models to downstream tasks. Quantization methods including GPTQ, AWQ, and INT8 further \
    reduce inference memory requirements. Speculative decoding, continuous batching, and \
    paged KV-cache (as in vLLM) are now standard optimizations in production serving stacks. \
    Retrieval-augmented generation (RAG) extends LLMs by grounding responses in an external \
    knowledge base retrieved at inference time, reducing hallucinations for knowledge-intensive \
    tasks. Evaluation benchmarks such as MMLU, HellaSwag, ARC, and GSM8K provide standardized \
    comparisons across model families and sizes, though they are increasingly saturated by \
    state-of-the-art frontier models.";

const MIXED_CJK: &str = "使用LoRA进行fine-tuning可以显著降低大模型的训练成本。\
    Transformer架构的核心是multi-head attention机制。";

// ── Tokenizer benchmarks ──────────────────────────────────────────────────────

fn bench_tokenize(c: &mut Criterion) {
    let mut g = c.benchmark_group("tokenize");
    g.sample_size(100);

    let tok = WhitespaceTokenizer;

    for (label, text) in [("short", SHORT), ("medium", MEDIUM), ("long", LONG)] {
        g.bench_with_input(BenchmarkId::new("whitespace", label), text, |b, t| {
            b.iter(|| tok.tokenize(black_box(t)))
        });
    }

    let cjk_tok = CjkCharTokenizer;
    g.bench_with_input(BenchmarkId::new("cjk", "mixed"), MIXED_CJK, |b, t| {
        b.iter(|| cjk_tok.tokenize(black_box(t)))
    });

    g.finish();
}

// ── Analysis pipeline benchmarks ─────────────────────────────────────────────

fn bench_analyze(c: &mut Criterion) {
    let mut g = c.benchmark_group("analyze");
    g.sample_size(100);

    let standard = preset::standard();
    for (label, text) in [("short", SHORT), ("medium", MEDIUM), ("long", LONG)] {
        g.bench_with_input(BenchmarkId::new("standard", label), text, |b, t| {
            b.iter(|| standard.analyze(black_box(t)))
        });
    }

    let simple = preset::simple();
    for (label, text) in [("short", SHORT), ("medium", MEDIUM)] {
        g.bench_with_input(BenchmarkId::new("simple", label), text, |b, t| {
            b.iter(|| simple.analyze(black_box(t)))
        });
    }

    let cjk = preset::cjk();
    g.bench_with_input(BenchmarkId::new("cjk", "mixed"), MIXED_CJK, |b, t| {
        b.iter(|| cjk.analyze(black_box(t)))
    });

    let kg = preset::kg_name();
    g.bench_with_input(
        BenchmarkId::new("kg_name", "identifier"),
        "bert-base-uncased",
        |b, t| b.iter(|| kg.analyze(black_box(t))),
    );

    g.finish();
}

// ── Language / script detection ───────────────────────────────────────────────

fn bench_lang_detect(c: &mut Criterion) {
    let mut g = c.benchmark_group("lang_detect");
    g.sample_size(200);

    g.bench_function("contains_cjk/latin", |b| {
        b.iter(|| contains_cjk(black_box(MEDIUM)))
    });

    g.bench_function("contains_cjk/mixed", |b| {
        b.iter(|| contains_cjk(black_box(MIXED_CJK)))
    });

    g.bench_function("script_profile/short", |b| {
        b.iter(|| ScriptProfile::analyze(black_box(SHORT)))
    });

    g.bench_function("script_profile/long", |b| {
        b.iter(|| ScriptProfile::analyze(black_box(LONG)))
    });

    g.bench_function("is_meaningful_query/normal", |b| {
        b.iter(|| is_meaningful_query(black_box("transformer attention mechanism")))
    });

    g.bench_function("is_meaningful_query/gibberish", |b| {
        b.iter(|| is_meaningful_query(black_box("aaaaaaaaa")))
    });

    g.finish();
}

// ── Filter benchmarks ─────────────────────────────────────────────────────────

fn bench_filter(c: &mut Criterion) {
    let mut g = c.benchmark_group("filter");
    g.sample_size(200);

    // Build a token list representative of medium-size analysis output.
    let tokens: Vec<String> = WhitespaceTokenizer
        .tokenize(MEDIUM)
        .into_iter()
        .map(|t| t.to_lowercase())
        .collect();

    let lowercase = LowercaseFilter;
    let stopword = StopWordFilter;
    let minlen = MinLengthFilter(2);

    // Use iter_batched so the Vec<String> clone is in the setup phase,
    // not on the measured path. Only the filter application is timed.
    g.bench_function("lowercase/medium_tokens", |b| {
        b.iter_batched(
            || tokens.clone(),
            |ts| {
                use khive_text::TokenFilter;
                ts.into_iter()
                    .filter_map(|t| lowercase.apply(black_box(t)))
                    .count()
            },
            BatchSize::SmallInput,
        )
    });

    g.bench_function("stopword/medium_tokens", |b| {
        b.iter_batched(
            || tokens.clone(),
            |ts| {
                use khive_text::TokenFilter;
                ts.into_iter()
                    .filter_map(|t| stopword.apply(black_box(t)))
                    .count()
            },
            BatchSize::SmallInput,
        )
    });

    g.bench_function("min_length/medium_tokens", |b| {
        b.iter_batched(
            || tokens.clone(),
            |ts| {
                use khive_text::TokenFilter;
                ts.into_iter()
                    .filter_map(|t| minlen.apply(black_box(t)))
                    .count()
            },
            BatchSize::SmallInput,
        )
    });

    // Chain: full standard pipeline over a pre-tokenized vec.
    let analyzer = StandardAnalyzer::with_tokenizer(WhitespaceTokenizer)
        .filter(LowercaseFilter)
        .filter(StopWordFilter)
        .filter(MinLengthFilter(2));

    g.bench_function("pipeline_chain/medium", |b| {
        b.iter(|| analyzer.analyze(black_box(MEDIUM)))
    });

    g.finish();
}

// ── Registry ──────────────────────────────────────────────────────────────────

criterion_group!(
    benches,
    bench_tokenize,
    bench_analyze,
    bench_lang_detect,
    bench_filter,
);
criterion_main!(benches);
