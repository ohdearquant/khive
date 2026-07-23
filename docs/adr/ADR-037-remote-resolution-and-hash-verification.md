# ADR-037: Remote Entity Resolution and Content-Hash Verification

**Status**: accepted
**Date**: 2026-05-23
**Authors**: khive maintainers

## Context

[ADR-020](./ADR-020-git-native-kg-implementation.md) establishes the git-native KG implementation:
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

| Form                             | Meaning                                                            |
| -------------------------------- | ------------------------------------------------------------------ |
| `<uuid>`                         | Local UUID parse in caller namespace                               |
| `<short-id>`                     | Local 8+ hex UUID-prefix lookup in caller namespace                |
| `<entity-name>`                  | Local entity-name lookup in caller namespace                       |
| `<namespace>:<uuid>`             | Reserved local shorthand; not part of shipped `resolve_uuid_async` |
| `kg://<remote>/<namespace>/<id>` | Reserved/deferred remote ref form                                  |

The shipped resolver is local-only. `resolve_uuid_async` follows this precedence:

1. Parse a full UUID string.
2. Resolve an 8+ hex-character UUID prefix via `runtime.resolve_prefix`.
3. Treat every other string as an entity name and call `resolve_name_async`.

Remote cache/fetch ordering, stale-cache fallback, and `kg://` parsing are deferred.
Runtime verb calls do not expose `--fetch`; operators pre-populate remote data with
`kkernel kg fetch` / `kkernel kg sync` before local resolution.

#### Ambiguity handling

Short IDs that match multiple local entities in the searched namespace produce
`RuntimeError::AmbiguousId { id, count }`. There is no first-match fallback. This preserves
the existing behavior for local short-ID resolution.

When remote lookup is implemented, short IDs inside a `kg://` reference must resolve against
the selected remote cache and must preserve the same ambiguity error. The shipped resolver
does not execute this path.

#### Remote configuration and cache status

Schema-driven remote lookup is deferred. The shipped remote sync/fetch path builds
`RemoteConfig` from explicit `kkernel kg fetch` / `kkernel kg sync` CLI arguments
(`remote`, `--url`, `--ref`, `--namespace`, `--pin`) rather than parsing a
`schema.yaml remotes` block.

`khive-vcs::sync::run_sync_remote` does implement fail-closed hash verification and cache
publication for explicit sync/fetch calls. Runtime `resolve_uuid_async` does not consult
that cache.

#### Failure modes

| Condition                                           | Shipped behavior                                                  |
| --------------------------------------------------- | ----------------------------------------------------------------- |
| Full UUID parses                                    | returned directly                                                 |
| 8+ hex prefix misses                                | `InvalidInput("no record matches prefix")`                        |
| 8+ hex prefix is ambiguous                          | `runtime.resolve_prefix` error                                    |
| Name lookup fails                                   | `resolve_name_async` error                                        |
| `kg://` ref, stale cache, missing cache, remote ref | deferred; not parsed by shipped resolver                          |
| Hash mismatch during explicit sync/fetch            | `VcsError::HashMismatch { expected, actual }` before cache writes |

### Part 2: Content-Hash Verification

Every `kkernel kg sync` operation that fetches a remote KG archive must verify a SHA-256
content hash before writing the archive into the live `.khive/kg/` tree or the local working
DB.

#### Hash requirement

If an explicit sync/fetch call supplies a `pin`, verification is mandatory. A mismatch
aborts the operation and returns `VcsError::HashMismatch { expected, actual }` before any
live path is modified. Future schema-driven remote lookup must preserve this invariant when
it supplies the pin from `schema.yaml`.

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

The planned schema representation uses the same pin string accepted by explicit sync/fetch:

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

#### Failure behavior: fail closed

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
5. Atomically publish the complete staged cache directory at
   `.khive/kg/remotes/<remote>/`.
6. Write `meta.json` with `{ fetched_at, ref, commit_sha, content_hash }`.

Step 5 renames the staged directory as one unit. Either the old cache remains intact (any failure
before step 5) or the new cache is fully populated. There is no intermediate state visible
to concurrent readers.

#### Repin workflow

`kkernel kg sync --repin <remote>` skips hash comparison and returns the computed
`SnapshotId` / `repinned` result to the caller. It does not write the new pin back into
`schema.yaml`; schema updates are caller-managed. The caller is responsible for verifying
remote content independently before committing a new pin.

## Rationale

### Why a `kg://` scheme rather than extending `<remote>:<uuid>`

ADR-020 establishes `<remote>:<uuid>` for cross-repo edges in NDJSON files (the `target`
field of an edge record). That form is unambiguous in the serialization context where
`<remote>` is always a known name. In resolver inputs, however, the same form collides with
`<namespace>:<uuid>`: a remote name and a namespace name may be identical. The `kg://`
scheme provides a syntactically distinct surface for fully-qualified remote refs in
resolver-facing contexts (CLI arguments and MCP inputs) while the `<remote>:<uuid>`
form is preserved as-is in NDJSON edge records where the ambiguity does not arise.

### Why remote fetch remains explicit

Automatic remote fetch on a resolver cache miss would introduce nondeterministic network
latency into an otherwise local operation. The shipped resolver therefore remains local-only.
Remote archives are fetched through explicit `kkernel kg fetch` or `kkernel kg sync` calls;
runtime parsing and lookup of `kg://` references remains deferred.

### Deferred stale-cache policy

Runtime cache lookup is not shipped, so no resolver TTL behavior is active. A future cache
policy should preserve offline reads when explicitly requested, surface staleness, and make
any network refresh an explicit caller choice.

### Why canonical hash rather than raw file hash

