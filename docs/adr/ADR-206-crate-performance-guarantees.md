# ADR-206: Crate Performance Guarantees

**Status**: Proposed\
**Date**: 2026-10-17\
**Depends on**: [ADR-166](ADR-166-hot-path-latency-regression-guard.md),
[ADR-205](ADR-205-dependency-scoped-pull-request-ci.md)\
**Grounding commit**: `d3619dec9e32045f273c61b5a7b2eb70fe6463a3`

## Decision summary

Each performance-critical crate owns an executable performance contract in
`crates/<crate>/perf/guarantees.toml`. Its benchmark ledger explains and displays that
contract. Deterministic work bounds gate affected pull requests. Timings on hosted
runners remain advisory. A published absolute performance promise requires qualified
reference measurements and fresh evidence before release. Benchmark selection uses
the code map's dependents closure; complete scheduled and release runs catch omissions.

This is a new contract built on ADR-166, not a claim that its proposed trend lane or
ADR-205's selector already exists. Both records are marked Proposed at the grounding
commit; `bench/trend-config.toml` and `scripts/ci-test-scope.py` are absent there.
Their acceptance and the relevant implementation are prerequisites for activation.

## Context

The repository has 16 crate benchmark ledgers, but having a ledger or a benchmark
binary does not establish an enforced guarantee. For example, the comm, text and
retrieval ledgers last changed in June while their benchmarks changed in October.
The retrieval benchmark's `label_scoring` group has no ledger entry. Storage, runtime,
MCP and the CLI have no crate-local benchmark targets at the grounding commit, though
some of their behavior is exercised indirectly.

`bench-track.yml` selects four hand-written groups of five crates for pull requests
and runs the complete component and end-to-end set daily and on release tags. It
also supports manual dispatch. Metric values are advisory; component compilation,
panics and timeouts can fail their job. `bench_track.py` appends observations to
`perf-data`; it does not evaluate performance bounds. Some workflow comments and
`scripts/perf/README.md` still describe a run on every main push, contrary to the
workflow's actual triggers.

`perf/targets.toml`, `perf/ledger.csv` and `bench-1m.yml` already provide Vamana-specific
checks. They are not a workspace contract. In particular, the synthetic check called
structural includes `speedup_vs_brute_force`, a timing ratio. Dividing two timings
does not make the result independent of CPU, cache or contention.

ADR-166 supplies the useful separation: count mechanisms in ordinary tests, observe
timing separately, and keep thresholds in versioned configuration. This record extends
that separation to crate coverage, benchmark identity, publication and release evidence.
It does not assert that counters exhaust all possible performance regressions.

## Decision

### D1. A separate ADR owns the workspace contract

ADR-166 continues to own G1-G5, its counter-attribution and quiescence requirements,
and the non-gating timed trend lane. This record owns crate registration, metric
classification, selection, coverage and publication. ADR-205 owns the dependency-map
classifier and its parity controls. No second dependency classifier is introduced.

Acceptance of this record must also reconcile three narrow points in ADR-166:

1. D1 invariants execute whenever their owning crate is selected by the safe closure,
   and on full runs; they need not execute on an unrelated documentation change.
2. A green mechanism suite does not prove that all latency regressions are covered.
3. D2/D3 retain their advisory authority and rolling window, with the baseline admission
   and fixed-reference safeguards in D4 below. A separate, explicitly promoted release
   contract under D5 does not turn D2 into a timing gate.

These are explicit companion edits at adoption. The dependency is not permission to
silently reinterpret ADR-166 or to expand its read-path invariant constants to other verbs.

### D2. Crates own manifests; ledgers show their evaluated coverage

The workspace inventory comes from Cargo workspace membership, not the list of crates
which happen to have benches today. Each member gets a manifest with a schema version,
crate name, responsible maintainer group, classification and rationale. Classification
is `critical`, `covered-by` or `not-critical`:

