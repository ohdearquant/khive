# ADR-042: KG Versioning Implementation — Snapshots, Branches, and Remotes

**Status**: proposed\
**Date**: 2026-05-19\
**Authors**: Ocean, lambda:khive

## Context

ADR-010 established "GitHub for knowledge graphs" as the strategic direction. ADR-015 designed the
versioning model at the conceptual level: snapshot-based commits, pointer-style branches, three-way
merge, and seven MCP tools (`commit`, `branch`, `checkout`, `merge_branch`, `log`, `diff`,
`apply_diff`). ADR-017 specified the diff format.

What ADR-015 left open:

1. **Remote protocol**: no concrete push/pull transport defined.
2. **Delta migration path**: ADR-015 chose full-snapshot storage for v0.1 and deferred delta
   storage to v0.4+, but the migration contract was not specified.
3. **Implementation boundaries**: which code lives where, how snapshots are referenced across
   crates, what the `khive-vcs` crate boundary looks like in Rust terms.
4. **Content-addressing details**: how the SHA-256 hash is computed deterministically, what the
   canonical JSON serialization order is, how hash collisions are handled.
5. **Index consistency during checkout**: the FTS5 and vector stores go stale when `checkout`
   replaces the live namespace; the recovery procedure was not designed.

This ADR fills those gaps. It is the implementation contract for the `khive-vcs` crate introduced
in ADR-015.

### Scope boundary with ADR-015

ADR-015 is the _design_ ADR for the versioning model. This ADR is the _implementation_ ADR. Where
ADR-015 says "store snapshots in `kg_snapshots`," this ADR says exactly what the hash input
algorithm is and how the archive table is split. Where ADR-015 says "push/pull to remotes," this
ADR defines the wire protocol. Decisions in ADR-015 are not re-opened here; this ADR only adds
precision where ADR-015 left blanks.

### Relationship to issues

This ADR addresses GitHub issue #2 (KG versioning: snapshots, branches, and remotes).

## Decision

### 1. Snapshot content-hash algorithm

**Decision: SHA-256 of the deterministically serialized canonical JSON of all entities and edges in
the namespace, sorted by entity UUID then by edge composite key.**

The canonical serialization rule:

1. Collect all non-soft-deleted entities from the namespace, sort by `id` (UUID string,
   case-insensitive ascending).
2. For each entity, serialize as: `{"id","kind","name","description","properties","tags"}` with
   keys in that fixed order, `properties` keys sorted alphabetically, `tags` sorted
   lexicographically.
3. Collect all edges from the namespace, sort by `(source_id, target_id, relation)` (all
   ascending, lexicographic).
4. For each edge, serialize as: `{"source","target","relation","weight"}` with keys in that fixed
   order.
5. Produce the final JSON: `{"entities":[...],"edges":[...]}`. No whitespace.
6. Hash the UTF-8 bytes with SHA-256. Prefix the hex digest with `"sha256:"`.

**Why this exact algorithm**:

- UUID-sorted entity order is deterministic across all implementations and platforms.
- Fixed key order in object serialization removes `serde` field ordering as a hash input variable.
- Soft-deleted entities are excluded because the snapshot represents _live_ graph state, not the
  mutation log.
- The `sha256:` prefix enables future hash algorithm migration without changing the ID namespace
  (a `blake3:` prefix would be unambiguous).

**Collision handling**: SHA-256 collisions are computationally infeasible with present technology.
If two distinct namespace states produce the same hash (impossible in practice), the second
`commit` will fail with a `SnapshotAlreadyExists` error. The caller must investigate — this is not
silently overwritten.

### 2. Snapshot storage split

**Decision: `kg_snapshots` holds metadata; `kg_snapshot_archives` holds the serialized archive.
All `log` queries hit only `kg_snapshots`. Only `checkout` and `diff` read `kg_snapshot_archives`.**

SQL schema (extends ADR-015 §Implementation with the `archive_id` FK clarification):

