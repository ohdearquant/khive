# khive-text Benchmark Ledger

<!-- bench-reference:begin -->

Generated from `perf/reference.json` by `scripts/render-bench-reference.py`; do not edit this block.

Reference baselines for `khive-text`. Values are the committed reference median for each benchmark, in the unit shown; A/A is the ratio between the two same-commit runs. The date is the committer date of the baseline commit.

Comparison class: `rustc 1.95.0 (59807616e 2026-04-14)`, `bench` profile, AMD Ryzen 7 PRO 8700GE w/ Radeon 780M Graphics, cores 2,3, kernel 6.8.0-142-generic.

| Benchmark                                   | Reference median | A/A ratio | Baseline commit                            | Commit date |
| ------------------------------------------- | ---------------- | --------- | ------------------------------------------ | ----------- |
| `analyze/cjk/mixed`                         | 2.479 µs         | 1.032     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `analyze/kg_name/identifier`                | 531.298 ns       | 1.012     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `analyze/simple/medium`                     | 2.543 µs         | 1.012     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `analyze/simple/short`                      | 214.966 ns       | 1.001     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `analyze/standard/long`                     | 13.317 µs        | 1.005     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `analyze/standard/medium`                   | 3.443 µs         | 1.011     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `analyze/standard/short`                    | 314.963 ns       | 1.039     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `filter/lowercase/medium_tokens`            | 1.078 µs         | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `filter/min_length/medium_tokens`           | 509.191 ns       | 1.002     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `filter/pipeline_chain/medium`              | 3.734 µs         | 1.020     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `filter/stopword/medium_tokens`             | 1.149 µs         | 1.001     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `lang_detect/contains_cjk/latin`            | 439.760 ns       | 1.001     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `lang_detect/contains_cjk/mixed`            | 119.610 ns       | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `lang_detect/is_meaningful_query/gibberish` | 229.841 ns       | 1.010     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `lang_detect/is_meaningful_query/normal`    | 698.010 ns       | 1.024     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `lang_detect/script_profile/long`           | 2.759 µs         | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `lang_detect/script_profile/short`          | 73.839 ns        | 1.000     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `tokenize/cjk/mixed`                        | 1.013 µs         | 1.012     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `tokenize/whitespace/long`                  | 6.049 µs         | 1.040     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `tokenize/whitespace/medium`                | 1.598 µs         | 1.032     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |
| `tokenize/whitespace/short`                 | 133.817 ns       | 1.015     | `212bf72e498de9fbbda42cc0e2902906c730e3ea` | 2026-10-17  |

<!-- bench-reference:end -->

## Additional isolated workloads

The additional targets isolate KeywordTokenizer, IdentifierTokenizer and the
feature-gated UnicodeWordTokenizer, plus the f32 CJK classifier and the legacy
query entry point. All current baseline values await qualified-host measurement;
the older release ledger below records historical observations only.

```bash
cd crates
# Smoke: compile and actually execute every target, including Unicode.
cargo bench -p khive-text --benches --features unicode -- --test
# Short Criterion run of all targets.
cargo bench -p khive-text --benches --features unicode -- --quick --noplot
# Full additional measurements.
cargo bench -p khive-text --bench text_additional -- --noplot
cargo bench -p khive-text --bench text_unicode --features unicode -- --noplot
```

The `text_unicode` target declares `required-features = ["unicode"]`. A command
without that feature skips its three scenes and is not complete coverage. The
existing `scoring-retrieval` producer owns this crate; its measuring invocation
must enable `unicode` to cover this target. Feature defaults and dependencies are
unchanged. `--list` lists selected IDs without constructing the lazy corpora.

| Target            | Scene suffix under `tokenize_additional/` | Actual input scalar characters | Witness                                      |
| ----------------- | ----------------------------------------- | ------------------------------ | -------------------------------------------- |
| `text_additional` | `keyword/{N}`                             | 200 / 2,000 / 20,000           | Exactly one complete trimmed phrase          |
| `text_additional` | `identifier/{N}`                          | 204 / 2,006 / 20,026           | Complete ordered original-plus-parts vector  |
| `text_unicode`    | `unicode_word/{N}`                        | 200 / 2,000 / 20,000           | All accented Latin and Greek words, in order |

