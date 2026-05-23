# ADR-010: KG Versioning Strategy

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

khive is a research knowledge graph. Research evolves: entities are revised, edges are
reweighted, entire subgraphs are restructured as understanding deepens. Without versioning,
a KG is a single mutable snapshot with no history, no rollback, and no collaboration.

The versioning strategy must satisfy:

1. **Agent-native.** AI agents are the primary graph writers. They need durable, diffable,
   replayable research memory. Versioning must work without human intervention.
2. **Human-reviewable.** Humans need to review graph changes using familiar workflows.
   `git diff`, pull requests, and code review tools should work on KG data.
3. **Git-native transport.** Git is the remote protocol. Git credentials are the auth layer.
   No custom sync server in v1.
4. **Deterministic identity.** Snapshot identity must be deterministic — same graph state
   produces the same hash. This enables deduplication, cache validation, and merge-base
   detection.
5. **Federation-aware future.** Multi-backend deployments (ADR-029, Substrate Coordinator) will need federated
   snapshots. The v1 design must not preclude this.

## Decision

### Strategic position: "GitHub for Knowledge Graphs"

khive's versioning strategy serves two audiences:

1. **Agents** need durable, diffable, replayable research memory.
2. **Humans** need reviewable graph changes using familiar GitHub workflows.

"GitHub for knowledge graphs" is the external strategic framing. The implementation is
agent-native: agents write KG deltas, git stores them, humans review them through GitHub
plus khive semantic tooling.

### Git-native versioning (ADR-020)

v1 versioning is git-native. KG state is serialized as NDJSON files in a git repository:

```text
.khive/kg/
  entities.ndjson    — one entity per line
  edges.ndjson       — one edge per line
  schema.yaml        — ontology declaration (kinds, relations, constraints)
```

Git provides: branches, commits, log, diff, merge, remotes, access control, CI.
`kkernel sync` rebuilds a queryable SQLite database from the NDJSON sources.

There is no custom `khive-vcs` command set and no custom `khive-sync` HTTP server in v1.

### ADR-010 as strategic root

ADR-010 is the strategic root for khive KG versioning. It explains the direction and tracks
implementation status. It does not inline the serialization format, merge algorithm, remote
transport, or canonical hash algorithm in detail. Those are owned by implementation ADRs.

### Implementation status

| Area                                | Status                    | Authority                            |
| ----------------------------------- | ------------------------- | ------------------------------------ |
| NDJSON KG export/import             | Shipped                   | ADR-020                              |
| `schema.yaml` ontology file         | Shipped                   | ADR-020                              |
| Git branches / commits / log / diff | Shipped (through git)     | ADR-020                              |
| `kkernel sync` DB rebuild           | Shipped                   | ADR-020                              |
| `KgArchive` representation          | Retained                  | ADR-042 / ADR-020                    |
| Canonical hash algorithm            | Retained, under-specified | ADR-042; future canonicalization ADR |
| Custom `khive-vcs` commands         | Superseded                | ADR-020                              |
| Custom `khive-sync` HTTP server     | Superseded for v1         | ADR-020                              |
| Custom merge engine                 | Superseded for v1         | ADR-020; conflict taxonomy retained  |
| Conflict resolution UX              | Future                    | Future conflict resolution ADR       |
| Federated snapshots                 | Future                    | New federation ADR                   |
| Notes versioning                    | Future (v2)               | New note export/versioning ADR       |
| Semantic review / PR enrichment     | Future (v2)               | New review ADR                       |

### Authority chain

```text
ADR-010: strategic root (this ADR)
  └── ADR-020: current implementation contract (git-native versioning)
        ├── ADR-042: retained for KgArchive + content-hash algorithm
        │     (until superseded by canonicalization ADR)
        └── ADR-043: conflict taxonomy retained for future conflict resolution ADR
              (custom merge engine superseded for v1)
```

### Snapshot coverage

v1 snapshots cover entities and edges only. Notes, tasks, memories, and events are excluded.

```rust
pub struct SnapshotCoverage {
    pub entities: bool,
    pub edges: bool,
    pub notes: bool,
}

pub const KG_V1_COVERAGE: SnapshotCoverage = SnapshotCoverage {
    entities: true,
    edges: true,
    notes: false,
};
```

