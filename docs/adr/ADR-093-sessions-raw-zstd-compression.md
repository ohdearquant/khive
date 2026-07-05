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
(`crates/khive-pack-session/src/mirror/parse.rs:121`, `:200`, `:237` produce it via
`secret_gate::mask_secrets(trimmed)`; `:538` produces it via
`secret_gate::mask_secrets(&raw_json)` where `raw_json` comes from
`serde_json::to_string(node).unwrap_or_default()` at `:537`, so this call site can hand
`mask_secrets` an empty string on a serialization failure and store an empty `raw`). `text`
is a derived, much shorter extraction (the human-readable message body) used for display and
search; `raw` exists so a message can be losslessly reconstructed later, independent of what
today's parser chose to extract into `text`.

Every row written today, by every one of these call sites, is a masked JSONL string (or, per
the failure case above, an empty string) bound as plain SQLite text:
`SqlValue::Text(ev.raw.clone())` at `crates/khive-pack-session/src/mirror/ingest.rs:693`.
Nothing in the current write path prepends a marker byte. A masked JSONL line starts with the
first printable character of the original JSON object, in practice `{` (`0x7B`), never a
reserved format byte. This matters directly for the encoding decision below: any scheme that
introduces a leading version byte must not confuse "no prefix, plain text" with "prefix byte
0x00" -- they are different representations of the same 1,070,410 rows that exist on disk
today, and conflating them would make every existing row look either corrupt or
misinterpreted.

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
where a unified `fts_notes` shadow table indexes plaintext note content separately from the
row (`crates/khive-db/sql/004-fts-consolidation.sql:22-33`, `CREATE VIRTUAL TABLE IF NOT
EXISTS fts_notes USING fts5(...)`) --- that pattern is available as the template a future FTS
addition here would have to follow if one is ever added: the FTS copy would index the
decompressed plaintext, never compressed bytes, exactly as `fts_notes` already does for KG
substrate notes.

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

### 1. Encoding: discriminate on SQLite value type, version-prefix only the BLOB side

Legacy rows cannot be given a version byte after the fact without a rewrite, and today's rows
have no such byte (see Context: every existing `raw` value is a bare masked JSONL string, or
occasionally an empty string, bound as `SqlValue::Text`). SQLite is dynamically typed per
value: a column declared `TEXT` still accepts a `BLOB`-typed value, and `typeof(raw)` at read
time distinguishes them exactly. This ADR uses that distinction as the coexistence mechanism,
instead of an in-band prefix byte that every row -- including the 1,070,410 already on disk --
would need to carry:

- **Stored value is SQLite `TEXT`** -- legacy plaintext. Returned unchanged, byte for byte,
  including the empty string produced by the `unwrap_or_default()` failure path
  (`crates/khive-pack-session/src/mirror/parse.rs:537`). No prefix is stripped, no
  decompression is attempted. This is a passthrough decode, not a new format: it is exactly
  what every reader does today, made explicit as one of two branches. This passthrough
  necessarily has weaker corruption detection than the BLOB branch below -- a legacy `TEXT`
  row that was corrupted in place is indistinguishable from a legacy row that was always
  that way, since there is no prefix or checksum to fail against. This ADR accepts that
  tradeoff for the ~1M rows already written; it does not require rewriting them to gain
  stronger detection (see Migration, #4).
- **Stored value is SQLite `BLOB`** -- versioned format. The first byte is a version prefix,
  read via `typeof(raw) = 'blob'` before touching the payload:
  - `0x00` -- reserved for an explicitly-uncompressed `BLOB` write (`0x00 || UTF-8 masked
    JSONL`). No code path emits this today and none is required for this ADR's initial cut;
    it exists so "store uncompressed but already migrated off legacy `TEXT`" has a defined
    on-disk shape if a future need for it appears, without colliding with the legacy
    passthrough above.
  - `0x01` -- zstd-compressed, no dictionary. Remaining bytes are a raw zstd frame.
  - `0x02` -- zstd-compressed with dictionary generation 1 (see #2). Remaining bytes are a
    zstd frame produced against that dictionary.

A `BLOB` value whose first byte is not a prefix this build recognizes, or whose payload fails
to decompress (bad frame, wrong dictionary, dictionary ID mismatch -- see #2), fails loud (see
#6); it never guesses and never falls back to treating `BLOB` bytes as `TEXT`. New rows are
always written as `BLOB`, in the current preferred format (`0x02` once a dictionary is
deployed, `0x01` before that). No new row is ever written as legacy `TEXT` again once this
ships; `TEXT` is a read-only legacy shape from this point forward.

**Required tests** (in addition to whatever test suite implements this): a pre-existing `TEXT`
row whose payload starts with `{`, decoded as plaintext passthrough; a pre-existing `TEXT` row
whose payload is the empty string, decoded as passthrough (not an error); a `BLOB` row with
prefix `0x01`, decoded via plain zstd; a `BLOB` row with prefix `0x02`, decoded via the
dictionary; and a `BLOB` row with an unrecognized prefix byte, which must fail loud rather than
return any bytes.

### 2. Dictionary: yes, trained from the pack's own corpus, pinned per prefix byte forever

Use a trained zstd dictionary (format `0x02`). The measurement above shows the dictionary
closing roughly half the gap between plain per-row compression (2.1-2.2x) and the
theoretical concatenated ceiling (4.3-5.2x), at a small, size-independent per-call cost ---
exactly the shape of win a dictionary is for: many small, structurally similar values. Format
`0x01` (no dictionary) is kept in the version scheme as a fallback for rows written before a
dictionary is trained or in a codepath that cannot supply one, not as the target steady state.

**Prefix-to-dictionary registry.** A prefix byte alone does not identify which dictionary
bytes produced a given `0x02` row unless the mapping from prefix to dictionary content is
fixed and never reused for different bytes. This ADR specifies a registry, not a single
mutable dictionary slot:

- `0x02` maps to dictionary generation 1, and only ever to generation 1. The generation-1
  dictionary bytes are embedded once (via `include_bytes!`) and never replaced under that
  prefix.
- A retrain that changes dictionary content is generation 2, shipped under a new prefix
  byte, `0x03`. Every build of `khive-pack-session` that claims to read `0x03` rows embeds
  both the generation-1 and generation-2 dictionary bytes (`include_bytes!` per generation,
  additive, never swapped). A build that only ships generation 1 does not claim to support
  `0x03` and fails loud on it, per the unrecognized-prefix rule in #1.
- This continues for any further generation: each retrain is a new prefix, each prefix's
  dictionary bytes are permanent, and a reader's supported-prefix set is exactly the set of
  dictionary generations it has embedded.

**Downgrade rule.** An older binary that does not recognize a newer prefix (for example, one
built before generation 2 shipped, encountering `0x03`) fails loud on that row rather than
guessing at a dictionary. Older binaries are not rollback readers for rows written by newer
ones under a prefix they do not know; rollback safety in this design only ever guarantees that
a row survives forward, never that a stale binary can read formats introduced after it was
built.

**Integrity check.** A zstd frame produced with a dictionary carries a dictionary ID in its
frame header. The decoder must check that ID against the dictionary it is about to decompress
with and fail loud on a mismatch, rather than attempting to decompress against the wrong
dictionary and risking a garbage or truncated result. This is in addition to, not instead of,
the unrecognized-prefix check in #1: prefix routing picks which embedded dictionary to try,
frame-ID verification confirms that choice was actually correct before trusting the output.

**Compression level.** Format `0x02` uses zstd level 3. The measured gain from level 19 over
level 3 (3.256x vs 2.937x, both per-row-plus-dictionary) does not justify its added CPU cost
on the ingest path, which runs inline with the background mirror tailing live session files;
level 3 is the standard "fast" zstd operating point and keeps ingest overhead low per message.
Revisiting the level is a config-level follow-up, not a schema or format change, if a future
measurement shows the CPU cost is acceptable for the extra ratio at a given ingest volume.

### 3. Write and read path: the store layer owns compression, transparently

Compression happens at the point `raw` is bound into the `INSERT` (currently
`crates/khive-pack-session/src/mirror/ingest.rs:693`) and decompression happens at the point
a `raw` value is read out of a row, before it is handed to any caller. Neither the mirror's
event parsing (`mirror/parse.rs`) nor any future consumer of `raw` should see the SQLite value
type or the prefix byte --- they read and write plain masked JSONL strings. Concretely, this
means introducing a small codec module in `khive-pack-session`: encode always produces a
`BLOB` (mask -> compress -> prepend prefix byte -> bind as `SqlValue::Blob`); decode first
checks `typeof(raw)` (`TEXT` -> passthrough, per #1; `BLOB` -> strip prefix, dispatch on it,
decompress). Every write and every read of the `raw` column routes through this codec, rather
than compressing ad hoc at each call site. This keeps the format an implementation detail of
the session pack's storage, matching the existing separation where `khive-storage` is the
trait-only capability surface and packs own their own schema and encoding choices (ADR-017,
ADR-028).

### 4. Migration: lazy, not one-shot bulk recompress

New rows are written as compressed `BLOB` values from the day this ships. Existing rows are
left as legacy `TEXT` and are read exactly as before --- the decode path recognizes `TEXT` via
`typeof(raw)` and returns the bytes unchanged (see #1), so no existing row needs to be touched
for correctness. This ADR does not recommend a one-shot bulk recompression pass:
`session_messages` rows are immutable after insert (`INSERT OR IGNORE`, no `UPDATE` path
touches `raw`), so the only way to shrink an old row's stored footprint is to rewrite it, and a
background rewrite pass is strictly a size-reclamation optimization, not a correctness
requirement. If disk pressure on `~/.khive/sessions.db` later makes that worth doing, it should
be a separate follow-up (a one-shot pass that reads each legacy `TEXT` row, re-encodes it as a
`0x02` `BLOB`, and updates in place) rather than bundled into this ADR's initial cut. Whichever
way old rows are handled, `VACUUM` is a separate, necessary step: SQLite does not shrink a
database file on `UPDATE`/`DELETE` by itself, so reclaiming space freed by any later
recompression pass requires an explicit `VACUUM` afterward (the session pack has no equivalent
to the memory pack's `memory.vacuum` verb today; a bulk-recompress follow-up would need to add
one, or reuse a maintenance path if one exists by then).

### 5. FTS interaction: none today, and none introduced by this change

As established above, `session_messages.raw` is not FTS-indexed (`vocab.rs` schema plan
declares no FTS5 table for this pack). This ADR does not add FTS indexing. If FTS indexing of
`raw` or `text` is added later, it must index the decompressed plaintext, exactly as the KG
substrate's `fts_notes` shadow table already indexes plaintext note content separately from
the primary row (`crates/khive-db/sql/004-fts-consolidation.sql:22-33`) --- an FTS trigger or
population job must go through the same decode path as any other reader, never index
compressed bytes directly. This ADR states that requirement explicitly so a future FTS
addition does not silently index zstd frames as if they were text.

### 6. Failure and rollback: fail loud on decode, never on encode

Decompression failure (corrupt frame, unrecognized prefix byte, dictionary ID mismatch) must
return an error to the caller, never silently return the compressed/ciphertext bytes as if
they were plaintext, and never silently substitute an empty string. This is a data-integrity
bug if it happens, not a degraded-but-usable path --- `raw` exists specifically so message
content can be losslessly reconstructed, and a masked failure would defeat that guarantee
invisibly. Rollback is inherent in the type-discriminated design: because every reader supports
legacy `TEXT` passthrough unconditionally and that passthrough never changes, "roll back to
uncompressed" is never an operation that has to be performed on data --- it only means
reverting the write path to stop emitting `BLOB` rows, at which point existing rows, in either
shape, remain readable under the same decode function. There is no scenario in which rolling
back requires touching already-written rows. A downgrade to a binary that recognizes fewer
`BLOB` prefixes than were in use (see #2) is a distinct case from a same-format rollback: it
fails loud on the prefixes it does not know, by design, rather than silently misreading them.

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
- `session_messages.raw` values on disk become one of two SQLite value types (legacy `TEXT`
  passthrough, or a version-prefixed `BLOB` in one of `0x00`/`0x01`/`0x02`); every reader
  (test suite included) must go through the shared decode function rather than reading the
  column as a plain string.
- Storage footprint for new session ingestion drops to roughly 30-35% of current size once the
  trained dictionary lands (format `0x02`), versus today's 100% (legacy `TEXT`, no
  compression). Existing data does not shrink until/unless a separate recompression follow-up
  is scoped and run, with `VACUUM` after it.
- A dictionary-generation bump is a compiled-binary change (additional `include_bytes!`
  content plus a new prefix byte, never a replacement of an existing generation's bytes), not
  a schema migration; it does not touch `crates/khive-db/src/migrations.rs`, since
  `session_messages` schema itself (column list, types) is unchanged by this ADR.
