# ADR-037: Remote Entity Resolution and Content-Hash Verification

**Status**: accepted
**Date**: 2026-05-23
**Authors**: Ocean, lambda:khive

## Context

[ADR-020](ADR-020-git-native-kg-implementation.md) establishes the git-native KG implementation:
NDJSON files under `.khive/kg/`, a two-layer storage model (working DB + committed NDJSON), and
the `schema.yaml` remotes section with mandatory commit-SHA pins. That ADR defines the
`<remote>:<uuid>` cross-repo reference syntax and lays out the remote cache layout under
`.khive/kg/remotes/<remote>/`. What it does not specify is:

1. The full reference syntax for remote entities (beyond the bare `<remote>:<uuid>` form).
2. The resolver precedence that `resolve_uuid_async` follows when input might be a local UUID,
   a namespace-qualified local ref, or a fully-qualified remote ref.
3. The content-hash verification contract that `kkernel kg sync` must fulfill before writing a
   fetched archive into the live `.khive/kg/` tree.

These two gaps are addressed here. Both concern the boundary between local and remote KG
state: one defines how the runtime resolves identifiers across that boundary; the other
defines how the CLI hardens the boundary against corruption and substitution.

The hash primitives already exist: `snapshot_id_for_archive` in `crates/khive-vcs/src/hash.rs`
computes a deterministic `SnapshotId` from a `KgArchive` by sorting entities and edges
canonically before hashing. `SnapshotId` carries the invariant `"sha256:" + 64 hex chars`.
`VcsError::HashMismatch` is defined and serializable. The DB and sync path has no documented
contract requiring these primitives to be used; ADR-020's commit-SHA pins address git-level
reproducibility but not archive-content integrity.

## Decision

### Part 1: Reference Syntax and Resolver Order

#### Reference syntax

Three accepted forms, in order of specificity:

| Form                             | Meaning                                                   |
| -------------------------------- | --------------------------------------------------------- |
| `<uuid>` or `<short-id>`         | Local search in default namespace — existing behavior     |
| `<namespace>:<uuid>`             | Namespace-qualified local ref; no remote lookup triggered |
| `kg://<remote>/<namespace>/<id>` | Fully qualified remote ref                                |

The `kg://` scheme is reserved for remote refs. `<namespace>:<uuid>` is a local shorthand;
it does not require or trigger a network fetch. The short-id form resolves within the
caller's configured namespace, unchanged from prior behavior.

#### Resolver order (v1)

`resolve_uuid_async` follows this precedence, stopping at the first match:

1. **Local working tree**: if the path resolves locally, return it (no fetch).
2. **Hash-verified cache**: if the cache has an entry matching the requested
   content hash, return it (fast path).
3. **Stale cache + `--fetch=auto`**: if the cache has a _stale_ entry (hash
   mismatch or age past TTL):
   a. If `--fetch=auto` (default) or `--fetch=always`: re-fetch from remote into
   a staging path, verify SHA-256, atomic-rename over the stale cache entry,
   return the refreshed entry.
   b. If `--fetch=never`: emit a warning, return the stale entry, mark response
   `from_stale_cache=true`.
4. **No cache + `--fetch` permitted**: fetch from remote, verify SHA-256, store
   in cache, return.
5. **No cache + `--fetch=never`**: error.

Remote fetch (steps 3a and 4) is never triggered automatically during normal verb dispatch.
MCP tool calls and programmatic verb calls default to `--fetch=never`. Remote fetch requires
explicit opt-in (`--fetch=auto` or `--fetch=always`) to prevent unexpected network access
inside agent-driven workflows. The key change: **hash check before accepting a cache hit**
(step 2 requires hash match; stale entries route to step 3).

#### Ambiguity handling

Short IDs that match multiple local entities in the searched namespace produce
`RuntimeError::AmbiguousId { id, count }`. There is no first-match fallback. This preserves
the existing behavior for local short-ID resolution.

Short IDs presented inside a `kg://` ref resolve against all entities in the remote cache
whose UUID begins with the prefix. Ambiguity is also an error there.

#### `schema.yaml` remotes section

Extending the remotes block established in ADR-020:

```yaml
remotes:
  - name: upstream
    url: https://github.com/org/kg-data.git
    ref: main
    namespace: research
    pin: "sha256:abc123...64hexchars" # optional; when present, sync is mandatory-verify
```

| Field       | Required | Description                                                       |
| ----------- | -------- | ----------------------------------------------------------------- |
| `url`       | yes      | Git remote URL                                                    |
| `ref`       | yes      | Branch or tag to resolve against                                  |
| `namespace` | yes      | Namespace scoping entity resolution for this remote               |
| `pin`       | no       | SHA-256 content hash; when present, cache is rejected on mismatch |

