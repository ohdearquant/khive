# Token filters

Token filters transform or discard one token at a time and are applied in analyzer insertion order.

## Case and length

`LowercaseFilter` applies Unicode-aware lowercase conversion. `MinLengthFilter(n)` and
`MaxLengthFilter(n)` count characters rather than bytes and drop tokens outside their inclusive
constraints.

## Stop words

`StopWordFilter` removes its general English stop-word set and assumes a lowercase input token.
`Bm25StopWordFilter` is the compatibility filter used only by `preset::standard()`; it exactly
matches `khive-bm25::SimpleTokenizer::default()` without changing the public stop list.

## Snowball stemming

`SnowballStemmer::english()` selects the English algorithm, while `for_algorithm` accepts any
`rust_stemmers::Algorithm`. Only ASCII-alphabetic tokens are stemmed; numbers, punctuation, and
non-ASCII text pass through unchanged.
