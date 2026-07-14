# Tokenizer implementations

The crate provides tokenizers for ordinary prose, CJK text, exact keywords, source identifiers, and
optional Unicode word boundaries.

## `WhitespaceTokenizer`

Splits on ASCII whitespace, removes leading and trailing ASCII punctuation, and drops empty tokens.
It performs no case normalization.

## `CjkCharTokenizer`

Emits each recognized CJK character as a one-character token. Contiguous non-CJK text is split on
whitespace with ASCII edge punctuation removed, allowing mixed CJK and Latin input.

## `KeywordTokenizer`

Returns the whitespace-trimmed input as one token, or no tokens for empty input. Interior whitespace
is preserved.

## `IdentifierTokenizer`

For identifier-shaped input, emits the lowercased original plus lowercased parts from separators,
camel-case transitions, acronym boundaries, and letter/digit transitions. Parts shorter than
`min_part_len` characters are removed. Plain words fall back to `WhitespaceTokenizer`.

Length checks use Unicode character counts, not UTF-8 bytes.

## `UnicodeWordTokenizer`

Available with the `unicode` feature, this implementation uses Unicode word boundaries from
`unicode-segmentation` and drops empty segments.
