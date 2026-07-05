# ADR-093: zstd Compression for Session-Mirror Raw Storage

**Status**: Proposed
**Date**: 2026-07-04
**Depends on**: ADR-028 (pack-scoped backends), ADR-080 (session pack OSS storage mechanism)

## Context

### The session mirror stores a full masked copy of every ingested line

`khive-pack-session` runs a background mirror (`SessionPack::warm`,
`crates/khive-pack-session/src/pack.rs:89`) that tails Claude Code and Codex CLI transcript
JSONL files and replays them into a `session_messages` table. The table is declared in
`crates/khive-pack-session/src/vocab.rs:28-40`; the column this ADR is about is `raw`, line 37:

```
"CREATE TABLE IF NOT EXISTS session_messages (\
    id              TEXT PRIMARY KEY,\
    session_id      TEXT NOT NULL,\
    seq             INTEGER NOT NULL,\
    parent_uuid     TEXT,\
    is_sidechain    INTEGER NOT NULL DEFAULT 0,\
    role            TEXT,\
    msg_type        TEXT NOT NULL,\
    text            TEXT,\
    raw             TEXT NOT NULL,\
    created_at      INTEGER NOT NULL,\
    namespace       TEXT\
)",
```

`raw` holds the verbatim source JSONL line, secret-masked, for every ingested message
(`crates/khive-pack-session/src/mirror/parse.rs:121`, `:200`, `:237`, `:538` all produce it via
`secret_gate::mask_secrets(trimmed)`). `text` is a derived, much shorter extraction (the
human-readable message body) used for display and search; `raw` exists so a message can be
losslessly reconstructed later, independent of what today's parser chose to extract into
`text`.

This pack runs against its own SQLite backend, not `khive.db`. `[[backends]]` routes the
`session` pack to a dedicated file (`docs/configuration.md:90-103`):

```toml
[[backends]]
name   = "sessions"
kind   = "sqlite"
path   = "~/.khive/sessions.db"

[packs.session]
backend = "sessions"
```

On this machine that file is 4.4GB (`~/.khive/sessions.db`, measured 2026-07-04) holding
1,070,410 rows in `session_messages`. Full cross-tool history (Claude Code + Codex CLI JSONL
on disk, ~87.6GiB combined) projects to a multi-GB `sessions.db` as ingestion catches up ---
cheap relative to source, but growing linearly with session volume and with no compression
in the current write path.

### Write and read paths

Write path: `crates/khive-pack-session/src/mirror/ingest.rs:669-693`. Each ingested event is
inserted with `INSERT OR IGNORE INTO session_messages (..., raw, ...)`, binding
`SqlValue::Text(ev.raw.clone())` at line 693. This is the only place `raw` is written.

Read path: today, nothing in the production verb surface reads `raw` back. `session.store`,
`session.list`, `session.resume`, and `session.export` (`crates/khive-pack-session/src/
handlers/{store,list,resume,export}.rs`) operate on a separate concept: `session` kind notes
in the KG substrate (`fetch_session_note`, `crates/khive-pack-session/src/handlers/mod.rs:166`),
not on the `session_messages` mirror table at all. The only current reader of `raw` is the
test suite, verifying round-trip and secret-masking behavior with a direct `SELECT text, raw
FROM session_messages ...` (`crates/khive-pack-session/src/mirror/ingest.rs:2362`). `raw` is
therefore, as of this ADR, a write-heavy / read-rare column: a lossless backfill store for a
reconstruction or replay feature that does not exist yet, not a column on a hot query path.
This shapes the failure-mode requirements below --- correctness on the rare read matters more
than shaving decompression latency on a path nothing currently exercises at request time.

### No FTS indexing on this table

`SESSION_SCHEMA_PLAN_STMTS` (`crates/khive-pack-session/src/vocab.rs:14-49`) declares exactly
three tables and three indexes; none of them is an FTS5 virtual table, and
`SessionPack::edge_rules()` / the pack's schema plan register no FTS shadow table for
`session_messages`. `raw` is not FTS-indexed today. This differs from KG substrate notes,
where an FTS5 shadow table indexes plaintext content separately from the row
(`crates/khive-db/sql/schema.sql`) --- that pattern is available as the template a future FTS
addition here would have to follow if one is ever added: the FTS copy would index the
decompressed plaintext, never compressed bytes, exactly as the KG substrate already does for
notes.

