# ADR-147 D5 join feasibility

This exhibit measures the history-to-structure join required by
[ADR-147](ADR-147-repo-showcase-bundle.md) against the khive repository at
`c2979d2443738a075e55a170c772d1dc86cf0f91`.

The reproducible bundle command is:

```bash
scripts/generate-repo-showcase.sh
```

That script uses commits-only history, omits mutable tag observations, supplies
`main` explicitly as the default-branch label, and writes the canonical golden
to `docs/schemas/examples/khive-repo-v1-khive.json`.

## Producer-aligned current-snapshot join

`git.digest` persists repository-relative `changed_paths` on commit notes.
`code.ingest` persists repository-relative `source_path` and `source_revision`
on module entities. At export, a Rust path matches a module only when the path
is equal and the module revision is the bundle HEAD. The resulting edge is
marked `origin: "derived"` with derivation method
`changed_path_source_path_exact`; neither source database is represented as
having ingested that edge.

| Repository          | Language | Files | Derived keys | Entity keys | Matched | Resolution |
| ------------------- | -------- | ----: | -----------: | ----------: | ------: | ---------: |
| `ohdearquant/khive` | Rust     |   658 |          658 |         658 |     658 |     100.0% |

Current-snapshot path residuals: none. Current-snapshot entity residuals: none.
The checked-out corpus also contains 86 tracked TypeScript/TSX files and 57
tracked Python files. Their path conventions are not measured in v1, so the
bundle reports both language joins as unavailable rather than assigning them a
zero rate.

## Why the standalone Cargo path table is not the join

Applying the earlier proposed path-to-module table literally to the same 658
tracked Rust files produces a different result:

| Repository          | Language | Files | Derived keys | Entity keys | Matched | Resolution |
| ------------------- | -------- | ----: | -----------: | ----------: | ------: | ---------: |
| `ohdearquant/khive` | Rust     |   658 |          656 |         658 |     654 |      99.4% |

The residuals are:

- path unresolved: `crates/khive-runtime/build.rs`;
- derived key without an entity: `khive-pack-gtd/tests::common::mod`;
- derived key without an entity: `khive-pack-schedule/tests::support::mod`;
- entity unreached: `khive-pack-gtd/tests::common` from
  `crates/khive-pack-gtd/tests/common/mod.rs`;
- entity unreached: `khive-pack-schedule/tests::support` from
  `crates/khive-pack-schedule/tests/support/mod.rs`;
- entity unreached: `khive-runtime/build` from
  `crates/khive-runtime/build.rs`;
- entity unreached: `kkernel/crate::main` from
  `crates/kkernel/src/main.rs`.

The table maps both `src/lib.rs` and `src/main.rs` to `crate`, creating a key
collision in `kkernel`; the live code producer deliberately maps the latter to
`crate::main`. It also leaves `build.rs` unresolved and retains the terminal
`mod` segment for nested `mod.rs` files, while the producer maps those files to
their containing module. Reimplementing that table in the exporter would
therefore disagree with the source of truth it is meant to join.

The producer-aligned semantic mapping, when it is needed independently of an
already persisted `source_path`, is:

- `src/lib.rs` -> `crate`;
- `src/main.rs` -> `crate::main`;
- any nested `mod.rs` -> its containing module path;
- `src/a/b.rs` -> `a::b`;
- `tests/a/b.rs` -> `tests::a::b`;
- `benches/a.rs` -> `benches::a`;
- `examples/a.rs` -> `examples::a`;
- `build.rs` -> `build`.

Crate ownership uses the nearest governing `Cargo.toml`, equivalently the
longest manifest-directory prefix. This resolves all 658 current Rust paths to
the 658 module entities produced by `code.ingest`. The exporter still joins on
the persisted, revision-pinned `source_path`; the mapping above documents why
those producer facts have the identities they do.

## Historical coverage

Current modules describe one snapshot, while commit paths span repository
history. Deleted and renamed Rust paths therefore remain legitimate historical
residuals rather than current-map failures.

| Changed paths | Rust in scope | Matched Rust events | Out of scope | Unresolved Rust events |
| ------------: | ------------: | ------------------: | -----------: | ---------------------: |
|         7,558 |         4,344 |               4,309 |        3,214 |                     35 |

All 4,309 emitted commit-to-module edges use the exact-path derived provenance.
The 35 unresolved events are retained in the bundle with their commit SHA and
reason. They refer to these 14 distinct paths:

- `crates/khive-hnsw/src/index/build_batch.rs`
- `crates/khive-mcp/src/main.rs`
- `crates/khive-pack-brain/src/section.rs`
- `crates/khive-pack-brain/src/state.rs`
- `crates/khive-pack-session/src/handlers/get.rs`
- `crates/khive-retrieval/src/graph/bfs.rs`
- `crates/khive-retrieval/src/graph/compat.rs`
- `crates/khive-retrieval/src/graph/dfs.rs`
- `crates/khive-retrieval/src/graph/helpers.rs`
- `crates/khive-retrieval/src/graph/mod.rs`
- `crates/khive-retrieval/src/graph/shortest.rs`
- `crates/khive-retrieval/src/graph/tests.rs`
- `crates/khive-retrieval/src/graph/types.rs`
- `crates/kkernel/src/pending_events.rs`

The 3,214 out-of-scope events are disclosed separately and do not reduce the
Rust resolution denominator. This avoids representing unmeasured languages,
documentation, configuration, or other repository paths as failed Rust joins.
