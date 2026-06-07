# khive-text Benchmark Ledger

## Run command

```bash
cargo bench -p khive-text --bench text_bench
```

To run in test mode (compile-only check, no timing):

```bash
cargo bench -p khive-text --bench text_bench -- --test
```

## Scenarios

### tokenize

Hot path: raw string → token vec.

| ID | Input | Tokenizer | Description |
|----|-------|-----------|-------------|
| `tokenize/whitespace/short` | ~50 chars | `WhitespaceTokenizer` | 5-word phrase |
| `tokenize/whitespace/medium` | ~500 chars | `WhitespaceTokenizer` | Multi-sentence paragraph |
| `tokenize/whitespace/long` | ~5 000 chars | `WhitespaceTokenizer` | Full article-length text |
| `tokenize/cjk/mixed` | ~120 chars | `CjkCharTokenizer` | Mixed CJK + Latin |

### analyze

Full pipeline: tokenize + filter chain via `StandardAnalyzer`.

| ID | Input | Preset | Description |
|----|-------|--------|-------------|
| `analyze/standard/short` | ~50 chars | `preset::standard()` | Whitespace + lowercase + stop-words + length filters |
| `analyze/standard/medium` | ~500 chars | `preset::standard()` | Same preset, longer input |
| `analyze/standard/long` | ~5 000 chars | `preset::standard()` | Same preset, article-length input |
| `analyze/simple/short` | ~50 chars | `preset::simple()` | Whitespace + lowercase only |
| `analyze/simple/medium` | ~500 chars | `preset::simple()` | Same preset, longer input |
| `analyze/cjk/mixed` | ~120 chars | `preset::cjk()` | CJK char-level unigrams + Latin whitespace |
| `analyze/kg_name/identifier` | 18 chars | `preset::kg_name()` | Identifier-aware splitting for entity names |

### lang_detect

Script and quality checks over raw strings.

| ID | Input | Function | Description |
|----|-------|----------|-------------|
| `lang_detect/contains_cjk/latin` | ~500 chars | `contains_cjk` | Latin-only text — fast early exit |
| `lang_detect/contains_cjk/mixed` | ~120 chars | `contains_cjk` | Mixed CJK + Latin text |
| `lang_detect/script_profile/short` | ~50 chars | `ScriptProfile::analyze` | Fraction computation on short text |
| `lang_detect/script_profile/long` | ~5 000 chars | `ScriptProfile::analyze` | Fraction computation on article-length text |
| `lang_detect/is_meaningful_query/normal` | 32 chars | `is_meaningful_query` | Normal English query — passes all checks |
| `lang_detect/is_meaningful_query/gibberish` | 9 chars | `is_meaningful_query` | Repeated-char gibberish — rejected early |

### filter

Individual filter application and chained pipeline over a pre-tokenized medium corpus.

| ID | Input | Component | Description |
|----|-------|-----------|-------------|
| `filter/lowercase/medium_tokens` | ~80 tokens | `LowercaseFilter` | Unicode lowercasing per token |
| `filter/stopword/medium_tokens` | ~80 tokens | `StopWordFilter` | Hash-set lookup per token |
| `filter/min_length/medium_tokens` | ~80 tokens | `MinLengthFilter(2)` | Char-count check per token |
| `filter/pipeline_chain/medium` | ~500 chars | lowercase + stopword + minlen | Full chained pipeline |

## Baseline (2026-06-06, post-sweep)

**Toolchain:** rustc 1.94.1 (e408947bf 2026-03-25)
**Machine:** arm64 (Apple Silicon), macOS Darwin 25.5.0

### Tokenize

| Scenario | Low | Median | High | Outliers |
| --- | --- | --- | --- | --- |
| tokenize/whitespace/short | 197.1 ns | 211.0 ns | 227.0 ns | 11/100 (11%) |
| tokenize/whitespace/medium | 2.501 µs | 3.005 µs | 3.567 µs | 16/100 (16%) |
| tokenize/whitespace/long | 9.506 µs | 10.96 µs | 12.56 µs | 17/100 (17%) |
| tokenize/cjk/mixed | 1.751 µs | 2.030 µs | 2.336 µs | 12/100 (12%) |

### Analyze

| Scenario | Low | Median | High | Outliers |
| --- | --- | --- | --- | --- |
| analyze/standard/short | 521.0 ns | 623.4 ns | 737.1 ns | 17/100 (17%) |
| analyze/standard/medium | 4.952 µs | 5.648 µs | 6.692 µs | 10/100 (10%) |
| analyze/standard/long | 17.30 µs | 18.26 µs | 19.45 µs | 18/100 (18%) |
| analyze/simple/short | 293.8 ns | 309.8 ns | 330.1 ns | 17/100 (17%) |
| analyze/simple/medium | 3.296 µs | 3.617 µs | 4.066 µs | 8/100 (8%) |
| analyze/cjk/mixed | 2.533 µs | 2.571 µs | 2.631 µs | 8/100 (8%) |
| analyze/kg_name/identifier | 762.6 ns | 787.5 ns | 822.5 ns | 8/100 (8%) |

### Language Detection

| Scenario | Low | Median | High | Outliers |
| --- | --- | --- | --- | --- |
| lang_detect/contains_cjk/latin | 582.0 ns | 604.3 ns | 633.5 ns | 5/200 (3%) |
| lang_detect/contains_cjk/mixed | 593.3 ns | 650.4 ns | 713.6 ns | 8/200 (4%) |
| lang_detect/script_profile/short | 210.8 ns | 216.2 ns | 222.9 ns | 16/200 (8%) |
| lang_detect/script_profile/long | 5.919 µs | 6.329 µs | 6.747 µs | 5/200 (3%) |
| lang_detect/is_meaningful_query/normal | 1.237 µs | 1.276 µs | 1.322 µs | 12/200 (6%) |
| lang_detect/is_meaningful_query/gibberish | 620.7 ns | 674.6 ns | 735.5 ns | 5/200 (3%) |

### Filter

| Scenario | Low | Median | High | Outliers |
| --- | --- | --- | --- | --- |
| filter/lowercase/medium_tokens | 3.634 µs | 3.909 µs | 4.236 µs | 11/200 (6%) |
| filter/stopword/medium_tokens | 2.441 µs | 2.505 µs | 2.583 µs | 17/200 (9%) |
| filter/min_length/medium_tokens | 1.989 µs | 2.063 µs | 2.148 µs | 19/200 (10%) |
| filter/pipeline_chain/medium | 5.830 µs | 6.424 µs | 7.000 µs | — |