The `commit` field from ADR-020 (the git commit SHA pin) and the `pin` field here are
independent. `commit` is the git-level content address of the remote tree at a specific
commit. `pin` is the logical content hash of the parsed KG archive, computed from the
canonical entity and edge representation. Both may be present simultaneously.

#### Remote cache layout

```
.khive/kg/remotes/<remote-name>/
    entities.ndjson   # remote entities at last sync, sorted by UUID
    edges.ndjson      # remote edges at last sync, sorted by (source, target, relation)
    meta.json         # { fetched_at, ref, commit_sha, content_hash }
```

The cache is read-only from the runtime's perspective. Only `kkernel kg sync` or
`kkernel kg fetch <remote>` populates it. A stale cache (older than `cache_ttl_seconds` in
config, default 86400 seconds) produces a warning but is still used; `--fetch` or an
explicit sync refreshes it.

#### Trust and authorization

Remote resolution is read-only. A `link` or `create` that references an entity by `kg://`
ref targets the entity's local UUID — the entity must be imported into the local namespace
first, or the link targets a locally cached copy. Writes that would create an entity in a
remote namespace are rejected with `RuntimeError::CrossNamespaceWrite`. Authorization rules
from [ADR-018](ADR-018-authorization-gate.md) apply at the runtime layer regardless of
whether the entity originated locally or from a remote cache.

#### Failure modes

| Condition                                                  | Error                                      |
| ---------------------------------------------------------- | ------------------------------------------ |
| `kg://` ref names a remote not in `schema.yaml`            | `UnknownRemote { name }`                   |
| Cache absent and `--fetch` not requested                   | `RemoteCacheMissing { remote, namespace }` |
| Cache present but content hash mismatches `pin`            | `HashMismatch { expected, actual }`        |
| Short ID matches multiple remote cache entries             | `AmbiguousId { id, count }`                |
| Namespace in `kg://` ref differs from configured namespace | `NamespaceMismatch { expected, actual }`   |
| Offline or fetch fails                                     | `RemoteFetchError { remote, message }`     |

### Part 2: Content-Hash Verification

Every `kkernel kg sync` operation that fetches a remote KG archive must verify a SHA-256
content hash before writing the archive into the live `.khive/kg/` tree or the local working
DB.

#### Hash requirement

If a `pin` is declared in `schema.yaml` for the remote being synced, verification is
mandatory. A mismatch aborts the sync and returns `VcsError::HashMismatch { expected, actual }`
before any live path is modified.

If no `pin` is declared, the hash is still computed and logged in `meta.json` for
auditability. The sync proceeds. A future ADR may make pin presence mandatory for all
remotes.

#### Canonicalization

For archive-level sync, reuse `snapshot_id_for_archive` from `crates/khive-vcs/src/hash.rs`.
That function:

1. Sorts entities by UUID (case-insensitive ascending).
2. Sorts edges by `(source, target, relation)` triple (lexicographic ascending).
3. Sorts property keys alphabetically within each record.
4. Sorts tags lexicographically.
5. Serializes to compact JSON (no whitespace).
6. Computes SHA-256 of the resulting bytes.
7. Returns `"sha256:" + hex(digest)`.

For file-level sync (NDJSON files delivered directly, not wrapped in a `KgArchive` envelope),
the canonical hash is computed by:

1. Parsing all lines from `entities.ndjson` and `edges.ndjson` into `KgArchive` form.
2. Applying the same sort order as `canonical_json`.
3. Hashing the resulting canonical JSON bytes via SHA-256.

This makes the hash independent of line ordering in the source NDJSON files. Two NDJSON
exports of the same logical graph state produce the same `SnapshotId` regardless of which
tool generated them or in what order lines were emitted.

#### Pin format

```yaml
remotes:
  - name: upstream
    url: https://github.com/org/kg-data.git
    ref: main
    namespace: research
    pin: "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
```

The `pin` value must match the `SnapshotId` invariant exactly: the literal string `"sha256:"`
followed by exactly 64 lower-case hexadecimal characters. Schema validation via
`kkernel pack check` or `kkernel kg validate` rejects malformed pin values at parse time,
not at sync time.

#### Failure behavior — fail closed

On hash mismatch, the sync fails closed:

1. Do not update `.khive/kg/remotes/<remote>/entities.ndjson` or `edges.ndjson`.
2. Do not update the working DB or the remote cache `meta.json`.
3. Return `VcsError::HashMismatch { expected, actual }`.
4. CLI output prints: remote name, expected hash, actual hash, and a remediation hint:
   `kkernel kg sync --repin <remote>` to write a new pin after independently verifying
   the remote content.
5. Exit with code 1.

Remote URLs are not printed in full in error output. The remote name is used.

#### Durability and staging

The sync workflow uses a staging directory to ensure partial failure leaves the existing
cache intact:

1. Fetch the remote archive into a temporary staging directory under `.khive/state/`.
2. Parse staged files into `KgArchive` form.
3. Compute `SnapshotId` of the staged archive via `snapshot_id_for_archive`.
4. Compare against `pin` if present. Abort on mismatch (staging directory is discarded).
5. Atomically publish: rename staging NDJSON files into `.khive/kg/remotes/<remote>/`.
6. Write `meta.json` with `{ fetched_at, ref, commit_sha, content_hash }`.

Step 5 is a single filesystem rename. Either the old cache remains intact (any failure
before step 5) or the new cache is fully populated. There is no intermediate state visible
to concurrent readers.

#### Repin workflow

`kkernel kg sync --repin <remote>` skips hash comparison and writes the computed
`SnapshotId` back into `schema.yaml` as the new `pin` value for the named remote. This is
a deliberate trust-upgrade operation. The caller is responsible for verifying remote content
independently (via git log, PR review, or out-of-band audit) before repinning. The repin
command does not suppress other verification — schema compliance and referential integrity
checks still run.

## Rationale

### Why a `kg://` scheme rather than extending `<remote>:<uuid>`

ADR-020 establishes `<remote>:<uuid>` for cross-repo edges in NDJSON files (the `target`
field of an edge record). That form is unambiguous in the serialization context where
`<remote>` is always a known name. In resolver inputs, however, the same form collides with
`<namespace>:<uuid>` — a remote name and a namespace name may be identical. The `kg://`
scheme provides a syntactically distinct surface for fully-qualified remote refs in
resolver-facing contexts (CLI args, agent verb calls, MCP inputs) while the `<remote>:<uuid>`
form is preserved as-is in NDJSON edge records where the ambiguity does not arise.

### Why resolver step 5 requires explicit opt-in

MCP verb calls are invoked by agents in contexts where network latency is unexpected and
where the caller has not signaled willingness to wait for a remote operation. Automatic
remote fetch on cache miss would introduce non-deterministic latency into every verb call
that touches a `kg://` ref. Requiring `--fetch` or `allow_remote_fetch: true` makes the
network boundary explicit. Agents that need fresh remote data run `kkernel kg sync` first,
then use the populated cache for all subsequent resolver calls within the session.

### Why stale cache warns but does not block (with `--fetch=never`)

A cache that is 25 hours old is almost certainly still correct for typical research KGs,
which change slowly. When `--fetch=never`, blocking on staleness would make offline work
impossible and would force teams running in air-gapped environments to disable the feature
entirely. Warning gives operators visibility without breaking the common case. When
`--fetch=auto` (the default in CLI contexts), stale entries are re-fetched automatically.
Teams that need freshness guarantees set `cache_ttl_seconds: 0` in config, which ensures
every session re-fetches rather than relying on a stale entry.

### Why canonical hash rather than raw file hash

Raw NDJSON file hashes are unstable across exporters. Two tools that export the same
logical graph state may emit different line orderings (different UUID sort collation, locale
differences, timestamp format differences). A pin over raw bytes would break every time any
exporter detail changed, even if no graph content changed.

The canonical hash is defined over the logical content — sorted entities, sorted edges,
alphabetical properties, compact JSON — and is independent of serialization details. This
is the same invariant that makes the two-layer storage model work: re-export of the same
logical state always produces the same bytes.

### Why fail closed on hash mismatch (vs. warn and continue)

A sync that continues past a hash mismatch defeats the purpose of pinning. The `pin` field
is a security and reproducibility primitive. If it is present, the only acceptable outcomes
are: match (sync proceeds) or mismatch (sync aborts). Warn-and-continue would allow a
substituted or corrupted archive to enter the live KG silently. `--repin` is the explicit
escape hatch for legitimate upstream updates.

## Alternatives Considered

| Alternative                                                      | Why rejected                                                                                                                                                                         |
| ---------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| Extend `<remote>:<uuid>` as the resolver input form              | Ambiguous with `<namespace>:<uuid>` in resolver context; `kg://` is syntactically distinct                                                                                           |
| Auto-fetch on cache miss without opt-in                          | Introduces non-deterministic network latency into MCP verb calls; agents cannot control timing                                                                                       |
| Block on stale cache (treat TTL as hard deadline)                | Breaks offline workflows and air-gapped deployments; warning achieves visibility without blocking                                                                                    |
| Raw NDJSON file hash as pin                                      | Unstable across exporters; breaks on sort-order or whitespace differences without content change                                                                                     |
| Warn-and-continue on hash mismatch                               | Renders the pin field meaningless as a security primitive                                                                                                                            |
| Separate `kkernel kg repin` command (vs. `--repin` flag on sync) | Adding a dedicated verb for a single-field write in `schema.yaml` creates surface area without benefit; `--repin` collocates the trust-upgrade action with the operation it modifies |

