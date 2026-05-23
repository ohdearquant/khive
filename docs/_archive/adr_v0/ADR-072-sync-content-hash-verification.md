# ADR-072: Sync Content-Hash Verification

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive\
**Depends on**: ADR-048 (Git-Native KG Versioning), ADR-051 (CLI KG Git Workflow), ADR-055 (KG Import/Export Format Adapters)

## Context

`khive-vcs` already ships SHA-256 content addressing for KG archives:

- `snapshot_id_for_archive` (`crates/khive-vcs/src/hash.rs:19`) computes a deterministic
  `SnapshotId` from a `KgArchive` by sorting entities and edges canonically before hashing.
- `SnapshotId` (`crates/khive-vcs/src/types.rs:11`) carries the invariant `"sha256:" + 64 hex chars`.
- `VcsError::HashMismatch` (`crates/khive-vcs/src/error.rs:42`) is the error for detected mismatch.

The DB and sync path lacks a documented contract requiring these primitives to be used. A
`khive kg sync` that pulls a remote NDJSON archive and writes it into `.khive/kg/` without
verifying a declared hash is vulnerable to corruption or substitution.

ADR-048 defines schema-level `pin` fields for remote sources. No code enforces those pins at
sync time.

## Decision

Every `khive kg sync` operation that fetches a remote KG archive **must** verify a SHA-256
content hash before accepting the archive into the live `.khive/kg/` tree or the local DB.

### Hash requirement

If a `pin` is declared in `schema.yaml` for the remote being synced, verification is
**mandatory**: mismatch aborts with `VcsError::HashMismatch`.

If no `pin` is declared, the hash is still **computed and logged** (for auditability), but the
sync proceeds. A future version of this ADR may make pin presence mandatory.

### Canonicalization

For archive-level sync, reuse `snapshot_id_for_archive` from `crates/khive-vcs/src/hash.rs`.
This function:

1. Sorts entities by UUID (case-insensitive).
2. Sorts edges by `(source, target, relation)`.
3. Sorts property keys alphabetically within each entity.
4. Sorts tags lexicographically.
5. Serializes to compact JSON (no whitespace).
6. Returns `sha256:<hex>`.

For file-level sync (NDJSON files, not full archives), a canonical NDJSON hash is computed
as follows:

1. Parse all lines from `entities.ndjson` and `edges.ndjson` into the `KgArchive` type.
2. Apply the same sort order as `canonical_json`.
3. Hash the resulting canonical JSON bytes via SHA-256.

This means the canonical hash is independent of line ordering in the source NDJSON files,
preventing hash instability from sort-order differences between exporters.

### Pin format

```yaml
remotes:
  upstream:
    url: https://github.com/org/kg-data.git
    ref: main
    namespace: research
    pin: "sha256:abc123...64hexchars"
```

The `pin` value must match `SnapshotId` format exactly: `"sha256:"` followed by exactly
64 lower-case hex characters. `schema.yaml` validation (`khive pack check` or `khive kg validate`)
rejects malformed pin values.

### Failure behaviour

Fail closed. On hash mismatch:

1. Do **not** update `.khive/kg/` NDJSON files.
2. Do **not** update the local DB or remote cache.
3. Return `VcsError::HashMismatch { expected, actual }`.
4. CLI output includes: remote name, expected hash, actual hash, and a remediation hint
   (`khive kg sync --repin <remote>` to update the pin, or investigate the discrepancy).
5. Exit code 1.

Sensitive remote URLs are not printed in full if `schema.yaml` marks the remote as private
(a future field; current behaviour prints the remote name only, not the URL).

### Durability

The sync workflow mirrors the import safety protocol (ADR-055):

1. Fetch remote archive into a temporary staging directory.
2. Compute `SnapshotId` of the staged archive.
3. Compare against pin (if present). Abort on mismatch.
4. Atomically publish: rename staging files into `.khive/kg/remotes/<remote>/`.
5. Update `meta.json` with `{ fetched_at, ref, commit_sha, content_hash }`.

Staging ensures that a hash-check failure never leaves a partial archive in the live path.

### Repin workflow

`khive kg sync --repin <remote>` performs the sync, skips hash comparison, and writes the
computed hash back into `schema.yaml` as the new pin. This is a deliberate trust-upgrade
operation and requires the caller to verify the remote content independently before repinning.

## Consequences

### Positive

- Corruption and substitution attacks detected before live KG files are touched.
- Reuses existing `snapshot_id_for_archive` — no new hash logic.
- `VcsError::HashMismatch` is already defined and serializable.
- Staging prevents partial-update state on mismatch.

### Negative

- Pin maintenance overhead: every legitimate upstream update requires a repin.
  Teams that sync frequently from a moving `main` branch should use `--no-pin` mode
  (or omit the `pin` field) and accept the lower assurance.
- Canonical hash is over the logical archive, not the raw NDJSON bytes. A file with
  different line ordering but identical content hashes identically — this is correct
  but means the pin does not detect re-ordering without content change.

### Tests required

- Valid archive: hash matches pin, sync completes.
- Hash mismatch: `.khive/kg/remotes/` not updated, error serialized, exit 1.
- No pin declared: sync proceeds, hash logged but not enforced.
- Malformed pin in `schema.yaml`: rejected at validation time, not at sync time.
- `--repin`: hash computed and written back to `schema.yaml`.
- Canonical order invariance: two NDJSON files with same content but different line order
  produce identical `SnapshotId`.
- Staging rollback: simulated mid-sync failure leaves old remote cache intact.

## References

- ADR-048: Git-Native KG Versioning — `schema.yaml` remotes and pin fields
- ADR-051: CLI KG Git Workflow — `khive kg sync` command
- ADR-055: KG Import/Export Format Adapters — import safety and journal protocol
- ADR-071: Remote Entity Resolution — consumes the verified remote cache
- `crates/khive-vcs/src/hash.rs`: `snapshot_id_for_archive`, `canonical_json`
- `crates/khive-vcs/src/types.rs:11`: `SnapshotId` invariant
- `crates/khive-vcs/src/error.rs:42`: `VcsError::HashMismatch`
