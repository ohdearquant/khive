# khive-text

Text analysis primitives for khive: tokenization, normalization, and token
filtering. A pluggable `Tokenizer` -> `TokenFilter` chain composed into an
`Analyzer`, plus named presets for common cases (standard English, CJK,
identifiers, KG entity names).

## Features

- `unicode` — adds `UnicodeWordTokenizer` (Unicode word-boundary splitting via `unicode-segmentation`)
- `stem` — adds `SnowballStemmer`, a `TokenFilter` backed by `rust-stemmers` (English by default via `SnowballStemmer::english()`, or any `rust_stemmers::Algorithm` via `for_algorithm`)
- `full` — both of the above

## Usage

```rust
use khive_text::{preset, Analyzer};

let tokens = preset::standard().analyze("The quick brown fox jumps over the lazy dog");
assert_eq!(tokens, vec!["quick", "brown", "fox", "jumps", "over", "lazy", "dog"]);
```

`preset::standard()` chains `WhitespaceTokenizer` with `LowercaseFilter`,
a BM25-compatible stop-word filter, and `MinLengthFilter(1)` — producing the
same token stream as `khive-bm25`'s `SimpleTokenizer::default()` (same stop
list, no max-length cap). This is intentionally different from the public
`StopWordFilter` in `khive_text::filter`, which has its own stop list for
callers building custom pipelines. Other named presets: `preset::simple()`
(whitespace + lowercase only, keeps stop words),
`preset::keyword()` (whole input as one token), `preset::cjk()`
(character-level unigrams for CJK scripts, whitespace for Latin), and
`preset::kg_name()` (identifier-aware splitting tuned for entity names like
`"bert-base-uncased"`). Build a custom pipeline with the same builder:

```rust
use khive_text::analyzer::StandardAnalyzer;
use khive_text::filter::LowercaseFilter;
use khive_text::tokenizer::IdentifierTokenizer;

let analyzer = StandardAnalyzer::with_tokenizer(IdentifierTokenizer::default())
    .filter(LowercaseFilter);
```

## Script detection

`is_cjk_char`, `contains_cjk`, and `is_meaningful_query` (a `ScriptProfile`-based
heuristic for rejecting queries that are too short or punctuation-only to search
on) live in the `lang` module for callers that need to branch on script before
choosing an analyzer.

## Where this sits

`khive-text` has no required khive-* dependencies and no current in-workspace
consumers — it is a standalone tokenization library extracted for reuse by any
future retrieval crate that needs a pluggable analyzer independent of
`khive-bm25`'s built-in `SimpleTokenizer`.

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
