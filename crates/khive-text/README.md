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

`is_cjk_char` and `contains_cjk` classify supported CJK blocks. `is_meaningful_text_query`
rejects empty or symbol-only input, a single ASCII letter, and dominant-character repetition.
These helpers and `ScriptProfile` live in `lang`. The deprecated `is_meaningful_query` name
delegates to the unchanged text policy for one release.

## Where this sits

`khive-text` has no required khive-* dependencies. `khive-bm25` consumes its
tokenizer trait and normalization filters while retaining `SimpleTokenizer` as
its compatibility-oriented configuration wrapper.

## License

Apache-2.0.
