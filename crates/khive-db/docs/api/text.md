# Text Search (FTS5)

`SqliteTextSearch` (`crates/khive-db/src/stores/text.rs`) implements the
`TextSearch` capability trait over a SQLite FTS5 virtual table. This is the
function-specific technical reference for its write routing and the FTS5
query-sanitization pipeline, which has grown incrementally to close a series
of MATCH-expression syntax errors and false-negative matches.

## `with_writer` — WriterTask routing (ADR-067 Component A, Fork C slice 2)

See `crates/khive-db/src/stores/text.rs` — private method `with_writer`.

Routes a single-row write through the pool-wide `WriterTask` when
`KHIVE_WRITE_QUEUE=1` and a handle is available; otherwise falls back to the
legacy standalone-connection / pool-mutex path.

This is the routing point for `with_writer` callers whose closure is
DML-only (`delete_document`/`fts_delete`, `rebuild`/`fts_rebuild`): on the
flag-on path the closure runs inside the WriterTask's own transaction, so a
bare `BEGIN IMMEDIATE` would violate SQLite's nested-transaction rule.
`upsert_document`/`upsert_documents` (the single-doc and batch write
methods) do their own flag check and return early on `Some`, so their
fallback calls into this helper only ever execute on the flag-off path
(`self.writer_task` is `None` by construction whenever those calls are
reached) — no double-routing.

`rename_namespace` (`#[allow(dead_code)]`, no production caller — see
ADR-067's `BEGIN IMMEDIATE` site inventory, EXEMPT) manages its own manual
transaction and calls `with_writer_unmanaged` instead of this helper —
routing its closure through the WriterTask would nest a bare `BEGIN
IMMEDIATE` inside the WriterTask's own transaction.

## FTS5 query sanitization pipeline

Three private functions cooperate to turn a raw user query into a safe FTS5
MATCH expression:

### `sanitize_fts5_query` — the primary two-pass sanitizer

See `crates/khive-db/src/stores/text.rs` — private fn `sanitize_fts5_query`.

1. **Replace** grouping/separator chars with spaces so adjacent tokens are
   not merged. This prevents `NEAR(smile,5)` from becoming `NEARsmile5`. It
   also keeps punctuated identifiers searchable: `khive-pack-memory` becomes
   `khive pack memory`, not `khivepackmemory`.
   Chars replaced with space: `(`, `)`, `,`, `:`, `-`, `.`, `/`
   (`/` added: FTS5's MATCH-expression parser rejects a bareword containing
   `/` as a syntax error, e.g. the throughput query `GB/s`)
2. **Remove** remaining FTS5 operator characters (H1: `~`, `!` added; issue
   #388: `$` added — FTS5's MATCH-expression parser treats a bareword
   starting with, containing, or consisting solely of `$` as a syntax error
   regardless of tokenizer, e.g. the DSL-doc query `$prev.id`):
   `*`, `"`, `'`, `+`, `^`, `~`, `!`, `$`, `\0`, control characters

After character processing, split on whitespace and remove FTS5 keyword
tokens: AND, OR, NOT, NEAR.

### `sanitize_fts5_query_legacy_merged` — pre-#397 compatibility

See `crates/khive-db/src/stores/text.rs` — private fn `sanitize_fts5_query_legacy_merged`.

Hyphen and dot are stripped outright instead of being space-split, so
`khive-pack-memory` normalizes to the single merged bareword
`khivepackmemory` rather than three terms. Slash is still space-split
because this fallback only preserves the pre-#397 hyphen/dot behavior and
must not create a `GBs` alias for `GB/s`.

Used only to build the merged-form OR-alternative in
`sanitize_fts5_token_group` — never as the sole sanitized query. Content
indexed before #397, or under a tokenizer whose own token-splitting rules
collapse punctuation differently than the current space-split pass, may
still carry this merged token; kept as a fallback match term so those
documents stay reachable.

### `sanitize_fts5_token_group` — per-token OR-alternative assembly (#397)

See `crates/khive-db/src/stores/text.rs` — private fn `sanitize_fts5_token_group`.

#397 split punctuated identifiers (`khive-pack-memory` -> three terms
`khive pack memory`, ANDed together) so they are searchable as distinct
words. That is correct against content indexed with word-splitting
tokenizers (`unicode61`, and multi-word `trigram` content where each word is
long enough to trigram on its own), but two more cases need covering when
punctuation actually causes a split:

- A query for content that only ever matched the pre-#397 merged bareword
  (`khivepackmemory`) would silently stop matching — kept reachable via the
  legacy-merged alternative. The merged form is dropped only when it is a
  literal duplicate of an alternative already emitted into the expression
  (the split AND-group's own space-joined content, or the quoted literal
  phrase) — not merely when it equals the *concatenation* of the split
  terms, which for an ordinary punctuated identifier is always true and
  would suppress the legacy alternative unconditionally, leaving rows
  stored under the pre-#397 merged token unreachable under `unicode61`.
- Under the production `trigram` tokenizer, a split segment shorter than
  `FTS5_TRIGRAM_MIN_SAFE_LEN` (e.g. the `07`/`10` in a `2026-07-10` date)
  tokenizes to zero trigrams and silently drops out of its AND clause,
  broadening the match to anything sharing the longer segment (any `2026`,
  not just that date). Neither the plain split AND-group nor the merged
  form avoids this — both were checked empirically against a live
  `tokenize='trigram'` table and both either broaden or simply fail to
  match. The fix that works is a literal phrase-quoted alternative: FTS5
  matches it by exact substring under `trigram` (confirmed: quoting
  `"2026-07-10"` discriminates the exact day) and by exact adjacent
  sub-tokens under word tokenizers. So when any split segment is
  trigram-unsafe, the split AND-group is dropped entirely rather than
  paired with the merged/phrase alternatives — an OR would still admit its
  broadened matches. Retrieval stays correct (still matches the right
  content, still discriminates) at the cost of requiring adjacency for that
  token — the trade-off intended by the fix (khive #397): correctness over
  a marginally more lenient match.
- Punctuation outside the legacy sanitizer, such as `#`, `%`, and `=`, is
  invalid or meaningful FTS5 bareword syntax. Bareword alternatives are
  emitted only when every term is safe; the quoted phrase preserves the
  literal spelling for all other input.

All emitted readings are additive OR-alternatives otherwise — the result is
never a narrower match than any single (safe) form alone.
