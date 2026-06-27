# khive v1 ADR Index

Architecture Decision Records (ADRs) for khive v1. These are **desired-state specifications** — the contract that code must implement. ADRs use closed taxonomies and bear normative weight; changes require explicit ADR amendments.

For historical context, see [v0 archive](../_archive/adr_v0/README.md). v0 ADRs are preserved for reference but are not authoritative for v1.

## Foundation

| #                                               | Title                                                                                     |
| ----------------------------------------------- | ----------------------------------------------------------------------------------------- |
| [ADR-001](ADR-001-entity-kind-taxonomy.md)      | Entity Kind Taxonomy                                                                      |
| [ADR-002](ADR-002-edge-ontology.md)             | Closed Edge Ontology                                                                      |
| [ADR-003](ADR-003-system-architecture.md)       | System Architecture                                                                       |
| [ADR-004](ADR-004-substrate-observables.md)     | Substrate Observables                                                                     |
| [ADR-005](ADR-005-storage-capability-traits.md) | Storage Capability Traits                                                                 |
| [ADR-006](ADR-006-deterministic-scoring.md)     | Deterministic Scoring                                                                     |
| [ADR-007](ADR-007-namespace.md)                 | Namespace (Rev 6 — attribution-only, dumb storage, single Gate, per-actor episodic write) |
| [ADR-008](ADR-008-query-layer-separation.md)    | Query Layer Separation                                                                    |
| [ADR-009](ADR-009-backend-architecture.md)      | Backend Architecture                                                                      |
| [ADR-010](ADR-010-kg-versioning.md)             | KG Versioning Strategy                                                                    |
| [ADR-011](ADR-011-embedding-and-inference.md)   | Embedding and Inference Architecture                                                      |
| [ADR-012](ADR-012-retrieval-composition.md)     | Retrieval Composition                                                                     |
| [ADR-013](ADR-013-note-kind-taxonomy.md)        | Note Kind Taxonomy                                                                        |
| [ADR-014](ADR-014-curation-operations.md)       | Curation Operations                                                                       |
| [ADR-015](ADR-015-schema-migrations.md)         | Schema Migrations                                                                         |

## MCP / Pack Surface

| #                                                  | Title                                                  |
| -------------------------------------------------- | ------------------------------------------------------ |
| [ADR-016](ADR-016-request-dsl.md)                  | Request DSL                                            |
| [ADR-017](ADR-017-pack-standard.md)                | Pack Standard                                          |
| [ADR-018](ADR-018-authorization-gate.md)           | Authorization Gate                                     |
| [ADR-019](ADR-019-gtd-pack.md)                     | GTD Pack                                               |
| [ADR-020](ADR-020-git-native-kg-implementation.md) | Git-Native KG Implementation                           |
| [ADR-021](ADR-021-memory-pack.md)                  | Memory Pack                                            |
| [ADR-022](ADR-022-events-query-surface.md)         | Events Query Surface                                   |
| [ADR-023](ADR-023-declarative-pack-format.md)      | Pack Verb Surface, Visibility, and Composition         |
| [ADR-024](ADR-024-fold-cognitive-primitives.md)    | Fold Cognitive Primitives                              |
| [ADR-025](ADR-025-verb-speech-acts.md)             | Verb Surface as Speech-Act Taxonomy                    |
| [ADR-026](ADR-026-rust-binary-packaging.md)        | Rust Binary Packaging via Per-Platform npm Subpackages |
| [ADR-027](ADR-027-dynamic-pack-loading.md)         | Dynamic Pack Loading via Self-Registration             |

## Backend / Retrieval

| #                                            | Title                                                                                    |
| -------------------------------------------- | ---------------------------------------------------------------------------------------- |
| [ADR-028](ADR-028-pack-scoped-backends.md)   | Pack-Scoped Backends and Per-Pack Schema Declaration                                     |
| [ADR-029](ADR-029-substrate-coordinator.md)  | SubstrateCoordinator — Cross-Backend Operations                                          |
| [ADR-030](ADR-030-retrieval-stack-port.md)   | Retrieval Stack Port — khive-retrieval                                                   |
| [ADR-031](ADR-031-multi-engine-retrieval.md) | Multi-Engine Retrieval — Embedder Trait, Registry, Configuration, and Pack Orchestration |

## Brain / Validation / Recall

| #                                                 | Title                                                       |
| ------------------------------------------------- | ----------------------------------------------------------- |
| [ADR-032](ADR-032-brain-profile-orchestration.md) | Brain as Profile-Orchestration over Fold + Objective        |
| [ADR-033](ADR-033-recall-pipeline.md)             | Recall Pipeline — Configurable Multi-Stage Memory Retrieval |
| [ADR-034](ADR-034-kg-validation-pipelines.md)     | KG Validation Pipelines                                     |

## CLI / Import-Export / Remote

| #                                                             | Title                                                  |
| ------------------------------------------------------------- | ------------------------------------------------------ |
| [ADR-035](ADR-035-cli-config-and-auto-embed.md)               | CLI Configuration and Automatic Embedding              |
| [ADR-036](ADR-036-kg-import-export-adapters.md)               | KG Import/Export Format Adapters                       |
| [ADR-037](ADR-037-remote-resolution-and-hash-verification.md) | Remote Entity Resolution and Content-Hash Verification |
| [ADR-038](ADR-038-bulk-operations.md)                         | Bulk Operations                                        |
| [ADR-039](ADR-039-note-merge.md)                              | Note Merge Operation                                   |

## New v1 Surfaces

