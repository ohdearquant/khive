# Identifier and script analysis

These helpers classify source-like identifiers, split their structural parts, detect CJK scripts,
and reject low-information retrieval queries.

## `is_identifier`

Returns true only for text with no whitespace, at least one ASCII letter, and a structural boundary:
an explicit separator, a lowercase-to-uppercase transition, or a letter/digit transition. A single
plain letter and an all-lowercase word are not identifiers.

## `split_identifier`

Splits `_`, `-`, `.`, `/`, and `:` separators; camel case; acronym-to-word boundaries such as
`XMLParser`; and digit/letter changes. Output is lowercase and filtered by the caller's minimum
character length. Character counting prevents a one-character CJK part from passing merely because
it occupies multiple UTF-8 bytes.

## CJK helpers and `ScriptProfile`

`is_cjk_char` covers unified ideographs (including extensions A/B and compatibility), Hiragana,
Katakana, and Hangul. `contains_cjk` returns true when more than 15% of characters are CJK; exactly
15% is false.

`ScriptProfile::analyze` returns CJK fraction, ASCII-letter fraction, and total character count.
Empty input produces zero fractions. `is_cjk_dominant` uses the same strict 15% threshold.

## `is_meaningful_query`

Rejects empty or whitespace-only input, symbol/punctuation/emoji-only input, a single ASCII letter,
and repeated-character gibberish when one character exceeds 80% of non-whitespace characters. A
single digit and meaningful non-ASCII characters remain eligible.