```sql
CREATE TABLE kg_snapshots (
    id           TEXT    PRIMARY KEY,           -- "sha256:<hex>"
    namespace    TEXT    NOT NULL,
    parent_id    TEXT    REFERENCES kg_snapshots(id),
    message      TEXT    NOT NULL DEFAULT '',
    author       TEXT,
    created_at   INTEGER NOT NULL,              -- Unix microseconds (i64)
    entity_count INTEGER NOT NULL DEFAULT 0,
    edge_count   INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE kg_snapshot_archives (
    snapshot_id  TEXT    PRIMARY KEY REFERENCES kg_snapshots(id) ON DELETE CASCADE,
    archive_json TEXT    NOT NULL                -- KgArchive serialization
);

CREATE TABLE kg_branches (
    namespace    TEXT    NOT NULL,
    name         TEXT    NOT NULL,
    head_id      TEXT    NOT NULL REFERENCES kg_snapshots(id),
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    PRIMARY KEY (namespace, name)
);

CREATE INDEX idx_snapshots_ns_created
    ON kg_snapshots(namespace, created_at DESC);

CREATE INDEX idx_snapshots_parent
    ON kg_snapshots(parent_id)
    WHERE parent_id IS NOT NULL;
```

The `idx_snapshots_parent` index accelerates the LCA scan for three-way merge (ADR-043).

**Archive size budget**: `archive_json` is a SQLite TEXT blob. SQLite does not impose a per-row
size limit in WAL mode (the practical limit is the 2 GB maximum database file size). For
namespaces with 100K entities at 600 bytes/entity average, one snapshot archive is approximately
60 MB. Three simultaneous in-flight archives (base + ours + theirs for a merge) is 180 MB resident
in memory. This is acceptable for a desktop tool; document the limit clearly.

### 3. `khive-vcs` crate boundary

**Decision: a new `crates/khive-vcs` crate holds all versioning types and operations. It depends
on `khive-types`, `khive-storage`, and `khive-runtime`. It does NOT depend on `khive-diff` (v0.4)
or `khive-merge` (v0.5).**

Directory layout:

```
crates/khive-vcs/
├── Cargo.toml
└── src/
    ├── lib.rs           — re-exports; feature flags
    ├── types.rs         — KgSnapshot, KgBranch, SnapshotId, RemoteConfig
    ├── hash.rs          — canonical_json() + sha256_snapshot()
    ├── snapshot.rs      — commit() operation
    ├── branch.rs        — branch(), checkout(), list_branches(), current_branch()
    ├── log.rs           — log() operation
    ├── remote.rs        — RemoteClient trait + HTTP transport
    ├── push.rs          — push() operation
    ├── pull.rs          — pull() operation
    └── migrations.rs    — ServiceSchemaPlan migration for kg_snapshots + kg_branches
```

The merge algorithm lives in `khive-merge` (ADR-043), not here. `khive-vcs` calls into
`khive-merge` via a trait object (`MergeEngine`) to avoid a hard crate dependency before
`khive-merge` ships.

```rust
/// Pluggable merge engine — implementations live in khive-merge (v0.5).
/// The NoOpMergeEngine ships with khive-vcs and returns NotImplemented for
/// all calls; it is replaced when khive-merge is linked.
pub trait MergeEngine: Send + Sync {
    fn merge_branch(
        &self,
        base: &KgArchive,
        ours: &KgArchive,
        theirs: &KgArchive,
        strategy: MergeStrategy,
    ) -> MergeResult;
}
```

This lets `khive-vcs` ship independently of `khive-merge` without `merge_branch` causing a link
error.

### 4. Rust types

```rust
/// Content-addressed snapshot identifier.
///
/// Invariant: always starts with "sha256:" followed by 64 hex characters.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn from_hash(hex: &str) -> Result<Self, InvalidSnapshotId>;
    pub fn as_str(&self) -> &str;
}

/// Immutable point-in-time capture of a namespace's entity and edge set.
#[derive(Clone, Debug)]
pub struct KgSnapshot {
    pub id: SnapshotId,
    pub namespace: String,
    pub parent_id: Option<SnapshotId>,
    pub message: String,
    pub author: Option<String>,
    pub created_at: i64,       // Unix microseconds
    pub entity_count: u64,
    pub edge_count: u64,
}

/// Named mutable pointer to a snapshot within a namespace.
#[derive(Clone, Debug)]
pub struct KgBranch {
    pub namespace: String,
    pub name: String,
    pub head_id: SnapshotId,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Connection parameters for a remote khive instance.
#[derive(Clone, Debug)]
pub struct RemoteConfig {
    pub name: String,        // "origin"
    pub url: String,         // "https://khive.example.com"
    pub auth: RemoteAuth,
    pub namespace_map: Option<(String, String)>  // (local_ns, remote_ns)
}

#[derive(Clone, Debug)]
pub enum RemoteAuth {
    None,
    Bearer(String),          // API key
    Basic { user: String, password: String },
}
```