- `critical` requires named workloads, at least one executable guarantee, and coverage
  of each declared performance-relevant surface. A small utility need not promise latency
  if a bounded operation count describes its actual contract.
- `covered-by` names exact guarantee IDs and the producing crate. It is appropriate for
  a contract-only crate whose relevant behavior is measured at a concrete implementation
  seam. It cannot mean merely "some downstream test probably covers this."
- `not-critical` carries a reviewed reason. Introducing a hot path or publishing a
  performance claim requires revisiting that classification in the same change.

An existing gap may be registered as `pending`, with its missing producer, owner,
tracking issue and expiry no later than the next release cycle after activation.
Pending is visible coverage debt, not an executable guarantee or a successful result.
No active guarantee can be demoted to pending to bypass a failure. New critical paths
must arrive with their producer; the temporary allowance is for migration only.
Pending records have a separate schema that names the missing deliverable; validation
reports the gap rather than requiring a nonexistent executable or counting it as run.

Each guarantee has a stable, globally unique ID, prefixed by crate name. Its required
fields have these meanings; the schema must reject unknown fields and invalid combinations:

| Field group | Required contract                                                                                                                                          |
| ----------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Identity    | ID, schema revision, covered API/path, responsible maintainer group, status, enforcement lane                                                              |
| Workload    | Fixture or generator and digest, seed set, input sizes/distributions, concurrency, configuration, correctness or quality preconditions                     |
| Producer    | Workspace-relative source, Cargo package and target or script, harness kind, features, working directory and command arguments; no implicit shell fragment |
| Observation | Exact emitted scenario IDs, statistic, unit, direction, measurement boundary and expected record count; bounded parameter expansion is explicit            |
| Bound       | Typed operator and numeric limit, or an exact reference to an existing normative threshold; scaling bounds also identify sizes and formula                 |
| Environment | Applicable platform classes, compiler/profile/features, allocator where relevant, cache-state vector and pre-registered warm-up rule                       |
| Evidence    | Plan, immutable result and raw-artifact references, tested source revision, negative-control evidence, qualification and last verification                 |
| Policy      | Pull-request/scheduled/release applicability, trend-config reference where needed, fixed-reference ID, and permitted evidence age                          |

Duration, operation count, memory, quality and dimensionless ratio are distinct metric
types. NaN, infinity, absent measurements, unknown units, zero ratio denominators and
an unexpected number of samples are invalid evidence, never a satisfied inequality.

One producer may emit several guarantees; one guarantee may need several size points.
The planner records `(package, target, features, scenario, metric, contract revision)`
for every expected result. Duplicate or missing identities fail completeness. A fresh,
run-specific output directory prevents an old Criterion estimate satisfying a new run.
Every discovered benchmark target and emitted scenario must be registered either as a
guarantee or as explicitly advisory exploration with a purpose. A target count is not
a metric count, and a nonempty Criterion tree is not proof of complete coverage.

`docs/benchmarks.md` remains the human entry point. A generated section lists IDs,
workloads, bounds, commands, enforcement lanes, known gaps and latest evidence links;
historical measurements remain outside that section. A documentation check detects drift.
Numerical limits are not copied into hand-maintained tables.
Generated sections use stable links to the latest evidence view; they do not require a
source commit after each scheduled measurement. Updating an evidence pointer does not
change the semantic contract revision or reset its comparison history.

**Readers.** The new `scripts/perf/guarantees.py` validates and renders manifests, plans
the expected results, and evaluates evidence. Existing test jobs run the declared
deterministic producers. The trend workflow uses the plan to record observations with
`bench_track.py`; the evaluator consumes those observations. Release preparation reads
the evaluator's complete manifest. Thus the manifest has named automated consumers.