| #                                                        | Title                                                                                                           |
| -------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------- |
| [ADR-040](ADR-040-communication-and-schedule-packs.md)   | Communication and Schedule Packs                                                                                |
| [ADR-041](ADR-041-event-provenance-projection.md)        | Event Provenance Projection — Hybrid Log + Graph Edges                                                          |
| [ADR-042](ADR-042-local-rerank-via-lattice-inference.md) | Composable Rerank Pipeline (local cross-encoder + salience + graph-proximity)                                   |
| [ADR-043](ADR-043-embedding-model-migration.md)          | Embedding Model Migration                                                                                       |
| [ADR-044](ADR-044-vector-store-extensions.md)            | Vector Store Extensions — Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep                   |
| [ADR-045](ADR-045-verb-response-presentation.md)         | Verb Response Presentation Modes                                                                                |
| [ADR-046](ADR-046-event-sourced-proposals.md)            | Event-Sourced Agent KG Proposals                                                                                |
| [ADR-047](ADR-047-knowledge-pack.md)                     | Knowledge Pack — Concept Registration, Citation, and Topic Search                                               |
| [ADR-048](ADR-048-knowledge-section-profiles.md)         | Knowledge Section Profiles                                                                                      |
| [ADR-049](ADR-049-khived-daemon.md)                      | khived Daemon — Persistent Warm Runtime over a Unix Socket                                                      |
| [ADR-050](ADR-050-kg-token-namespace-contract.md)        | KG Token Namespace Contract (Proposed)                                                                          |
| [ADR-051](ADR-051-section-embeddings-hybrid-compose.md)  | Section-Level Embeddings and Hybrid Compose Scoring (Accepted)                                                  |
| [ADR-052](ADR-052-ann-production-lifecycle.md)           | ANN Production Lifecycle — SQ8 Quantization, Tombstone Delete, Consolidation, Crash-Safe Persistence (Accepted) |
| [ADR-053](ADR-053-authorization-gate.md)                 | Authorization Gate — ActorStore, SessionStore, and Cloud-Tier Caller Propagation (Proposed)                     |
| [ADR-054](ADR-054-ann-build-strategy-scaling-limits.md)  | ANN Build Strategy and Scaling Limits (Proposed)                                                                |
| [ADR-055](ADR-055-epistemic-edge-relations.md)           | Epistemic Edge Relations — `supports` and `refutes` (Accepted)                                                  |
| [ADR-056](ADR-056-channel-transport-layer.md)            | Channel Transport Layer — `khive-channel` and External Messaging Adapters (Proposed)                            |
| [ADR-057](ADR-057-comm-actor-addressed-delivery.md)      | Comm Actor-Addressed Delivery (Accepted)                                                                        |
| [ADR-058](ADR-058-brain-posterior-read-path.md)          | Brain Posterior Read Path — Wiring Profile Posteriors into Recall Ranking (Proposed)                            |
| [ADR-059](ADR-059-namespace-write-tiers.md)              | Namespace Write Tiers and Cross-Namespace Link Access Control (Withdrawn — superseded by ADR-007 Rev 2)         |
| [ADR-061](ADR-061-pack-extensible-by-id-resolution.md)   | Pack-Extensible by-ID Resolution (Accepted)                                                                     |
| [ADR-066](ADR-066-autonomous-merge-pipeline.md)          | Autonomous Merge Pipeline — Gate Wall as Reviewer, Human Gate at Release (Proposed)                             |
| [ADR-067](ADR-067-write-owner-daemon.md)                 | Write-Owner Daemon — Single-Writer Task and Write Queue (Proposed)                                              |
| [ADR-068](ADR-068-cloud-multitenancy-topology.md)        | Cloud Multi-Tenancy Topology and Tenant Isolation (Proposed)                                                    |
| [ADR-069](ADR-069-subject-model.md)                      | Subject Model — Domain-Ontology Ingestion and Map Pipeline (Proposed)                                           |
| [ADR-073](ADR-073-pack-core-backend-accessor.md)         | Pack Core-Backend Accessor (Proposed)                                                                           |

## Closed Taxonomies — Quick Reference

- **Entity kinds**: 8 shared base kinds in `khive_types` (`concept`, `document`, `dataset`, `project`, `person`, `org`, `artifact`, `service`) plus KG pack-side `resource` governance for actionable knowledge resources (ADR-001, ADR-048)
- **Edge relations (17 in 9 categories)** (ADR-002, extended by ADR-055):
  - Structure: `contains`, `part_of`, `instance_of`
  - Derivation: `extends`, `variant_of`, `introduced_by`, `supersedes`
  - Provenance: `derived_from`
  - Temporal: `precedes`
  - Dependency: `depends_on`, `enables`
  - Implementation: `implements`
  - Lateral: `competes_with`, `composed_with`
  - Annotation: `annotates`
  - Epistemic: `supports`, `refutes`
- **Note kinds (5 base)**: `observation`, `insight`, `question`, `decision`, `reference` (ADR-013). Packs may add (e.g., GTD adds `task`; memory pack adds `memory`).

## Cross-Cutting Principles

- **Data vs view**: never mutate stored data to fix a query result. Use `supersedes` + view-layer filter. Curation (`update`/`delete`/`merge`) is for deliberate correction only. See [ADR-014](ADR-014-curation-operations.md).
- **No stubs**: every ADR claim must be implementable; stubs and placeholders are not acceptable.
- **Closed taxonomies**: entity kinds, edge relations, note kinds are closed enums. Extension requires ADR amendment.
- **ADRs are desired-state specs**: ADRs describe the intended v1 design, not the state of any specific deployment context.
