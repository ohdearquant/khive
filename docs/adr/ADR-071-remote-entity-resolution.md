# ADR-071: Remote Entity Resolution

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-048 (Git-Native KG Versioning), ADR-051 (CLI KG Git Workflow), ADR-052 (KG Storage Model), ADR-056 (KG Validation Pipelines)

## Context

All entity resolution today is local: `resolve_uuid_async` accepts an 8-char short ID or a
full UUID and searches the configured namespace in the local SQLite database. Cross-namespace
resolution is unsupported; cross-repository resolution does not exist.

As KGs grow and teams share graph fragments, agents and CLI commands need to reference entities
that live in a different namespace or a separate repository. Import and sync commands
(`ADR-055`, `ADR-051`) copy remote content locally, but the resolution layer has no concept of
a canonical remote address.

Two gaps exist:

1. No reference syntax for a remote entity (agents must hard-code UUIDs after an import).
2. No resolver strategy that consults a remote cache before declaring "not found".

## Decision

### Reference syntax

A fully-qualified remote entity reference is:

```
kg://<remote>/<namespace>/<id>
```

Where `<remote>` is a name defined in `schema.yaml` remotes, `<namespace>` is the entity's
namespace in that remote, and `<id>` is a full UUID or 8-char short ID.

Accepted shorthands:

| Form                               | Meaning                                               |
| ---------------------------------- | ----------------------------------------------------- |
| `<namespace>:<uuid>`               | Namespace-qualified local ref (no remote lookup)      |
| `kg://<remote>/<namespace>/<uuid>` | Fully qualified remote ref                            |
| `<uuid>` or `<short-id>`           | Current behaviour — local search in default namespace |

The `kg://` scheme is reserved for remote refs. `namespace:id` is a local shorthand that
does not trigger a network fetch.

### Resolver order

`resolve_uuid_async` follows this precedence:

1. **Local exact UUID** — direct DB lookup by full UUID in the specified namespace.
2. **Local short ID** — DB lookup of 8-char prefix in the specified namespace.
3. **Namespace-qualified local** — if the input is `namespace:id`, search that namespace in
   the local DB.
4. **Remote cache** — if the input is a `kg://` ref, search `.khive/kg/remotes/<remote>/`
   NDJSON cache.
5. **Remote fetch** (opt-in) — if the cache is absent or stale AND `--fetch` is passed to the
   CLI (or `allow_remote_fetch: true` in the runtime config), clone/pull the remote ref.

Step 5 is never triggered automatically during normal verb dispatch — it requires explicit
opt-in to prevent unexpected network access inside MCP tool calls.

### Ambiguity

Short IDs that match multiple local entities in the searched namespace produce a
`RuntimeError::AmbiguousId` error. No "first match" fallback. This matches the existing
behaviour for local short-ID resolution.

Short IDs presented as `kg://` refs resolve to all entities in the remote cache whose
UUID begins with the prefix. Ambiguity is an error there too.

### Remote configuration

`schema.yaml` remotes section (extending ADR-048):

```yaml
remotes:
  upstream:
    url: https://github.com/org/kg-data.git
    ref: main
    namespace: research
    pin: "sha256:abc123..." # optional content hash for verification (ADR-072)
```

Fields:

| Field       | Required | Description                                                             |
| ----------- | -------- | ----------------------------------------------------------------------- |
| `url`       | yes      | Git remote URL                                                          |
| `ref`       | yes      | Branch or tag to resolve against                                        |
| `namespace` | yes      | Namespace to scope entity resolution                                    |
| `pin`       | no       | SHA-256 content hash; when present, cache is invalid if hash mismatches |

### Cache layout

```
.khive/kg/remotes/<remote-name>/
  entities.ndjson   # remote entities at last fetch
  edges.ndjson      # remote edges at last fetch
  meta.json         # { fetched_at, ref, commit_sha, content_hash }
```

The cache is read-only from the runtime's perspective. Only `khive kg sync` or
`khive kg fetch <remote>` populates it. Stale cache (older than `cache_ttl_seconds` in
config, default 24h) produces a warning but is still used; `--fetch` or explicit sync
refreshes it.

### Trust and authorization

Remote resolution is read-only. A `link` or `create` that references a remote entity by
`kg://` ref is allowed only for the resolved local UUID — the entity must be imported
locally first, or the link must target a locally-cached copy.

Writes that would cross namespace boundaries (creating an entity in a remote namespace) are
rejected with `RuntimeError::CrossNamespaceWrite`.

### Failure modes

| Condition                                                    | Error                                         |
| ------------------------------------------------------------ | --------------------------------------------- |
| `kg://` ref but remote not configured in `schema.yaml`       | `UnknownRemote { name }`                      |
| Cache absent and `--fetch` not requested                     | `RemoteCacheMissing { remote, namespace }`    |
| Cache present but content hash mismatches pin                | `HashMismatch { expected, actual }` (ADR-072) |
| Short ID matches multiple remote entities                    | `AmbiguousId { id, count }`                   |
| Namespace in `kg://` ref does not match configured namespace | `NamespaceMismatch { expected, actual }`      |
| Offline / fetch fails                                        | `RemoteFetchError { remote, message }`        |

## Consequences

### Positive

- Agents can reference entities across repositories using a stable `kg://` address.
- Read-only remote access prevents accidental cross-repo writes.
- Resolver order preserves backward compatibility — existing short IDs and UUIDs resolve as before.

### Negative

- Cache freshness adds operational complexity. Teams must run `khive kg sync` or pass `--fetch`
  to stay current with upstream.
- The `kg://` scheme requires parser changes in `resolve_uuid_async` and any code path that
  accepts entity IDs from users.
- Remote configuration in `schema.yaml` ties entity resolution to VCS state; rotating a remote
  URL requires a schema commit.

### Integration points

- `resolve_uuid_async` (`crates/khive-runtime/src/operations.rs`) — primary resolver entry point
- `link` endpoint validation — must resolve remote refs before checking endpoint kinds
- `khive kg import` / `khive kg sync` — populate and refresh the remote cache
- `khive kg doctor` — report stale caches, missing pins, hash mismatches
- `khive kg diff` — should resolve remote refs in `kg://` form when comparing archives

### Tests required

- Local UUID resolution unchanged.
- `namespace:id` form resolves to correct namespace without remote lookup.
- `kg://` ref resolved from cache when cache is present and valid.
- Cache absent + no `--fetch` returns `RemoteCacheMissing`.
- Cache hash mismatch with pin returns `HashMismatch`.
- Ambiguous short ID in remote cache returns `AmbiguousId`.
- Unknown remote name returns `UnknownRemote`.
- Cross-namespace write rejected.

## References

- ADR-048: Git-Native KG Versioning — `schema.yaml` remotes section this ADR extends
- ADR-051: CLI KG Git Workflow — `khive kg sync` / `khive kg fetch` commands
- ADR-052: KG Storage Model — local DB resolution path
- ADR-056: KG Validation Pipelines — doctor integration for cache health
- ADR-072: Sync Content-Hash Verification — pin/hash mismatch behaviour
- `crates/khive-runtime/src/operations.rs`: `resolve_uuid_async`
- `crates/khive-vcs/src/error.rs:42`: `VcsError::HashMismatch`