### 5. Remote protocol

**Decision: HTTP/HTTPS with a `khive-sync` JSON API. No gRPC or custom binary protocol in v0.1.**

The remote protocol is the simplest possible: a small REST API over HTTPS that lets two khive
instances exchange `KgArchive` payloads.

#### 5.1 Server-side endpoints (served by `khive-vcs` when `--sync-server` flag is active)

```
GET  /v1/sync/{namespace}/snapshots
     → [{id, parent_id, message, author, created_at, entity_count, edge_count}]
     Lists all snapshot metadata for the namespace. No archive JSON.

GET  /v1/sync/{namespace}/snapshots/{snapshot_id}
     → KgArchive JSON
     Returns the full archive for one snapshot.

GET  /v1/sync/{namespace}/branches
     → [{namespace, name, head_id, ...}]
     Lists all branches.

POST /v1/sync/{namespace}/snapshots
     Body: KgArchive JSON + parent_id (optional) + message + author
     → {id: "sha256:..."}
     Stores a snapshot. The server verifies the hash matches the content.
     Returns 409 Conflict if the snapshot_id already exists with different content.

POST /v1/sync/{namespace}/branches/{branch_name}
     Body: {head_id: "sha256:..."}
     → {}
     Advance a branch pointer (fast-forward only — no force push in v0.1).
```

Authentication: `Authorization: Bearer <api_key>` header. If absent or invalid, 401.

Namespace access: a namespace on the remote is always the remote's own namespace. The client maps
its local namespace to the remote's namespace via `RemoteConfig.namespace_map`.

#### 5.2 `push` operation

```rust
pub async fn push(
    runtime: &KhiveRuntime,
    remote: &RemoteConfig,
    local_namespace: &str,
    branch_name: &str,
) -> Result<PushSummary, VcsError>;
```

Algorithm:

1. Load the local branch HEAD snapshot.
2. Fetch the remote's snapshot metadata list.
3. Compute the set of local snapshots that the remote does not have (walk the parent chain from
   HEAD until a snapshot_id the remote already has, or until the root).
4. For each missing snapshot (oldest first), POST its `KgArchive` to the remote.
5. POST the branch HEAD update to the remote.

**Fast-forward only**: if the remote branch HEAD is not an ancestor of the local HEAD, `push`
fails with `NonFastForward`. The caller must `pull` first, resolve any merge conflicts, commit,
then push. This prevents overwriting diverged remote work without an explicit decision.

#### 5.3 `pull` operation

```rust
pub async fn pull(
    runtime: &KhiveRuntime,
    remote: &RemoteConfig,
    local_namespace: &str,
    branch_name: &str,
) -> Result<PullSummary, VcsError>;
```

Algorithm:

1. Fetch the remote's branch HEAD snapshot_id.
2. If the local branch HEAD equals the remote HEAD, return `PullSummary { already_up_to_date: true }`.
3. Compute the set of remote snapshots that local does not have (walk the remote's parent chain
   until a snapshot_id local already has, or until the root).
4. Download each missing snapshot archive and store locally in `kg_snapshot_archives`.
5. Store each snapshot record in `kg_snapshots`.
6. Do NOT auto-merge. Return `PullSummary { fetched_snapshots, remote_head_id }`.
7. The caller is responsible for deciding whether to `checkout` the remote HEAD directly (if local
   has no diverged work) or `merge_branch` from the remote HEAD.

Pull is a fetch, not a fetch-and-merge. This is deliberate: automatic merge-on-pull would hide
conflicts. The agent decides what to do with the remote's snapshots after fetching.

#### 5.4 Error taxonomy

```rust
pub enum VcsError {
    SnapshotAlreadyExists(SnapshotId),     // hash collision (impossible) or double-commit
    SnapshotNotFound(SnapshotId),          // archive deleted or never fetched
    BranchNotFound { namespace: String, name: String },
    NonFastForward { local_head: SnapshotId, remote_head: SnapshotId },
    RemoteUnreachable { url: String, cause: String },
    AuthFailed { url: String },
    HashMismatch { expected: SnapshotId, actual: SnapshotId },
    MergeRequired,                         // pull fetched diverged history
    UncommittedChanges { count: usize },   // checkout blocked by dirty working state
    MergeNotImplemented,                   // merge_branch called before khive-merge ships
    Storage(StorageError),
    Io(std::io::Error),
}
```