### No zstd dependency in the workspace yet

`grep -rn "zstd" crates/*/Cargo.toml` returns nothing: no crate in the workspace depends on
zstd today. Adding compression here means adding the `zstd` crate (safe Rust bindings over
libzstd) as a new dependency of `khive-pack-session`, not reusing an existing binding.

### Measured compression (2026-07-04, `~/.khive/sessions.db`)

Read-only sample: every 200th row of `session_messages` by `rowid` (a spread sample, not just
the head of the table), taken via `sqlite3 "file:~/.khive/sessions.db?mode=ro"` in query mode.
Sample size: 5,352 rows (>= the 1,000-row floor), 16,838,985 raw bytes, average row 3,146
bytes. No write or lock was taken against the production file; the sample was copied out to a
scratch file and measured with the `zstandard` Python bindings (libzstd) in a worktree-local
scratch directory.

| Strategy                             | Compressed bytes | Ratio (raw / compressed) | % of original |
| ------------------------------------ | ---------------: | -----------------------: | ------------: |
| Per-row, level 3                     |        7,843,981 |                   2.147x |        46.58% |
| Per-row, level 19                    |        7,490,117 |                   2.248x |        44.48% |
| Concatenated, level 3                |        3,932,145 |                   4.284x |        23.34% |
| Concatenated, level 19               |        3,242,644 |                   5.195x |        19.25% |
| Per-row + 110KB trained dict, lvl 3  |        5,734,272 |                   2.937x |        34.05% |
| Per-row + 110KB trained dict, lvl 19 |        5,170,977 |                   3.256x |        30.71% |

Dictionary trained via `zstandard.train_dictionary()` on the first 2,000 sampled rows, applied
to all 5,352 sampled rows (train and eval sets overlap for the first 2,000 rows; this measures
achievable ratio with a representative dictionary, not out-of-sample generalization). Smaller
dictionary sizes were also measured at level 3: 16KB -> 2.753x, 32KB -> 2.827x, 64KB -> 2.884x
--- ratio keeps improving with dictionary size but with diminishing returns past ~64KB for this
row-size distribution (average row 3,146 bytes; small values are exactly where a shared
dictionary earns its keep, because plain per-row zstd has no cross-row history to draw on).

Reading: per-row compression without a dictionary barely beats 2x, because each row is short
enough that most of zstd's window is spent re-learning structure (JSON key names, tool-call
scaffolding, provider boilerplate) that is nearly identical across rows. Concatenated
compression captures that redundancy and does far better (4.3x-5.2x), but concatenation is not
a viable row-store strategy --- it would mean grouping rows into blocks and losing single-row
random access, a materially bigger change than this ADR's scope. A trained dictionary recovers
most of that cross-row redundancy while keeping per-row independence and random access intact:
2.94x-3.26x, roughly 30-34% of original size, close to half again as much saving as no
dictionary at negligible per-row cost (dictionary-primed compression is a fixed small
constant-time cost per call, not a function of corpus size).

## Decision

### 1. Encoding: one-byte version prefix, coexisting formats

Every `raw` value is prefixed with a single version byte before being stored as a `BLOB`
(changing the column's logical content, not necessarily its declared SQL type --- SQLite is
dynamically typed per value regardless of column affinity, so a `TEXT`-affinity column can
hold blob bytes without a schema change):

- `0x00` --- uncompressed. The remaining bytes are the masked JSONL line, UTF-8, exactly as
  today. Every row written before this ADR ships reads as this format forever; no backfill
  rewrite is required for old rows to remain readable (see Migration, below).
- `0x01` --- zstd-compressed, no dictionary. Remaining bytes are a raw zstd frame.
- `0x02` --- zstd-compressed with the pack's trained dictionary (see #2). Remaining bytes are
  a zstd frame produced against the pinned dictionary content ID; the dictionary itself is
  not repeated per row.

