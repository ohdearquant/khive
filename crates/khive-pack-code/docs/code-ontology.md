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

## Runtime surface

The pack depends on `kg`, registers the finding hook and vocabulary, and contributes zero verbs.
`findings.json` ingestion is an admin CLI path through `kkernel code-ingest`, not an MCP operation;
the accepted `code.ingest` source-ingest verb remains unimplemented. Unknown dispatch attempts fail
with `RuntimeError::InvalidInput` rather than silently succeeding.