`bench/trend-config.toml`, delivered with ADR-166 D3, remains the single source for
trend budgets, `noise_ceiling`, `breach_multiplier` and `baseline_window`, keyed by
guarantee ID and comparison class. Crate manifests reference those keys. Shared runner
definitions and cost ceilings live there as well. A crate-local deterministic or
release-only bound lives in its manifest. A bound cannot have two authoritative values.

### D3. Choose the metric for the promise being made

| Kind                             | What it establishes                                                                                                                     | Enforcement                                                                     |
| -------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------- |
| Deterministic work               | Attributable acquisitions, statements, rows visited, bytes processed, distance evaluations, or bounded allocations for a fixed workload | Affected PR and release                                                         |
| Deterministic scaling            | Work-count bound across named sizes and distributions, preferably also an absolute work cap                                             | Affected PR for bounded fixtures; heavy fixtures on schedule and before release |
| Quality/correctness              | Minimum result quality or exact semantic result on a declared corpus and configuration                                                  | Affected PR for bounded fixtures; all required fixtures before release          |
| Relative timing/throughput       | Movement against a comparable rolling baseline and a fixed accepted reference                                                           | Advisory trend; sustained breach files an issue                                 |
| Absolute timing or timed scaling | A bound under a named, qualified environment and workload                                                                               | Advisory until explicitly promoted; then release only on the reference class    |

Finite size points establish a bounded empirical scaling contract, not a universal
asymptotic theorem. Ratios of elapsed time or RSS retain environmental dependence.
Allocation counts depend on the measured boundary, allocator and compiler choices;
they gate only in the declared configuration with unrelated work excluded.

Deterministic tests exercise the production path, assert useful results, and observe
the actual resource seam. SQL statement count alone cannot detect a full scan replacing
an indexed lookup; use rows visited or a second appropriate work measure. ADR-166's
background-task drain applies wherever detached work changes the observed counters.
Instrumentation which cannot attribute a counter reliably is not eligible to gate.
Assertions must read their bound from the same authoritative configuration, or emit
raw counts for the evaluator to compare with it. Copying a limit into both a Rust test
and the manifest is not an acceptable implementation of a versioned bound.

Quality and cost must be evaluated together. ANN recall at an automatically widened
beam can remain green while cost grows sharply. A claim about that workload therefore
also bounds beam/distance work or pairs recall with its timed cost. A scaling ratio
alone can miss a constant-factor slowdown; include an absolute count or fixed timing
reference when that failure matters.

Criterion estimates must keep their actual meaning. A median estimate of batch
iteration cost is not per-request p50, and an upper confidence bound is not p95.
Published tail latency requires a producer of individual request observations with
the declared arrival, queueing and error accounting, not relabelled Criterion output.

### D4. Timing uses comparable series and visible incomplete states

Hosted runners provide broad advisory trends. The existing Linux build host can provide
reference measurements only during an exclusive reserved window that meets the evidence
protocol in `scripts/perf/README.md`. Being outside GitHub does not establish isolation.
If such a window cannot be obtained, its result is UNMEASURED. Buying or assigning a
dedicated machine is a separate resource decision; this ADR does not require a purchase.

A comparison key includes guarantee and workload revision, fixture digest and seed,
statistic, features, compiler/build profile, dependency configuration, allocator, cache
state, OS/architecture, CPU identity and enabled instruction path, effective CPU/memory
limits, and storage class where relevant. Hosted runner labels alone are insufficient.
Unknown or changed material fields cannot reuse the old series. Source SHA is recorded
on every observation but is not part of the comparison key: source changes are what
the trend is intended to measure. Harness or fixture changes require an explicit revision.

For each timed metric, scheduled measurement includes two same-SHA runs of the same
workload and state. A/A spread is `max(a,b)/min(a,b)` for positive observations. Above
the configured `noise_ceiling`, or when isolation/state cannot be established, that
metric reports UNMEASURED. Two consecutive UNMEASURED attempts file one deduplicated
environment issue. A/A measures repeatability, not hardware equivalence; two equally
slow runs on a different CPU cannot establish comparability with the old CPU.

