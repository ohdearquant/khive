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

## Source and coverage provenance

Every L1.5 module carries `source_project`, repository-relative `source_path`, `source_revision`,
and `content_hash`. `source_revision` is the observed git `HEAD`, or `unversioned` outside a
committed repository; `content_hash` still describes the scanned working-tree bytes. Module UUIDs
remain the stable semantic identities from ADR-085 B4. Rust `src/lib.rs` and `src/main.rs` roots use
`crate` and `crate::main` module paths so both physical files remain independently addressable.

`import_scan_status` distinguishes `scanned`, `partially_resolved`, and `unscanned` modules, with
`import_specifier_count` and `unresolved_import_count` recording the completed scan's coverage.
Both the project and module endpoints of `project contains module` carry the same
`source_project`, so either endpoint supports ownership aggregation.

## Runtime surface

The pack depends on `kg`, registers the finding hook and vocabulary, and contributes one verb:
`code.ingest`, the L1 manifest + L1.5 import-scan source ingester targeting a dedicated map
database. `findings.json` ingestion is an admin CLI path through `kkernel code-ingest`, not an
MCP operation. Unknown dispatch attempts fail
with `RuntimeError::InvalidInput` rather than silently succeeding.
