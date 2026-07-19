# khive v1 ADR Index

Architecture Decision Records (ADRs) for khive v1. These are **desired-state specifications** — the contract that code must implement. ADRs use closed taxonomies and bear normative weight; changes require explicit ADR amendments.

For historical context, see [v0 archive](../_archive/adr_v0/README.md). v0 ADRs are preserved for reference but are not authoritative for v1.

## Foundation

| #                                               | Title                                                                                                      |
| ----------------------------------------------- | ---------------------------------------------------------------------------------------------------------- |
| [ADR-001](ADR-001-entity-kind-taxonomy.md)      | Entity Kind Taxonomy                                                                                       |
| [ADR-002](ADR-002-edge-ontology.md)             | Closed Edge Ontology                                                                                       |
| [ADR-003](ADR-003-system-architecture.md)       | System Architecture                                                                                        |
| [ADR-004](ADR-004-substrate-observables.md)     | Substrate Observables                                                                                      |
| [ADR-005](ADR-005-storage-capability-traits.md) | Storage Capability Traits                                                                                  |
| [ADR-006](ADR-006-deterministic-scoring.md)     | Deterministic Scoring                                                                                      |
| [ADR-007](ADR-007-namespace.md)                 | Namespace as Attribution-Only Open String — Dumb Storage, Single Gate, Operator-Configured Read Visibility |
| [ADR-008](ADR-008-query-layer-separation.md)    | Query Layer Separation                                                                                     |
| [ADR-009](ADR-009-backend-architecture.md)      | Backend Architecture                                                                                       |
| [ADR-010](ADR-010-kg-versioning.md)             | KG Versioning Strategy                                                                                     |
| [ADR-011](ADR-011-embedding-and-inference.md)   | Embedding and Inference Architecture                                                                       |
| [ADR-012](ADR-012-retrieval-composition.md)     | Retrieval Composition (High-Level Composition Layer)                                                       |
| [ADR-013](ADR-013-note-kind-taxonomy.md)        | Note Kind Taxonomy                                                                                         |
| [ADR-014](ADR-014-curation-operations.md)       | Curation Operations                                                                                        |
| [ADR-015](ADR-015-schema-migrations.md)         | Schema Migrations                                                                                          |

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