### 6. Delta storage migration path

**Decision: delta storage is the designated v0.5 upgrade. The migration contract is versioned via
the `kg_snapshot_archives.format` column.**

Add a `format` column to `kg_snapshot_archives`:

```sql
ALTER TABLE kg_snapshot_archives
    ADD COLUMN format TEXT NOT NULL DEFAULT 'full';
-- Values: 'full' (KgArchive JSON), 'delta' (GraphDiff JSON against parent_id)
```

When `format = 'delta'`, the archive column contains a `GraphDiff` (ADR-017 format), not a
`KgArchive`. Reconstruction of the full state requires walking the parent chain until a `'full'`
snapshot is found, then applying each `GraphDiff` in sequence.

The `full` format remains the default in v0.1 and v0.4. In v0.5 (when `khive-diff` ships), the
`commit` operation may store `'delta'` for non-root snapshots while keeping a `'full'` snapshot
every N commits (configurable; default N=50) as a "checkpoint" to bound reconstruction depth.

This migration is schema-additive (new column with a default). It does not break existing `v0.1`
snapshot records.

### 7. Index consistency during checkout

**Decision: `checkout` triggers a synchronous index rebuild for FTS5 and an async re-embed for the
vector store.**

When `checkout` restores a snapshot, it replaces the entities and edges tables. The FTS5 index and
vector store are now stale. Two possible policies:

1. **Rebuild synchronously** — `checkout` blocks until indexes are consistent. Correct but slow for
   large namespaces.
2. **Rebuild asynchronously** — `checkout` returns immediately; indexes rebuild in the background.
   Fast but the agent may see stale search results immediately after checkout.

**Decision**: FTS5 is rebuilt synchronously (it is fast — a scan + INSERT INTO fts5 for 10K
entities takes < 1 second on a laptop). Vector re-embedding is async (it requires model inference;
at 100K entities this could take minutes). The `checkout` response includes a
`vector_index_status: "rebuilding"` field when the vector store is not yet consistent, so callers
can decide whether to wait.

The vector rebuild is implemented as a background task in the `khive-runtime` thread pool. Its
completion can be polled via a future `status` verb (deferred to v0.2).

### 8. Dirty-working-state detection

**Decision: a `kg_vcs_state` table tracks the last-committed snapshot ID per namespace. The
`checkout` guard compares the current live entity+edge count and content hash against the last
committed snapshot.**

```sql
CREATE TABLE kg_vcs_state (
    namespace    TEXT    PRIMARY KEY,
    current_branch TEXT,                -- NULL if in detached HEAD state
    last_committed_id TEXT REFERENCES kg_snapshots(id),
    dirty        INTEGER NOT NULL DEFAULT 0  -- 1 if uncommitted changes exist
);
```

The `dirty` flag is set to `1` by any write operation (`create`, `update`, `delete`, `link`) and
cleared to `0` by `commit`. This is cheaper than recomputing the hash on every `checkout` call.

**Trade-off**: if the process dies between a write and setting `dirty = 1`, the flag is stale (it
says 0 but there are uncommitted changes). Since the flag is written in the same SQLite transaction
as the entity write, this scenario cannot occur in WAL mode with `PRAGMA synchronous = NORMAL`.

### 9. MCP tool additions

This ADR adds two new MCP tools (`push`, `pull`) beyond ADR-015's seven. These are wired in
`crates/khive-mcp/src/tools/vcs.rs` alongside `commit`, `branch`, `checkout`, `merge_branch`,
`log`, `diff`, `apply_diff`.

**`push`**

```
Parameters:
  namespace: string (optional, default "local")
  remote: string (required) — remote name as configured in .khive/remotes.toml
  branch: string (optional, default current branch)
  force: boolean (optional, default false) — RESERVED; not honoured in v0.1

Returns: {snapshots_pushed: integer, branch_updated: boolean}
```

**`pull`**

```
Parameters:
  namespace: string (optional, default "local")
  remote: string (required) — remote name
  branch: string (optional, default current branch)

Returns: {fetched_snapshots: integer, remote_head_id: string, already_up_to_date: boolean}
```

Remote configuration (stored in `.khive/remotes.toml`, not in the database):

```toml
[[remote]]
name = "origin"
url  = "https://khive.example.com"
auth = { type = "bearer", token_env = "KHIVE_REMOTE_TOKEN" }
namespace_map = ["local", "shared/llm-research"]
```

