# Timing assertion census for #2348

This is the reviewable population for the two-tier timing repair at base
`db226c9abf4c705ec2451ca6014a25ac042e0e84` (28 September 2026). It lives
in `docs/testing/` because it records test contracts and the repair backlog,
not a new product design decision. The companion
[`timing-assertion-census-2348-exclusions.tsv`](timing-assertion-census-2348-exclusions.tsv)
is the exact file-level exclusion union used for this pass.

The classes below are **D** (a discriminating claim whose bound comes from a
configured deadline, injected delay, competing path, or deterministic virtual
clock), **H** (a watchdog or fixture-progress guard; its timeout only prevents
an indefinite test), and **A** (a live wall-clock claim with an independently
chosen absolute ceiling). A lower-bound assertion that an operation _remains_
blocked is marked D when it tests a named synchronization contract; increasing
runner overhead cannot turn it red in the same way as a tight upper ceiling.
An equality on a paused Tokio clock is D, since no runner wall time is being
measured. Assertions on serialized durations, configured timeout fields,
timestamps, and telemetry values injected by a fake clock are outside this
wall-clock population.

## Enumeration and controls

The broad source screen, followed by inspection of the containing test and
assertion, was:

```sh
rg -n --glob '*.rs' 'elapsed\(\)|duration_since\(|Instant::now\(|recv_timeout\(|tokio::time::timeout\(|Duration::from_(millis|secs|micros|nanos)\(' crates
rg -n --glob '*.py' 'monotonic\(|perf_counter\(|assertLess|assertGreater|timeout=|TimeoutError' scripts/tests
rg -n 'writer_stage_sample_attributes_a_slow_body|filtered_count_partitions_share_one_snapshot_during_concurrent_update|LLVM_PROFILE_FILE' crates scripts/tests
```

The third command is the **must-MATCH** control in the same screening pass. At
this base it matched both named #2348 tests (`writer_task.rs:3148`,
`stores/note_tests.rs:1284`) and all four pre-existing Rust detection clusters:
`khive-db/tests/support/caller_timing.rs:5`, `khive-mcp/src/daemon.rs:7742,7872`,
`khive-pack-knowledge/src/knowledge/vamana.rs:4492`, and
`khive-pack-memory/src/handlers/recall.rs:1714,6670`. It also found the
additional Python detection at `scripts/tests/test_contract_harness.py:165`.
These are base line numbers; the implementation may move them.

The two issue controls are materially different. The writer test injects a
400 ms body and checks body-stage telemetry against the other stages (D;
`writer_task.rs:3177-3193`), so its former coverage failure was a comparison
between real measured stages, not an `Instant` ceiling. The note snapshot test
reaches the seam through `recv_timeout(60 s)` at `note_tests.rs:1214` (H), but
the `tokio::select!` branch reports an early query exit separately. The source
explicitly labels the 60 s wait “Hang protection only.” A channel
`Disconnected` can arise when the spawned query exits before the seam; simply
relaxing the watchdog cannot repair that race.

## Measured-time assertions

Rows group assertions that share one measurement and contract. `D-V` is a D
assertion under paused/advanced Tokio time. `D-L` is a lower-bound claim that
the fixture must remain blocked or await its configured delay. `excluded`
means the owner file is in the companion TSV; the site remains a follow-up.

