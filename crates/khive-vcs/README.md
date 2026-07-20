# khive-vcs

KG versioning — content-addressed snapshot hashing and NDJSON-to-SQLite sync.

Git-native v1: KG state lives as sorted NDJSON files (`entities.ndjson`, `edges.ndjson`)
in a git repository. This crate provides the canonical hash that identifies a snapshot's
content, the sync routine that rebuilds a queryable SQLite database from those NDJSON
files, and remote-archive fetch with hash-pin verification. Branching, history, and push/pull
are git's job — there is no custom khive remote protocol.

## Features

- **Canonical SHA-256 snapshot hashing** — deterministic, order-independent content hash
  over entities and edges (`SnapshotId`, prefixed `sha256:<hex64>`)
- **NDJSON-to-SQLite sync** — atomic rebuild of a working database from NDJSON sources
- **Remote archive fetch** — sparse `git clone` of a remote KG archive with SHA-256 pin
  verification, fail-closed on mismatch
- **Validated wire types** — `SnapshotId` and its `Deserialize` impl reject anything that
  isn't exactly `sha256:` + 64 lower-case hex characters

## Usage

```rust
use khive_runtime::portability::KgArchive;
use khive_vcs::hash::snapshot_id_for_archive;

fn print_snapshot_id(archive: &KgArchive) {
    // Deterministic regardless of entity/edge insertion order or property key order.
    let id = snapshot_id_for_archive(archive).expect("archive hashes cleanly");
    println!("{id}"); // "sha256:<64 hex chars>"
}
```

Rebuilding a working database from NDJSON sources (used by `kkernel sync`):

```rust
use std::path::Path;

async fn rebuild(repo_root: &Path, db_path: &Path) -> anyhow::Result<()> {
    let report = khive_vcs::sync::run_sync(repo_root, db_path, "local").await?;
    println!("{} entities, {} edges -> {}", report.entities, report.edges, report.db_path);
    Ok(())
}
```

`run_sync` reads `.khive/kg/{entities,edges}.ndjson` under `repo_root`, validates every
edge relation before touching disk, builds the new database in a `.tmp` sibling file, and
renames it over `db_path` only on success — a crash or parse error leaves the previous
database intact. `run_sync_remote(repo_root, &RemoteConfig, repin)` fetches a remote KG
archive (sparse, depth-1 clone), verifies its content hash against `RemoteConfig::pin`
when set, and populates `.khive/kg/remotes/<name>/` with a `meta.json` recording the
resolved commit and hash.

## Semantics

- `SnapshotId::from_hash` / `from_prefixed` are the only ways to construct one outside
  `snapshot_id_for_archive`; both reject anything but 64 lower-case hex characters.
- `canonical_json` sorts entities by UUID, edges by `(source, target, relation)`, tags
  lexicographically, and object keys recursively — two archives differing only in
  insertion order hash identically; two differing in `entity_type` or edge weight do not.
- `VcsState` tracks `namespace`, `current_branch`, and `last_committed_id` — there is no
  `dirty` flag; uncommitted changes are computed fresh via a DB-vs-NDJSON diff, not cached.
- `KG_V1_COVERAGE` records that v1 snapshots cover entities and edges only; notes are
  excluded pending a note pack that defines versioned export/import/redaction/merge.

## Where this sits

`khive-vcs` depends on `khive-runtime`, `khive-storage`, and `khive-types`, and is a
regular workspace member and published crate — not forward-deployed. `kkernel` consumes
it directly for the `sync`, `kg status`, and `kg fetch` subcommands.

[`khive-merge`](https://crates.io/crates/khive-merge) is the forward-deployed v2 semantic
merge layer that will consume `khive-vcs`'s snapshot ancestry once ADR-039's LCA-walk
integration lands; today's merge path is git's own line-merge over sorted NDJSON.

Governed by [ADR-010](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-010-kg-versioning.md)
(versioning strategy) and [ADR-020](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-020-git-native-kg-implementation.md)
(git-native implementation).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
