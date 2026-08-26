# khive-repo-showcase Design

## Purpose

`khive-repo-showcase` is an offline, read-only exporter for deterministic `khive.repo.v1`
repository showcase bundles. It joins a repository snapshot with khive history and code-map
databases, builds bounded graph and analysis views, and emits a closed JSON model plus its JSON
Schema.

## Key types and modules

- `ExportRequest` identifies the clone, history database, map database, generation time,
  repository URL, source provenance, default-branch disclosure, and per-section bounds.
- `RepoBundle` is the closed top-level wire model: metadata, graph, aggregates, and capability
  catalog. `SchemaVersion` is fixed to `khive.repo.v1`.
- `Page`, `Disclosure`, `Availability`, and `SourceCoverage` distinguish complete, truncated,
  stopped, skipped, unknown, and unavailable data instead of treating missing evidence as empty.
- `RepoGraph` contains repository/package/module/symbol/history nodes, typed edges, navigation, and
  join-resolution evidence. `RepoAggregates` contains dependency, hotspot, coupling, treemap,
  cadence, ownership, API-surface, and scorecard analyses.
- `export.rs` validates inputs, composes the snapshot, produces canonical bytes, and persists them
  atomically. `read.rs` opens both SQLite inputs read-only.
- `join.rs` derives stable natural ids and resolves repository, Cargo/module, history, tag, and
  changed-path evidence. `aggregate.rs` computes deterministic bounded views from the joined graph.
- `ExportError` keeps missing data, ambiguous repository identity, malformed source records, git,
  SQLite, serialization, and persistence failures distinct.

## Invariants

- Source SQLite databases are opened with read-only/no-mutex flags. Repository inspection invokes
  non-interactive git with optional locks, automatic maintenance, hooks, and ambient git context
  disabled; the exporter does not mutate the clone.
- The bundle is tied to an exact 40-character HEAD SHA and explicit pipeline provenance. Mutable
  default-branch refs are never inferred from the clone; unavailable metadata is represented as
  unavailable.
- Public wire structs reject unknown fields, constrained scalar wrappers validate on decode, and
  the generated JSON Schema describes the same Rust model.
- Natural ids are SHA-256 hashes over a domain-separated schema version, node kind, and canonical
  components. Producers use explicit stable sort orders and ordered collections so equal inputs
  yield equal output bytes.
- Every page carries its bound, order, total-count availability, truncation bit, cursor, and
  disclosure status. Missing source coverage must degrade view capability explicitly, never appear
  as a silently complete empty collection.
- Caller-provided bounds are validated against schema maxima before source expansion. Aggregates
  are computed from the full admitted source sets and then bounded for output, preserving analysis
  meaning under pagination.
- `canonical_bytes` emits the compact serialized bundle followed by one newline.
  `write_canonical_atomic` writes and syncs a temporary file in the destination directory before
  persisting it over the destination.
- History repository matching must resolve to exactly one project. Zero or ambiguous matches fail
  with typed errors rather than joining unrelated data.