Remotes are not stored in the database to keep the database portable — a cloned archive does not
carry remote configuration.

## Rationale

### Why HTTP/HTTPS over gRPC for the remote protocol?

gRPC provides better streaming for large payloads, but adds `protobuf` compilation and a generated
client as dependencies. For a v0.1 implementation where "large payloads" means 60 MB JSON archives
(not gigabytes), HTTP is simpler to implement, simpler to debug (curl-testable), and easier to
proxy through standard infrastructure (nginx, Cloudflare). gRPC is the right v0.2+ upgrade when
streaming large archives is measurably a bottleneck.

### Why is pull a fetch-only operation?

Automatic merge-on-pull (git's default) hides merge decisions. For a research KG where semantic
correctness matters more than throughput, the agent should explicitly call `merge_branch` after
deciding that the remote state is worth integrating. This matches the "agent provides judgment"
principle established in ADR-015 §D.3.

### Why SHA-256 over content-defined chunking?

Content-defined chunking (CDC, as in git's object store) would deduplicate unchanged entities
across snapshots at the object level, dramatically reducing storage cost for incremental commits.
However, CDC requires an object store abstraction (pack files, ref objects) that is significantly
more complex than a single SQL table. At v0.1 scale (research KGs, not production databases), the
storage cost of full snapshots is acceptable. The `format = 'delta'` migration path in §6 is the
right v0.5 direction.

### Why is the dirty flag in a separate table rather than derived from the hash?

Recomputing the snapshot hash on every `checkout` guard check requires reading all entities and
edges — an O(N) operation. With 100K entities, this is hundreds of milliseconds. A dirty flag is
an O(1) read, set transactionally with each write. The flag can be wrong only if the write
transaction commits without the flag update, which WAL mode prevents.

### Why FTS5 sync but vector async?

FTS5 is a SQL virtual table; rebuilding it is a SQL-level operation (truncate + re-insert from the
entities table). At 100K entities this takes under 5 seconds. Vector re-embedding requires model
inference, which can take minutes. Blocking checkout on vector inference would make `checkout`
unusable for large namespaces. The `vector_index_status` field in the checkout response gives
callers the information they need to wait if freshness matters.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
|---|---|---|---|
| gRPC remote protocol | Better streaming, strongly typed | `protobuf` compile dep; complex for v0.1 scale | HTTP is sufficient at v0.1 archive sizes; upgrade later |
| Pull = fetch + auto-merge | Fewer steps for the common case | Hides merge decisions; ADR-015 and ADR-043 require explicit merge | Violates "agent provides judgment" principle |
| Content-defined chunking for snapshots | O(changed entities) storage per commit | Object store complexity; not justified at v0.1 scale | Defer to v0.5 with delta format |
| `dirty` flag derived from hash recompute | Always accurate | O(N) on every checkout guard; blocks on large namespaces | O(1) transactional flag wins |
| Vector rebuild synchronous | Consistent immediately after checkout | Blocks checkout for minutes on large namespaces | User experience is unacceptable |
| Merge-on-pull default | Familiar to git users | Hides conflicts; wrong for research KG correctness | ADR-015 §D.3 rationale applies |

## Consequences

### Positive

- The remote protocol is curl-testable and proxy-friendly from day one.
- The `format` column migration to delta storage is schema-additive — no breaking change when v0.5
  ships.
- The `MergeEngine` trait allows `khive-vcs` to ship before `khive-merge` without a link error;
  `NoOpMergeEngine` returns `MergeNotImplemented` cleanly.
- SHA-256 content addressing makes snapshots tamper-evident — any corruption of the archive
  produces a hash mismatch on verification.
- FTS5 sync + vector async provides responsive checkouts for common operations while flagging when
  search results may be stale.

### Negative

- Full-snapshot storage at v0.1 is O(N) per commit. A 100K-entity namespace committing daily for a
  year produces approximately 22 GB of snapshot archives. This is a desktop storage concern.
  Mitigated: the delta migration path (§6) is designed; the `format` column ships in v0.1 ready for
  the upgrade.
- Pull-as-fetch requires an extra agent step to merge after pulling. Agents accustomed to git's
  default behavior may find this surprising. Mitigated: clear documentation in the `pull` tool
  description.
- The `khive-sync` HTTP server is a new binary target (or an optional feature flag on the existing
  daemon). This adds build surface.

### Neutral

- Remote configuration in `.khive/remotes.toml` rather than the database is consistent with git's
  `.git/config` model but is a separate file format to document.
- The `SnapshotId` newtype enforces the `sha256:` prefix invariant at the type level; callers that
  construct IDs from raw strings must call `SnapshotId::from_hash`, which validates the format.

## Open Questions

1. **Multi-namespace push**: should `push` support pushing all branches across all namespaces in
   one call, or always namespace-specific? Namespace-specific is the v0.1 design; multi-namespace
   is a v0.2 addition if needed.

2. **Remote namespace creation**: if the remote does not have the target namespace, does `push`
   create it, or fail? v0.1 recommendation: fail with a `NamespaceNotFound` error and require the
   operator to pre-create the namespace on the remote. This avoids surprising implicit namespace
   creation.

3. **Sync server access control**: the `POST /v1/sync/{namespace}/snapshots` endpoint requires the
   caller to have write access to that namespace. The v0.1 auth model is bearer-token-per-instance
   (all-or-nothing). Fine-grained namespace ACLs belong in ADR-029/ADR-032 (authorization gate);
   defer to that design.

4. **Snapshot GC**: as snapshots accumulate, storage grows without bound. A `gc` verb (e.g., delete
   snapshots with no branch reference and older than N days) is a v0.2 concern. v0.1 has no GC.

5. **Streaming archives**: the current HTTP protocol sends the entire `KgArchive` JSON as a single
   response body. For 60 MB archives this is one HTTP response. Adding `Transfer-Encoding:
   chunked` or NDJSON streaming (defined in ADR-015 §C.2) on the server side is a v0.2 concern.

## Implementation Plan

### Phase 1 — Schema and types (v0.4 prep)

- Add `crates/khive-vcs/` with `types.rs`, `hash.rs`, `migrations.rs`.
- Ship the `ServiceSchemaPlan` migration for `kg_snapshots`, `kg_snapshot_archives`, `kg_branches`,
  `kg_vcs_state`.
- Unit-test: hash determinism (same namespace state → same hash across process restarts), hash
  uniqueness (one-entity change → different hash).

### Phase 2 — Local VCS operations (v0.4 target)

- Implement `commit`, `branch`, `checkout`, `log` in `snapshot.rs`, `branch.rs`, `log.rs`.
- Wire into `khive-mcp` alongside `khive-diff` tools (ADR-017).
- Integration test: full commit/branch/checkout/log round-trip on an in-memory runtime.

### Phase 3 — Merge integration (v0.5 target, requires ADR-043)

- Implement `merge_branch` in `khive-vcs` by delegating to `khive-merge::MergeEngine`.
- `NoOpMergeEngine` ships in v0.4 (returns `MergeNotImplemented`).
- `ThreeWayMergeEngine` ships in `khive-merge` for v0.5 and is registered at startup.

### Phase 4 — Remote protocol (v0.6 target)

- Implement `RemoteClient`, `push`, `pull` in `remote.rs`, `push.rs`, `pull.rs`.
- Implement the `khive-sync` HTTP server (feature flag: `--features sync-server`).
- Integration test: two in-memory runtime instances, push from A to B, pull from B to A.

### Coverage targets

| Module | Target |
|--------|--------|
| `hash.rs` — canonical JSON + SHA-256 | 95% |
| `snapshot.rs` — commit + dirty detection | 90% |
| `branch.rs` — branch + checkout | 90% |
| `log.rs` | 85% |
| `remote.rs` — push + pull | 85% |
| MCP tool wiring | 80% |

## References

- ADR-010: KG Versioning Direction (strategic vision)
- ADR-015: KG Versioning Model (conceptual design; this ADR implements it)
- ADR-017: Graph Diff Format (diff format used in delta storage migration path)
- ADR-043: KG Merge Algorithm (three-way merge implementation; `MergeEngine` consumer)
- ADR-022: Schema Migrations (`ServiceSchemaPlan` migration pattern)
- ADR-005: Storage Capability Traits (storage trait contract `khive-vcs` builds on)
- ADR-014: Curation Operations (write operations that set the `dirty` flag)
- ADR-007: Namespace as Open String (namespace scope for branches)
- `crates/khive-runtime/src/portability.rs` — `KgArchive` serialization reused for snapshot archives
- git documentation on fast-forward merges and remote protocol — inspiration, not imitation
