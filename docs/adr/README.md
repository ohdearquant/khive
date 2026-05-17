# Architecture Decision Records

ADRs document the design decisions behind khive — what was decided, why, and what was considered as
alternatives. New decisions should be added as ADR-NNN-kebab-case-title.md using the
[template](../_templates/ADR_TEMPLATE.md).

## Index

| ADR                                               | Title                                    | Status   | Topic                                                                                                                                                                         |
| ------------------------------------------------- | ---------------------------------------- | -------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| [001](ADR-001-entity-kind-taxonomy.md)            | Entity Kind Taxonomy                     | accepted | 6 entity kinds for research KG (Concept, Document, Dataset, Project, Person, Org)                                                                                             |
| [002](ADR-002-edge-ontology.md)                   | Closed Edge Ontology                     | accepted | 13 canonical relations in 6 categories                                                                                                                                        |
| [003](ADR-003-four-layer-architecture.md)         | Four-Layer Architecture                  | accepted | Frontend / Deno / MCP / Crates separation                                                                                                                                     |
| [004](ADR-004-substrate-observables.md)           | Substrate Observables                    | accepted | Three primitives: Note, Entity, Event                                                                                                                                         |
| [005](ADR-005-storage-capability-traits.md)       | Storage Capability Traits                | accepted | Trait-only crate, 6 capabilities, zero implementations                                                                                                                        |
| [006](ADR-006-deterministic-scoring.md)           | Deterministic Scoring                    | accepted | i64 fixed-point with 2^32 scale for cross-platform ordering                                                                                                                   |
| [007](ADR-007-namespace-as-open-string.md)        | Namespace as Open String                 | accepted | Simplified namespace model for OSS                                                                                                                                            |
| [008](ADR-008-query-layer-separation.md)          | Query Layer Separation                   | accepted | Separate `khive-query` crate for SPARQL/GQL/Cypher (designed; built in v0.2 phase 2)                                                                                          |
| [009](ADR-009-backend-portability.md)             | Backend Portability                      | accepted | One crate per backend (SQLite/Postgres/Neo4j)                                                                                                                                 |
| [010](ADR-010-kg-versioning-direction.md)         | KG Versioning Direction                  | planned  | Strategic direction: "GitHub for knowledge graphs"                                                                                                                            |
| [011](ADR-011-deno-mcp-only-server.md)            | Deno Server + MCP-Only                   | accepted | Deno for the user-facing layer (server + CLI); MCP as the only programmatic interface                                                                                         |
| [012](ADR-012-retrieval-architecture.md)          | Retrieval Architecture                   | accepted | Inference in `lattice-embed`, storage + fusion in khive                                                                                                                       |
| [013](ADR-013-retrieval-port-scope.md)            | Retrieval Scope for v0.1                 | accepted | What's in v0.1 retrieval, what's deferred                                                                                                                                     |
| [014](ADR-014-curation-operations.md)             | KG Curation Operations                   | accepted | Runtime ops for update/merge entity, edge CRUD; surfaced via the verb-consolidated MCP tools (ADR-023)                                                                        |
| [015](ADR-015-kg-versioning-and-portability.md)   | KG Versioning Model                      | planned  | Commit/branch/checkout/merge model; snapshot-based VCS; portability extensions                                                                                                |
| [017](ADR-017-graph-diff-format.md)               | Graph Diff Format                        | planned  | 9-op diff format (entity/edge/property + entity_merge); sequence semantics; slim ops; structured conflict markers                                                             |
| [019](ADR-019-note-kind-taxonomy.md)              | Note Kind Taxonomy                       | accepted | 5 closed note kinds (observation, insight, question, decision, reference); NoteKind enum                                                                                      |
| [020](ADR-020-request-dsl.md)                     | Request DSL                              | planned  | Generic `request` MCP tool with function-call batch syntax `[op(args), op(args)]`; parallel only; deferred past v0.1                                                          |
| [021](ADR-021-edge-relation-enum.md)              | EdgeRelation Enum                        | accepted | Close substrate taxonomy: 13 canonical relations as enum — `annotates` (note→any substrate), `supersedes` (same-substrate: note→note or entity→entity), 11 entity→entity      |
| [022](ADR-022-schema-migrations.md)               | Schema Migrations                        | accepted | Versioned ordered idempotent migrations via `_schema_migrations` table; `run_migrations()` applies V1+ in transaction per version                                             |
| [023](ADR-023-verb-consolidated-mcp-surface.md)   | Verb-Consolidated MCP Surface            | accepted | 11-tool verb surface with `kind=` discriminant; `merge` is entity-only in v0.1 (note merge deferred); `supersede` deferred                                                    |
| [024](ADR-024-note-search-and-cross-substrate.md) | Note Search + Cross-Substrate Navigation | accepted | Auto-index notes (FTS5 + vector); hybrid retrieval with salience weight; cross-substrate via `annotates` edges; `get(id)` serves UUID resolution (no separate `resolve` verb) |
| [025](ADR-025-pack-standard.md)                   | Pack Standard                            | accepted | Pack trait in khive-types; composable vocabulary extension; edge relations stay closed; runtime vocabulary merging                                                             |

## Reading Order

For new contributors:

1. **ADR-003** — get the high-level architecture first
2. **ADR-004** — understand the three core observables
3. **ADR-001 + ADR-002** — entity/edge taxonomies (essential for any agent using the KG)
4. **ADR-005** — storage abstractions
5. **ADR-006** — scoring and ranking
6. **ADR-007** — namespace model

## Status Values

- **accepted**: Decision made. Implementation in progress or complete for v0.1.
- **planned**: Designed but deferred to a later version (v0.2+). Tracked for forward compatibility.
- **deprecated**: No longer guidance. See replacement ADR (none in v0.1).