ADR-166's initial configuration values remain initial values: `noise_ceiling=1.15`,
`breach_multiplier=1.5`, `baseline_window=7`. They must be calibrated per metric/class,
not copied into all new contracts as evidence-based limits. Thresholds are not widened
automatically to make a noisy runner pass. Quick-profile PR observations do not enter
a qualified full-profile baseline or supply release evidence. No A/A pass is added to PRs.

The rolling baseline is the median of the preceding accepted observations of the same
class, excluding the candidate run. Fewer than three observations means BASELINE-PENDING;
three through six use the interim median under the initial seven-run window. For other
window values, the same minimum of three applies. Bootstrap admits only complete,
valid A/A observations satisfying any declared absolute budget. The fixed reference is
approved independently; an unreviewed bootstrap cannot establish its own promise.
Thereafter a breaching observation is retained in history but
not admitted to the window that would normalize that breach away.

Also compare with a fixed accepted reference, identified by immutable evidence and
contract revision. Its tolerance is reviewed configuration; it never moves with the
rolling window. This catches accumulated small slowdowns. Every active trend contract
requires that reference before it can report healthy coverage; lacking one remains
BASELINE-PENDING even if exploratory rolling comparisons are available. An absolute
budget, where declared, is evaluated separately.

For a lower-is-better metric, breach means `current > multiplier * baseline`; for a
higher-is-better metric it means `current < baseline / multiplier`. Fixed-reference
comparisons use their own configured multiplier with the same direction rule. Three
consecutive measured breaches of either comparison or a declared budget file an issue
with raw evidence and available mechanism counters. An UNMEASURED attempt pauses the
sequence; a measured non-breach resets it. Gaps remain visible. An open issue is updated
on later evidence, rather than filing an issue per metric per run.

A class change starts BASELINE-PENDING without deleting the old series or accepting a
new performance promise. A release contract needs qualification on the new class and
a reviewed new reference, preferably replaying the old accepted source there. Compiler,
fixture or hardware changes must not become an unreviewed baseline reset that hides a
regression. When an old workload cannot be replayed, retire or revise the promise with
that limitation explicitly recorded.

### D5. Gates distinguish violated bounds from absent evidence

Pull requests gate on manifest/document consistency, successful execution and complete
output of selected producers, and selected deterministic bounds. A failed build, panic,
timeout, missing fixture or missing declared scenario fails its required execution
check; a timing value outside its advisory threshold does not fail a PR. Where a job
is a required check, an unexplained skip cannot satisfy it.

The full scheduled run evaluates every active contract, reports pending coverage, and
files sustained timing/environment findings. Timing findings remain advisory even when
deterministic tests are green; their investigation must consider uninstrumented CPU,
I/O, contention and workload effects rather than assume a missing counter is always enough.

Release preparation requires an evidence manifest for the exact candidate SHA covering
all active contracts and all applicable configurations. Scheduled results may be reused
only for that same SHA, contract/workload revision and class, within each contract's
declared maximum age. Otherwise run them before publication. The release-tag workflow
is too late to be this gate; it remains an observation or reuses the qualifying run.

| Release input                                                       | Required disposition                                                                                                             |
| ------------------------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------- |
| Deterministic/quality bound                                         | Complete passing evidence, including required heavy fixtures                                                                     |
| Unexpired migration gap                                             | Visible debt; not counted as coverage; expiry later than this release must be explicit                                           |
| Expired migration gap or ADR-166 expected-fail older than one cycle | Release blocked until fixed or the contract is explicitly retired with its dependent claims                                      |
| Hosted timing trend                                                 | Attempt and status recorded; breach or UNMEASURED files a finding, does not itself block release                                 |
| Promoted absolute/timed-scaling promise                             | Fresh qualified reference evidence satisfying the bound; breach, UNMEASURED, BASELINE-PENDING or missing evidence blocks release |