Sizes include separating whitespace. Each Criterion `Throughput::Elements`
element is one Unicode scalar value, so throughput is characters/second, not
UTF-8 bytes/second or tokens/second. The multilingual corpus has more UTF-8 bytes
than characters. Identifier input repeats `fine_tuning camelCase HTTP2Client`;
expected output explicitly names the separator, camel-case and digit/acronym
parts. Keyword/Unicode input repeats `café naïve résumé κόσμος`. Fixtures and
expected vectors are built once per selected scene. Exact token vectors are
checked before and after each Criterion callback. `BatchSize::PerIteration`
keeps returned-vector destruction outside the timer and retains at most one
result at a time. The timer includes tokenization and its result allocations.

Four scenes in `text_additional` use the `language_additional/` prefix:

| Suffix                        | Input                             | Expected |
| ----------------------------- | --------------------------------- | -------- |
| `legacy_meaningful/normal`    | `transformer attention mechanism` | true     |
| `legacy_meaningful/gibberish` | `aaaaaaaaa`                       | false    |
| `contains_cjk_f32/latin`      | `retrieval index search`          | false    |
| `contains_cjk_f32/mixed`      | `检索系统 retrieval`              | true     |

`is_meaningful_query` is a deprecated compatibility wrapper around
`is_meaningful_text_query`, not a separate non-text query policy. The alias is
called intentionally under a narrowly scoped deprecation allowance and checked
against the canonical function. The memory pack's recall policy is a different
API and is outside this benchmark. `contains_cjk_f32` counts the whole input and
compares its f32 ratio with a strict 15% threshold; no fast early-exit behavior is
claimed.

Each selected language scene reports the median of 101 actual individually
timed calls before independent Criterion sampling. The middle sorted duration
is the reported median; input character/UTF-8 byte lengths accompany it. These
single-call observations include function-pointer dispatch, black-box and
`Instant` overhead. Validation, sorting and printing are outside those samples.
No tail percentile is reported or inferred from Criterion confidence bounds.
Short calls are especially sensitive to clock overhead; compare like-for-like
commands, toolchains and hosts. Even smoke mode executes these witnesses.

## Performance guarantees

| Operation                       | Input                                                             | Metric            | Bound                                    | Current baseline                                                              |
| ------------------------------- | ----------------------------------------------------------------- | ----------------- | ---------------------------------------- | ----------------------------------------------------------------------------- |
| `WhitespaceTokenizer::tokenize` | Existing legacy long fixture (historically labelled ~5,000 chars) | characters/second | Advisory trend against recorded baseline | Awaiting qualified measurement; existing target has no throughput declaration |
| `IdentifierTokenizer::tokenize` | 2,006 scalar characters                                           | characters/second | Advisory trend against recorded baseline | Awaiting qualified measurement                                                |
| `is_meaningful_text_query`      | Existing normal fixture                                           | median latency    | Advisory trend against recorded baseline | Historical entry below; no new measurement                                    |
| `contains_cjk`                  | Existing mixed CJK fixture                                        | median latency    | Advisory trend against recorded baseline | Historical entry below; no new measurement                                    |

These rows do not introduce a hard timing gate. Baseline records must include
the exact commit, toolchain, machine, feature set, command and raw output. Do not
promote a historical value into a current guarantee. The 13 additional scenes
have no measured baseline in this change.

## Historical interpretation

The old target is preserved byte-for-byte. Its `lang_detect/is_meaningful_query`
ID currently calls `is_meaningful_text_query`; the new legacy-alias scenes have
distinct IDs. Its filter setup now uses `iter_batched`, whereas the original
release note below describes the implementation measured in that release.
Historical size estimates are labels, not verified current character counts.
Criterion test mode executes callbacks and assertions; it is not compile-only.
The original ledger follows unchanged, including its original caveats.

## Release Ledger

### v0.2.8 - 2026-06-08

