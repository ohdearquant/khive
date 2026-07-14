# Session mirror ingest

Technical reference for the bounded tail-read ingest algorithm in
`crates/khive-pack-session/src/mirror/ingest.rs` — how `mirror_file` and
`mirror_chatgpt_export_file` read, bound, and durably checkpoint session transcripts. The
`.rs` file keeps the published contract for those two entry points plus short pointers
back to the sections below.

## Bounded tail-read algorithm (module overview)

`mirror_file` reads new bytes from a JSONL file starting at `start_offset` via
a buffered, line-at-a-time reader bounded by an internal per-pass byte/event
cap AND a per-line byte cap, parses complete lines using the parser selected
by `LineTailSource` (mapped internally to `MirrorSource`), and writes the
resulting bounded chunk to the session mirror tables in a single transaction.
A single call processes at most one bounded chunk — never the whole file at
once — so the caller's polling loop advances the persisted cursor
incrementally across multiple calls for large deltas. It is safe to call
repeatedly on the same file; `INSERT OR IGNORE` keyed by the event UUID
ensures idempotency.

No single line, complete or partial, is ever buffered past
`MirrorLimits::max_line_bytes` (see `read_line_bounded` below): a complete
line over that cap is skipped with a `tracing::warn!` naming the file and
byte offset, and the offset advances past it so ingestion never wedges on one
oversized line. The pass cap is gated on at least one complete line (blank or
not) having been consumed, and the cursor is persisted whenever a pass
durably advances the offset even if it scanned zero parseable events — a long
run of blank or oversized lines can no longer read to EOF unbounded, nor lose
its cursor advance.

A line that crosses `max_line_bytes` with no terminating `\n` yet — a
still-growing file's in-progress final line, or a genuinely truncated /
corrupt tail — is its own bounded case, distinct from the complete
(terminated) oversized-line skip above: `read_line_bounded` reports
`LineRead::OversizedUnterminated` as soon as one bounded read window crosses
the cap without finding `\n`, instead of scanning onward to EOF looking for
one. The cursor is intentionally left at that line's start (like an ordinary
`Partial`), so the next poll — or the next daemon start — repeats the same
bounded read rather than an unbounded tail scan; once the line eventually
terminates (or the file stops growing and reaches true EOF mid-line), it
resolves to the normal `Oversized` skip-and-advance path or stays a bounded
`Partial`/`OversizedUnterminated` retry, never a full-file read in one call
(PACKSESSION-AUD-003).

## `MirrorLimits` / per-pass caps

Per-call caps on how much of a file's delta `mirror_file` will read and parse
before writing a bounded chunk. Production always uses
`MirrorLimits::production`; tests use a much smaller cap to force multi-pass
behavior without giant fixtures.

- `MIRROR_MAX_BYTES_PER_PASS` (8 MiB) — ceiling on bytes read per
  `mirror_file` call in production. Bounds worst-case memory use when a file
  has accumulated a very large delta (e.g. after daemon downtime or a
  multi-GB transcript).
- `MIRROR_MAX_EVENTS_PER_PASS` (1024) — ceiling on parsed events collected
  per `mirror_file` call in production.
- `MIRROR_MAX_LINE_BYTES` (= `MIRROR_MAX_BYTES_PER_PASS`) — hard ceiling on a
  single JSONL line's buffered size in production. Enforced by
  `read_line_bounded` itself (never appended to past this many bytes),
  independently of `max_bytes_per_pass` — a single oversized line must not be
  able to allocate past this bound even as the very first line of a pass
  (PACKSESSION-AUD-003).

## `LineRead` / `read_line_bounded` — the PACKSESSION-AUD-003 bound

`read_line_bounded` reads one line from `reader` into `buf`, never buffering
more than `max_line_bytes` regardless of how long the underlying line turns
out to be.

This is the hard bound behind PACKSESSION-AUD-003: `BufRead::read_until`
alone appends an entire line to its buffer before any cap check can run, so a
single arbitrarily large complete line (or a line that starts below a
per-pass threshold and ends far beyond it) can still allocate without limit
before the calling loop ever inspects it. Reading via `fill_buf`/`consume`
directly means a line longer than `max_line_bytes` is never appended to `buf`
past the cap — bytes beyond it are scanned for `\n` and dropped immediately,
bounding this function's own resident memory to `max_line_bytes` (plus one
`BufRead` internal buffer) no matter how long the real line is.