A timing contract may be promoted only with the existing evidence protocol, at least
ten same-SHA calibration runs on the reference class, at least two weeks of zero false
alarms, and a demonstrated slowdown control that violates the proposed bound. Its
pre-registered plan declares sample count, estimator and confidence method. Upper bounds
require the upper confidence limit to meet the bound; lower bounds require the lower
limit. Overlap is inconclusive, not pass. A bounded rerun policy is registered in advance;
all attempts remain in evidence and retry-until-green is prohibited. The zero-alarm
window alone does not establish sensitivity to regressions.

Maintainers change a guarantee through a reviewed diff carrying old and new bounds,
the reason, comparable before/after evidence, affected publication references and the
tradeoff in correctness or quality. Changing features, fixture sizes, lanes, class,
expiry, reference or scenario IDs is a contract change just as loosening a number is.
Changing a machine profile or marking a result advisory cannot waive an active promise.
If qualification is unavailable, the options are to wait, fix the environment, or retire
the promise through that same review; an unrecorded release override is not an option.

### D6. Select by dependents closure and account for the whole cost

For PRs, reuse ADR-205's classifier result for the exact base/head pair and validated
map. Select changed crates plus their transitive dependents across normal, development
and build edges, then select their declared PR producers. Bench-only runtime relationships,
shared fixtures and generator/script inputs that are absent from Cargo's graph are
declared in manifests and add affected producers. A `covered-by` contract adds its
producer even when that dependency is not a Cargo edge. The expanded set is the logged
benchmark scope; execution is never limited to only the directly changed crates.

Manifests, claims and referenced Markdown fixtures are classified before a docs-only
exit. A pure prose edit can avoid compilation but must still pass the cheap claims and
manifest checks in the existing documentation/classification job. Changing a shared
benchmark policy, toolchain, lockfile, manifest graph or unclassified executable input
selects the full applicable set. Map failure, warning, missing project or uncertain
input mapping also selects the full set with a reason. If that work cannot fit the
approved capacity, required checks remain incomplete; safety is not traded for a skip.

The selector is a proposed deliverable until ADR-205's implementation is available.
Implement benchmark mode by extending `scripts/ci-test-scope.py` or its shared library,
with the same main-built classifier, refusal-before-ingest ordering, map freshness and
Cargo parity controls. Do not treat a stale map or the absent script as an empty scope.
Do not use a branch-name exemption: all PRs follow the same scope rule at activation.

Selection limits executed benchmark/test targets, not what Cargo must compile as their
dependencies. Compile selected packages together where feature sets permit, in a reused
target directory keyed by toolchain/profile/features/lockfile. Partition only when the
measured critical path justifies another build. Each emitted measurement retains its
exact feature set; changed feature unification must not blend histories.

Use the existing daily run for the complete active set, and the pre-publication run for
each release candidate. Heavy fixtures run in those windows on available approved
capacity, not on every PR. A finite full sweep must actually cover the full set; silent
rotation of missing scenarios or a permanently skipped dataset is not full coverage.
Coalesce a scheduled and release run at the same SHA when their requirements match.

Before activation, record the current costs from job records, distinguishing PR,
scheduled and release work. Commit numerical ceilings and the measurement window in
the shared configuration. Compare at least twenty representative PRs, a complete
scheduled cycle and a release cycle against the old policy at matched change mix.
Count compilation, classification, execution, calibration, retries and cancelled work;
amortize qualification work over the stated window. Track both hosted runner-minutes
and all compute-minutes including the build host. Shorter elapsed time from parallelism
is not a cost reduction.

