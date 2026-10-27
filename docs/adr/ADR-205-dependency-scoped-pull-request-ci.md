# ADR-205: Dependency-Scoped Pull-Request CI from the Code Map

- **Status:** Proposed
- **Date:** 2026-10-16

## Context

A pull-request run of `ci.yml` costs about 148 runner-minutes (20 completed runs sampled on 2026-10-15: coverage measurement 43, the two nextest shards 64 together, the Windows compile check 12, the core-only build 11, the rest under 20 combined). The hosted runner pool is shared, so the queue, not the suite, sets merge latency: at 17:35Z on 2026-10-16, 26 jobs were running in this repository's runs while 57 runs waited, 30 of them pull-request CI runs whose gates waited on jobs that test crates the pull request cannot have changed.

The workflow already classifies a change before building. `scripts/ci-docs-only.sh` admits a Markdown-only change to the documentation lane, and `scripts/ci-integration-only.sh` lets a pull request from this repository's automation branches (the head-ref prefix that `ci.yml` checks before calling it) skip the build and test jobs when it touches crate source and Markdown only, on the premise that an integration branch tests it later. Both scripts judge paths; neither knows which crates depend on which.

The code pack does. `code.ingest` reads every `Cargo.toml` under a folder and writes `project` entities with `depends_on` edges, each edge carrying the manifest sections it came from (`dependency_kinds`, such as `dependencies` or `dev-dependencies`) and the scopes they map to (`dependency_scopes`: `normal`, `dev` or `build`), into a dedicated map database and never into the production graph (ADR-085). On this workspace it ingests 53 crates and their dependency edges in about 15 seconds. ADR-139 defines the read side, `code.list`, `code.search` and `code.deps` over a registry of named maps; it is Accepted and not yet implemented (#5581).

Measured on the 99 pull requests merged from 2026-10-15T00:00:00Z through 2026-10-16T16:07:45Z (`gh pr list --state merged --limit 400 --search 'merged:>=2026-10-15T00:00:00Z'`, 99 rows under a 400-row limit), using the workspace dependency graph from `cargo metadata`: 78 change crate source only, 6 are Markdown only, and 15 touch a workflow, script, manifest or a path outside any crate. For the 78, the set of crates that either changed or transitively depend on a changed crate (normal, dev and build dependencies) has a mean size of 17.6 of 53 crates and a median of 23. The distribution has two modes: 36 of the 78 sets have at most 10 crates (a pack, a channel, a leaf library), 39 have between 23 and 32 (a change to a shared layer such as `khive-db`, `khive-storage` or `khive-runtime`), none falls between 11 and 22, and 3 have 47 (changes to `khive-types`).

These closure numbers were measured once by the maintainers, from `cargo metadata` and each pull request's changed paths; no committed script reproduces them. The D8 parity test is what makes them re-derivable: once it holds, the closures the map computes are the ones `cargo metadata` computes, and the D9 acceptance numbers are read from CI's own run records.

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

The Classify job evaluates the D3 path rules first, on the list of changed paths alone, before it fetches the binary or runs the ingest. A pull request that changes any D3 path, any `Cargo.toml` included, goes to the full suite without the pack reading its tree. A run that reaches the ingest therefore has manifests identical to those of its base on `main`, and the binary from `main` parses no manifest the pull request wrote. The order holds for pull requests from forks and from this repository alike.

After the D3 rules pass, the Classify job obtains the `kkernel` binary that the newest successful push run on `main` uploaded as an artifact (the `ci` job uploads its debug build under a fixed artifact name, retained seven days). It writes a configuration file that registers one map under the checkout, as ADR-139 requires (`[[code_maps]]` with `name`, `path` and `source_root`, and `code_map_root`), ingests the checkout with `code.ingest(path, languages=["rust"], tiers=["l1"])`, resolves the changed crates to project identifiers with `code.list(kind="project")`, and takes the incoming `depends_on` closure with `code.deps(direction="incoming")`, iterating one depth at a time until the set stops growing so that the verb's depth bound never truncates a closure. The job emits `test_scope`: either `full` or the space-separated crate names, and prints the changed paths, their crates and the closure.

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

The implementation ships a test that computes the reverse-dependency sets of the workspace from `cargo metadata` and from the map written by `code.ingest`, and requires them to be equal, crate by crate and scope by scope: `normal`, `dev` and `build`, read from each edge's `dependency_scopes` and compared with the dependency `kind` that `cargo metadata` reports. The test runs in the full suite, so a `main` push that breaks the ingest reds before any pull request is scoped by a wrong map.

The classifier's own tests carry the ordering arm of D4: a fixture change that touches a D3 path, a `Cargo.toml` among them, must print its full-suite reason and no ingest line, and a fixture change to crate source only must print the ingest line and its closure. A classifier that ingests before it applies D3 fails the first fixture.

An escape is a `main` push run that fails in a crate outside the scope of the pull request that produced it, a full-suite run on `main` that fails in a crate outside the closure of a mechanical range that a scoped push run passed, or a run that fails the coverage ratchet after a scoped pull request merged. The maintainers who merge into `main` own the count: every Monday they read the previous week's run records and post the count, with the run id of each escape, as a comment on a pinned issue titled `CI scope escapes`, which the phase 2 pull request opens. The target is zero, and any escape widens D3 by amendment before scoping resumes for the path class involved. An escape from a mechanical range suspends D3a until an amendment states the widened rule.

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

Phase 1 has landed: the map registry loaded from configuration (#5612) and `code.list`, `code.search` and `code.deps` (#5635), which closed #5581.

Phase 2 has landed in part (#5638): the classifier script and the Classify job's `test_scope` output (D1 to D4), the scoped `ci` job (D5), the gate reading the scope (D6), the debug `kkernel` artifact published by `main` push runs, and the retirement of the integration-only lane (D7). #5717 adds a full-suite rule for a changed file that another crate includes. The escape audit issue is #5637 (D8).

D8's parity test landed in #6123 (`crates/khive-pack-code/src/source_ingest/workspace_parity_tests.rs`) and reached `main` with the merge from `dev/khive` of 2026-10-26; it runs in the full suite. Amendment 3 withdraws D3a, so no decision in D1 to D8 is waiting on code. D9's acceptance has not been read, and the status stays Proposed until it is.

## Amendment 1: Scope integration-branch pushes, and narrow three full-suite triggers (2026-10-18)

**Status**: Proposed (2026-10-18)

### Context

On the 40 newest pull-request runs that had ended by 2026-10-18 21:00 UTC (32 succeeded, 7 were cancelled, 1 failed),
the classifier chose a crate scope for 2, sent 1 to the documentation lane, and ran the full suite for 37. The reasons it
printed for those 37 (six runs printed two): a change to `crates/Cargo.lock` (15 runs; most added one development
dependency to one crate, one bumped workspace versions), `CHANGELOG.md` named by a non-Markdown file (9; the only such
file is a `.tsv` census list under `docs/testing/`), other Markdown named by code (8, correctly full), a crate's own
`Cargo.toml` (6), CI scripts (3), a Rust source file (1) and a verb-count document (1).
Every push to `integration/**` runs the full suite under D3, including a push whose merged pull requests change only
Markdown. Run over the 60 newest commits on `main` up to `fb10dc43`, the classifier with Decisions 2 and 3 below scoped
2 and ran the full suite for 58; both scoped commits changed a dependency entry, and the lockfile reach widened them to
52 and 37 of the 53 workspace members, so in practice Decision 2 scopes manifest edits that change no dependency entry,
such as a new target or bench.

In this amendment the _tested base_ of a push is the newest commit on `main` whose push or scheduled CI run concluded
successfully as a whole, ran the full suite (its recorded scope is `full`, as D3a requires of its range start), passed
Linux test shard 1, and is an ancestor of the pushed head. The documentation lane already looks up this base for pushes
to `main` (`ci.yml`, Classify step). Until D3a's range check is implemented every run on `main` runs the full suite, so
the newest successful run qualifies; once runs on `main` can be scoped, the lookup skips a run whose recorded scope is
not `full`.

### Decision

1. **Integration-branch pushes.** D3's first sentence no longer lists pushes to `integration/**`. Such a push is
   classified like a pull request over the range from its tested base to its head: if every changed path is Markdown the
   documentation lane admits, the push takes that lane; otherwise D2-D4 run on that range and the push runs the closure
   or the full suite. No tested base, a failed lookup, or a head whose ancestry cannot be read puts the push on the full
   suite. Pushes to `main` keep D3 and D3a. D8 counts as an escape a run on `main` that fails in a crate outside the
   scope printed by an integration push whose head has the same tree as the failing commit. The join is by tree, read
   from the head commit of each run, because an integration branch is tested and then its pull requests land one by
   one, so the commit that reaches `main` is usually a different commit with the same tree; a join by tree also covers
   the case where the integration head itself reaches `main`. When more
   than one integration run has that tree, the failure counts as an escape if the crate lies outside the scope any of
   them printed, and the count lists every matching run id with its printed scope. A failing commit whose tree no
   integration head has is counted under D8's existing categories, which remain.
2. **A crate's own manifest.** D3's path list and D4's ordering rule change for one path class: a `Cargo.toml` under a
   crate's manifest directory (the workspace manifest `crates/Cargo.toml` stays a D3 path). Such a change no longer puts
   the run on the full suite by itself, but only when every difference between the base and head copy of that manifest
   is inside a dependency table (`[dependencies]`, `[dev-dependencies]`, `[build-dependencies]`, and their
   `[target.*]` forms) or a target table (`[lib]`, `[[bin]]`, `[[test]]`, `[[bench]]`, `[[example]]`); no dependency entry gains, loses or changes `features`, `default-features` or `optional`; and
   in every target table that differs, each `path`, resolved with every `..` segment and symbolic link
   followed, lies inside the crate's manifest directory and `required-features` is unchanged. A target whose file lies outside the crate, or whose required features change, can
   compile code or enable features the dependency map does not show, so it goes to the full suite.
   `[package.metadata]` is not admitted: tools other than the compiler read it, and this rule audits none of them.
   The classifier reads both copies as data (a TOML parse in the classifier, never the binary). Any other difference,
   including any change to `[features]`, puts the run on the full suite, because a feature a crate enables on another
   package can change that package's build for every crate in a whole-workspace build, which the dependency closure
   does not follow. For the same reason, when the change adds, removes or edits a dependency entry, every workspace member
   whose resolved dependencies in the base or head `Cargo.lock` reach the package that entry names (inside or outside
   the workspace) also counts as changed: a new entry with default features can change that package's unified build.
   When a crate manifest changed, the crate counts as changed, and the binary ingests the base commit's
   tree instead of the head's, so it still parses no manifest the change wrote (D4); the closure is taken over the
   dependency map ingested from that base tree. It contains the head's closure: every dependency edge the change adds starts at a crate whose manifest
   changed, which is itself in the changed set.
3. **The lockfile.** A change to `crates/Cargo.lock` no longer puts the run on the full suite by itself. The classifier
   parses the base and head lockfiles as data and keys each package entry by name, version and source. Every entry that
   exists on one side only, or whose checksum or dependency list differs, is changed. Every workspace member whose
   resolved dependencies in either lockfile reach a changed entry, and every member whose own entry differs, counts as
   changed. A lockfile that does not parse, a change to its `version` header, or a change touching every workspace
   member puts the run on the full suite. Any other `Cargo.lock`, such as a nested crate's own lockfile
   (`crates/khive-merge/Cargo.lock`), stays a D3 path.
4. **Markdown named only by an inert documentation data file.** The documentation-lane check that no non-Markdown file
   names a Markdown file ignores a naming file listed in `scripts/ci-inert-docs-data.txt`. A file may be listed only if
   it is under `docs/`, has a `.tsv`, `.csv` or `.txt` extension, and no test, script or build reads it by name or by a
   directory pattern, other than a test the documentation lane itself runs; the line states why. The workflow test sees
   naming only, so a reader that reaches the file by walking the repository is outside what it checks; the readers that
   walk the repository at the pinned revision are the content sweeps, which the documentation lane runs
   (`scripts/ci.sh:536–545`). The list is under `scripts/`, so a change to it runs the full suite, and a
   workflow test fails when a listed file is outside `docs/`, has another extension, or when any non-Markdown file
   outside `docs/` names the listed file, a directory below `docs/` that contains it (for example `docs/testing`), or a
   pattern over its extension (for example `*.tsv`). A line names the listed file when it holds the file's name. A path
   token is a run of letters, digits and the characters `_ . - / * ? [ ] { } ,`; its pieces are its parts between braces
   and commas, and the parts between commas of each expansion of its brace lists, each without any leading `[`, `./`,
   `../` or `/` or any trailing `]`, period or comma. A line names a directory when a piece is that directory's path
   followed by `/`, `/*` or `/**`, or, for a path of two or more segments, the path alone; a longer path that starts with
   it does not count. A line names a pattern over the extension when a piece's last segment holds `*`, `?` or `[` and
   ends in the extension, such as `*.tsv`, `docs/**/*.tsv` or `*.{tsv,csv}`. Outside `docs/`, `docs/` itself is named
   only by the formatter, which reads only the file types it supports and never a `.tsv`, `.csv` or `.txt` file
   (`deno fmt docs/` and `deno fmt --check docs/` in the `Makefile`, and the `include` list in `deno.json`), by two
   lines of `.github/workflows/ci.yml` that use `docs/` as a branch-name prefix, and by the comment in `scripts/ci.sh` on
   the shipping pack-set copy check, a content sweep the documentation lane runs that reads only `.rs`, `.py`, `.md`,
   `.json` and `.toml` files; the test admits exactly those lines and fails on any other naming of `docs/`. Any other
   naming file still counts.

Every new rule prints the crates it added and why, and every failure in it chooses the full suite.

### Verify by

Each check runs the classifier, or the workflow step, on fixture ranges and fails if the printed scope differs. Each
fixture case below is an arm of its check, and each named mutation must turn the arm it names red.

1. Integration push: Markdown-only commits after the tested base take the documentation lane; the same range with one
   crate source commit runs that crate's closure, and a range of two commits (a source change in one crate, then in
   another) runs both closures. A candidate base whose run concluded with a failure, whose shard 1 failed, or whose
   recorded scope is not `full` is not used. Mutations: compare against the pushed head's parent (the range of two
   commits then runs only the second crate's closure); accept a run by its shard alone (the candidate whose run failed
   is then used); ignore the recorded scope (the scoped candidate is then used).
2. Crate manifest: adding a development dependency on an existing external package runs that crate's closure plus the
   crates whose lockfile dependencies reach the package, and the binary is handed the base tree; editing `[features]`,
   or a dependency's `features`, runs the full suite; a `[[test]]` whose `path` moves outside the crate, or whose
   `required-features` changes, runs the full suite, while a new `[[bench]]` with a path inside the crate runs the
   crate's closure; a `[[test]]` whose `path` is a symbolic link inside the crate to a file outside it, and any change
   under `[package.metadata]`, run the full suite. Mutations: hand the binary the head tree (the fixture's base-tree assertion fails); drop the
   `[features]` rule (the feature arm fails); drop the target-path rule (the outside-path arm fails); drop the `required-features` comparison (the
   changed-required-features arm fails); resolve `path` lexically (the symbolic-link arm fails); admit
   `[package.metadata]` changes (the metadata arm fails).
3. Lockfile: bumping one external package runs the closure of the members that reach it; a changed checksum or
   dependency list alone does the same; two versions of one package where only one changes select only that one's
   reachers; a header change or an unparseable lockfile runs the full suite. Mutations: drop the reverse walk; key
   entries by name alone (the two-version arm fails).
4. Inert data: `CHANGELOG.md` named only by a listed `.tsv` takes the documentation lane; the same file unlisted runs
   the full suite; the workflow test fails when a script outside `docs/` names the listed file, its directory
   (`docs/testing`), `docs/`, or `*.tsv`, including inside a brace, bracket or comma list and after a variable, and
   passes on the repository's own tree, where `docs/testing` occurs only inside longer paths. Mutations: ignore the list
   (the listed arm fails); check only the file's own name (the directory and pattern arms fail); admit every naming of
   `docs/` (the `docs/` arm fails); match a directory as the start of a longer path (the repository-tree arm fails);
   take each path token whole (the list and variable arms fail).
5. A change to `crates/Cargo.toml`, or a crate manifest change outside the admitted tables, runs the full suite.
   Mutation: admit `[package]` changes (the arm editing `[package]` fails).
6. D8 join: on a recorded pair, an integration run and a failing run on `main` whose head commits have the same tree,
   with the failing crate outside the integration run's printed scope, the audit reports one escape; with one file of
   the main commit's tree changed, it reports none in this category; with two integration runs at distinct commits of
   that tree, one whose scope includes the failing crate and one whose scope omits it, it reports one escape naming
   both run ids. Mutations: join by commit instead of tree (the fixture's integration and main commits are distinct
   commits with equal trees, so the pair is then missed); take only the newest matching run (the two-run arm fails when
   the newest one's scope includes the crate).

## Amendment 2: A dependency that appears or disappears, and a symbolic link under `crates/`, run the full suite (2026-10-19)

**Status**: Proposed (2026-10-19)

### Context

Amendment 1 Decision 2 scopes a crate manifest change that adds, removes or edits a dependency entry by counting as
changed every workspace member whose resolved dependencies reach the package the entry names, and Decision 3 scopes a
lockfile change by the members that reach a changed entry. Both read reach from the lockfile, and the lockfile does not
record which features a package requests of its own dependencies. A feature that a newly reached package enables on a
third package changes that third package for every member in a whole-workspace build, and no lockfile entry shows it.

A probe workspace (Cargo 1.98.0, resolver 2) shows the gap. Its members are `m`, `k` and `j`, where `j` depends on `k`.
Two path crates sit outside the workspace: `shared`, with features `default` and `std`, and `foo`, which depends on
`shared` with `features = ["std"]`. `m` depends on `shared`. Adding `foo` to `k`'s `[dependencies]` with no features adds
a `foo` entry to the lockfile and adds `foo` to `k`'s dependency list; `cargo tree --workspace -e features -i shared`
printed the `default` feature alone before the change, and `std` as well after it, enabled through `foo`. The reach walk
counts `k` and `j`, the members that reach `foo`. It does not count `m`, whose tests in a whole-workspace build now
compile `shared` with `std`. In a second probe `foo` moved from version 0.1.0, which requests no feature of `shared`, to
0.2.0, which requests `std`. The only lockfile difference was the `version` line of `foo`'s entry, and `std` was enabled
on `shared` again. Removing an entry is the same change in reverse: the features it enabled leave the unified build.

A symbolic link has a related gap. A changed path resolves to a crate by the location of the path itself, while the
compiler reads the file the link names, which can lie in another crate. A link that did not change also reads the new
contents of a changed file in another crate, and the crate holding the link is then not among the changed crates.
Amendment 1's classifier (#5754) sends a manifest or lockfile that is a symbolic link on either side to the full suite;
crate source has no such rule.

Measured on the 60 newest first-parent commits on `main` up to `850a88b5`: 9 add a dependency entry to a crate manifest
and none removes one. All 9 are benchmark pull requests, and all 9 already ran the full suite because a non-Markdown
file names a Markdown file in the crate. One commit changes the set of lockfile entries, the 0.14.0 release, which bumps
the version of every workspace member and of nothing else; no commit adds or removes an entry for a package outside the
workspace. Amendment 1's classifier (#5754), run with `--paths-only` on each commit, admitted 5 of the 60
past the path rules, and none of the 5 adds or removes an entry, so this amendment changes no admission on that sample;
the 9 benchmark pull requests are the shape it sends to the full suite. The tree at `850a88b5` holds no symbolic link
(`git ls-tree -r`, 0 of 3,970 entries under `crates/`).

### Decision

1. **A dependency entry that appears or disappears.** Amendment 1 Decision 2 no longer admits a crate manifest change
   that adds or removes a dependency entry; such a change runs the full suite. An entry is identified by its table
   (`[dependencies]`, `[dev-dependencies]` or `[build-dependencies]`, at top level or under one `[target.*]` key), the
   package it names (its `package` value when present, otherwise its key) and where that package comes from (its
   `path`, its `git` with any `branch`, `tag` or `rev`, its `registry`, or `workspace = true`). An entry whose identity
   has no match in the other copy of the manifest was added or removed. Moving an entry to another table, another
   target, another path or another source therefore runs the full suite. A change to any other key of an entry whose
   identity matches, such as its version requirement, is an edit, and Amendment 1's lockfile reach still decides which
   members it adds. Amendment 1's rule for `features`, `default-features` and `optional` is unchanged.
2. **A lockfile entry outside the workspace that appears, disappears or changes.** Amendment 1 Decision 3 keeps keying
   lockfile entries by name, version and source. A difference in an entry for a package that is not a workspace member
   runs the full suite: an entry on one side only, which includes a version bump (it removes one key and adds another),
   and an entry whose checksum or dependency list differs. Entries for workspace members keep Amendment 1's reach: a
   member whose own entry differs counts as changed, together with the members that reach it.
3. **Symbolic links under `crates/` (D3).** D3's list gains: a symbolic link (Git mode `120000`) anywhere under
   `crates/` in the base tree or in the head tree, whether or not the link itself changed. The classifier reads modes
   with `git ls-tree` at both commits, as #5754 already does for manifests and the lockfile. Reading the whole tree, and
   not only the changed paths, is what catches an unchanged link whose target changed.

Every new rule prints its reason, and every failure in it chooses the full suite. Amendment 1's Verify by 2 and 3 change
with Decisions 1 and 2: in Verify by 2, the arm that adds a development dependency on an existing external package now
expects the full suite, and the base-tree assertion moves to an arm that edits the version requirement of an existing
entry; in Verify by 3, the bump, checksum, dependency-list and two-version arms concern packages outside the workspace
and now expect the full suite, the header and unparseable arms are unchanged, the name-keying mutation moves to Verify
by 2 below, and the reverse-walk mutation moves to the version-requirement arm of Verify by 1 below.

### Verify by

Each check runs the classifier on fixture ranges and fails if the printed scope or reason differs. Each fixture case is
an arm of its check, and each named mutation must turn the arm it names red.

1. Manifest entries: adding a path package outside the workspace to a crate's `[dependencies]` with no features, adding
   a development dependency on an external package already in the lockfile, removing an entry, changing an entry's
   `path`, and changing an entry's `package` each run the full suite and print the entry; changing only the version
   requirement of an existing entry runs Amendment 1's reach, and the binary is handed the base tree. Mutations:
   identify an entry by its key alone (the `package` arm fails); compare only entries new at head (the removal arm
   fails); leave the source out of the identity (the `path` arm fails); hand the binary the head tree (the
   version-requirement arm's base-tree assertion fails); drop the lockfile reverse walk (the version-requirement arm
   then omits the members that reach the package).
2. Lockfile: a version bump of an external package whose only lockfile difference is its `version` line runs the full
   suite; a new external entry, a removed one, and an external entry whose checksum or dependency list changes under an
   unchanged key each run the full suite; a workspace member's entry whose dependency list changes, with no external
   entry changed, runs the reach of that member. Mutations: key entries by name alone (the bump arm fails, because the
   name is on both sides); apply the rule to workspace members too (the member arm runs the full suite and fails);
   compare only key sets (the checksum and dependency-list arms fail).
3. Symbolic links: a new link in one crate that names a file in another crate runs the full suite; a link that is
   unchanged, in crate `a`, naming a file in crate `b`, with only that file changed, runs the full suite (the closure of
   `b` alone omits `a`); a link present at the base and removed at the head runs the full suite; a range with no link
   in either tree is unaffected. Mutations: read modes of the changed paths only (the unchanged-link arm fails); read
   the head tree only (the removed-link arm fails).

## Amendment 3: Pushes to `main` are never scoped, and D3a is withdrawn (2026-10-27)

**Status**: Accepted 2026-10-27. It does not change the status of the base ADR.

### Context

D3a lets a push to `main` run the dependents closure instead of the full suite when every commit since the newest
full-suite run on `main` is the squash merge of a pull request labeled `mechanical`. Three facts at `main` a789c1b6
make it a rule that never applies:

1. It was never built. The scope step in `ci.yml` scopes pull requests and pushes to `integration/**` only; every other
   run prints "runs on <ref> are never scoped" and takes the full suite. The workflow tests pin that scheduled,
   dispatched and other-branch runs get no comparison base, which leaves the scope at full
   (`test_job_other_events_run_the_full_suite`). The repository has no `mechanical` label.
2. Its input no longer reaches `main`. Feature work, moves and renames land on the `dev/khive` integration branch and
   reach `main` in merge commits from that branch. A range that contains such a merge commit fails D3a's test by
   construction, because a merge commit is not the squash merge of a labeled pull request. The pull requests that still
   squash-merge into `main` directly are dependency updates, releases and fixes: of the twelve newest first-parent
   commits on `main`, eight are squash merges of that kind and four are merges from `dev/khive`, and none is a move.
3. D9 makes it a condition of acceptance ("a mechanical push to `main` whose range spans at least two merges has run
   its closure"). With no mechanical pull request reaching `main`, that condition cannot be met, so this record cannot
   be accepted as written.

No push to `main` is therefore scoped to a closure today. Each runs the full suite, the outcome D3a falls back to, unless
every path changed since its tested base is Markdown that the documentation lane admits; such a push takes that lane,
which D3a never changed (Consequences, Neutral: "`docs_only` and the documentation lane are unchanged").

### Decision

1. D3a is withdrawn. No push to `main` is scoped to a dependents closure, and D3's first sentence reads "on every push
   to `main`" with no exception. A push to `main` runs the full suite unless every path changed since its tested base is
   Markdown that the documentation lane admits (D2), in which case it takes that lane, as it did before this amendment.
2. Runs other than pull requests keep recording the scope their suites ran (`test-scope-full`, `test-scope-scoped`,
   `test-scope-docs`), and Amendment 1's tested-base lookup keeps reading that record. Every successful run on `main`
   records `full`, or `docs` for a push that took the documentation lane; none records `scoped`.
3. D8's escape definition loses its mechanical-range clause and its last sentence. An escape remains a `main` push run
   that fails in a crate outside the scope of the pull request that produced it, the integration-push case Amendment 1
   adds, or a run that fails the coverage ratchet after a scoped pull request merged.
4. D9 loses the mechanical-range condition. The rest of D9 stands: 20 consecutive pull-request runs with runner-minutes
   read beside the baseline, the parity test green on `main`, one deliberately scoped run refused by the gate, and no
   escape.
5. Phase 2 no longer includes the D3a range check.

If code moves reach `main` again as direct squash merges, a later amendment can restore a range rule. It counts toward
acceptance only once the label exists and the range check is built.

### Verify by

1. Every successful push run on `main` that carries a scope record lists `test-scope-full` or `test-scope-docs` among
   its artifact names, and none lists `test-scope-scoped`. Read on 2026-10-27 over the 49 successful push runs on
   `main` since #5638 merged (2026-10-17): 33 list `test-scope-full`, 1 lists `test-scope-docs` (run 39378323211 at
   2caac4ad), none lists `test-scope-scoped`, and the 15 with no record all ran before the first recorded run
   (39231651652, 2026-10-19). Control, run in the same read: an integration push run whose scope was a closure lists
   `test-scope-scoped` (run 39843379071, `integration/train-193`, does), so the read can tell them apart.
2. Two workflow tests stay green. `test_a_main_push_is_never_scoped_even_with_a_tested_base`: the Scope step keeps
   `full` for a push to `refs/heads/main` that has a tested base and crate changes, and prints why.
   `test_job_main_push_compares_with_the_newest_fully_tested_main_commit`: the Classify step sends such a push to the
   documentation lane when only Markdown changed since its tested base, and to the full suite when code did. Mutation:
   let the Scope step take the integration branch's path for `refs/heads/main`; the first test fails and the second
   stays green. (`test_job_other_events_run_the_full_suite` gives no comparison base to any of its runs, so a push to
   `refs/heads/main` added there would pass without reaching either decision.)