`SnapshotCoverage` participates in snapshot identity metadata. Two archives with identical
entity/edge content but different coverage claims are not semantically equivalent:

```text
A: entities+edges complete, notes not covered   → coverage = { entities: true, edges: true, notes: false }
B: entities+edges complete, notes covered+empty → coverage = { entities: true, edges: true, notes: true }
```

A claims "I don't track notes." B claims "I track notes and there are none." These are
different statements about completeness.

`notes: true` becomes eligible only when the relevant note packs define versioned export,
import, validation, privacy/redaction, and merge semantics. This is v2 work.

### Canonical hash algorithm

Snapshot identity depends on a versioned canonical serialization algorithm. ADR-010
requires a frozen `khive-canon-json-v1` spec with golden test vectors, but does not define
the spec inline. A dedicated canonicalization ADR owns that contract.

The spec must define: root key order, sort keys for entity/edge arrays, field order within
records, UUID format, float encoding, Unicode normalization policy, algorithm labels, and
golden test vectors for round-trip verification.

### Federated versioning: future work

`KgArchive` remains a single-backend archive. It captures entities and edges from one
SQLite backend.

Federated versioning uses a `FederatedSnapshotManifest` as the commit root. A federated
branch head points to a manifest ID, not directly to one backend snapshot ID.

```json
{
  "algorithm": "khive-canon-json-v1",
  "coverage": { "entities": true, "edges": true, "notes": false },
  "backends": [
    { "backend_id": "main", "archive_hash": "..." },
    { "backend_id": "lore", "archive_hash": "..." }
  ]
}
```

The manifest hash changes when either a backend archive changes OR a logical archive is
moved to a different backend binding. This fixes the hash non-injectivity defect where
moving an entity to a different backend would leave the hash unchanged.

The manifest schema, branch pointer model, backend mapping, and validation rules need
their own ADR. ADR-010 states the intent and the invariant; the implementation details
are deferred.

### Remote protocol

```text
v1: git is the remote protocol. Git credentials are the auth layer.
    There is no khive-sync HTTP server.
```

Federated remotes are specified by the future `FederatedSnapshotManifest` ADR. Candidate
layouts include one repo with per-backend archive directories, a manifest repo referencing
backend repos, or another git-native layout. ADR-010 does not choose among them.

### Social layer

v1 social layer is delegated to GitHub: forks, branches, pull requests, review comments,
CI, and access control use existing git hosting.

khive-native social work is limited to semantic enrichment:

- `khive kg review` — semantic PR summary (entity/edge diff with kind-aware context)
- Conflict explanations — human-readable descriptions of merge conflicts
- Validation reports — ontology constraint checks on proposed changes
- Entity/edge diff visualization

khive does not build a GitHub replacement. The value is semantic enrichment on top of
existing git workflows, not a parallel collaboration platform.

### Data privacy in NDJSON snapshots

NDJSON files in git are permanent plaintext. Git history preserves every prior version —
a `hard-delete` removes a record from the live database but the NDJSON at a prior commit
remains in git history unless a force-push rewrite is performed.

Data classes that may appear in NDJSON snapshots:

| Data class        | Example                          | Sensitivity                                   |
| ----------------- | -------------------------------- | --------------------------------------------- |
| Entity names      | "Sinkhorn Distances", "Ocean Li" | Low–Medium (Person entities carry real names) |
| Entity properties | JSON key-value metadata          | Variable (may contain internal notes)         |
| Edge properties   | `dependency_kind`, `weight`      | Low                                           |
| Edge topology     | Which entities are linked        | Medium (reveals research structure)           |

For hosted deployments:

- Private git repositories only. Do not push NDJSON snapshots to public repos.
- Consider content encryption at the NDJSON layer before git commit if the deployment
  handles sensitive entity data.
- Document which entity kinds and property keys may contain PII or proprietary content.

### MergeStrategy naming

Two distinct merge concepts use the name "MergeStrategy" in the codebase:

1. **Entity deduplication merge** (`curation.rs`): `PreferInto` / `PreferFrom` / `Union`
2. **KG snapshot merge** (VCS): `Ours` / `Theirs` / `Auto`

These are semantically unrelated. To prevent confusion:

- Entity deduplication: rename to `EntityDedupMergePolicy`
- KG snapshot merge: rename to `SnapshotMergeStrategy`