The same bound applies to the number of bytes *read* per call, not just
buffered: once a line has crossed `max_line_bytes` without a terminating
`\n`, the very next `fill_buf` window that still has no `\n` returns
`OversizedUnterminated` immediately rather than looping `fill_buf`/`consume`
onward in search of one. A line that is oversized but DOES terminate within
that same window still comes back as `Oversized` (the existing skip-and-advance
path) — only the no-terminator-in-this-window case is capped early. This
means one call to `read_line_bounded` never reads more than `max_line_bytes`
plus one `BufRead` internal buffer for a line with no discoverable `\n`,
whether that line is still growing (append-in-progress) or truly unterminated
at EOF — instead of scanning the remainder of the file (or forever, on a
still-growing file) in a single pass.

`LineRead` variants:
- `Eof` — EOF with nothing read at all.
- `Partial` — EOF reached before a terminating `\n`: an incomplete trailing
  line, left for the next pass. No bytes are considered consumed by the
  caller, regardless of how large the partial line has already grown.
- `Complete { bytes }` — a complete line fit within `max_line_bytes`.
- `Oversized { bytes }` — a complete line exceeded `max_line_bytes` before
  the newline was found; bytes past the cap were scanned for `\n` and
  discarded without buffering, so the caller must skip it, not parse `buf`.
- `OversizedUnterminated { bytes }` — the line has already exceeded
  `max_line_bytes` and no terminating `\n` has been found yet, but this is
  NOT end-of-file. Unlike `Oversized`, the caller must not advance past it.

## `read_bounded_chunk` — oversized-line handling

Reads at most one bounded chunk of a file starting at `start_offset`, one
complete line at a time via a buffered reader — never allocating more than
`limits.max_line_bytes` for any single line. A partial trailing line (no
terminating `\n`) is left for the next call.

A complete line whose buffered size would exceed `limits.max_line_bytes` is
rejected outright: it is never parsed, its bytes are counted and the offset
advances past it (so ingestion does not wedge on it forever), and a
`tracing::warn!` names the file and starting byte offset so an operator can
find and inspect it (PACKSESSION-AUD-003 — no silent coercion).

On `OversizedUnterminated`, the offset is intentionally NOT advanced past
`line_offset` — the next call re-reads from the same `line_offset` and is
bounded the same way, whether the file is still growing (a later pass will
eventually see the terminator and fall into the `Oversized` skip-and-advance
arm) or genuinely corrupt/truncated (every later poll or daemon restart
repeats this same bounded read, never an unbounded tail scan).

## ChatGPT export whole-file re-parse (`mirror_chatgpt_export_file`)

Unlike `mirror_file` (append-only line-tail), a ChatGPT export is a single
static JSON array with no stable "new bytes" boundary to tail, so
`mirror_chatgpt_export_file` always re-reads and re-parses the whole file.
`start_offset` is used only as a cheap re-poll guard: if the file has not
grown past it, nothing is read or parsed. `new_offset` is set to the whole
file's byte length only after a successful parse and commit — any IO, parse,
or DB error leaves the persisted cursor untouched, so a partially-downloaded
export is retried whole on the next tick, never half-consumed.

`DEFAULT_CHATGPT_MAX_BYTES` (256 MiB), overridable via
`KHIVE_MIRROR_CHATGPT_MAX_BYTES`: unlike the JSONL line-tail sources, this is
a ceiling on the *entire file*, not a per-pass delta. An export over this
size is skipped for that pass (loudly logged via `tracing::warn!`, never a
crash or an unbounded `read_to_string`), and the cursor is left untouched so
the oversized source keeps being retried — and re-warned — on every later
tick instead of silently dropping forever (PACKSESSION-AUD-003).
`chatgpt_max_bytes()` falls back to the default for missing, non-numeric, or
zero env values (a zero ceiling would skip every export unconditionally,
which is never useful, so it is treated the same as unset).

## Write path: `write_events_and_cursor` and friends (ADR-099 D5)

`write_events_and_cursor` is shared by `mirror_file`'s eventful line-tail
path and `mirror_chatgpt_export_file`'s whole-file path, so the
session/message row construction and cursor semantics (create-only sessions,
`INSERT OR IGNORE` message dedup, monotonic `last_seen_at`, cursor advances
only on success) live in exactly one place.

Its closure is verified suspension-free (it drives only `writer` with
inline-built `SqlStatement`s — session/message INSERTs, the count refresh,
and the cursor UPDATE — with no embedding, no ANN warming, and no other
`await` on an external service), so handing it to `atomic_unit` satisfies the
atomic-unit suspend-free invariant identically on the single-writer and
flag-off paths. This replaces the standalone `begin_tx` this function used
before ADR-099: the whole sequence still commits once or rolls back as one
unit, but no longer opens its own connection outside the writer task.