Both totals must be no greater than their pre-change baselines for comparable volume,
with no loss of required coverage. Savings come first from removing four overlapping
component builds, reusing selection, avoiding duplicate synthetic runs, and sharing
scheduled/release evidence. New cases are admitted only within the measured envelope.
There is no savings claim yet: the current workflow is already daily, closure selection
can exceed a five-crate component, and removing branch exemptions adds work. If the
full contract cannot fit, defer activation and present the measured capacity/coverage
fork for review; do not silently weaken guarantees or relabel hosted cost as free work
elsewhere. Routine runs record budget overruns as operational findings; they never turn
unrun mandatory checks into passes.

### D7. Published performance numbers are traceable and scoped

Every project performance number in README files, crate ledgers, guides and ADRs must
reference its producing benchmark, guarantee ID and retained measurement evidence.
Register the claim in the owning manifest with a stable document anchor, the exact
claim text or table-range digest, and one of these meanings:

- **Current promise:** states a bound, applicable workload/class and enforcement lane.
  An absolute timing promise requires D5 promotion before publication as a guarantee.
- **Historical observation:** states source revision, environment and observed result.
  It links the producing contract and evidence but does not say the same exact value
  holds on current code. A whole historical measurement table may share one registration
  if every metric/row is mapped and the evidence covers the entire table.
- **Unmeasured target:** uses explicit target language and a pending contract with a
  named future producer and expiry. It cannot appear as an achieved result or a current
  product promise. This temporary form follows the migration-gap rules in D2/D5.

The documentation check verifies anchors, text digests, producer references, evidence
references and promise/contract consistency. It flags newly added or changed numeric
performance statements for registration. Unit/ratio patterns are a useful backstop,
not a proof that prose contains no unregistered claim; reviewers explicitly check
performance statements in the diff. Ordinary identifiers, dates and external comparative
citations are not product performance claims.

A published achieved number without a reproducible producer is measured with the correct
workload or removed. A microkernel benchmark cannot support an end-to-end speedup claim,
and a synthetic recall result cannot support a different corpus's recall bar. Historical
ADRs gain a dated correction rather than a fabricated retrospective measurement. Existing
git history preserves the previous text; it is not a reason to leave unsupported current
wording untouched. Historical observations may remain only when their producer and
evidence can actually be identified.

Threshold revisions update the registered current claims in the same diff. Preserve
raw evidence by content-addressed reference in `perf-data` or another durable release
archive; the current 90-day workflow-artifact retention is insufficient for a long-lived
public claim. Never put measurements from private corpora into public history without
a publishable fixture and evidence plan.

### D8. Extend the existing tooling and migrate without competing thresholds

All commands in this table are implementation deliverables, not commands claimed to
exist at the grounding commit. They run from the repository root.

| Deliverable command                                                                                                                                                  | Reader/output contract                                                                                                                                                                                |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `python3 scripts/perf/guarantees.py check --all`                                                                                                                     | Read every workspace member/manifest and claim registration; validate schema, producer paths, threshold references, gap expiry, documentation and coverage declarations; no builds or daemon required |
| `python3 scripts/perf/guarantees.py render --check`                                                                                                                  | Verify generated ledger sections match manifests; a separate `render --write` updates them intentionally                                                                                              |
| `python3 scripts/ci-test-scope.py --mode bench --base <sha> --head <sha> --output <scope.json>`                                                                      | Emit validated scope, source/map identity, selected packages and declared extra inputs; report full-scope fallback and why                                                                            |
| `python3 scripts/perf/guarantees.py plan --lane <pr\|scheduled\|release> --scope <scope.json> --output <plan.json>`                                                  | Expand manifests into exact commands, expected IDs, environments and budgets; scheduled/release plans require a full scope                                                                            |
| `python3 scripts/perf/guarantees.py run --plan <plan.json> --output <run-dir>`                                                                                       | Execute declared producers with lane timeouts; capture commands, exit codes, exact test/scenario execution and fresh outputs; preserve partial/error evidence                                         |
| `python3 scripts/perf/guarantees.py evaluate --lane <pr\|scheduled\|release> --plan <plan.json> --results <run-dir> --history <history-dir> --output <verdict.json>` | Join expected and observed IDs, evaluate typed bounds and trend states, emit complete verdict with missing/invalid/failed entries and lane-specific exit code                                         |