A reader that encounters a byte other than `0x00`, `0x01`, `0x02` fails loud (see #6); it never
guesses. New rows are always written in the current preferred format (`0x02` once a dictionary
is deployed, `0x01` before that, `0x00` never again once this ships). This mirrors the
versioned-prefix pattern already used for secret masking in this pack
(`secret_gate::mask_secrets`, `crates/khive-pack-session/src/mirror/parse.rs:121`), which
similarly embeds a marker (`***MASKED***`) directly in the stored string rather than in a
side table.

### 2. Dictionary: yes, trained from the pack's own corpus, versioned

Use a trained zstd dictionary (format `0x02`). The measurement above shows the dictionary
closing roughly half the gap between plain per-row compression (2.1-2.2x) and the
theoretical concatenated ceiling (4.3-5.2x), at a small, size-independent per-call cost ---
exactly the shape of win a dictionary is for: many small, structurally similar values. Format
`0x01` (no dictionary) is kept in the version scheme as a fallback for rows written before a
dictionary is trained or in a codepath that cannot supply one, not as the target steady state.

Storage and versioning: the dictionary is generated offline (from a sample of ingested rows,
following the measurement recipe above) and shipped as a static asset alongside the pack
(embedded via `include_bytes!` at build time, analogous to how `SESSION_SCHEMA_PLAN_STMTS` is
a static compiled-in array rather than a runtime-loaded file). The dictionary's content
constitutes part of the on-disk format for every `0x02` row, so it is versioned by a dictionary
generation number embedded in the pack's compiled binary; format `0x02` implicitly refers to
"the dictionary this build of `khive-pack-session` embeds." A retrained (regenerated)
dictionary that changes the bytes read back incompatibly requires a new prefix byte (`0x03`),
not a silent swap under the existing `0x02` meaning --- old `0x02` rows must remain decodable
against the dictionary generation they were written with. This ADR does not commit to a
retraining cadence; it only requires that retraining, if it happens, bump the prefix.

### 3. Write and read path: the store layer owns compression, transparently

Compression happens at the point `raw` is bound into the `INSERT` (currently
`crates/khive-pack-session/src/mirror/ingest.rs:693`) and decompression happens at the point
a `raw` value is read out of a row, before it is handed to any caller. Neither the mirror's
event parsing (`mirror/parse.rs`) nor any future consumer of `raw` should see compressed bytes
or know the format byte exists --- they read and write plain masked JSONL strings. Concretely,
this means introducing a small codec module in `khive-pack-session` (encode: mask -> prefix ->
optionally compress; decode: strip prefix -> optionally decompress) and routing every write and
every read of the `raw` column through it, rather than compressing ad hoc at each call site.
This keeps the format an implementation detail of the session pack's storage, matching the
existing separation where `khive-storage` is the trait-only capability surface and packs own
their own schema and encoding choices (ADR-017, ADR-028).

### 4. Migration: lazy, not one-shot bulk recompress

New rows are written compressed from the day this ships. Existing rows are left as format
`0x00` and are read exactly as before --- the decode path recognizes `0x00` and returns the
bytes unchanged, so no existing row needs to be touched for correctness. This ADR does not
recommend a one-shot bulk recompression pass: `session_messages` rows are immutable after
insert (`INSERT OR IGNORE`, no `UPDATE` path touches `raw`), so the only way to shrink an
old row's stored footprint is to rewrite it, and a background rewrite pass is strictly a
size-reclamation optimization, not a correctness requirement. If disk pressure on
`~/.khive/sessions.db` later makes that worth doing, it should be a separate follow-up (a
one-shot pass that reads each `0x00` row, re-encodes it as `0x02`, and updates in place) rather
than bundled into this ADR's initial cut. Whichever way old rows are handled, `VACUUM` is a
separate, necessary step: SQLite does not shrink a database file on `UPDATE`/`DELETE` by
itself, so reclaiming space freed by any later recompression pass requires an explicit
`VACUUM` afterward (the session pack has no equivalent to the memory pack's
`memory.vacuum` verb today; a bulk-recompress follow-up would need to add one, or reuse a
maintenance path if one exists by then).

### 5. FTS interaction: none today, and none introduced by this change

As established above, `session_messages.raw` is not FTS-indexed (`vocab.rs` schema plan
declares no FTS5 table for this pack). This ADR does not add FTS indexing. If FTS indexing of
`raw` or `text` is added later, it must index the decompressed plaintext, exactly as the KG
substrate's FTS5 shadow tables already index plaintext note content separately from the
primary row (`crates/khive-db/sql/schema.sql`) --- an FTS trigger or population job must go
through the same decode path as any other reader, never index compressed bytes directly. This
ADR states that requirement explicitly so a future FTS addition does not silently index
zstd frames as if they were text.

### 6. Failure and rollback: fail loud on decode, never on encode

Decompression failure (corrupt frame, unrecognized prefix byte, dictionary mismatch) must
return an error to the caller, never silently return the compressed/ciphertext bytes as if
they were plaintext, and never silently substitute an empty string. This is a data-integrity
bug if it happens, not a degraded-but-usable path --- `raw` exists specifically so message
content can be losslessly reconstructed, and a masked failure would defeat that guarantee
invisibly. Rollback is inherent in the versioned-prefix design: because every reader supports
format `0x00` unconditionally and format `0x00` never changes, "roll back to uncompressed" is
never an operation that has to be performed on data --- it only means reverting the write path
to stop emitting `0x01`/`0x02`, at which point existing rows in every format remain readable
under the same decode function. There is no scenario in which rolling back requires touching
already-written rows.

### 7. Non-goals

- No changes to `khive.db` or any KG substrate table, schema, or verb. This ADR is scoped
  entirely to the `sessions` backend and the `session_messages.raw` column.
- No MCP wire-surface changes. `session.store`, `session.list`, `session.resume`,
  `session.export` are unaffected; they do not read `session_messages` at all (see Context).
- No new verbs. Compression is an internal storage-layer change, invisible to every caller of
  the `request` tool.

## Alternatives considered

- **Column-level SQLite compression extensions** (e.g., a custom VFS or page-level
  compression). Rejected: operates below the row/column boundary, would apply uniformly to
  every column in the backend file (including primary keys and indexes where compression buys
  nothing and adds risk), and is a much larger surface to validate than a single-column codec.
- **Store `raw` compressed in a separate blob table, keyed by message id.** Rejected: adds a
  join to a read path that today is already rare (see Context), and duplicates the versioning
  problem (still need a prefix or a format column to distinguish compressed/uncompressed blobs)
  without the simplicity of an in-place, single-table encoding.
- **Concatenated block compression** (group N rows into a compressed block). Rejected despite
  the best measured ratio (4.3x-5.2x): breaks `INSERT OR IGNORE` per-row idempotent replay
  semantics and single-row random access (`SELECT ... WHERE id = ?`), both load-bearing for the
  mirror's replay-safety design (`crates/khive-pack-session/src/mirror/ingest.rs`). Revisit only
  if per-row-plus-dictionary compression later proves insufficient at scale.
- **No dictionary, per-row only.** Rejected as the sole strategy: leaves roughly a third of
  the achievable ratio on the table (2.1-2.2x vs. 2.9-3.3x measured) for a fixed, one-time
  dictionary-training cost with no ongoing overhead per row.

## Consequences

- New dependency: `zstd` crate added to `khive-pack-session/Cargo.toml`.
- `session_messages.raw` values on disk become one of three self-describing formats; every
  reader (test suite included) must go through the shared decode function rather than reading
  the column as a plain string.
- Storage footprint for new session ingestion drops to roughly 30-35% of current size once the
  trained dictionary lands (format `0x02`), versus today's 100% (format `0x00`, no
  compression). Existing data does not shrink until/unless a separate recompression follow-up
  is scoped and run, with `VACUUM` after it.
- A dictionary-generation bump is a compiled-binary change (new `include_bytes!` content), not
  a schema migration; it does not touch `crates/khive-db/src/migrations.rs`, since
  `session_messages` schema itself (column list, types) is unchanged by this ADR.