| Site at base                                                                                                                          | Class                | Contract and repair implication                                                                                                                                                                                                                                                                                                                                    |
| ------------------------------------------------------------------------------------------------------------------------------------- | -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `crates/khive-db/src/writer_task.rs:3177-3193`                                                                                        | D                    | #2348 body-stage sample is compared with its injected 400 ms delay and other measured stages; retain relative stage evidence.                                                                                                                                                                                                                                      |
| `crates/khive-db/src/read_cancellation.rs:983`                                                                                        | D                    | Return must precede the fixture's 6 s blocking closure; the `timeout(5.8 s)` above it is a separate H watchdog.                                                                                                                                                                                                                                                    |
| `crates/khive-db/src/read_cancellation.rs:1053,1089`                                                                                  | D-L                  | An admitted write must last through the fixture's 120/900 ms body; the surrounding 1/3 s waits guard completion.                                                                                                                                                                                                                                                   |
| `crates/khive-db/src/checkpoint.rs:6515`                                                                                              | D                    | PASSIVE checkpoint must beat half the configured 2 s TRUNCATE busy timeout.                                                                                                                                                                                                                                                                                        |
| `crates/khive-db/src/stores/text_tests.rs:839`                                                                                        | D                    | Rowid-map deletion must beat the same-fixture full-scan baseline; no absolute runner ceiling.                                                                                                                                                                                                                                                                      |
| `crates/khive-db/tests/checkpoint_dedicated_connection.rs:265`                                                                        | D                    | Writer admission must beat `CHECKOUT_TIMEOUT * 4` while checkpoint holds its own connection; the 5 s lock handshake is H.                                                                                                                                                                                                                                          |
| `crates/khive-db/tests/writer_timeout_sink_stalled_writer.rs:60`                                                                      | D                    | Pool construction must beat half the fixture's slow-sink write delay.                                                                                                                                                                                                                                                                                              |
| `crates/khive-db/tests/support/caller_timing.rs:4-12`, called by `writer_timeout_sink_stalled.rs:138`, `_writer.rs:87`, `_fifo.rs:98` | D                    | Uninstrumented bound is `checkout_timeout * 10`; current coverage branch skips the numeric check, leaving a separate hang watchdog. Shared helper migration owns this cluster.                                                                                                                                                                                     |
| `crates/khive-mcp/src/server.rs:7439,9062-9066`                                                                                       | D-V, excluded        | Remaining request deadline is compared with the configured full bound; a 50 ms outer deadline must expire before the default. `server.rs` is open-PR owned.                                                                                                                                                                                                        |
| `crates/khive-mcp/src/daemon.rs:7802,7923`                                                                                            | D, excluded          | Write/read forwarding use a 2 s request timeout, 3 s ordinary ceiling, 6 s coverage ceiling, and 10 s H watchdog. The two local `LLVM_PROFILE_FILE` checks are a helper migration follow-up.                                                                                                                                                                       |
| `crates/khive-mcp/src/daemon.rs:10203`                                                                                                | D-L, excluded        | Cold-boot fallback must wait at least the fixture's `GUARD_HOLD`.                                                                                                                                                                                                                                                                                                  |
| `crates/khive-mcp/src/daemon/supervisor_tests.rs:206,274,430,464,491,542,741`                                                         | D-L / D-V            | Three-second and caller-deadline lower bounds; exact 30/3 s equalities and the `<200 ms` supervision-entry check at 741 use paused Tokio time.                                                                                                                                                                                                                     |
| `crates/khive-pack-knowledge/src/knowledge/vamana.rs:4488-4504`, called at `4519,6811`                                                | D                    | Terminal ANN path uses ten poll intervals ordinarily and half the warm-wait timeout under coverage. Replace its local environment read with the common rule.                                                                                                                                                                                                       |
| `crates/khive-pack-memory/src/handlers/recall.rs:1851-1858,7110-7117`                                                                 | D, closed            | Caller bounds remain `CALLER_DEADLINE_MS` ×2/×10. Both use the shared `timing::duration_bound(bound, None)`; coverage skips only the numeric check, retaining the result assertions and 30 s hang watchdogs.                                                                                                                                                       |
| `crates/khive-pack-memory/src/handlers/recall.rs:4332-4339`                                                                           | D, closed            | Profile recall uses twice the same cached configured deadline as dispatch (`pack.rs:614-646,765-778`), with no request override: the 30 s default gives a 60 s ordinary bound. The shared helper skips the numeric check under coverage; dispatch outcome checks remain. This replaces the independent 2 s latency ceiling without asserting measured performance. |
| `crates/khive-pack-comm/tests/integration.rs:573,614,667`                                                                             | D-V, excluded        | Paused-clock long-poll tests assert the configured 250 ms, zero, and 30 s durations exactly.                                                                                                                                                                                                                                                                       |
| `crates/khive-pack-web/src/fetch.rs:1122`, `egress.rs:805,824`, `refresh.rs:1057`, `search.rs:557`                                    | D-V, partly excluded | Paused-clock tests assert exact configured 1/3 s timer behavior. `fetch.rs` and `refresh.rs` are in open PR #3486; no real runner latency is measured.                                                                                                                                                                                                             |
| `crates/kkernel/src/coordinator/tests.rs:1281`                                                                                        | D-V                  | Three hung backends use a 5 s request budget; the `>=5 s` and `<6 s` checks run under `#[tokio::test(start_paused = true)]` with `tokio::time::Instant`, so they assert deterministic timer behavior.                                                                                                                                                              |
| `crates/khive-runtime/src/daemon.rs:4401`                                                                                             | D-V                  | Paused-clock drain asserts configured 250 ms has elapsed and cleanup completes before 350 ms virtual time.                                                                                                                                                                                                                                                         |
| `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:958`                                                                    | D                    | Shed generation must finish before half the configured append deadline.                                                                                                                                                                                                                                                                                            |
| `crates/khive-runtime/tests/read_verb_admission_exhaustion.rs:480`                                                                    | D-L                  | The audit dispatch must remain pending during the 80 ms observation interval after an admitted append.                                                                                                                                                                                                                                                             |
| `crates/kkernel/tests/supervisor_lifecycle.rs:1303`                                                                                   | D-L                  | Nonexiting holder must consume the configured wait before failure; process readiness loops elsewhere are H.                                                                                                                                                                                                                                                        |
| `crates/khive-hnsw/src/alias/manager.rs:1210,1218`                                                                                    | H                    | `recv_timeout(5 s)` only proves unrelated registration eventually completes while validator is paused; it does not claim a 5 s service latency.                                                                                                                                                                                                                    |
| `crates/kkernel/src/exec.rs:4655-4657`                                                                                                | D-L                  | Construction must _not_ complete during the 500 ms lock hold; the fixed observation window is a negative synchronization claim.                                                                                                                                                                                                                                    |
| `crates/khive-runtime/src/daemon.rs:4519`                                                                                             | D-L                  | Draining must still be pending during the 150 ms blocked hydration phase.                                                                                                                                                                                                                                                                                          |
| `crates/khive-pack-comm/tests/integration.rs:15832`                                                                                   | D-L, excluded        | Unrelated rows must not satisfy the 30 ms long-poll wait; the negative claim is tied to that fixture window.                                                                                                                                                                                                                                                       |
| `crates/khive-pack-moodboard/src/handlers.rs:834`                                                                                     | D-L                  | Second hydration must remain blocked for the 50 ms observation while a source lease is held.                                                                                                                                                                                                                                                                       |
| `crates/khive-pack-blob/src/uploads/tests.rs:1486`                                                                                    | D                    | File age is explicitly advanced beyond `DEFAULT_ORPHAN_SWEEP_GRACE`; this is a fixture premise, not service latency.                                                                                                                                                                                                                                               |
| `crates/khive-pack-exec/src/handlers.rs:2118`, `src/grant_pin_tests.rs:223`                                                           | D                    | Returned elapsed telemetry must reach the configured time cap; no independent wall ceiling is imposed.                                                                                                                                                                                                                                                             |