## Consequences

### Positive

- Agents can reference remote entities with a stable `kg://<remote>/<namespace>/<id>` address
  that is unambiguous in all input contexts.
- Resolver backward compatibility: existing UUID, short-ID, and `<namespace>:<uuid>` inputs
  resolve as before; only `kg://` input triggers the new resolver steps.
- Hash verification catches corruption and substitution before any live KG file is touched.
- Staging-plus-atomic-rename ensures no partial archive state on mismatch.
- Canonical hash is independent of exporter details; the same logical archive always produces
  the same pin, regardless of which tool generated the NDJSON.
- `kkernel kg doctor` (ADR-034) can report stale caches and declared-but-unverified remotes
  as part of routine health checks.

### Negative

- Pin maintenance overhead: every legitimate upstream update requires a repin. Teams syncing
  frequently from a moving `main` branch may omit the `pin` field and accept lower assurance.
- The `kg://` form requires parser changes in `resolve_uuid_async` and in any CLI argument
  parsing that accepts entity IDs. All existing input forms continue to work.
- Remote configuration ties resolver behavior to `schema.yaml` state; renaming a remote
  requires a schema commit.
- The canonical-hash computation parses the full NDJSON on every sync. For archives above
  ~50K entities this is measurable (sub-second on modern hardware) but not free.

### Integration points

- `resolve_uuid_async` (`crates/khive-runtime/src/operations.rs`) — primary resolver entry
  point; gains `kg://` parsing and steps 3-5 of the resolver order.
- `link` verb validation — must resolve remote refs before checking endpoint kind constraints.
- `kkernel kg sync` — gains staging workflow, canonical hash computation, pin comparison, and
  `meta.json` content-hash write.
- `kkernel kg sync --repin <remote>` — new flag; skips pin comparison, writes computed hash.
- `kkernel kg doctor` — reports stale caches, missing pins, and hash mismatches as health
  findings.
- `kkernel kg validate --resolve-remotes` — confirms every `kg://` ref and `<remote>:<uuid>`
  edge target resolves against the remote cache or a live fetch.

## Open Questions

1. **Pin presence as a future requirement.** The current decision makes `pin` optional and
   treats its absence as "hash still computed but not enforced." A future ADR may make `pin`
   mandatory for all remotes. The threshold condition is unclear: when teams have demonstrated
   reliable repin workflows, or when remote KG sharing becomes sufficiently common that
   unverified syncs are a meaningful risk.

2. **`cache_ttl_seconds` default and configurability.** 86400 seconds (24h) is chosen as a
   reasonable default for research KGs. Production deployments with strict freshness
   requirements may want sub-hour TTLs, but setting `cache_ttl_seconds: 0` would make every
   session require `--fetch` — operationally burdensome. A per-remote TTL override in
   `schema.yaml` may be preferable to a global config value.

3. **Short-ID ambiguity in remote cache.** Short IDs are 8 characters from a UUID v4 space.
   Collision probability within a single remote cache is low but non-zero at scale. The
   current decision returns `AmbiguousId` and requires the caller to use a full UUID. An
   alternative would be to accept a remote cache that is small enough to have at most one
   8-char prefix match as collision-free by construction. No action taken; raised for review.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md): Entity Kind Taxonomy — entity resolution returns
  typed entities; remote cache entries must satisfy kind constraints
- [ADR-007](ADR-007-namespace.md): Namespace — namespace scoping enforced at resolver step 3;
  `CrossNamespaceWrite` enforced by runtime
- [ADR-013](ADR-013-note-kind-taxonomy.md): Note Kind Taxonomy — note resolution follows the
  same resolver order for note UUIDs
- [ADR-018](ADR-018-authorization-gate.md): Authorization Gate — remote resolution is
  read-only; cross-namespace writes rejected regardless of ref form
- [ADR-020](ADR-020-git-native-kg-implementation.md): Git-Native KG Implementation — establishes
  the `<remote>:<uuid>` edge syntax, commit-SHA pins, remote cache layout, and
  `.khive/kg/remotes/<remote>/` directory structure this ADR extends
- [ADR-034](ADR-034-kg-validation-pipelines.md): KG Validation Pipelines — `kkernel kg doctor`
  reports stale caches and hash mismatches as health findings; `validate --resolve-remotes`
  exercises the resolver against all declared remotes
- `crates/khive-runtime/src/operations.rs`: `resolve_uuid_async` — resolver entry point
- `crates/khive-vcs/src/hash.rs`: `snapshot_id_for_archive`, `canonical_json`
- `crates/khive-vcs/src/types.rs`: `SnapshotId` — `"sha256:" + 64 hex chars` invariant
- `crates/khive-vcs/src/error.rs`: `VcsError::HashMismatch`