- Commit: `d3629501c550fd2f3bb7ed350a2b60309d596465`
- Crate version: `0.2.8`
- Khive version: `0.2.8`
- Toolchain: `rustc 1.94.1 (e408947bf 2026-03-25)`, release profile (Criterion)
- Machine: Apple M-series arm64, macOS Darwin 25.5.0, 16 GB
- Feature flags: default
- Command: `cd crates && cargo bench -p khive-text --bench text_bench`
- Dataset: inline string literals in bench source; no external fixture; sample sizes 100 (tokenize/
  analyze/filter), 200 (lang_detect)
- vs prior: first formal release ledger entry — no prior comparable baseline

#### Tokenize

| Scenario                   | Low      | Median   | High     | Outliers     |
| -------------------------- | -------- | -------- | -------- | ------------ |
| tokenize/whitespace/short  | 197.1 ns | 211.0 ns | 227.0 ns | 11/100 (11%) |
| tokenize/whitespace/medium | 2.501 µs | 3.005 µs | 3.567 µs | 16/100 (16%) |
| tokenize/whitespace/long   | 9.506 µs | 10.96 µs | 12.56 µs | 17/100 (17%) |
| tokenize/cjk/mixed         | 1.751 µs | 2.030 µs | 2.336 µs | 12/100 (12%) |

#### Analyze

| Scenario                   | Low      | Median   | High     | Outliers     |
| -------------------------- | -------- | -------- | -------- | ------------ |
| analyze/standard/short     | 521.0 ns | 623.4 ns | 737.1 ns | 17/100 (17%) |
| analyze/standard/medium    | 4.952 µs | 5.648 µs | 6.692 µs | 10/100 (10%) |
| analyze/standard/long      | 17.30 µs | 18.26 µs | 19.45 µs | 18/100 (18%) |
| analyze/simple/short       | 293.8 ns | 309.8 ns | 330.1 ns | 17/100 (17%) |
| analyze/simple/medium      | 3.296 µs | 3.617 µs | 4.066 µs | 8/100 (8%)   |
| analyze/cjk/mixed          | 2.533 µs | 2.571 µs | 2.631 µs | 8/100 (8%)   |
| analyze/kg_name/identifier | 762.6 ns | 787.5 ns | 822.5 ns | 8/100 (8%)   |

#### Language Detection

| Scenario                                  | Low      | Median   | High     | Outliers    |
| ----------------------------------------- | -------- | -------- | -------- | ----------- |
| lang_detect/contains_cjk/latin            | 582.0 ns | 604.3 ns | 633.5 ns | 5/200 (3%)  |
| lang_detect/contains_cjk/mixed            | 593.3 ns | 650.4 ns | 713.6 ns | 8/200 (4%)  |
| lang_detect/script_profile/short          | 210.8 ns | 216.2 ns | 222.9 ns | 16/200 (8%) |
| lang_detect/script_profile/long           | 5.919 µs | 6.329 µs | 6.747 µs | 5/200 (3%)  |
| lang_detect/is_meaningful_query/normal    | 1.237 µs | 1.276 µs | 1.322 µs | 12/200 (6%) |
| lang_detect/is_meaningful_query/gibberish | 620.7 ns | 674.6 ns | 735.5 ns | 5/200 (3%)  |

#### Filter

| Scenario                        | Low      | Median   | High     | Outliers     |
| ------------------------------- | -------- | -------- | -------- | ------------ |
| filter/lowercase/medium_tokens  | 3.634 µs | 3.909 µs | 4.236 µs | 11/200 (6%)  |
| filter/stopword/medium_tokens   | 2.441 µs | 2.505 µs | 2.583 µs | 17/200 (9%)  |
| filter/min_length/medium_tokens | 1.989 µs | 2.063 µs | 2.148 µs | 19/200 (10%) |
| filter/pipeline_chain/medium    | 5.830 µs | 6.424 µs | 7.000 µs | —            |

- Notes: filter benchmarks include clone cost for the pre-tokenized token vec inside the timed
  loop (uses `b.iter`). This is intentional — ownership semantics of the filter pipeline mean
  tokens are consumed per call. If allocation-free timing is needed, switch to `iter_batched`.

Last reviewed: v0.2.8 (2026-06-08)