| Site at base                                                           | Class         | Why the ceiling is independently chosen                                                                                                                                                                                                                                                                                                                                  |
| ---------------------------------------------------------------------- | ------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `crates/khive-db/src/pool.rs:3333`                                     | A             | Live `<1 s` claim for a 20 ms request deadline versus a 5 s pool checkout; the ceiling is not computed from either.                                                                                                                                                                                                                                                      |
| `crates/khive-pack-comm/tests/integration.rs:502,642`                  | A, excluded   | Live `<1 s` wake/immediate-result claims versus a 5 s long-poll budget; `integration.rs` is in open PRs.                                                                                                                                                                                                                                                                 |
| `crates/khive-pack-comm/tests/hold_time_regression.rs:182,188,248,254` | A             | Live median/p95 are checked against fixed calibrated microsecond constants times a safety factor; the constants come from previous runner measurements, not each run's fixture. This is a performance gate and needs a deliberate policy, not an automatic large coverage multiplier.                                                                                    |
| `crates/khive-runtime/src/daemon.rs:4931`                              | A, excluded   | Probe uses `DUPLICATE_PROBE_TIMEOUT=500 ms`, but the test allows a separately chosen 2 s wall ceiling.                                                                                                                                                                                                                                                                   |
| `scripts/tests/test_contract_harness.py:296-314`                       | A setup bound | `test_stalled_request_is_bounded_and_reaped` starts the fake server through `adapter(... timeout=exchange_budget)` with an absolute 0.25 s initialize exchange budget before the measured stalled request. Hosted macOS exceeded this setup budget. Split startup readiness allowance from the request's ordinary 0.25 s timeout; the latter is the behavior under test. |

The A table is the actionable absolute-bound backlog after applying the
file-level exclusions. The existing D rows can still be fragile when the
test's expected-versus-failure gap is narrow; classification describes the
source of the bound, not proof that coverage passes.

### Current follow-up status (6 October 2026)

The A rows above retain the original census base and classifications. Subsequent repairs are:

- The MCP forwarding write/read timeout tests in `crates/khive-mcp/src/daemon_tests.rs` now select their bounds through the shared timing helper. Both modes retain an unconditional strict numeric assertion: the 2-second request timeout plus 1 second ordinarily (3 seconds), or three times that timeout under coverage (6 seconds). Both keep the separate 10-second watchdog, outcome classification, stream cleanup, and private-home child isolation. This consolidates policy selection without changing the timing contract or establishing coverage-run acceptance.

- `socket_speaks_khived_protocol_rejects_a_non_protocol_listener` now uses the shared timing helper with `DUPLICATE_PROBE_TIMEOUT * 4`: the ordinary strict bound remains 2 seconds for the production 500 ms timeout. Coverage omits only that numeric assertion. The real non-protocol-listener refusal and cleanup remain, with a separate 30-second hang watchdog in both modes. This is a timeout-derived timing assertion plus a watchdog, not a measured coverage result.
- The comm hold-time tests already use the shared helper: ordinary runs retain the calibrated median/p95 limits, while coverage omits those numeric gates and retains sample-integrity checks. The calibration remains historical rather than fixture-derived.
- `test_stalled_request_is_bounded_and_reaped` already gives initialization a separate 2-second startup allowance; the stalled request and reap budgets remain 0.25 seconds. Its setup no longer consumes the request's measured timeout.

These updates do not establish coverage-run acceptance or close the other census follow-ups.