| #                                                            | Title                                                                                                 |
| ------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------- |
| [ADR-040](ADR-040-communication-and-schedule-packs.md)       | Communication and Schedule Packs                                                                      |
| [ADR-041](ADR-041-event-provenance-projection.md)            | Event Provenance Projection — Hybrid Log + Graph Edges                                                |
| [ADR-042](ADR-042-local-rerank-via-lattice-inference.md)     | Composable Rerank Pipeline (local cross-encoder + salience + graph-proximity)                         |
| [ADR-043](ADR-043-embedding-model-migration.md)              | Embedding Model Migration                                                                             |
| [ADR-044](ADR-044-vector-store-extensions.md)                | Vector Store Extensions — Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep         |
| [ADR-045](ADR-045-verb-response-presentation.md)             | Verb Response Presentation Modes                                                                      |
| [ADR-046](ADR-046-event-sourced-proposals.md)                | Event-Sourced Agent KG Proposals                                                                      |
| [ADR-047](ADR-047-knowledge-pack.md)                         | Knowledge Pack                                                                                        |
| [ADR-048](ADR-048-knowledge-section-profiles.md)             | Knowledge Section Profiles                                                                            |
| [ADR-049](ADR-049-khived-daemon.md)                          | khived daemon — persistent warm runtime over a Unix socket                                            |
| [ADR-050](ADR-050-kg-token-namespace-contract.md)            | KG Token Namespace Contract                                                                           |
| [ADR-051](ADR-051-section-embeddings-hybrid-compose.md)      | Section-level embeddings and hybrid compose scoring                                                   |
| [ADR-052](ADR-052-ann-production-lifecycle.md)               | ANN Production Lifecycle -- SQ8 Quantization, Tombstone Delete, Consolidation, Crash-Safe Persistence |
| [ADR-053](ADR-053-authorization-gate.md)                     | Authorization Gate -- ActorStore, SessionStore, and Caller Propagation                                |
| [ADR-054](ADR-054-ann-build-strategy-scaling-limits.md)      | ANN Build Strategy and Scaling Limits                                                                 |
| [ADR-055](ADR-055-epistemic-edge-relations.md)               | Epistemic Edge Relations — `supports` and `refutes`                                                   |
| [ADR-056](ADR-056-channel-transport-layer.md)                | Channel Transport Layer -- `khive-channel` and External Messaging Adapters                            |
| [ADR-057](ADR-057-comm-actor-addressed-delivery.md)          | Comm Actor-Addressed Delivery                                                                         |
| [ADR-058](ADR-058-brain-posterior-read-path.md)              | Brain Posterior Read Path — Wiring Profile Posteriors into Recall Ranking                             |
| [ADR-059](ADR-059-namespace-write-tiers.md)                  | Namespace Write Tiers and Cross-Namespace Link Access Control                                         |
| [ADR-061](ADR-061-pack-extensible-by-id-resolution.md)       | Pack-Extensible by-ID Resolution                                                                      |
| [ADR-062](ADR-062-fts-ann-consolidation.md)                  | FTS and ANN Consolidation -- Unified Search Tables (Schema V4)                                        |
| [ADR-066](ADR-066-autonomous-merge-pipeline.md)              | Autonomous Merge Pipeline — Gate Wall as Reviewer                                                     |
| [ADR-067](ADR-067-write-owner-daemon.md)                     | Write-Owner Daemon — Single-Writer Task and Write Queue                                               |
| [ADR-068](ADR-068-process-isolation-topology.md)             | Per-Process Isolation Topology                                                                        |
| [ADR-069](ADR-069-subject-model.md)                          | The Subject Model -- Domain-Ontology Ingestion and Map Pipeline                                       |
| [ADR-071](ADR-071-backend-pluggable-runtime.md)              | Backend-Pluggable Runtime — Polystore Restoration                                                     |
| [ADR-072](ADR-072-subject-ontologyspec-as-data.md)           | Subject OntologySpec as Runtime Data -- Verbless Verticals and Pack Retirement                        |
| [ADR-073](ADR-073-pack-core-backend-accessor.md)             | Pack Core-Backend Accessor                                                                            |
| [ADR-074](ADR-074-graph-aware-recall.md)                     | Graph-Aware Recall — Graph-Proximity Signal in Memory Retrieval                                       |
| [ADR-075](ADR-075-owl-rdf-interoperability.md)               | OWL/RDF Interoperability -- Publishing the khive Vocabulary and Aligning with External Ontologies     |
| [ADR-076](ADR-076-relation-calculability-and-system-role.md) | Relation-Set Calculability — System Role and the Non-Redundancy Certificate                           |
| [ADR-078](ADR-078-output-format-shape-aware-rendering.md)    | Output Format and Shape-Aware Rendering                                                               |
| [ADR-079](ADR-079-ann-persistence-warm-path-integration.md)  | ANN Persistence Warm-Path Integration — Wiring v2 Persistence into the Daemon                         |
| [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md)     | Session Pack — OSS Storage Mechanism                                                                  |
| [ADR-082](ADR-082-retrieval-quality-measurement-loop.md)     | Retrieval Quality Measurement Loop                                                                    |
| [ADR-083](ADR-083-session-pack-t1-verbs.md)                  | Session Pack T1 Verb Surface                                                                          |
| [ADR-084](ADR-084-verb-surface-consistency.md)               | Verb-Surface Consistency Contract and Live Ontology Introspection                                     |
| [ADR-085](ADR-085-code-pack.md)                              | Code Pack — Source-Code Ontology and Audit-Finding Vocabulary                                         |
| [ADR-086](ADR-086-doc-file-pack.md)                          | Document/File Modeling — Content on the Existing `document` Entity Kind                               |
| [ADR-087](ADR-087-workspace-mirror.md)                       | Workspace Mirror — Folding `.khive/` Into the Graph Substrate                                         |
| [ADR-088](ADR-088-git-lifecycle-pack.md)                     | Git-Lifecycle Pack — Commit and Issue Note Kinds                                                      |
| [ADR-088 Amendment 1](ADR-088-amendment-1-git-digest.md)     | `git.digest` — Agent-Facing Digest Verb with Remote-URL Support (Accepted)                            |
| [ADR-089](ADR-089-context-verb.md)                           | `context` verb — entity-anchored graph context in one call                                            |
| [ADR-090](ADR-090-docs-site-standard.md)                     | Docs site standard — navigation, agent md/txt surfaces, visual style                                  |
| [ADR-091](ADR-091-wal-snapshot-lifetime.md)                  | Bounded read-transaction lifetime and WAL checkpoint escalation                                       |
| [ADR-092](ADR-092-context-composer.md)                       | Cross-substrate context composer — ContextContributor trait + `context.assemble`                      |
| [ADR-093](ADR-093-sessions-raw-zstd-compression.md)          | zstd Compression for Session-Mirror Raw Storage                                                       |
| [ADR-094](ADR-094-lifecycle-telemetry-events.md)             | Sequencing-Assertable Lifecycle Telemetry Events                                                      |
| [ADR-095](ADR-095-verb-surface-consolidation.md)             | Verb-Surface Consolidation and Field-Validation Governance                                            |
| [ADR-096](ADR-096-warm-daemon-per-request-identity.md)       | warm daemon per-request identity — serving many attribution identities over one shared backend        |
| [ADR-099](ADR-099-bulk-apply-atomic-units.md)                | Cross-Op Atomicity for Bulk Apply — Prepared Write Plans over the Single-Writer Seam                  |
| [ADR-100](ADR-100-store-backup-replication.md)               | Store backup and replication                                                                          |
| [ADR-101](ADR-101-kg-changeset-model.md)                     | KG Change-Set Model — Producer-Agnostic Op-List with Stage-Time Stable IDs                            |
| [ADR-102](ADR-102-tiered-validate-and-merge.md)              | Tiered Validate-and-Merge — Rule-Gated Fast Path and Reviewed Change-Set Path                         |
| [ADR-106](ADR-106-schedule-pack-executor.md)                 | Schedule Pack Executor — Daemon-Resident Tick for the Pending-Event Drain                             |
| [ADR-108](ADR-108-git-write-surface.md)                      | Git Write Surface Through khive (Phase B)                                                             |
| [ADR-109](ADR-109-sandboxed-kkernel-gateway.md)              | Sandboxed kkernel Gateway for Untrusted Execution (Phase C)                                           |
| [ADR-110](ADR-110-vamana-wasm.md)                            | WebAssembly Support for khive-vamana                                                                  |
| [ADR-111](ADR-111-blob-store.md)                             | BlobStore — Content-Addressed Binary Object Storage                                                   |
| [ADR-112](ADR-112-git-outbound-publish-verbs.md)             | Outbound GitHub Publish Verbs with a Publication-Hygiene Scan                                         |
| [ADR-113](ADR-113-identifier-continuity.md)                  | Identifier Continuity — Merged-Entity Redirect Resolution and Split Endpoint-Move                     |
| [ADR-114](ADR-114-code-audit-derived-report.md)              | Code-Audit Derived Report, Not Agent Findings                                                         |
| [ADR-115](ADR-115-secret-gate-content-manifest-exemption.md) | Exact-Content Manifest Exemption for the Write Secret Gate                                            |
| [ADR-117](ADR-117-session-continuity-search.md)              | Session Continuity — Cross-Session Search and Remote Ingestion                                        |

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