Raw NDJSON file hashes are unstable across exporters. Two tools that export the same
logical graph state may emit different line orderings (different UUID sort collation, locale
differences, timestamp format differences). A pin over raw bytes would break every time any
exporter detail changed, even if no graph content changed.

The canonical hash is defined over logical content: sorted entities, sorted edges,
alphabetical properties, and compact JSON. It is independent of serialization details. This
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
| Auto-fetch on cache miss without opt-in                          | Introduces non-deterministic network latency into MCP verb calls without caller consent                                                                                              |
| Block on stale cache (treat TTL as hard deadline)                | Breaks offline workflows and air-gapped deployments; warning achieves visibility without blocking                                                                                    |
| Raw NDJSON file hash as pin                                      | Unstable across exporters; breaks on sort-order or whitespace differences without content change                                                                                     |
| Warn-and-continue on hash mismatch                               | Renders the pin field meaningless as a security primitive                                                                                                                            |
| Separate `kkernel kg repin` command (vs. `--repin` flag on sync) | Adding a dedicated verb for a single-field write in `schema.yaml` creates surface area without benefit; `--repin` collocates the trust-upgrade action with the operation it modifies |

## Consequences

### Positive

- The reserved `kg://<remote>/<namespace>/<id>` syntax is unambiguous for future runtime
  remote resolution.
- Existing local UUID, short-ID, and entity-name resolution remains unchanged.
- Hash verification catches corruption and substitution before any live KG file is touched.
- Staging-plus-atomic-rename ensures no partial archive state on mismatch.
- Canonical hash is independent of exporter details; the same logical archive always produces
  the same pin, regardless of which tool generated the NDJSON.
- `kkernel kg doctor` (ADR-034) can report stale caches and declared-but-unverified remotes
  as part of routine health checks.

### Negative

- Pin maintenance overhead: every legitimate upstream update requires a repin. Teams syncing
  frequently from a moving `main` branch may omit the `pin` field and accept lower assurance.
- Activating the `kg://` form will require parser and cache-lookup changes in
  `resolve_uuid_async`.
- Schema-driven remote configuration remains deferred; current fetch and sync calls require
  explicit remote arguments.
- The canonical-hash computation parses the full NDJSON on every sync. For archives above
  ~50K entities this is measurable (sub-second on modern hardware) but not free.

### Integration points

- `resolve_uuid_async` (`crates/khive-pack-kg/src/handlers.rs`): shipped local resolver:
  full UUID, 8+ hex prefix, then entity name.
- `kkernel kg fetch` / `kkernel kg sync`: explicit operator paths for remote archive fetch,
  staging, canonical hash computation, pin comparison, and cache/meta publication.
- `kkernel kg sync --repin <remote>`: skips pin comparison and returns the computed hash for
  caller-managed schema update.
- `kg://` parsing, runtime remote lookup, `kkernel kg doctor`, and
  `kg validate --resolve-remotes` are deferred.

## Open Questions

1. **Pin presence as a future requirement.** The current decision makes `pin` optional and
   treats its absence as "hash still computed but not enforced." A future ADR may make `pin`
   mandatory for all remotes. The threshold condition is unclear: when teams have demonstrated
   reliable repin workflows, or when remote KG sharing becomes sufficiently common that
   unverified syncs are a meaningful risk.

2. **`cache_ttl_seconds` default and configurability.** 86400 seconds (24h) is chosen as a
   reasonable default for research KGs. Production deployments with strict freshness
   requirements may want sub-hour TTLs, but setting `cache_ttl_seconds: 0` would make every
   session require `--fetch`: operationally burdensome. A per-remote TTL override in
   `schema.yaml` may be preferable to a global config value.

3. **Short-ID ambiguity in remote cache.** Short IDs are 8 characters from a UUID v4 space.
   Collision probability within a single remote cache is low but non-zero at scale. The
   current decision returns `AmbiguousId` and requires the caller to use a full UUID. An
   alternative would be to accept a remote cache that is small enough to have at most one
   8-char prefix match as collision-free by construction. No action taken; raised for review.

## References

- [ADR-001](./ADR-001-entity-kind-taxonomy.md): Entity Kind Taxonomy: entity resolution returns
  typed entities; remote cache entries must satisfy kind constraints
- [ADR-007](./ADR-007-namespace.md): Namespace: namespace scoping enforced at resolver step 3;
  `CrossNamespaceWrite` enforced by runtime
- [ADR-013](./ADR-013-note-kind-taxonomy.md): Note Kind Taxonomy: note resolution follows the
  same resolver order for note UUIDs
- [ADR-018](./ADR-018-authorization-gate.md): Authorization Gate: remote resolution is
  read-only; cross-namespace writes rejected regardless of ref form
- [ADR-020](./ADR-020-git-native-kg-implementation.md): Git-Native KG Implementation: establishes
  the `<remote>:<uuid>` edge syntax, commit-SHA pins, remote cache layout, and
  `.khive/kg/remotes/<remote>/` directory structure this ADR extends
- [ADR-034](./ADR-034-kg-validation-pipelines.md): KG Validation Pipelines: `kkernel kg doctor`
  reports stale caches and hash mismatches as health findings; `validate --resolve-remotes`
  exercises the resolver against all declared remotes
- `crates/khive-runtime/src/operations.rs`: `resolve_uuid_async`: resolver entry point
- `crates/khive-vcs/src/hash.rs`: `snapshot_id_for_archive`, `canonical_json`
- `crates/khive-vcs/src/types.rs`: `SnapshotId`: `"sha256:" + 64 hex chars` invariant
- `crates/khive-vcs/src/error.rs`: `VcsError::HashMismatch`
