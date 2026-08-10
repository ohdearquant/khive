# Code ontology design

The code pack extends the shared graph vocabulary for source structure and audit findings without
adding a private storage substrate or a new edge relation.

## Entity and note modeling

Modules, functions, datatypes, and interfaces are `concept` entities distinguished by governed
`entity_type`; the pack contributes no new base entity kind. A finding is an epistemic note attached
to a project or code concept, not an entity. Its `defect` alias and lifecycle transitions are
registered through the shared note-kind registry.

## Edge rules

Twenty-two additive endpoint rules use the existing `depends_on`, `contains`, `implements`, and
`extends` relations. They model code dependencies, project/module containment, implementation of
interfaces or concepts, and interface/datatype inheritance. Declaring base-covered containment and
extension rows here keeps pack introspection complete without changing the closed relation enum.

L1/L1.5 dependency edges retain ecosystem evidence in `metadata.dependency_kinds` and expose the
portable policy contract in `metadata.dependency_scopes` (`normal`, `dev`, or `build`). Module
imports default to `build`; project imports inherit a matching manifest declaration's scope and
otherwise default to `build`. An edge is dev-only only when its scope set is exactly `{dev}`.

L1.5 import edges are a **static lexical-coupling signal**, not a runtime import graph. The
coverage-floor scanner is regex-based and does not classify block or guard scope: it includes
Python imports under `if TYPE_CHECKING:`, function-local imports, and equivalently indented or
nested matches in the other supported languages. Consequently, a `depends_on` cycle derived from
L1.5 says that the source texts reference one another; it does not by itself establish a
module-initialization or runtime dependency cycle. The current edge metadata does not distinguish
these cases, so consumers needing runtime-cycle claims must confirm scope from source or wait for a
scope-aware scanner tier.

L2 reuses the closed edge vocabulary: modules `contains` their current declarations, symbols
`depends_on` other declarations for resolved path calls and supported type references, and
datatypes `implements` interfaces for positive trait implementations. L2-derived edges are limited
to one source project and language. Their metadata records the evidence class without introducing
new relation names, and `last_seen_at` distinguishes edges observed in the current project/language
sweep from retained history. File-backed modules keep the established flat project-owned scaffold;
inline modules and declarations are contained by their immediate module owner.

## Source and coverage provenance

Every L1.5 module carries `source_project`, repository-relative `source_path`, `source_revision`,
and `content_hash`. `source_revision` is the observed git `HEAD`, or `unversioned` outside a
committed repository; `content_hash` still describes the scanned working-tree bytes. Module UUIDs
remain the stable semantic identities from ADR-085 B4. Rust `src/lib.rs` and `src/main.rs` roots use
`crate` and `crate::main` module paths so both physical files remain independently addressable.

The opt-in L2 tier stores Rust declarations as ordinary `concept` entities. Functions use the
`function` subtype, structs/enums/unions/type aliases use `datatype`, traits use `interface`, and
inline modules use `module`. Symbol identity is deterministic across re-ingest and includes the
source project, language, containing module path, declaration name, and canonical subtype. Each
symbol copies `source_project`, `language`, `source_path`, and `source_revision` for direct audit,
while the owning module's matching `source_revision` and `declaration_ids` array are the
authoritative statement of which declarations are current. A successful scan of an empty source
file records `declaration_ids=[]`; a changed file that no longer parses loses its ownership stamp
without deleting its historical symbol rows. The module's `l2_content_hash` records the bytes that
produced that L2 ownership stamp independently of the shared `content_hash`, which L1.5 may refresh
earlier in a combined pass. Resolution and edge refresh use only declarations proven current by
the active L2 invocation, never ownership left ambient by a prior sweep.

Rust L2 extraction uses `syn::parse_file` with a visitor. It covers functions, structs, enums,
unions, type aliases, traits, inline modules, selected type references, and positive
`impl Trait for Type` relationships. Direct path calls represented by `ExprCall` form the deliberate
call-graph floor. Method dispatch, macros, function values, dynamic dispatch, and other semantic
Rust relationships are outside that floor, so L2 must not be interpreted as a complete call graph.

`import_scan_status` distinguishes `scanned`, `partially_resolved`, and `unscanned` modules, with
`import_specifier_count` and `unresolved_import_count` recording the completed scan's coverage.
Both the project and module endpoints of `project contains module` carry the same
`source_project`, so either endpoint supports ownership aggregation.

## Runtime surface

The pack depends on `kg`, registers the finding hook and vocabulary, and contributes one verb:
`code.ingest`, the L1 manifest + L1.5 import-scan source ingester targeting a dedicated map
database, with an opt-in L2 Rust symbol tier. Its optional `tiers` array accepts `l1`, `l1.5`, and
`l2`. Omission or `null` preserves the L1+L1.5 default with L2 disabled; an empty array performs no
map writes. When L2 is disabled, its five report counters are omitted so the existing default
report shape is unchanged. `findings.json` ingestion is an admin CLI path through `kkernel
code-ingest`, not an MCP operation. Its v2 finding identity is repository/project-scoped and carries
a deterministic v1 UUID witness without rewriting legacy curated rows. Unknown dispatch attempts fail
with `RuntimeError::InvalidInput` rather than silently succeeding.

The dedicated map is an ordinary khive database, not a private code-pack format. Every
non-blocked entity upsert also updates its FTS document, and `code.ingest` reports the completed
count as `fts_indexed`; a failed FTS write fails the ingest instead of returning an unqualified
success for an unsearchable map. Point a normal `kkernel` process at the map through a
`[[backends]]` entry in a selected config to use generic KG reads such as `search`, `resolve`,
`neighbors`, `traverse`, and `context`. `kkernel code-audit` remains the separate policy-driven,
read-only report surface for the same database.