`write_events_and_cursor_on_writer` is the synchronous-DML body, run inside
one `atomic_unit` closure. It takes a plain `&mut dyn SqlWriter` (not `&mut
dyn SqlTransaction`) because `atomic_unit` owns the transaction boundary
entirely — this function must not, and does not, issue its own
`BEGIN`/`COMMIT`/`ROLLBACK`. Per-section notes:

- **sessions row (create-only)**: first sight of a session creates the row
  (`first_seen_at = last_seen_at` = this event's timestamp). Replays are a
  cheap no-op (`DO NOTHING`), so a pass that inserts no new messages writes
  no session metadata at all — strict replay idempotency. `last_seen_at` is
  advanced only when a genuinely new message lands.
- **session_messages insert**: idempotent via `INSERT OR IGNORE` keyed by the
  event UUID.
- **advance session metadata only when a new message landed**: keeps
  `last_seen_at` monotonic (`MAX`) so a timestamp-missing replay (whose
  `created_at` fell back to `now_us`) cannot move it forward, and backfills
  metadata that may have been NULL at create time. A pure replay
  (`affected == 0`) touches nothing.
- **refresh message_count for each distinct session**: in practice one file
  maps to one session_id, but every session_id touched is refreshed to stay
  correct even if that changes. Skipped entirely on a pure replay
  (`inserted == 0`) since the counts cannot have changed.
- **no explicit COMMIT**: `atomic_unit` owns the transaction boundary
  entirely and commits once the closure returns `Ok`, or rolls back the whole
  unit on `Err` — the same all-or-nothing contract the old
  `begin_tx`/`tx.commit()` shape gave, now provided by the seam instead of a
  manual transaction.

`upsert_cursor_on_writer` issues the one `session_mirror_cursor` upsert
statement inside the already-open `atomic_unit` transaction — no transaction
control of its own. `write_cursor_only` is the standalone path used when a
pass consumed bytes but produced no parseable events (blank/unparseable/
skipped-oversized lines): it still must persist the advanced offset so the
next poll does not re-read the same bytes, and a failure here must propagate
— silently swallowing it would let the cursor and the already-consumed bytes
drift apart.

## Test suite notes

### PACKSESSION-AUD-003 regression tests

`mirror_file` used to allocate and read the entire file delta in one shot via
`read_from_offset` (`Vec::with_capacity(file_len - offset)` + `read_to_end`),
which could OOM or stall the daemon on a very large accumulated delta. The
regression suite in `ingest.rs`'s `tests` module covers, with a tiny
test-only byte cap forcing multi-pass behavior instead of giant fixtures:

- **multi-pass bounded reads**: a multi-line file is consumed across multiple
  bounded passes — each committing its own chunk and cursor advance — never
  reading the whole file at once.
- **oversized complete line**: a single complete line larger than
  `max_line_bytes` must never be fully buffered and parsed — it is rejected
  outright, the offset advances past it so ingestion does not wedge, and
  surrounding valid lines in the same pass still land.
- **line just under cap followed by an oversized line**: the old bound only
  checked the pass cap before reading another line, so a line that starts
  under the cap but is followed by one that balloons far beyond it could
  still get fully buffered via `read_until` before any check ran. The
  per-line bound must catch this regardless of where in the pass it happens.
- **oversized-unterminated reads are capped per call**
  (`CountingReader` — counts every byte pulled through `Read::read` so the
  test can assert a hard ceiling independent of the backing buffer size): a
  huge final line with no trailing `\n` used to be scanned all the way to EOF
  in one `read_line_bounded` call even though the discarded bytes past the
  cap were never buffered. The READ itself must be bounded too, not just the
  buffered memory. A small, explicit `BufReader` capacity is used so the
  bound is provable independent of the platform default (8 KiB).
- **oversized-unterminated line leaves the cursor at line start and is
  bounded on retry**: a single huge line with NO trailing newline (a
  still-growing or corrupt final line) must not advance the cursor, and
  repeated calls from the same persisted offset must each be bounded, not
  replay an unbounded scan of the whole file every poll. Once the line
  eventually completes, it must be recognized as the ordinary
  complete-oversized-line skip and clear normally.
- **still-growing partial line under the cap is unaffected**: guards that an
  ordinary still-growing file whose latest line is under `max_line_bytes` and
  has no newline yet still behaves as a plain `Partial` — this must not
  regress from the new oversized-unterminated handling.
- **large run of blank lines is bounded and persists the cursor**: a run of
  blank lines used to bypass the pass cap (only nonblank `scanned` lines
  tripped it) and, when a chunk scanned zero events, the cursor was never
  persisted even though bytes had durably advanced. Both are fixed: the cap
  trips on blank lines too, and the cursor is written whenever the pass
  consumed any bytes.
- **ChatGPT export over `max_bytes` is skipped without reading**: an export
  over the configured ceiling must be skipped (not `read_to_string`'d, not
  parsed, not erroring) and the cursor must stay untouched so the oversized
  source is retried — and re-warned — on the next tick rather than silently
  dropped.

### Replay-idempotency invariant

A timestamp-missing event's `created_at` falls back to `now_us`, which
differs between calls. A pure replay (0 new messages) must NOT advance
`last_seen_at` or otherwise touch the session row — verified by mirroring a
no-timestamp line, then re-mirroring from offset 0 and asserting
`last_seen_at` is byte-identical even though `now_us` has advanced.

### ADR-099 D5 acceptance tests

- **suspension-free under single-writer**: exercises the real production
  closure (`write_events_and_cursor_on_writer` via `atomic_unit`) over a
  write-queue-enabled pool built directly (`write_queue_pool` — a bare,
  file-backed `SqlAccess` handle with no `KhiveRuntime` and no
  `KHIVE_WRITE_QUEUE` env var, mirroring khive-pack-brain's
  `fold_gate.rs`/`persist.rs` write-queue-routing tests: `PoolConfig` reads
  that env var at construction time and it is process-global, so mutating it
  would race every other test in the binary that calls `KhiveRuntime::new()`
  — a `PoolConfig` literal with `write_queue_enabled: true` sidesteps that
  entirely). Proves the actual shipped code never suspends: if it ever did,
  `block_on_sync` would return the "future suspended" error and the call
  would fail instead of returning `Ok`.
- **single-writer concurrency, mandatory**: with the write queue enabled,
  concurrent session-mirror ingest and normal write traffic through
  `SqlBridge::writer()` must not contend at `BEGIN IMMEDIATE` — the converted
  ingest path routes through the single writer task rather than opening its
  own standalone transaction (the `begin_tx` hole this ADR closes). Uses the
  same queue-depth + occupier-parked-on-oneshot technique as
  khive-pack-brain's
  `fold_gate_apply_routes_through_writer_task_when_flag_enabled` (a
  wall-clock/timing probe would be indistinguishable from the flag-off
  fallback, which also serializes via real SQLite file locking): while an
  occupier deterministically holds the writer task's one drain slot open, the
  ingest call must appear in the channel's queue depth rather than opening a
  second, competing standalone `BEGIN IMMEDIATE`.
- **revert-companion test**: the OLD shape — a closure that issues its own
  `BEGIN IMMEDIATE` through the writer it was handed, instead of relying on
  `atomic_unit`'s own transaction — must fail deterministically with a
  nested-transaction error. This proves the suspension-free /
  single-transaction-owner assertions above are non-vacuous: the
  pre-conversion shape (a caller managing its own `BEGIN`/`COMMIT` inside the
  seam) does NOT silently pass. Built over a write-queue-enabled pool so the
  closure is deterministically driven through `block_on_sync`'s
  `InlineWriter` on the real single-writer production path, not the flag-off
  manual-transaction fallback.

### `test_mid_transaction_db_error_leaves_no_partial_state_and_cursor_unadvanced`

SS6 invariants #4 ("an ingest error never advances the cursor") and #5 ("one
transaction per file pass") both rest on the same underlying contract
`write_events_and_cursor` depends on: an `atomic_unit` whose closure returns
`Err` before its final statement must leave no visible trace of ANY write it
made in that pass — including the cursor upsert, which runs last, right
before the closure returns `Ok`.

The real ingest loop can't be driven into a mid-loop DB error through crafted
event data: the `sessions` insert uses `ON CONFLICT(id) DO NOTHING` and the
`session_messages` insert uses `INSERT OR IGNORE`, both of which swallow
constraint violations by design (that's what makes re-ingest idempotent). So
this test drives the same `atomic_unit`/`writer.execute`/`Err`-return path
directly — the exact machinery `write_events_and_cursor`'s `?`-propagated
errors rely on (ADR-099 D5) — and forces a genuine, non-suppressed SQL error
(a `prepare()` failure on a nonexistent table) after a session write AND a
cursor advance have already succeeded within the same open unit.