The Python completion-envelope assertion at
`scripts/tests/test_contract_harness.py:157-167`, called at
`292,311,349,377,502`, is **D with a fixed scheduling floor**: its formula
derives from exchange, worker, and reap budgets, while some callers supply a
hosted-runner floor (the #3429 precedent). Its coverage branch skips the upper
assertion. This is separate from the A setup bound above.

The new macOS train failure is in `.github/workflows/ci.yml`'s
`CI_SUITE=full` shard 1, through `scripts/ci.sh lint` to the Python contract
harness. That lane is distinct from `cargo llvm-cov`, and the workflow does
not export `LLVM_PROFILE_FILE`. The current Python helper's coverage-only
skip at line 165 therefore would **not** have handled the observed fake-server
initialize timeout. The seat ruled **no hosted-runner tier**: keep
`LLVM_PROFILE_FILE` as the sole instrumentation detector, and give setup
readiness its own allowance while retaining the tight ordinary 0.25 s
request-stall contract.

## Watchdogs and screened-out time syntax

These sites impose a maximum wait to keep a broken fixture or stuck operation
from hanging the suite. Their pass/fail predicate is the outcome, stage,
channel signal, or lock state; increasing the watchdog does not test a caller
latency contract. They are H even where the timeout is written with a literal
`Duration`.

| Site at base                                                                                                                                                                                             | H role                                                                                                                                                                                                            |
| -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `crates/khive-db/src/stores/note_tests.rs:828,1042,1214`                                                                                                                                                 | Snapshot/row-step seam arrival; `1214` is the #2348 must-MATCH control and is excluded by held package `3487`.                                                                                                    |
| `crates/khive-db/src/writer_task.rs:1355,1359,1406,3222-3233`                                                                                                                                            | Writer/reply/blocking-worker rendezvous and dequeue setup.                                                                                                                                                        |
| `crates/khive-db/tests/writer_timeout_sink_stalled.rs:43,111,130`, `_writer.rs:79`, `_fifo.rs:65`                                                                                                        | Deadlock watchdogs surrounding the independent caller-latency helper.                                                                                                                                             |
| `crates/khive-db/src/checkpoint.rs:4194,4645,5727,8654,8672,8713,8953,8973,9038,9058,9198,9472,9494,9539,9576`                                                                                           | Aging/churn fixture intervals and checkpoint/heartbeat seam watchdogs.                                                                                                                                            |
| `crates/khive-db/src/reader_lease_tests.rs:97`, `src/stores/blob_uploads.rs:806,813`, `src/sql_bridge.rs:3719,4060`, `src/migrations_tests.rs:3983`                                                      | Progress, release, and cancellation rendezvous; `sql_bridge.rs:4060` uses the configured interrupt grace.                                                                                                         |
| `crates/khive-hnsw/src/alias/manager.rs:984,987,1119,1200,1210,1289`                                                                                                                                     | Concurrent migration/alias-lock rendezvous. `1289` ignores the timeout result; it is setup pacing, not an assertion.                                                                                              |
| `crates/khive-vamana/src/index.rs:5798,5811,5929,5990,6073,6093,6519,6546`                                                                                                                               | Publication-lock seam watchdogs and 10 s allocation-bomb safety caps; the latter only rule out a gross stall.                                                                                                     |
| `crates/khive-pack-knowledge/src/knowledge/vamana.rs:3697`, `src/pack.rs:762`                                                                                                                            | Unblock a held fixture and bound watcher teardown.                                                                                                                                                                |
| `crates/khive-pack-exec/src/handlers.rs:1993,1995`, `crates/kkernel/tests/mcp_closed_stderr.rs:65,107,144`                                                                                               | Independent-thread progress and child protocol watchdogs.                                                                                                                                                         |
| `crates/khive-runtime/src/operations.rs:17934-17958`                                                                                                                                                     | The 60 s outer timeout protects the parked-sibling test; its 5 s elapsed assertion only distinguishes an indefinite wait, so the latter is H. The daemon probe's separate A numeric ceiling is listed above.      |
| `crates/khive-pack-git/tests/support/digest_scale.rs:358,392`                                                                                                                                            | Two `<30 s` ceilings duplicate the enclosing 30 s dispatch watchdog and chiefly prevent a stuck scale fixture.                                                                                                    |
| `crates/khive-mcp/src/components.rs:1389`                                                                                                                                                                | The component should abort a worker that wants to sleep for an hour. `wait_for_state` asserts the unhealthy outcome; `<5 s` only prevents a gross wedge and is not a useful 30 ms shutdown-latency discriminator. |
| `crates/khive-mcp/src/daemon.rs:4228,4303,4384,4405,7785,7912,8124,9159,10144`, `src/daemon/test_harness.rs:97,224`, `src/server.rs:12418`, `src/serve.rs:12982,13258,13407,13735,14746`                 | Daemon readiness and forwarding watchdogs; the daemon's separate caller ceilings are D above.                                                                                                                     |
| `crates/kkernel/tests/supervisor_lifecycle.rs:326,380,414,614,625,639,773,799`, `crates/khive-runtime/tests/adr133_writer_census_crash.rs:53,193,274`, `events_registry_keeps_sqlite_locks.rs:47,59,150` | Child/process readiness and cleanup watchdogs.                                                                                                                                                                    |
| `scripts/tests/test_contract_harness.py:74,92,102,110,145,386,397`, `scripts/tests/test_verify_local_artifact.py:1111-1121,1929-1957,2052,2101,2104`                                                     | Child cleanup, subprocess exit, and artifact polling watchdogs.                                                                                                                                                   |

The screen also found deterministic/configuration uses that are not live
elapsed-time assertions: `khive-types/src/timestamp.rs:122`, lexical-timeout fake-clock telemetry,
backoff/timeout serialization checks, `scripts/tests/test_ci_workflows.py`
workflow timeout strings, and `scripts/tests/test_verify_local_artifact.py:460,467`
which assert a **timeout outcome**, not measured elapsed seconds.
`khive-quant` and `khive-vamana/tests/benchmark.rs` print measurements;
their ignored cost gate uses baseline ratios rather than an absolute wall
ceiling. These do not need the two-tier helper.

## Exclusions and follow-ups

The companion TSV contains 229 distinct paths, each with its owning open PR
or held package. It is the union of the 12 open PRs returned by
`gh pr list -R ohdearquant/khive --state open --json number,files` and the
unmerged manifests for `etxtbsy-credential`, `2025-adr165-slice3-r2`,
`lexical-health-suggest`, `hydration-class`, `2357`, `3465-r6`, `3467-r4`,
`3507-r2`, `k3-r3`, `k3-recipient-restack`, `3487-package1-r10`,
`3487-b-read-r5`, `3327-3324-r6`, `3486-r10`, and `3503`. Including both K3
and both #3487 package lines avoids treating a sibling held packet as free.

The seat explicitly ruled that `crates/khive-mcp/src/daemon.rs` and
`crates/khive-pack-memory/src/handlers/recall.rs` are **census-only follow-ups**
because ETXTBSY, #3465, #3507, and #3487 work is in flight. Do not edit those
files in #2348. Other measured-time follow-ups in the excluded set include
`stores/note_tests.rs` (#2348 control), `khive-mcp/src/server.rs`,
`khive-runtime/src/operations.rs`, `khive-runtime/src/daemon.rs`, and
`khive-pack-comm/tests/integration.rs`. Recheck ownership when those PRs or
packages merge; the exclusion list records the ownership at this base.
