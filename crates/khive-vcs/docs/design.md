# khive-vcs Design

## ADR Compliance

### ADR-010: KG Versioning (git-native v1)

- KG state is stored as sorted NDJSON files in a git repository, not in a custom
  versioning system.
- `SnapshotId` is a content-addressed SHA-256 hash of the canonical JSON representation
  of entities and edges.
- KG branches are git branches; there is no custom remote protocol or `khive-sync` server.
- Legacy types (`KgSnapshot`, `KgBranch`) and the `VcsState.dirty` flag were removed;
  uncommitted changes are detected by diffing DB state against NDJSON files at runtime.
- Snapshot coverage v1: entities and edges only. Notes are excluded until note packs
  define versioned export, import, privacy/redaction, and merge semantics.

### ADR-020: Git-native sync, validate-first gate

- `run_sync` rebuilds the SQLite working database from `.khive/kg/entities.ndjson` and
  `edges.ndjson` atomically: the database is built in a `.tmp` sibling file and renamed
  only on success (all-or-nothing guarantee).
- Validate-first gate: all edge relations are parsed before any database write. An invalid
  relation aborts the sync and leaves the existing database intact.
- NDJSON record shape for entities and edges is defined in this crate's `NdjsonEntity`
  and `NdjsonEdge` structs.
- The `dirty` flag was removed (§7): "There is no dirty flag. The diff is computed fresh
  on every invocation."
- `entity_type` is included in the canonical hash representation so that two snapshots
  differing only in `entity_type` produce different `SnapshotId` values.
- Custom merge engine variants (`MergeNotImplemented`) were removed; the merge engine is
  superseded for v1.
- Custom push/pull error variants (`RemoteUnreachable`, `AuthFailed`, `NonFastForward`,
  `MergeRequired`) were removed; git is the remote protocol.

### ADR-035: Vector embeddings are local-only derived state

- `run_sync` intentionally skips vector embedding during import. Vectors are local-only
  derived state computed lazily via `kkernel kg embed` when needed.
- FTS5 text index is populated during sync so that text search works immediately
  after sync without a separate embedding pass.

### ADR-036: Validation pipeline

- The validate-first gate in `run_sync` implements the validation pipeline constraint:
  all edge relations are validated before any DB write, ensuring a clean error path
  that leaves the existing database intact.

### ADR-037: Remote archive fetch with SHA-256 pin verification

- `run_sync_remote` fetches a remote KG archive via `git clone --depth=1` into a
  temporary staging directory, then sparse-checks out only the NDJSON files.
- Content hash verification is fail-closed: if a `pin` is present and the actual hash
  does not match, the function returns an error before any cache file is written.
- `RemoteConfig` maps to one entry in `schema.yaml`'s `remotes:` list.
- Cache layout: `.khive/kg/remotes/<name>/` with `entities.ndjson`, `edges.ndjson`,
  and `meta.json` (containing `fetched_at`, `git_ref`, `commit_sha`, `content_hash`).
- When `repin=true`, pin comparison is skipped and the actual hash is returned for the
  caller to write back to `schema.yaml`. The hash is always computed and written to
  `meta.json` for auditability even when no pin is present.

### ADR-042: Canonical hash algorithm

- Canonical JSON sort order for hashing:
  1. Entities sorted by UUID string (case-insensitive ascending).
  2. Edges sorted by (source, target, relation) ascending.
  3. Property keys sorted alphabetically within each entity.
  4. Tags sorted lexicographically within each entity.
- Root object key order: `{"edges": [...], "entities": [...]}` (alphabetical).
- `exported_at`, `namespace`, `format`, `version` are excluded from the hash;
  only entity and edge content contributes to the `SnapshotId`.
- `SnapshotId` canonical form: `"sha256:"` prefix followed by exactly 64 lower-case
  hex characters. Upper-case hex and whitespace are rejected by both `from_hash`
  (normalized) and the custom `Deserialize` (strict rejection).

### ADR-037 §exact-lower-case-pin-format

- `SnapshotId` deserialization requires exact canonical form: `"sha256:"` prefix
  followed by exactly 64 lower-case hex characters with no whitespace.
- `from_hash` accepts upper-case input and normalizes to lower-case.
- The custom `Deserialize` impl is strict: upper-case hex and whitespace in the
  hex portion are rejected (to prevent mismatched pin comparisons).

## Consistency Notes

- `VCS-AUD-003`: tests in `src/hash.rs` verify that `entity_type` changes produce
  different `SnapshotId` values, enforcing the canonical entity record shape.
- `VCS-AUD-004`: tests in `src/types.rs` verify that the custom `Deserialize` impl
  rejects non-canonical inputs (missing prefix, uppercase hex, whitespace, wrong length).
- The `build_kg_archive` function in `src/sync.rs` validates edge relations before
  computing the hash, ensuring that invalid relations are caught before any cache or
  database write (fail-closed behaviour consistent with ADR-037).
- Non-finite edge weights (`NaN`, `Infinity`) are rejected by `edge_to_canonical_value`
  with `VcsError::Internal` — this is a correctness gate, not just a serialization concern.
