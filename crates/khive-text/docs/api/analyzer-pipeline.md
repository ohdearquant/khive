# Analyzer pipelines and presets

`StandardAnalyzer` composes one tokenizer with an ordered sequence of token filters for deterministic
text normalization.

## `StandardAnalyzer`

`with_tokenizer` creates a pipeline with no filters. Each `filter` call appends one `TokenFilter`;
analysis tokenizes first and then applies filters in insertion order to every token, dropping values
whose filter returns `None`. Both tokenizer and filters are boxed, thread-safe trait objects.

## Presets

| Constructor | Pipeline |
| --- | --- |
| `standard()` | whitespace, lowercase, BM25-compatible stop words |
| `simple()` | whitespace, lowercase |
| `keyword()` | whole trimmed input, lowercase |
| `cjk()` | CJK character unigrams plus Latin whitespace runs, lowercase |
| `kg_name()` | identifier-aware original/part expansion, lowercase |

The standard preset intentionally matches `khive-bm25::SimpleTokenizer::default()`: it uses the
BM25-specific stop list, keeps tokens such as `over` and `am`, has minimum length one, and applies no
maximum-length filter. The public general-purpose `StopWordFilter` has a different list.

## Core traits

`Tokenizer`, `TokenFilter`, and `Analyzer` are deterministic `Send + Sync` boundaries. A filter
returns `None` to drop a token. `BoxedTokenizer` and `BoxedAnalyzer` are shared `Arc` trait objects.
