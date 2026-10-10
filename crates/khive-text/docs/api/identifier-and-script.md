# Identifier and script analysis

These helpers classify source-like identifiers, split their structural parts, detect CJK scripts,
and reject low-information retrieval queries.

## `is_identifier`

Returns true only for text with no whitespace, at least one ASCII letter, and a structural boundary:
an explicit separator, a lowercase-to-uppercase transition, an acronym-to-word boundary such as
`XMLParser`, or a letter/digit transition. An acronym boundary is an uppercase letter followed by
another uppercase letter and then a lowercase letter, except a terminal plural `s` with no
following ASCII lowercase letter. That suffix stays with the acronym at the end of input or
before a separator, uppercase letter, or digit: `APIs`, `IDs` and `URLs` remain whole words,
and `APIsFoo` splits as `apis`, `foo`. A longer lowercase run restores the acronym boundary:
`DBUsers` splits as `db`, `users`, `APIUsage` as `api`, `usage`, and `URLsafe` as `ur`, `lsafe`.
`XMLToJSON` still splits before `To`.

This is a character rule with no dictionary or acronym-length heuristic. `XMLIsEmpty` has the
same shape as `APIsFoo`, so it splits as `xmlis`, `empty`; the splitter cannot distinguish the
word `Is` from a plural suffix. Plain lowercase, titlecase, and all-uppercase words without a
boundary are not identifiers.

## `split_identifier`

Splits `_`, `-`, `.`, `/`, and `:` separators; camel case; acronym-to-word boundaries such as
`XMLParser`; and digit/letter changes. Output is lowercase and filtered by the caller's minimum
character length. Character counting prevents a one-character CJK part from passing merely because
it occupies multiple UTF-8 bytes.

## CJK helpers and `ScriptProfile`

`is_cjk_char` covers unified ideographs (including extensions A/B and compatibility), Hiragana,
Katakana, and Hangul. `contains_cjk` returns true when more than 15% of characters are CJK; exactly
15% is false. It computes the ratio in `f64`. `contains_cjk_f32` keeps the memory pack's existing
`f32` ratio at its public `scoring::contains_cjk` path. Both use the same character counts, but
can differ near the threshold because of rounding; choose the precision required by the caller.

`ScriptProfile::analyze` returns CJK fraction, ASCII-letter fraction, and total character count.
Empty input produces zero fractions. `is_cjk_dominant` uses the same strict 15% threshold.

## `is_meaningful_query`

Rejects empty or whitespace-only input, symbol/punctuation/emoji-only input, a single ASCII letter,
and repeated-character gibberish when one character exceeds 80% of non-whitespace characters. A
single digit and meaningful non-ASCII characters remain eligible.
