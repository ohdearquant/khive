APPROVE-WITH-FIXES

Findings: 0 Blocker, 0 High, 1 Medium, 0 Low

### [Medium] The `estimate_document_cost` regression test does not detect the estimate change

Evidence: `crates/khive-bm25/src/index/memory.rs:173` measures `cost` at line 178 and the
`memory_usage` delta at line 181, then only asserts `cost >= actual_delta` at line 183. The
new `usize` mirror is added to `memory_usage` at lines 58 and 68 and to the estimate at lines
124 and 143.

Why this matters: Removing both new `usize` charges reduces both sides of that inequality by
the same `size_of::<usize>()`, so this test still passes on the pre-fix accounting. For its
fixed eight-unique-term input, the existing model leaves 68 bytes of slack even before this PR,
so removing the estimate charge alone also passes. The test therefore does not protect the
`estimate_document_cost` half of #1024.

Suggested fix: Add a mutation-sensitive assertion for a fixed input that includes the explicit
`size_of::<usize>() + size_of::<f32>()` vector-mirror component in the expected estimate (or
factor the component into a testable helper). Keep the direct `memory_usage` delta test, which
does detect its corresponding accounting omission.

## Confirmed

- `crates/khive-fusion/src/weighted.rs:52`, `:135`, and `:143` use the identical finite-and-
  strictly-positive predicate. `normalize_weights` now excludes NaN and both infinities from
  both its sum and output map, matching `weighted_fusion`.
- All-non-finite input filters to a zero sum and reaches the established uniform branch at
  `crates/khive-fusion/src/weighted.rs:137`. The new test at `:387` would fail before the
  sanitization change because the old NaN sum produced NaN outputs.
- `crates/khive-bm25/src/index/core.rs:103` and `:106` define distinct `usize` and `f32`
  mirrors; `crates/khive-bm25/src/index/memory.rs:58-69` counts each once. The incremental
  estimate at `:123-145` likewise includes one slot for each, consistent with the one call to
  `set_doc_length_fast` at `crates/khive-bm25/src/index/indexing.rs:94-95`.
- The direct accounting regression at `crates/khive-bm25/src/index/memory.rs:154-169` would
  fail without the new `usize` accounting: it grows only the two mirrors and requires their
  combined per-slot byte increase.
- The four-file diff is scoped to #1022/#1024. No stubs, stale v0 terms, or whitespace errors
  were found.

## Checks

- `git diff origin/main`: full static diff reviewed.
- `git diff --check origin/main`: passed.
- Compilation and tests were intentionally not run, as requested.

Domain utility: SKIPPED — this narrow static review did not require a domain briefing.