The run command is a thin orchestrator over existing Cargo and script producers, not a
new benchmark engine. Existing CI tests can supply their already-recorded results to the
same plan rather than executing them twice. A `cargo test` filter that runs zero tests
cannot supply evidence. Schema errors and required execution errors exit nonzero in
every lane; performance breaches exit nonzero only where the enforcement matrix allows.
UNMEASURED trend observations are successful recordings of absent measurement, visibly
distinct from successful guarantees. Notification publication failure is recorded and
retained for retry, never described as a filed issue without a returned issue reference.

Extend `bench_track.py` and `publish_ledger.sh` with a versioned envelope carrying the
plan, producer/scenario identity, guarantee revision, comparison key, actual measurement
time, A/A samples, state and evidence references. Keep the existing `(sha, run_id,
run_attempt)` provenance and component sharding. Preserve the existing commit timestamp
separately from measurement time; release freshness cannot use commit date. Old records
remain readable as legacy observations, but lacking provenance makes them ineligible
for a new qualified baseline. The guarantee evaluator rejects duplicate identities even
where the historical renderer used last-write-wins aggregation.

Migration order:

1. Accept the governing ADRs and companion clarifications. Add the schema, cheap checker,
   registration census, generated ledger sections and dated gap list. Register current
   suites without claiming new timing guarantees. Remove or qualify unsupported claims.
2. Implement the shared safe selector, producer identity and complete-output checks.
   Demonstrate the cost envelope before changing the workflows. Preserve compilation,
   crash and timeout failure reporting and `perf-data` history.
3. Register ADR-166 invariants and add missing crate producers incrementally. Implement
   its trend config and A/A evaluation with the existing recorder. Reconcile old prose
   such as the retrieval ledger's generic five-percent alarm with the actual policy.
4. Register Vamana's existing targets by exact reference to `perf/targets.toml`; keep
   that file authoritative for those limits and keep both CSV ledgers as raw evidence.
   Separate deterministic recall/work checks from timed speedup, latency and fitted
   timing exponents. Remove `speedup_vs_brute_force` from the hosted blocking assertion
   set in the same change that exposes it as advisory and preserves deterministic checks.
   Real SIFT evidence needs the real fixture; the manual job's missing-data success
   cannot satisfy a release plan. Amend its adapter/workflow accordingly. Moving the
   limits into crate manifests later requires a separate atomic reader migration.
5. Qualify reference windows, retain public evidence and promote only supported release
   promises. Insert the release evidence check before publication and deduplicate matching
   scheduled/tag observations. No automatic dedicated-machine provisioning is included.

`BENCH_TRACK_DISABLED` may still stop advisory collection, with an explicit coverage
outage recorded. It cannot disable deterministic gates, documentation checks or required
release evidence. This restriction and removal of branch-name bypasses are explicit
workflow changes, not properties of the current implementation.

## Alternatives considered

1. **Widen ADR-166 into a workspace omnibus.** Rejected: it mixes read-path recovery
   invariants with crate ownership, publication and release qualification. A dependent
   record with narrow companion clarifications preserves both contracts' boundaries.
2. **Keep guarantees only in Markdown, or put everything in one workspace file.**
   Markdown invites drift from the runner; a single file separates ownership from crates
   and becomes a contention point. Per-crate typed manifests with generated prose retain
   automated readers and local review. Attributes beside bench code would require
   compile-aware discovery and do not naturally describe script/end-to-end producers.
3. **Use one metric kind for every crate.** Absolute timings on hosted runners fail on
   noise; rolling baselines can accept accumulated decay; pure counters miss uncounted
   costs; scaling ratios can hide a uniform slowdown. Mixed, explicitly scoped evidence
   avoids pretending that any one instrument proves all performance properties.
