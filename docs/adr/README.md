# khive v1 ADR Index

Architecture Decision Records (ADRs) for khive v1. These are **desired-state specifications** — the contract that code must implement. ADRs use closed taxonomies and bear normative weight; changes require explicit ADR amendments.

For historical context, see the untracked local `_archive/adr_v0` set. v0 ADRs are preserved for reference but are not authoritative for v1.

## ADR catalog

<!-- BEGIN GENERATED ADR CATALOG -->
| ADR | Status | Title |
| --- | --- | --- |
| [ADR-001](ADR-001-entity-kind-taxonomy.md) | Accepted | Entity Kind Taxonomy |
| [ADR-002](ADR-002-edge-ontology.md) | Accepted | Closed Edge Ontology |
| [ADR-003](ADR-003-system-architecture.md) | Accepted | System Architecture |
| [ADR-004](ADR-004-substrate-observables.md) | Accepted | Substrate Observables |
| [ADR-005](ADR-005-storage-capability-traits.md) | Accepted | Storage Capability Traits |
| [ADR-006](ADR-006-deterministic-scoring.md) | Accepted | Deterministic Scoring |
| [ADR-007 Rev 7](ADR-007-namespace.md) | Accepted | Namespace as Attribution-Only Open String — Dumb Storage, Single Gate, Operator-Configured Read Visibility |
| [ADR-008](ADR-008-query-layer-separation.md) | Accepted | Query Layer Separation |
| [ADR-009](ADR-009-backend-architecture.md) | Accepted | Backend Architecture |
| [ADR-010](ADR-010-kg-versioning.md) | Accepted | KG Versioning Strategy |
| [ADR-011](ADR-011-embedding-and-inference.md) | Accepted | Embedding and Inference Architecture |
| [ADR-012](ADR-012-retrieval-composition.md) | Accepted | Retrieval Composition (High-Level Composition Layer) |
| [ADR-013](ADR-013-note-kind-taxonomy.md) | Accepted | Note Kind Taxonomy |
| [ADR-014](ADR-014-curation-operations.md) | Accepted | Curation Operations |
| [ADR-015](ADR-015-schema-migrations.md) | Accepted | Schema Migrations |
| [ADR-016](ADR-016-request-dsl.md) | Accepted | Request DSL |
| [ADR-017](ADR-017-pack-standard.md) | Accepted | Pack Standard |
| [ADR-018](ADR-018-authorization-gate.md) | Accepted | Authorization Gate |
| [ADR-019](ADR-019-gtd-pack.md) | Accepted | GTD Pack |
| [ADR-020](ADR-020-git-native-kg-implementation.md) | Accepted | Git-Native KG Implementation |
| [ADR-021](ADR-021-memory-pack.md) | Accepted | Memory Pack |
| [ADR-022](ADR-022-events-query-surface.md) | Accepted | Events Query Surface |
| [ADR-023](ADR-023-declarative-pack-format.md) | Accepted | Pack Verb Surface, Visibility, and Composition |
| [ADR-024](ADR-024-fold-cognitive-primitives.md) | Accepted | Fold Cognitive Primitives |
| [ADR-025](ADR-025-verb-speech-acts.md) | Accepted | Verb Surface as Speech-Act Taxonomy |
| [ADR-026](ADR-026-rust-binary-packaging.md) | Accepted | Rust Binary Packaging via Per-Platform npm Subpackages |
| [ADR-027](ADR-027-dynamic-pack-loading.md) | Accepted | Dynamic Pack Loading via Self-Registration |
| [ADR-028](ADR-028-pack-scoped-backends.md) | Accepted | Pack-Scoped Backends and Per-Pack Schema Declaration |
| [ADR-029](ADR-029-substrate-coordinator.md) | Accepted | SubstrateCoordinator — Cross-Backend Operations |
| [ADR-030](ADR-030-retrieval-stack-port.md) | Accepted | Retrieval Stack Port — khive-retrieval |
| [ADR-031](ADR-031-multi-engine-retrieval.md) | Accepted | Multi-Engine Retrieval — Embedder Trait, Registry, Configuration, and Pack Orchestration |
| [ADR-032](ADR-032-brain-profile-orchestration.md) | Accepted | Brain as Profile-Orchestration over Fold + Objective |
| [ADR-033](ADR-033-recall-pipeline.md) | Accepted | Recall Pipeline — Configurable Multi-Stage Memory Retrieval |
| [ADR-034](ADR-034-kg-validation-pipelines.md) | Accepted | KG Validation Pipelines |
| [ADR-035](ADR-035-cli-config-and-auto-embed.md) | Accepted | CLI Configuration and Automatic Embedding |
| [ADR-036](ADR-036-kg-import-export-adapters.md) | Accepted | KG Import/Export Format Adapters |
| [ADR-037](ADR-037-remote-resolution-and-hash-verification.md) | Accepted | Remote Entity Resolution and Content-Hash Verification |
| [ADR-038](ADR-038-bulk-operations.md) | Accepted | Bulk Operations |
| [ADR-039](ADR-039-note-merge.md) | Accepted | Note Merge Operation |
| [ADR-040](ADR-040-communication-and-schedule-packs.md) | Accepted | Communication and Schedule Packs |
| [ADR-041](ADR-041-event-provenance-projection.md) | Accepted | Event Provenance Projection — Hybrid Log + Graph Edges |
| [ADR-042](ADR-042-local-rerank-via-lattice-inference.md) | Accepted | Composable Rerank Pipeline (local cross-encoder + salience + graph-proximity) |
| [ADR-043](ADR-043-embedding-model-migration.md) | Accepted | Embedding Model Migration |
| [ADR-044](ADR-044-vector-store-extensions.md) | Accepted | Vector Store Extensions — Capabilities, Metadata Filter, Batched Search, Update, Orphan Sweep |
| [ADR-045](ADR-045-verb-response-presentation.md) | Accepted | Verb Response Presentation Modes |
| [ADR-046](ADR-046-event-sourced-proposals.md) | Accepted | Event-Sourced Agent KG Proposals |
| [ADR-047](ADR-047-knowledge-pack.md) | Accepted | Knowledge Pack |
| [ADR-048](ADR-048-knowledge-section-profiles.md) | Accepted | Knowledge Section Profiles |
| [ADR-049](ADR-049-khived-daemon.md) | Accepted | khived daemon — persistent warm runtime over a Unix socket |
| [ADR-050](ADR-050-kg-token-namespace-contract.md) | Accepted | KG Token Namespace Contract |
| [ADR-051](ADR-051-section-embeddings-hybrid-compose.md) | Accepted | Section-level embeddings and hybrid compose scoring |
| [ADR-052](ADR-052-ann-production-lifecycle.md) | Accepted | ANN Production Lifecycle -- SQ8 Quantization, Tombstone Delete, Consolidation, Crash-Safe Persistence |
| [ADR-053](ADR-053-authorization-gate.md) | Superseded | Authorization Gate -- ActorStore, SessionStore, and Caller Propagation |
| [ADR-054](ADR-054-ann-build-strategy-scaling-limits.md) | Proposed | ANN Build Strategy and Scaling Limits |
| [ADR-055](ADR-055-epistemic-edge-relations.md) | Accepted | Epistemic Edge Relations — `supports` and `refutes` |
| [ADR-056](ADR-056-channel-transport-layer.md) | Accepted | Channel Transport Layer -- `khive-channel` and External Messaging Adapters |
| [ADR-057](ADR-057-comm-actor-addressed-delivery.md) | Accepted | Comm Actor-Addressed Delivery |
| [ADR-058](ADR-058-brain-posterior-read-path.md) | Accepted | Brain Posterior Read Path — Wiring Profile Posteriors into Recall Ranking |
| [ADR-059](ADR-059-namespace-write-tiers.md) | Withdrawn | Namespace Write Tiers and Cross-Namespace Link Access Control |
| [ADR-061](ADR-061-pack-extensible-by-id-resolution.md) | Accepted | Pack-Extensible by-ID Resolution |
| [ADR-062](ADR-062-fts-ann-consolidation.md) | Accepted | FTS and ANN Consolidation -- Unified Search Tables (Schema V4) |
| [ADR-063](ADR-063-comm-principal-model.md) | Proposed | Comm Pack Principal Model and Remote Backend Isolation |
| [ADR-066](ADR-066-autonomous-merge-pipeline.md) | Proposed | Autonomous Merge Pipeline — Gate Wall as Reviewer |
| [ADR-067](ADR-067-write-owner-daemon.md) | Accepted | Write-Owner Daemon — Single-Writer Task and Write Queue |
| [ADR-068](ADR-068-process-isolation-topology.md) | Proposed | Per-Process Isolation Topology |
| [ADR-069](ADR-069-subject-model.md) | Accepted | The Subject Model -- Domain-Ontology Ingestion and Map Pipeline |
| [ADR-071](ADR-071-backend-pluggable-runtime.md) | Accepted | Backend-Pluggable Runtime — Polystore Restoration |
| [ADR-072](ADR-072-subject-ontologyspec-as-data.md) | Proposed | Subject OntologySpec as Runtime Data -- Verbless Verticals and Pack Retirement |
| [ADR-073](ADR-073-pack-core-backend-accessor.md) | Accepted | Pack Core-Backend Accessor |
| [ADR-074](ADR-074-graph-aware-recall.md) | Proposed | Graph-Aware Recall — Graph-Proximity Signal in Memory Retrieval |
| [ADR-075](ADR-075-owl-rdf-interoperability.md) | Draft | OWL/RDF Interoperability -- Publishing the khive Vocabulary and Aligning with External Ontologies |
| [ADR-076](ADR-076-relation-calculability-and-system-role.md) | Accepted | Relation-Set Calculability — System Role and the Non-Redundancy Certificate |
| [ADR-078](ADR-078-output-format-shape-aware-rendering.md) | Accepted | Output Format and Shape-Aware Rendering |
| [ADR-079](ADR-079-ann-persistence-warm-path-integration.md) | Accepted | ANN Persistence Warm-Path Integration — Wiring v2 Persistence into the Daemon |
| [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md) | Accepted | Session Pack — OSS Storage Mechanism |
| [ADR-081](ADR-081-recall-retune-driver.md) | Accepted | Recall Retune Driver — Governed Ingestion of Implicit Feedback |
| [ADR-082](ADR-082-retrieval-quality-measurement-loop.md) | Proposed | Retrieval Quality Measurement Loop |
| [ADR-083](ADR-083-session-pack-t1-verbs.md) | Accepted | Session Pack T1 Verb Surface |
| [ADR-084](ADR-084-verb-surface-consistency.md) | Proposed | Verb-Surface Consistency Contract and Live Ontology Introspection |
| [ADR-085](ADR-085-code-pack.md) | Accepted | Code Pack — Source-Code Ontology and Audit-Finding Vocabulary |
| [ADR-086](ADR-086-doc-file-pack.md) | Proposed | Document/File Modeling — Content on the Existing `document` Entity Kind |
| [ADR-087](ADR-087-workspace-mirror.md) | Accepted | Workspace Mirror — Folding `.khive/` Into the Graph Substrate |
| [ADR-088 Amendment 1](ADR-088-amendment-1-git-digest.md) | Accepted | `git.digest` — Agent-Facing Digest Verb with Remote-URL Support |
| [ADR-088 Amendment 2](ADR-088-amendment-2-anchor-identity.md) | Proposed | Canonical Repo-Anchor Identity for `git.digest` |
| [ADR-088](ADR-088-git-lifecycle-pack.md) | Accepted | Git-Lifecycle Pack — Commit and Issue Note Kinds |
| [ADR-089](ADR-089-context-verb.md) | Accepted | `context` verb — entity-anchored graph context in one call |
| [ADR-090](ADR-090-docs-site-standard.md) | Accepted | Docs site standard — navigation, agent md/txt surfaces, visual style |
| [ADR-091](ADR-091-wal-snapshot-lifetime.md) | Accepted | Bounded read-transaction lifetime and WAL checkpoint escalation |
| [ADR-092](ADR-092-context-composer.md) | Proposed | Cross-substrate context composer — ContextContributor trait + `context.assemble` |
| [ADR-093](ADR-093-sessions-raw-zstd-compression.md) | Proposed | zstd Compression for Session-Mirror Raw Storage |
| [ADR-094](ADR-094-lifecycle-telemetry-events.md) | Accepted | Sequencing-Assertable Lifecycle Telemetry Events |
| [ADR-095](ADR-095-verb-surface-consolidation.md) | Proposed | Verb-Surface Consolidation and Field-Validation Governance |
| [ADR-096](ADR-096-warm-daemon-per-request-identity.md) | Accepted | warm daemon per-request identity — serving many attribution identities over one shared backend |
| [ADR-099](ADR-099-bulk-apply-atomic-units.md) | Accepted | Cross-Op Atomicity for Bulk Apply — Prepared Write Plans over the Single-Writer Seam |
| [ADR-100](ADR-100-store-backup-replication.md) | Accepted | Store backup and replication |
| [ADR-101](ADR-101-kg-changeset-model.md) | Accepted | KG Change-Set Model — Producer-Agnostic Op-List with Stage-Time Stable IDs |
| [ADR-102](ADR-102-tiered-validate-and-merge.md) | Accepted | Tiered Validate-and-Merge — Rule-Gated Fast Path and Reviewed Change-Set Path |
| [ADR-103](ADR-103-resource-attribution-model.md) | Accepted | Resource Attribution Model |
| [ADR-104](ADR-104-posterior-serving-recall.md) | Accepted | Profile Posteriors in the Recall Read Path |
| [ADR-105](ADR-105-cross-node-comm-transport.md) | Accepted | Cross-node comm transport (node channel adapter + hub ingress) |
| [ADR-106](ADR-106-schedule-pack-executor.md) | Accepted | Schedule Pack Executor — Daemon-Resident Tick for the Pending-Event Drain |
| [ADR-107](ADR-107-memory-ann-lifecycle.md) | Accepted | Memory ANN Lifecycle — Eventual Consistency Contract |
| [ADR-108](ADR-108-git-write-surface.md) | Accepted | Git Write Surface Through khive (Phase B) |
| [ADR-109](ADR-109-sandboxed-kkernel-gateway.md) | Proposed | Sandboxed kkernel Gateway for Untrusted Execution (Phase C) |
| [ADR-110](ADR-110-vamana-wasm.md) | Proposed | WebAssembly Support for khive-vamana |
| [ADR-111](ADR-111-blob-store.md) | Accepted | BlobStore — Content-Addressed Binary Object Storage |
| [ADR-112](ADR-112-git-outbound-publish-verbs.md) | Proposed | Outbound GitHub Publish Verbs with a Publication-Hygiene Scan |
| [ADR-113](ADR-113-identifier-continuity.md) | Proposed | Identifier Continuity — Merged-Entity Redirect Resolution and Split Endpoint-Move |
| [ADR-114](ADR-114-code-audit-derived-report.md) | Accepted | Code-Audit Derived Report, Not Agent Findings |
| [ADR-115](ADR-115-secret-gate-content-manifest-exemption.md) | Accepted | Exact-Content Manifest Exemption for the Write Secret Gate |
| [ADR-117](ADR-117-session-continuity-search.md) | Proposed | Session Continuity — Cross-Session Search and Remote Ingestion |
| [ADR-117a](ADR-117a-session-identity-tenant-isolation.md) | Proposed | Session Identity and Tenant Isolation |
| [ADR-118](ADR-118-fresh-tail-recall-visibility.md) | Proposed | Fresh-Tail Exact Leg — Read-Your-Writes Visibility for Vector Recall |
| [ADR-119](ADR-119-daemon-component-supervision.md) | Accepted | Host-Supervised Daemon Components Beside the Verb Plane |
| [ADR-120](ADR-120-khive-flow-control-flow-envelope.md) | Proposed | Khive Flow — A Bounded Control-Flow Envelope in the Request DSL |
| [ADR-121](ADR-121-attachments-first-class.md) | Proposed | Attachments — Role-Keyed Blob Renditions as a First-Class Substrate Property |
| [ADR-122](ADR-122-email-outbound-delivery.md) | Accepted | Email outbound delivery — outbox contract and supervised delivery component |
| [ADR-123](ADR-123-comm-forward.md) | Proposed | comm.forward — Provenance-Preserving Message Forwarding |
| [ADR-124](ADR-124-note-write-identity.md) | Proposed | Note-Write Identity — Deriving Pack-Owned Identity Properties at the Write |
| [ADR-125](ADR-125-reserved-property-keys.md) | Proposed | Reserved property keys on pack-owned note kinds |
| [ADR-127](ADR-127-authenticated-actor-and-grant-primitive.md) | Accepted | Authenticated actor and grant primitive |
| [ADR-128](ADR-128-custody-party-pairs-and-slot-authenticity.md) | Accepted | Custody party-pairs and slot authenticity |
| [ADR-129](ADR-129-fail-closed-gate-default.md) | Accepted | Fail-closed authorization gate default |
| [ADR-130](ADR-130-search-response-completeness-and-ranking-evidence.md) | Accepted | Search response completeness and ranking evidence |
| [ADR-131](ADR-131-batch-write-admission-control.md) | Accepted | Admission control for parallel write batches |
| [ADR-133](ADR-133-incidental-writes-off-the-request-hot-path.md) | Proposed | Reduce writer acquisitions on the request path |
| [ADR-134](ADR-134-store-durability-posture.md) | Proposed | Store durability posture, and the obligation it carries for accounting records |
<!-- END GENERATED ADR CATALOG -->

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