## Rationale

### Why git-native (not custom VCS)?

Git is ubiquitous. Every developer knows `git diff`, `git log`, `git merge`. GitHub
provides pull requests, code review, CI, and access control out of the box. Building
custom equivalents is the largest possible investment for the smallest marginal gain.

NDJSON files in a git repo give agents and humans a shared representation that both can
read, write, and diff. The `kkernel sync` step rebuilds the queryable database from the
canonical NDJSON source.

### Why entities + edges only in v1?

Notes carry temporal, cognitive, and potentially private content (memories, observations,
task state). Versioning notes requires export/import format decisions, privacy/redaction
policy, and merge semantics for mutable temporal records. These are substantially harder
than versioning entities and edges, which are comparatively static graph structure.

Excluding notes in v1 ships versioning sooner. The `SnapshotCoverage` field makes the
exclusion explicit rather than silent.

### Why strategic root (not museum piece)?

ADR-010 is the document that answers "what is khive's versioning story?" If it is marked
superseded, contributors must chase a chain of ADRs to reconstruct the strategy. If it
is a museum piece, it actively misleads by describing plans that no longer hold.

As a strategic root, ADR-010 stays current: it tracks what shipped, what was superseded,
and what is future work. Implementation contracts live in child ADRs.

### Why fix hash non-injectivity at manifest layer?

The defect is that moving an entity to a different backend leaves the single-backend
`KgArchive` hash unchanged. Putting `backend_id` into every entity/edge record inside
`KgArchive` would mix federated placement into the single-backend archive type.

The `FederatedSnapshotManifest` is the right layer: it captures backend placement as
part of the federated commit identity. `KgArchive` remains a clean single-backend archive.

### Why GitHub for social (not khive-native)?

Building fork/PR/review/CI infrastructure is years of work. GitHub already provides it.
khive's competitive advantage is semantic understanding of graph changes, not a
collaboration platform. Semantic enrichment (diff visualization, conflict explanation,
validation reports) adds value on top of GitHub without replacing it.

## Alternatives Considered

| Alternative                           | Why rejected                                                                                                       |
| ------------------------------------- | ------------------------------------------------------------------------------------------------------------------ |
| Custom `khive-vcs` commands           | Git provides branching, merging, remoting natively. Custom commands duplicate without improving.                   |
| Custom `khive-sync` HTTP server       | Git hosting (GitHub, GitLab) provides remotes with auth, CI, and collaboration. Custom server is unjustified.      |
| Custom merge engine in v1             | Git merge on NDJSON handles structural conflicts. Semantic merge enrichment is v2.                                 |
| Museum-piece ADR-010                  | Contributors lose the strategic overview. They must reconstruct from child ADRs.                                   |
| Mark ADR-010 superseded               | Misleads: the strategy is alive, only the implementation mechanism changed.                                        |
| Notes in v1 snapshots                 | Notes require export format, privacy policy, and merge semantics that are not designed. Ship entities+edges first. |
| Fix hash non-injectivity in KgArchive | Mixes federation placement into single-backend archive. Wrong layer.                                               |
| khive-native GitHub replacement       | Largest possible investment. GitHub already handles collaboration. Add semantic enrichment instead.                |

## Consequences

### Positive

- Git-native versioning works today with zero custom infrastructure.
- Agents and humans share the same representation (NDJSON in git).
- `SnapshotCoverage` makes data exclusions explicit, not silent.
- Authority chain is documented — contributors know which ADR governs what.
- Hash non-injectivity has a clean future fix at the manifest layer.
- Social layer leverages GitHub without reinventing it.

### Negative

- NDJSON files can be large for big graphs. Git performance degrades with very large files.
  Mitigated: git LFS or archive splitting if needed.
- Notes excluded from v1 means agent memories are not versioned.
  Mitigated: explicit exclusion via `SnapshotCoverage`. Notes versioning is v2.
- Canonical hash algorithm is under-specified until the canonicalization ADR ships.
  Mitigated: v1 hashing works; the spec just needs to be frozen with golden vectors.

### Neutral

- `KgArchive` representation unchanged from ADR-042.
- Git as remote protocol is already the v1 implementation.
- Conflict taxonomy from ADR-043 retained for future `khive kg resolve`.