4. **Make all timing a PR gate, or purchase a dedicated host immediately.** The first
   spends trust and runner time on environmental variance. The second is a resource
   commitment without a measured need. Broad hosted trends plus qualified reserved
   reference windows establish what can be promised before committing more hardware.
5. **Run every benchmark on every PR, or keep direct-path component selection.** Full
   PR runs ignore the cost constraint. Direct paths miss downstream effects. A safe
   dependents closure with explicit non-Cargo inputs gives the smallest justified set.
6. **Grandfather unsupported published numbers.** That preserves assertions the tooling
   cannot verify. Register reproducible historical evidence or remove the numbers;
   new measurements must not be retroactively attributed to an old revision.

## Consequences

**Positive.** Each covered crate has a discoverable contract, producer and enforcement
reader. Deterministic checks catch known mechanism regressions without timing noise.
Missing output is visible, comparisons retain their environment, and public promises
can be traced to fresh release evidence. Selection follows the dependency structure.

**Negative.** Fixture design, counter attribution, evidence retention and claim migration
require work. Some current numbers must be removed. A qualified release promise can
delay a release when no valid reference window is available. The cost-neutral rollout
is an acceptance condition whose feasibility is still unmeasured, not an assumed saving.

**Neutral.** Criterion, Cargo tests, existing script harnesses, `bench_track.py`, the
`perf-data` branch and Vamana's raw ledgers remain. The new evaluator supplies contract
semantics and completeness; it does not replace those measurement engines.

## Verification

Acceptance requires recorded evidence for all of these controls:

1. The census covers every workspace member; deleting a critical manifest, producer,
   feature declaration, expected scenario or claims anchor fails the cheap checker or
   run completeness check. An intentional noncritical classification remains visible.
2. A deterministic work-increasing mutation fails its check while returning the same
   correct result. Deferred work is included by the barrier. A no-op implementation,
   zero-test filter, stale output directory and missing target cannot pass.
3. Increasing scanned rows behind unchanged SQL statement count is detected by a
   declared work measure. A quality-preserving ANN slowdown is visible through work or
   paired timing; reducing useful output does not satisfy the performance check.
4. Synthetic evaluator controls demonstrate A/A UNMEASURED, two-attempt environment
   escalation, bootstrap BASELINE-PENDING, three-breach reporting, direction reversal
   for throughput, and a sequence of small slowdowns crossing the fixed reference.
   A class change, duplicate record or missing sample cannot report healthy coverage.
5. Map/Cargo closure parity holds for normal/dev/build edges. A shared-layer change
   selects downstream benches; a shared fixture selects every declared consumer; a
   missing map and an unknown executable path select the full set. Docs-only changes
   still receive claim checking. No branch name exempts a PR.
6. PR timing breaches remain advisory, required deterministic failures block, and
   release evaluation refuses stale/wrong-SHA evidence, missing real data, unqualified
   reference results and expired gaps. Disabling the advisory workflow cannot satisfy
   release requirements. An inconclusive reference interval is not a pass.
7. Migration preserves old history and the Vamana limits' single authority. The synthetic
   PR gate still detects quality/work regressions after its timed speedup is separated.
   Every public performance statement in the migration census is registered, corrected
   or removed; deliberately changing a registered number fails the documentation check.
8. The measured rollout meets both D6 cost ceilings across the representative PR,
   scheduled and release windows while the planned/observed census proves complete
   required coverage. No savings claim is accepted from fewer recorded scenarios.

## Implementation status

Proposed. This record specifies deliverables and controls; it does not report them as
implemented or measured. Before adoption, maintainers must resolve ADR-205's status
against the current repository, approve the companion ADR-166 clarifications, measure
the cost ceilings, identify reservable reference capacity and complete the crate/claim
registration census. Until qualification exists, numerical timing observations may be
published with their evidence, but no new absolute timing guarantee is active.
