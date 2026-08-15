# ADR-158: Cargo Source-Role Projections for Repository Showcase

**Status**: proposed\
**Date**: 2026-08-15\
**Authors**: khive maintainers\
**Depends on**: [ADR-085](ADR-085-code-pack.md),
[ADR-147](ADR-147-repo-showcase-bundle.md)\
**Tracks**: [issue #1976](https://github.com/ohdearquant/khive/issues/1976)

## Context

ADR-147's accepted `khive.repo.v1` bundle deliberately captures every Rust module produced by
`code.ingest`. That is the honest raw view, but it mixes Cargo source sets. Dependency fan-in,
strongly connected components (SCCs), hotspots, hidden coupling, ownership, and de-facto API
rankings therefore include integration tests, benchmarks, examples, and build scripts alongside
crate sources.

This is not a hypothetical distinction. Repository-showcase dogfooding in PR #1938 found a
high-ranked module whose captured dependents included six test modules. PR #1971 consequently
ships a source-role caveat rather than calling its hidden-coupling rank a production-architecture
fact. The checked v1 golden makes the scale visible too: it contains 658 Rust modules, and a
diagnostic directory census finds 106 paths under package `tests/`, 24 under `benches/`, one under
`examples/`, and one package-root `build.rs`. Those directory counts are evidence of the problem,
not classifier output: v1 has no role field, and paths alone do not account for custom Cargo target
declarations.

The current producer already has the inputs needed to do better. ADR-085 Amendment 5 requires every
L1.5 module to carry `source_project`, repository-relative `source_path`, and the pinned
`source_revision`. The ADR-147 exporter opens the pinned clone, parses tracked `Cargo.toml` files,
chooses the deepest governing package, and reconciles those facts with the code-map database. The
browser has none of that trusted context and must not recreate it with TypeScript regular
expressions.

Filtering the existing wire after serialization is also incorrect. Every page is bounded, and the
v1 golden's hidden-coupling page contains 1,000 of 104,263 candidates. Removing auxiliary rows from
that page cannot recover an in-scope row that ranked below the bound. More subtly, SCC membership,
fan-in, hotspot quadrants, and de-facto API ranks change when nodes and edges leave the graph. They
must be recomputed from a selected source scope, not filtered from already-computed output.

Independent bounds create a second presentation hazard. A structure edge, SCC, ownership row, or
history-navigation facet can survive its own page bound while the module/package/commit it references
is absent from a separately bounded node page. A strict browser then has no object to label or
inspect. V2 must therefore preserve full-scope computation while also closing every serialized
reference at the final presentation boundary.

## Decision

### D1 — Classification belongs to the exporter, not the ingest store or browser

`khive-repo-showcase` owns a deterministic Cargo source-role classifier over the same pinned clone
and full code-map rows it already reconciles. Classification happens during export, after the
existing source-revision and map-coverage checks and before any bundle page or aggregate bound.

`code.ingest` remains history-preserving and source-role-neutral. ADR-085 module identity and
properties do not change: no role is written back to the code-map database, and no module or edge is
deleted. A later consumer that needs roles outside the showcase can propose moving a shared
classifier upstream; this tranche does not invent that demand.

The TypeScript consumer reads the emitted role and projection. It MUST NOT classify or repair a role
from `source_path`, module name, package name, or import shape. A missing, unknown, or contradictory
producer value fails strict bundle validation; it does not trigger a client fallback.

### D2 — One closed role enum, with deliberately limited meaning

Every emitted module has exactly one `source_role` from this closed set:

| Role                      | Meaning at the pinned repository snapshot                                             |
| ------------------------- | ------------------------------------------------------------------------------------- |
| `crate_source`            | A Cargo library/binary source-set path or an explicitly declared lib/bin target root  |
| `integration_test_source` | A package integration-test source-set path or an explicitly declared test target root |
| `benchmark_source`        | A package benchmark source-set path or an explicitly declared benchmark target root   |
| `example_source`          | A package example source-set path or an explicitly declared example target root       |
| `build_script`            | The package's enabled default or explicitly configured build-script root              |
| `unclassified`            | The exporter cannot assign exactly one of the five known roles                        |

These are **source-set roles, not reachability claims**. In particular:

- `crate_source` does not mean runtime-reachable, deployed, public, non-test, or production. A file
  under `src/` may be feature-gated, platform-gated, dead, generated, or used only from a test
  configuration.
- A file such as `src/stores/graph_tests.rs` remains `crate_source`; names and `#[cfg(test)]` are not
  Cargo source-set evidence. The reduced projection excludes known auxiliary source sets, not every
  line of test-only code.
- `integration_test_source` describes the Cargo integration-test source set; it does not prove the
  test is discovered, enabled, or executed.
- The classifier does not run Cargo, expand `cfg`, resolve features, follow `mod`/`#[path]`, or build
  a call graph. Supporting files outside the conventional source-set directories stay
  `unclassified` unless they are themselves an explicit target root.

User-facing text and evidence exports MUST use the role names or the phrase "known auxiliary
excluded." They MUST NOT call either projection "production," "runtime," "shipping," or an
equivalent reachability label.

### D3 — Classifier `khive.cargo_source_role.v1`

The bundle records the classifier identifier `khive.cargo_source_role.v1`. Its rules are evaluated
in this order:

1. Every module's `source_revision` must equal the bundle HEAD under ADR-147's existing guard. The
   exporter parses every tracked `Cargo.toml` with a `[package].name`, using the existing safe
   manifest-path, regular-file, repository-containment, UTF-8, and 1 MiB limits.
2. Build a repository-wide map of explicit target-root proposals. A role-bearing `path` value MUST
   be a non-empty TOML string using `/` separators. The parser rejects backslashes, NUL, empty path
   components, leading `/`, Windows drive/UNC/device prefixes, and any other host-absolute form.
   Starting at the manifest directory, it removes `.` components and resolves `..` components
   lexically; escaping the pinned repository is an export error. This grammar is applied identically
   on every producer host and never delegates interpretation to the host `Path` rules. Exact roots
   from `[lib].path` and `[[bin]].path` propose `crate_source`; `[[test]].path`, `[[bench]].path`, and
   `[[example]].path` propose their corresponding roles. `[package].build = "..."` proposes
   `build_script`; when the key is absent, a tracked package-root `build.rs` proposes
   `build_script`; `build = false` disables that default. A missing target-table `path` contributes
   no exact proposal and falls through to conventions. A present target `path` of the wrong TOML
   type, an empty string, an unsafe path, or `[package].build` with a value other than a non-empty
   string or `false` is a contextual export error, never an ignored hint. This repository-wide exact
   map allows a declared target root outside its package directory but still inside the pinned
   repository to be classified without guessing ownership from containment.
3. For a Rust module, compare its exact repository-relative `source_path` against proposals from all
   tracked packages. More than one distinct exact-role proposal is `unclassified`; the exporter
   never chooses a role by arbitrary precedence. Repeated declarations of the same role are not a
   conflict. One exact proposal normally wins before directory conventions. There is one
   conservative cross-package guard: when every exact proposal comes from outside the deepest
   governing package and its role differs from that package's recognized D3.4 directory role, the
   result is `unclassified`. This prevents one package's `../other-package/src/lib.rs` target from
   silently relabelling another package's crate source. An outside-package target with no conflicting
   governing convention, or with the same role, keeps its exact role. Classification does not
   rewrite ADR-085 `source_project`; the role enum is not an ownership field.
4. With no exact proposal, select the deepest containing package root, then classify by the first
   component below that root:
   `src/` is `crate_source`, `tests/` is `integration_test_source`, `benches/` is
   `benchmark_source`, and `examples/` is `example_source`. These directory roles include helper
   modules below the source set; Cargo auto-target flags do not turn such paths into crate sources.
5. If D3.3 did not assign an exact role, a Rust module with no governing package, a path outside the
   D3.4 conventions, an unsupported-language module, or any other unresolved case is `unclassified`
   and remains in the bundle.

Malformed or unsafe manifest inputs that the current ADR-147 exporter rejects remain export errors;
D3 additionally rejects the malformed role-bearing values enumerated above. Normal absence, an
unrecognized layout, or conflicting target-role evidence is not an export error; it is visible
`unclassified` coverage. This distinction preserves the existing security boundary without turning
incomplete semantic knowledge into data loss.

### D4 — Bundle-wide role coverage is conserved and auditable

`khive.repo.v2` adds `meta.source_roles`:

```json
{
  "classifier": "khive.cargo_source_role.v1",
  "classified_revision": "<40 lowercase hex HEAD>",
  "classified_languages": ["rust"],
  "total_modules": 0,
  "known_role_modules": 0,
  "unclassified_modules": 0,
  "counts": {
    "crate_source": 0,
    "integration_test_source": 0,
    "benchmark_source": 0,
    "example_source": 0,
    "build_script": 0,
    "unclassified": 0
  }
}
```

The numbers above illustrate the shape only; a regenerated golden supplies the measured values.
The wire contract requires all six count keys even when a count is zero, and enforces these
invariants before serialization:

```text
sum(counts.*) == total_modules
counts.unclassified == unclassified_modules
total_modules - unclassified_modules == known_role_modules
classified_revision == meta.snapshot.head_sha
```

For classifier v1, `classified_languages` is the exact array `["rust"]`, not an open language list.
A future language classifier requires an amendment, a new classifier identifier, and new measured
fixtures.

Counts describe the full, unpaginated captured module set. They are not inferred from either
projection's bounded module page. Unsupported languages count as `unclassified` until an amendment
adds a named classifier and measured rules for them.

This coverage classifies captured rows only. It does not upgrade
`meta.ingest.code_ingest`, structure-edge, or history-join coverage: an incomplete or unavailable
producer input keeps the same unavailable/truncated analysis disclosures in both projections even
when every captured row has a role.

### D5 — Two closed, producer-owned projections

The projection enum is closed to:

| Projection                 | Included roles                 | Excluded roles                                                                  |
| -------------------------- | ------------------------------ | ------------------------------------------------------------------------------- |
| `all_captured`             | all six roles                  | none                                                                            |
| `known_auxiliary_excluded` | `crate_source`, `unclassified` | `integration_test_source`, `benchmark_source`, `example_source`, `build_script` |

`all_captured` is the default and preserves ADR-147's current meaning. The reduced projection keeps
`unclassified` deliberately: excluding unknown evidence would silently turn a conservative filter
into a production claim.

The included/excluded role arrays serialize in the D2 enum order and MUST equal the table above;
they are not caller-configurable sets. They are disjoint and their union is the six-role enum.
`all_captured.module_count == meta.source_roles.total_modules`, and
`known_auxiliary_excluded.module_count == counts.crate_source + counts.unclassified`.
The value of each projection's `scope` field MUST equal its containing object key.

ADR-147 requires renderers to obtain labels from capability data. V2 therefore adds these closed
fields rather than hardcoding new copy in the browser:

```json
{
  "source_projections": {
    "supported": ["all_captured", "known_auxiliary_excluded"],
    "default": "all_captured"
  },
  "labels": {
    "source_projections": {
      "all_captured": "All captured sources",
      "known_auxiliary_excluded": "Known auxiliary excluded"
    },
    "source_roles": {
      "crate_source": "Crate source",
      "integration_test_source": "Integration-test source",
      "benchmark_source": "Benchmark source",
      "example_source": "Example source",
      "build_script": "Build script",
      "unclassified": "Unclassified source"
    }
  }
}
```

All arrays and maps above serialize in D2/D5 enum order and contain every and only the declared
keys **and exact string values**. In particular, the reduced projection label is fixed to **Known
auxiliary excluded**; producer-supplied replacement copy such as "Production" is invalid. Changing
any label requires an ADR amendment and a wire revision rather than an unreviewed content change.

V2 factors scope-independent repository/history evidence from two producer-computed source graph
and aggregate projections:

```text
khive.repo.v2
├── meta (including source_roles)
├── graph
│   ├── shared
│   │   ├── repository + packages
│   │   ├── commits + issues + pull_requests + history_edges
│   │   └── commit_module_edges + history_navigation + join_resolution
│   └── source_projections
│       ├── all_captured
│       │   ├── scope + included_roles + excluded_roles + module_count
│       │   └── modules + functions + datatypes + interfaces + structure_edges
│       └── known_auxiliary_excluded
│           ├── scope + included_roles + excluded_roles + module_count
│           └── modules + functions + datatypes + interfaces + structure_edges
├── aggregates
│   ├── repository
│   │   └── cadence_timeline
│   └── source_projections
│       ├── all_captured
│       │   ├── scope + included_roles + excluded_roles + module_count
│       │   └── scope-keyed ADR-147 aggregates
│       └── known_auxiliary_excluded
│           ├── scope + included_roles + excluded_roles + module_count
│           └── scope-keyed ADR-147 aggregates
└── capability
```

Each graph projection owns independently bounded module, symbol, and structure-edge pages computed
from that projection's full selected set. `module_count` is its full pre-bound count, and its module
page's available `total_count` MUST equal that number. Every serialized module role belongs to the
projection's exact `included_roles`. Scope-independent structure edges with no module endpoint remain
in both projections. An edge with one or more module endpoints survives only when every such endpoint
belongs to the selected full module set. Every full pre-bound edge endpoint resolves in the union of
shared graph nodes and that same-keyed projection's nodes, and non-module-only structure edges are
byte-equivalent across projections. Functions, datatypes, and interfaces remain explicitly
unavailable while the ADR-147 symbol tier is deferred, but locating those pages inside the
projection prevents a future symbol implementation from reintroducing cross-scope rows.

Full-scope integrity is not enough when node, edge, navigation, and aggregate pages have independent
bounds. V2 therefore makes every bounded collection, including `SymbolPage` and nested navigation
pages, reference-closed at its presentation boundary. Each page adds this required availability:

```json
{
  "presentation_counts": {
    "status": "available",
    "value": {
      "reference_eligible_count": 0,
      "reference_omitted_count": 0,
      "bound_omitted_count": 0
    }
  }
}
```

When source coverage makes `total_count` unavailable, `presentation_counts` is unavailable with the
same reason. Otherwise these invariants hold:

```text
reference_eligible_count + reference_omitted_count == total_count.value
items.length + bound_omitted_count == reference_eligible_count
truncated == (reference_omitted_count > 0 || bound_omitted_count > 0)
next_cursor != null iff bound_omitted_count > 0
```

`total_count` continues to mean the full pre-presentation candidate count. A candidate is reference
eligible only when every typed ID reference other than its own defining `id` resolves in the
serialized shared nodes and, for a scope-keyed candidate, the same-keyed projection nodes or derived
analysis rows. Nested pages account for their own references. SHA strings, paths, and other evidence
values that are not typed object IDs are not references. A reference-free page has
`reference_omitted_count == 0`. The disclosure reason names both omission counts when either is
nonzero; a page is `complete` only when both are zero.

The producer preserves the full-scope sort/rank order, removes reference-ineligible candidates, then
applies that page's bound. This is a presentation operation only: it does not recompute thresholds,
SCCs, ranks, totals, or metric values from serialized nodes. It may expose a later full-scope row
after an earlier row was omitted for a missing serialized reference, while preserving the row's
full-scope value and the typed omission disclosure.

The shared graph is explicitly all-captured historical evidence, not a third projection. It keeps
repository/package/history nodes, commit-to-module edges, history navigation, and join coverage once.
Its serialized edges and navigation pages are reference-closed against serialized shared history
nodes plus the `all_captured` projection nodes; their `total_count` and presentation counts disclose
any all-captured evidence omitted by those node pages. A browser may further restrict a shared edge
or navigation facet mechanically to module IDs present in the selected projection; that is membership
application, not classification. A complete scoped-history claim requires complete shared evidence
and both complete `all_captured` and selected projection module pages. The browser MUST NOT inspect a
path or use shared history rows to recompute producer-owned metrics.

Each source projection carries the module-dependent ADR-147 analyses: dependency topology and SCCs,
hotspot quadrant, hidden coupling, structure treemap, module ownership, de-facto API surface, and
scorecard. Repository cadence is serialized once. To preserve the existing self-contained ownership
analysis shape, each projection's ownership record retains its repository author concentration, bus
factor, and author page. Those repository-only subfields MUST be byte-equivalent across the two
projections; only ownership module rows are recomputed.

Shared history evidence and each projection-local graph page keep independent bounds and
disclosures. Join-resolution coverage continues to describe the all-captured ingest/join.
Projection metadata and `meta.source_roles` disclose role membership; neither rewrites or upgrades
ingest, join, commit-edge, or navigation coverage.

The Rust producer/model and TypeScript strict parser MUST run semantic validation in addition to
closed object/enum validation. They reject a classifier or classified-language value other than the
fixed D3/D4 values, a projection whose `scope` differs from its key, role arrays that differ in value
or order from the D5 table, supported/default capability that differs from the two fixed projections,
label keys or string values that differ from the fixed D5 fragment, or module counts that contradict
D4/D5. The graph and aggregate records for the same key MUST also carry byte-equivalent projection
metadata, and both ownership projections MUST carry byte-equivalent repository-only subfields. Plain
deserialization without this validation is not a valid producer or consumer entry point. Validation
also enforces every presentation-count equation and rejects every dangling serialized ID reference,
including package/module ownership, graph-edge endpoints, nested navigation IDs, SCC membership and
topology cycle IDs, and every module/package/symbol ID in an aggregate. The generated JSON Schema
closes all objects, enum values, fixed keys, fixed arrays, and availability shapes and uses constants
for the classifier, classified-language array, and eight label strings. The runtime validators
enforce reference resolution, arithmetic, and cross-section equalities that JSON Schema cannot
express.

This factoring is a v2 contract, not an optional encoding optimization. The checked 5.6 MiB v1
golden is already close to the browser's 8 MiB limit. V2 duplicates the much smaller module/symbol/
structure slice and module-derived aggregates, but not the v1 golden's 2.1 MiB commit-to-module edge
page or 1.3 MiB history-navigation page. Phase 1 MUST keep the canonical v2 golden at or below the
existing 8 MiB browser limit without raising that limit in the source-role tranche.

### D6 — Scope first, compute second, reference-close and paginate last

The producer evaluates each projection independently from full pre-bound evidence:

1. classify the complete module set;
2. select the projection's modules;
3. project the structure-edge and commit-to-module working sets from the complete edge sets;
4. recompute dependency fan-in/fan-out and SCCs, hotspot thresholds and rows, hidden-coupling pairs,
   module ownership, structure treemap rows, de-facto API ranks, and every module-derived scorecard
   field;
5. build serialized ID registries in dependency order: the shared repository/package/history node
   pages first, then each projection's module pages (eligible only when `package_id` is serialized),
   then symbol pages (eligible only when `module_id` is serialized), then derived SCC rows and every
   remaining referenced page; and finally
6. preserve each full-result order, remove candidates whose typed references do not resolve, and
   apply that page's bound with the D5 presentation counts. Shared history evidence is reference-
   closed and bounded separately from its unchanged all-captured working set.

Filtering a bounded `all_captured` page, reusing its SCC identifiers, or retaining its hotspot
thresholds is a contract violation. Hidden-coupling direct-edge exclusion uses the complete scoped
dependency edge set before its edge page is bounded. Its support denominator remains the same full
repository commit window defined by ADR-147; the projection changes eligible module pairs, not what
a repository commit means. A consumer may state that no direct edge was captured only when that
projection's serialized structure-edge page is complete; otherwise absence remains unknown even
though the producer used the full scoped edge set to rank hidden coupling.

Repository-only cadence values stay identical. Repository author concentration stays identical;
module ownership rows are recomputed. The scorecard's repository age, activity trend, and package
count stay identical, while module count, top hotspots, cycle count, and ownership warnings are
recomputed for the selected projection. Symbol count remains unavailable while ADR-147's symbol tier
is deferred.

Every scope-keyed `AnalysisMeta.inputs` entry is also a v2 provenance pointer, not inherited v1
copy. It MUST resolve to `graph.shared.*` or `graph.source_projections.<same scope>.*`. A structure-
only analysis names its same-keyed module/structure inputs; a join analysis additionally names the
needed shared history/commit-module evidence. No v2 aggregate may retain stale paths such as
`graph.modules` or `graph.commit_module_edges`, and a scope-keyed analysis may not point at the other
projection. Rust and TypeScript semantic validation resolve these paths against the bundle before a
renderer or evidence brief can use them.

### D7 — `khive.repo.v2` is a re-export boundary, not an in-place migration

Adding required module roles and two required aggregate projections breaks the closed v1 Rust
model, JSON Schema, and TypeScript parser. The producer therefore emits
`schema_version = "khive.repo.v2"`; `khive.repo.v1` is immutable.

The envelope version is not the natural-identity version. Existing repository, package, module,
history, edge, and SCC identifiers retain the current `khive.repo.v1` domain-separated natural-ID
algorithm. The same evidence at the same pinned repository snapshot therefore keeps its ID across
re-export, and a node that is a member of both v2 projections has one shared ID. Only changed scoped
membership can change a derived SCC's member set and therefore its ID. Implementations MUST NOT
replace the natural-ID domain separator with `khive.repo.v2` as part of the envelope cutover.

There is no v1-to-v2 JSON transformer. V1 bytes contain paths but not the pinned Cargo-manifest
classification context, and transforming them with path regexes would violate D1 and mishandle
custom targets. The only supported migration is:

```text
pinned clone + history database + code-map database → khive repo export → khive.repo.v2
```

`khive repo build` recreates the two databases in a fresh work directory and exports in one run.
`khive repo export` may reuse existing databases under ADR-147's existing rules: the clone is clean
and pinned, tracked inputs pass preflight, module source revisions reconcile to HEAD, and both stores
open read-only. Because existing stores have no pipeline-identity/completeness handshake,
export-only mode continues to encode ingest provenance as unknown and preserves section-level
unavailable/truncated disclosures; unknown provenance alone is not a new v2 rejection gate. If the
clone or databases are unavailable, a security/revision guard fails, or a role-bearing manifest
field violates D3, the bundle cannot be migrated and remains an explicitly old v1 artifact.

The implementation cutover is coordinated in one repository change: Rust model/exporter, generated
JSON Schema, strict TypeScript parser, real golden, curated static asset, and documentation advance
together. Long-lived dual parsing is rejected. A materialized DB-snapshot deployment re-exports and
atomically replaces its report before or with the v2 frontend rollout; an old v1 report presented to
the v2 consumer renders the existing visible invalid-source state, never an assumed
`all_captured` projection.

### D8 — Frontend scope is wire-driven; URL replay is a separable follow-up

The consumer defaults to `capability.source_projections.default` and may offer a control using the
two `capability.labels.source_projections` values. One selected graph projection, its same-keyed
aggregate projection, and the explicitly shared repository/history evidence are the only inputs for
a render. Every graph, module-derived inspector field, aggregate, evidence brief, and caveat uses
that same projection key. Shared history facets retain their all-captured disclosure and are
mechanically restricted as D5 permits; they never masquerade as producer-recomputed scoped pages.
Switching scope is atomic: if the selected module or focused pair is absent from the new projection,
that dependent focus is cleared rather than retained against mismatched evidence.

An optional frontend follow-up may extend the typed URL-state work proposed in PR #1973 with
`scope=known_auxiliary_excluded`. The canonical default omits `scope`; absence means
`all_captured`. Unknown or duplicate values fail closed to the default with a visible recovery
announcement. URL work does not gate the exporter, schema, or golden tranche and MUST NOT be
approximated with an untyped query parameter in that tranche.

### D9 — Phases and acceptance gates

**Phase 0 — this ADR.** No production code. Acceptance requires ADR-reference lint, docs format
check, and `git diff --check`, plus an independent contract audit against ADR-085, ADR-147, the Rust
model/exporter, JSON Schema, TypeScript model, and checked golden.

**Phase 1 — producer, wire, and re-export.** One implementation tranche must make all of these gates
green:

1. Classifier fixtures cover `src/lib.rs`, `src/main.rs`, `src/bin`, a `src/*_tests.rs` module,
   explicit custom lib/bin/test/bench/example roots, default/custom/disabled build scripts,
   conventional helper modules, nested workspaces with deepest-package ownership, an explicit
   target outside its declaring package and every other package but inside the repository, a separate
   genuinely unowned row with no exact proposal, a cross-owner target conflicting with the governing
   package convention, unsupported languages, and a path declared under conflicting exact-role
   classes. Path fixtures cover `.`/`..` normalization and reject wrong TOML types plus POSIX/Windows
   absolute, backslash, empty-component, and repository-escaping forms identically on every host.
2. Every fixture and the regenerated real golden satisfies the D4 count-conservation invariants;
   every module has exactly one closed role. Shape mutations for an unknown role, projection, count
   key, label key, or object field, and fixed-value mutations for the classifier ID,
   `classified_languages`, or any role/projection label (including a reduced label of "Production")
   are rejected by Rust, JSON Schema, and TypeScript. Cross-field mutations for key/`scope`
   disagreement, reordered or contradictory role arrays, a wrong default, count arithmetic,
   projection module counts, presentation-count equations, or classified-revision mismatch are
   rejected by both mandatory runtime validators; schema checks reject each cross-field
   contradiction its static vocabulary can express.
3. One adversarial small-bound fixture puts excluded candidates ahead of included candidates in the
   all-captured order for projection modules and structure edges and for dependency-topology module/
   cycle, hotspot, hidden-coupling, treemap, ownership-module, de-facto API, and module-derived
   scorecard results. Every reduced page/field still returns the independently computed included
   result that began below the all-captured bound, with correct full-scope values, totals, omission
   counts, and disclosures. `module_count` reports full selected membership. This fixture proves
   selection and recomputation precede every graph and aggregate presentation bound named in issue
   #1976.
4. Under those same adversarial bounds, a cycle containing an integration-test node disappears from
   the reduced SCC set while a crate-source cycle remains with a deterministically recomputed ID.
   Fan-in/out, hotspot thresholds and quadrant, hidden-coupling support, treemap size, de-facto API
   rank, module ownership, and every module-derived scorecard assertion equal independent reduced-
   scope calculations rather than filtered all-captured values.
5. A separate reference-closure fixture independently truncates shared package/history nodes, both
   projection module pages, structure and commit-module edges, navigation, topology cycles/modules,
   and every module-referencing aggregate. Every serialized typed reference resolves in the shared
   plus correct same-keyed serialized registry; every page satisfies D5 presentation conservation;
   and mutations that add a dangling package, module, commit, edge endpoint, or cycle ID fail Rust
   and TypeScript validation. Non-module-only structure edges appear byte-equivalently in both
   projections. `unclassified` fixture modules appear in both projections, while the one shared
   history section retains all-captured semantics and typed omissions.
6. Every scope-keyed `AnalysisMeta.inputs` path resolves to a shared or same-keyed v2 section. Tests
   reject stale v1 paths and a path into the other projection, and evidence briefs expose the exact
   projection key plus shared input paths used for each result.
7. The `all_captured` projection matches the v1 full-scope analysis algorithms and preserves every
   existing natural ID on the same fixture. V2 deliberately does not preserve a v1 presentation row
   that would dangle after independent node bounds; D5 omission accounting replaces that behavior.
   Apart from the v2 envelope, required role/coverage/reference fields, and reference-closed
   presentation, the same inputs remain equivalent. Two exports from identical inputs produce
   identical canonical bytes.
8. The checked JSON Schema and TypeScript strict model validate the exact regenerated golden bytes;
   the static asset is byte-identical to the schema example; `khive repo build` and `khive repo
   export` both produce v2; and the canonical golden remains at or below
   `REPO_BUNDLE_MAX_BYTES` (8 MiB) without increasing that limit.

**Phase 2 — consumer control and dogfood.** The frontend tranche must prove:

1. no source-path classifier exists in TypeScript;
2. one scope switch updates topology/SCC, hotspot, hidden coupling, ownership, API, scorecard,
   inspector, and evidence-brief provenance from the same projection;
3. stale module/pair focus clears when absent, while Back/Forward behavior remains correct for all
   then-supported URL state;
4. controls use the capability-provided labels, copied evidence records the canonical
   `known_auxiliary_excluded` key, both enumerate retained `unclassified` coverage, and neither
   claims production reachability; and
5. real Chromium exercises both the curated static v2 asset and the materialized DB-snapshot route
   without console errors or mobile overflow.

The optional typed URL parameter in D8 may land in Phase 2 or a separate focused PR. It is not a
Phase 1 acceptance condition.

## Non-Goals

- No Cargo build, feature resolution, `cfg` evaluation, call graph, or deployment reachability
  analysis.
- No source-role field in ADR-085's persistent module identity and no mutation of existing code-map
  rows.
- No arbitrary user-authored role filter and no combinatorial projection matrix in v2.
- No removal of auxiliary evidence from `all_captured`.
- No frontend role inference, JSON migration heuristic, or silent v1 compatibility shim.

## Alternatives Considered

| Alternative                                      | Why rejected                                                                                                                     |
| ------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------- |
| TypeScript path regexes                          | The browser lacks safe manifest parsing, custom target declarations, revision binding, and full pre-bound evidence.              |
| Drop tests/benches/examples during `code.ingest` | Violates the history-preserving data/view boundary, removes useful test architecture, and changes ADR-085 for one consumer.      |
| Call the reduced view `production`               | Cargo layout and target roots do not prove feature, platform, runtime, deployment, or call reachability.                         |
| Filter bounded v1 pages                          | Cannot recover rows below a bound and leaves topology/SCC/hotspot/API calculations based on excluded nodes.                      |
| Exclude `unclassified`                           | Converts unknown evidence into a false negative and makes the reduced view look more authoritative than the classifier permits.  |
| Emit arbitrary per-role combinations             | Multiplies bundle size and UI states before a demonstrated user need; the two named projections answer the observed dogfood gap. |
| Transform v1 JSON to v2                          | V1 lacks manifest target evidence; any transform would reproduce the rejected client heuristic under a different name.           |
| Duplicate complete history evidence per scope    | Repeats the golden's largest pages and breaks the accepted 8 MiB browser input ceiling.                                          |
| Permit dangling bounded-page references          | Leaves the browser unable to label or inspect a referenced object and makes evidence briefs non-replayable.                      |
| Add an unbounded identity index                  | Moves the same size/unboundedness problem into a second node representation.                                                     |
| Recompute metrics from serialized node pages     | Makes bounds change SCCs, ranks, thresholds, and totals; D5 instead reference-closes only after full-scope computation.          |

## Consequences

### Positive

- Users can compare raw captured architecture with a conservative auxiliary-excluded view without
  losing unknown evidence or making a production claim.
- Dogfood findings become falsifiable across two producer-defined scopes, and copied evidence can
  name exactly which scope and classifier produced a rank.
- Scope-dependent totals, bounds, SCCs, and rankings remain mathematically coherent because the
  producer recomputes them from full evidence.

### Negative

- V2 adds one more projection-local module/structure slice and complete set of module-derived
  aggregates plus per-module roles, so static and materialized reports grow even though large
  history evidence stays shared. The 8 MiB browser ceiling can force tighter representation work
  inside the decided v2 shape.
- V1 reports require a pinned-clone/database re-export and cannot be upgraded from their JSON bytes
  alone.
- The classifier is intentionally conservative: custom supporting modules outside conventional
  source-set directories remain `unclassified` until stronger producer evidence exists.
- On repositories that exceed node bounds, reference closure can omit a fully computed edge or
  aggregate row from the presentation. Typed counts make that loss visible, but the static browser
  cannot inspect evidence whose referenced object was not serialized.

### Neutral

- `all_captured` remains the default and preserves the current product interpretation. This ADR adds
  a comparison scope; it does not redefine old evidence as wrong.
- Source roles can expose test-driven false positives and test architecture worth studying. The
  reduced projection is a lens, not a cleanup operation.

## Implementation Status

Proposed, docs-only. At live `main` when this ADR was authored:

- ADR-085 and ADR-147 are accepted;
- `ModuleNode` has `source_path` and `source_revision` but no `source_role`;
- `RepoBundle` has one top-level `graph` and `aggregates`, both all-captured;
- `build_aggregates` computes from full internal module/edge vectors before its one set of bounds,
  but it has no second source projection;
- `docs/schemas/khive-repo-v1.schema.json`, the TypeScript strict model, and the checked golden accept
  only `khive.repo.v1`; and
- no open issue or PR owned this exact classifier/projection scope before issue #1976.

## References

- [ADR-085](ADR-085-code-pack.md): module source/revision identity and coverage
- [ADR-147](ADR-147-repo-showcase-bundle.md): accepted bundle, analyses, bounds, and exporter-owned
  history-structure join
- [Issue #1976](https://github.com/ohdearquant/khive/issues/1976): implementation tracking
- [PR #1938](https://github.com/ohdearquant/khive/pull/1938): repository-cockpit dogfood that exposed
  source-role contamination
- [PR #1971](https://github.com/ohdearquant/khive/pull/1971): hidden-coupling lens and current
  source-role caveat
- [PR #1973](https://github.com/ohdearquant/khive/pull/1973): proposed typed URL state that an
  optional `scope` follow-up may extend
