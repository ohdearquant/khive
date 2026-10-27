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

## Advisory CI observation — 2026-10-27 UTC (#5671)

The earlier “awaiting qualified measurement” and “no measured baseline” statements
predate this observation. This appendix records an existing CI artifact for the
measured scenes; it leaves the generated fixed-reference/A/A block and historical
release entry unchanged. Timing remains advisory, with no cross-host improvement,
rolling-reference ratio, hard latency bound, or full-workflow acceptance inferred.

### Provenance and observed coverage

- Source: commit `a72ad862cbfc57b6063fa26a1d7dfcebd0749c91`, tree
  `ae5b6ef2b3fde51324783e963302fff4b4d1c8fe`; the recorded tree is also the train
  head `a8aaafaf104d76f3dd67a3232246b94900744efe` tree. The measured khive-text
  providers match frozen dev `f7524e536512c6a57b039933be034bbc5b75c4d3` byte-for-byte.
- [CI job](https://github.com/ohdearquant/khive/actions/runs/40430486230/job/121947482310),
  run `40430486230`, job `121947482310`, attempt 1;
  [artifact `12702217093`](https://github.com/ohdearquant/khive/actions/runs/40430486230/artifacts/12702217093),
  SHA-256 `2879796e358155b3e1d6e90ae30356da5a93f89d724d820a80f73a5a989a5544`.
- Machine record: Linux `x86_64`, 4 reported CPUs, GitHub Actions runner
  `1000462429`. Actual toolchain: `rustc 1.95.0 (59807616e 2026-04-14)`,
  `x86_64-unknown-linux-gnu`, optimized `bench` profile, `CARGO_INCREMENTAL=0`.
- The component selected khive-score, khive-fusion, khive-retrieval, khive-text and
  khive-bm25. Its compile phase exited 0 in 325 seconds and Criterion phase exited
  0 in 90 seconds; these are whole-component phase durations.

The logged commands, from `crates`, expanded the selected package list as follows:

```bash
cargo bench -p khive-score -p khive-fusion -p khive-retrieval -p khive-text -p khive-bm25 --benches --no-run
cargo bench -p khive-score -p khive-fusion -p khive-retrieval -p khive-text -p khive-bm25 --benches -- --quick --noplot
```

The run contains 31 unique khive-text Criterion IDs: the 21 existing scenes and
10 additional scenes below. It compiled and ran `text_bench` and `text_additional`.
It did not enable `unicode`; `text_unicode` and the three
`tokenize_additional/unicode_word/{200,2000,20000}` scenes are absent from the log
and artifact. Their baselines and complete #5671 feature-enabled coverage remain
pending. The successful component phases do not establish that acceptance item.

Each table's Criterion median is the exact `median.point_estimate` from the
corresponding `crates/target/criterion/<directory_name>/new/estimates.json`, in
**nanoseconds per operation**. IDs come from `benchmark.json.full_id`, not the
filesystem spelling. Every cited quick-profile `sample.json` contains **2 actual
iteration/time samples**, not the source's configured 100/200 samples or the
separate 101-call report. No tail percentile is inferred.

### Additional tokenizer observations

| Criterion full ID                      | Scalar characters | Criterion median (ns/op) | Console throughput point (Elements/s) |
| -------------------------------------- | ----------------- | ------------------------ | ------------------------------------- |
| `tokenize_additional/keyword/200`      | 200               | 41.95300769805908        | 4.7792 Gelem/s                        |
| `tokenize_additional/keyword/2000`     | 2,000             | 106.68834590911865       | 18.802 Gelem/s                        |
| `tokenize_additional/keyword/20000`    | 20,000            | 633.6586380004883        | 31.720 Gelem/s                        |
| `tokenize_additional/identifier/204`   | 204               | 7856.1612548828125       | 25.921 Melem/s                        |
| `tokenize_additional/identifier/2006`  | 2,006             | 88009.5947265625         | 22.742 Melem/s                        |
| `tokenize_additional/identifier/20026` | 20,026            | 863765.9921875           | 23.191 Melem/s                        |

One declared Element is one Unicode scalar character, so the reported throughput
is characters/second, not UTF-8 bytes/second or tokens/second. The throughput
column transcribes the console's central `thrpt` estimate; it is not a rate
recomputed from the separate median column. Tokenization and result allocation
are timed; fixture setup, exact token-vector witnesses and returned-vector
destruction remain outside the timer as described above.

### Additional language observations

| Criterion full ID                                 | Criterion median (ns/op) | Logged 101-call single-call median (ns) |
| ------------------------------------------------- | ------------------------ | --------------------------------------- |
| `language_additional/legacy_meaningful/normal`    | 751.9421081542969        | 781                                     |
| `language_additional/legacy_meaningful/gibberish` | 227.3451976776123        | 280                                     |
| `language_additional/contains_cjk_f32/latin`      | 38.88330602645874        | 90                                      |
| `language_additional/contains_cjk_f32/mixed`      | 31.53233551979065        | 80                                      |

The last column is the separately logged middle duration of 101 individually
timed calls, including clock, black-box and function-pointer overhead. It is not
the Criterion statistic. The legacy rows call deprecated `is_meaningful_query`,
which forwards to `is_meaningful_text_query`; the f32 rows call
`contains_cjk_f32`. Language rows declare no throughput unit.

### Existing guarantee-row context

| Criterion full ID                        | Actual measured API / metric                                             | Criterion median (ns/op) |
| ---------------------------------------- | ------------------------------------------------------------------------ | ------------------------ |
| `tokenize/whitespace/long`               | `WhitespaceTokenizer::tokenize`; latency only, no throughput declaration | 7889.766784667969        |
| `lang_detect/is_meaningful_query/normal` | `is_meaningful_text_query`; the ID retains the older name                | 835.3163909912109        |
| `lang_detect/contains_cjk/mixed`         | `contains_cjk`; mixed-CJK latency                                        | 141.82712841033936       |

The `lang_detect/is_meaningful_query/normal` ID measures the canonical text
function, while `language_additional/legacy_meaningful/normal` measures its
compatibility entry point; these distinct IDs are not duplicate measurements of
a second query policy. The whitespace guarantee's characters/second baseline
remains pending because its measured target declares no throughput. The old
long-fixture size label is not promoted into a current scalar-character count.
This CI appendix supplies observed context for the other measured rows without
replacing the fixed A/A reference or claiming a comparable-host trend.
