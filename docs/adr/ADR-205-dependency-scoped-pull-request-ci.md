# ADR-205: Dependency-Scoped Pull-Request CI from the Code Map

- **Status:** Proposed
- **Date:** 2026-10-16

## Context

A pull-request run of `ci.yml` costs about 148 runner-minutes (20 completed runs sampled on 2026-10-15: coverage measurement 43, the two nextest shards 64 together, the Windows compile check 12, the core-only build 11, the rest under 20 combined). The hosted runner pool is shared, so the queue, not the suite, sets merge latency: at 17:35Z on 2026-10-16, 26 jobs were running in this repository's runs while 57 runs waited, 30 of them pull-request CI runs whose gates waited on jobs that test crates the pull request cannot have changed.

The workflow already classifies a change before building. `scripts/ci-docs-only.sh` admits a Markdown-only change to the documentation lane, and `scripts/ci-integration-only.sh` lets a pull request from this repository's automation branches (the head-ref prefix that `ci.yml` checks before calling it) skip the build and test jobs when it touches crate source and Markdown only, on the premise that an integration branch tests it later. Both scripts judge paths; neither knows which crates depend on which.

The code pack does. `code.ingest` reads every `Cargo.toml` under a folder and writes `project` entities with `depends_on` edges, each edge carrying its dependency kind (normal, dev or build) and scope, into a dedicated map database and never into the production graph (ADR-085). On this workspace it ingests 53 crates and their dependency edges in about 15 seconds. ADR-139 defines the read side, `code.list`, `code.search` and `code.deps` over a registry of named maps; it is Accepted and not yet implemented (#5581).

Measured on the 99 pull requests merged from 2026-10-15T00:00:00Z through 2026-10-16T16:07:45Z (`gh pr list --state merged --limit 400 --search 'merged:>=2026-10-15T00:00:00Z'`, 99 rows under a 400-row limit), using the workspace dependency graph from `cargo metadata`: 78 change crate source only, 6 are Markdown only, and 15 touch a workflow, script, manifest or a path outside any crate. For the 78, the set of crates that either changed or transitively depend on a changed crate (normal, dev and build dependencies) has a mean size of 17.6 of 53 crates and a median of 23. The distribution has two modes: 36 of the 78 sets have at most 10 crates (a pack, a channel, a leaf library), 39 have between 23 and 32 (a change to a shared layer such as `khive-db`, `khive-storage` or `khive-runtime`), none falls between 11 and 22, and 3 have 47 (changes to `khive-types`).

## Decision

### D1. The unit of selection is the dependents closure

For a `pull_request` run, the set of crates under test is the set of changed crates together with every crate that depends on one of them transitively, following `depends_on` edges of every kind. A dev-dependency edge is followed because the dependent's tests compile the changed crate; a build-dependency edge is followed because the dependent's build script does. A crate outside this set compiles the same source against the same dependencies as on `main` and is not rebuilt or tested by the pull-request run.

### D2. Paths resolve to crates through the map's project roots

A changed path resolves to the project whose manifest directory is its longest prefix. A Markdown path resolves to no crate and is admitted under the existing documentation-lane rule, including its check that no non-Markdown file names the Markdown file. A non-Markdown path under no project root puts the run on the full suite.

### D3. The full suite runs whenever scoping cannot be shown safe

The full suite runs on every push to `integration/**`, on every push to `main` other than a mechanical range under D3a, on every scheduled and dispatched run, and on a pull request whenever any of the following holds:

- a changed path is a workflow or local action under `.github/`, a script under `scripts/`, the `Makefile`, a `Cargo.toml` or `Cargo.lock` at any depth, a toolchain file, a `deny.toml`, `rustfmt.toml` or `clippy.toml`, anything under `crates/.cargo/` or `crates/.config/`, or anything under `python/`, `apps/`, `tests/` or `benches/` (the allow-list `scripts/ci-integration-only.sh` already states);
- the head has a submodule;
- the classifier cannot obtain a binary, the ingest reports a blocked path or a warning, a changed crate is not a project in the map, the closure is empty while a non-Markdown path changed, or any classifier step fails or times out.

Every refusal prints its reason. The classifier can only fail toward the full suite.

### D3a. A mechanical range on `main` runs its closure

A push to `main` runs the dependents closure instead of the full suite when every commit between the head of the newest successful full-suite run on `main` (push or scheduled) and the pushed head is the squash merge of a pull request labeled `mechanical`, and the combined diff of that range passes D3 as a pull request would. Mechanical means a verbatim move of code between files with its move proof, an import or path update that the move requires, or a rename with no other change. The maintainer who merges such a pull request applies the label after its move proof and its own crate-scoped run passed. The label is a claim and the path check is the guard: a labeled pull request that touches a D3 path still puts the push on the full suite.

Every run on `main` uploads its `test_scope` as an artifact, and the range starts at the head of the newest successful run whose recorded scope is `full`. The classifier resolves each commit in the range to its pull request through `GET /repos/{owner}/{repo}/commits/{sha}/pulls`. A commit with no pull request, a pull request without the label, a range it cannot list, or the absence of any successful full-suite run on `main` puts the push on the full suite.

The range sets the scope, not the newest commit alone. A newer push to `main` supersedes an earlier run that has not completed (the concurrency rule in `ci.yml`), so the superseded commits reach CI only inside a later run; scoping that run by its own commit would leave their crates untested. The daily scheduled run stays on the full suite, and each successful one resets the range.

### D4. The classifier runs the code pack in the Classify job

The Classify job obtains the `kkernel` binary that the newest successful push run on `main` uploaded as an artifact (the `ci` job uploads its debug build under a fixed artifact name, retained seven days). It writes a configuration file that registers one map under the checkout, as ADR-139 requires (`[[code_maps]]` with `name`, `path` and `source_root`, and `code_map_root`), ingests the checkout with `code.ingest(path, languages=["rust"], tiers=["l1"])`, resolves the changed crates to project identifiers with `code.list(kind="project")`, and takes the incoming `depends_on` closure with `code.deps(direction="incoming")`, iterating one depth at a time until the set stops growing so that the verb's depth bound never truncates a closure. The job emits `test_scope`: either `full` or the space-separated crate names, and prints the changed paths, their crates and the closure.

The binary comes from `main`, not from the pull request. The pack reads manifests only, so a change to the pack itself is tested by the run that changes it, through D3 (a change under `crates/khive-pack-code/` puts that crate and its dependents under test) and through the parity control in D8.

### D5. Scoped jobs run the closure, nothing else

When `test_scope` is a crate list:

- the two `ci` shards run `cargo nextest run --no-fail-fast --partition count:<i>/2` with one `-p` per crate, then `cargo test --doc` for the same crates;
- clippy runs with `--all-targets --all-features -D warnings` for the same crates;
- the feature phases of `scripts/ci.sh` (`khive-mcp` channel features, `khive-retrieval` executor, `khive-fold` without default features) run when their crate is in scope;
- `cargo fmt --check` keeps its workspace form, and the workspace compile guard becomes `cargo check --workspace --all-targets --locked`. A `-p` selection unifies features over the selected packages only, so a test, bench or example target that compiles only under whole-workspace feature unification would be invisible both to the scoped run and to today's lib-and-bin check (`cargo check --workspace --locked` in the `lockfile` phase of `scripts/ci.sh`). Checking every target of every crate closes that class for the cost of type-checking the test targets, without code generation for them;
- the coverage measurement and coverage ratchet are skipped: a coverage number over a subset is not comparable with the `main` baseline the ratchet reads, and coverage keeps running on every full-suite run on `main`, the daily scheduled run included. A coverage-ratchet failure therefore first appears on `main` after the merge. The merging maintainer fixes it forward on `main` (a test, or a ratchet update with its reason), and the failure counts as an escape under D8.

The Windows compile check, the MSRV check, the core-only build, `wasm-parity`, `vamana-portability` and `minio-blob-compat` keep their current form in this phase. Each is a compile-level or single-crate guard; scoping them is a later amendment with its own measurement.

### D6. The gate reads the scope

`ci-gate` reads `test_scope` beside `docs_only`. The coverage jobs may be skipped only when the Classify job succeeded with a crate-list scope; every job that ran must be green; any other skip fails the gate, as today. The `ci` job prints the `-p` list it ran as its first step, so a run's log states what it tested.

### D7. The integration-only lane is retired

A pull request from an automation branch no longer skips its build and test jobs. Its own run tests the dependents closure like any other pull request. `scripts/ci-integration-only.sh` is deleted with the lane; its path allow-list moves into the new classifier script as the D3 list.

### D8. Parity control and escape audit

The implementation ships a test that computes the reverse-dependency sets of the workspace from `cargo metadata` and from the map written by `code.ingest`, and requires them to be equal, crate by crate and dependency kind by dependency kind. The test runs in the full suite, so a `main` push that breaks the ingest reds before any pull request is scoped by a wrong map.

An escape is a `main` push run that fails in a crate outside the scope of the pull request that produced it, a full-suite run on `main` that fails in a crate outside the closure of a mechanical range that a scoped push run passed, or a run that fails the coverage ratchet after a scoped pull request merged. Escapes are counted from the run records weekly; the target is zero, and any escape widens D3 by amendment before scoping resumes for the path class involved. An escape from a mechanical range suspends D3a until an amendment states the widened rule.

### D9. Acceptance

The change is accepted when, on 20 consecutive pull-request runs after it lands, the runner-minutes per run are read from the job records the same way as the baseline and reported beside it, the parity test is green on `main`, the gate has refused at least one deliberately scoped run whose closure test failed, a mechanical push to `main` whose range spans at least two merges has run its closure, and no escape has been recorded.

## Phases

1. ADR-139 implemented (#5581; the ADR was accepted on 2026-10-16): the map registry loaded from configuration, `code.list` and `code.search`, and `code.deps`, each with the fixture-map tests that issue names.
2. The classifier script and the Classify job output, the `ci` job wiring, the gate change, the artifact upload on `main` and the D3a range check for `main` pushes, behind D3 so that a classifier defect reads as the full suite.
3. Later, by amendment: file-level scope from the L1.5 import map for crates whose test targets are independent, and scoping of the compile-level guard jobs.

## Alternatives considered

### Read the dependency graph from `cargo metadata` in the classifier

`cargo metadata --no-deps --offline` answers the same question in a fraction of a second and needs no binary. It is kept as the oracle in the D8 parity test and rejected as the mechanism: the map is the dependency authority the repository maintains for its own code, and a consumer that reads it in every pull request is the check that keeps it correct.

### Commit a generated crate graph file

A checked-in graph avoids fetching a binary, but it is a derived artifact that goes stale the moment a manifest changes, and the guard that would catch the drift needs the same binary. Rejected; recorded as a fallback if the artifact fetch proves unreliable.

### Download a map that the `main` run precomputed

Under D3 every scoped run has manifests identical to `main`'s, so a map ingested by the newest green `main` run and uploaded beside the binary describes the same graph as one ingested in the Classify job. Downloading it would skip the ingest. The in-run ingest is kept because it exercises the pack on every pull request, which is the reason the pack is the mechanism; the precomputed map is the second fallback, ahead of a committed graph.

### Select tests with nextest filtersets by changed file

Finer than crates, but a filterset cannot see that a changed function is called from another crate's test, so it needs the import map first. Deferred to phase 3.

### Keep the integration-only lane

It costs nothing on the pull request and tests nothing there. A dependency-scoped run costs the closure and tests it; the measured mean closure is 17.6 of 53 crates.

## Consequences

### Positive

A crate-only pull request tests 17.6 of 53 crates on average and skips coverage, which is 43 of today's 148 runner-minutes; the test shards, the largest remaining cost, shrink with the closure, and the queue clears in proportion. Pull requests in the small mode (36 of 78 measured) test at most 10 crates. The full suite still runs on every `main` push outside a mechanical range and on the daily scheduled run, so `main` reaches a complete verdict at every other merge and at least once a day. A burst of mechanical merges no longer holds runners for a full suite per merge. The code pack gains a consumer that fails the build when its map is wrong.

### Negative

The classifier depends on a binary built by an earlier `main` run; when none is available the run is full and slower, never wrong. A mechanical range is verified only for its closure until the next full-suite run; a defect outside the closure surfaces there and counts as an escape. ADR-139 must be accepted and implemented first. The gate grows a third lane and its allowed-skip list, which the gate's control test must keep covering.

### Neutral

`docs_only` and the documentation lane are unchanged. Release and scheduled runs are unchanged.

## Implementation Status

Proposed; nothing implemented. Phase 1 is tracked by #5581.
