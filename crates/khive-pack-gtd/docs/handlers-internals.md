# Handler internals — rationale for `src/handlers.rs`

Background for maintainers; not part of the published API contract (each linked
item's doc-comment carries the complete caller-facing contract already).

## `ensure_audit_schema` — why per-call, not `OnceLock`

The DDL (`CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit` + index) is applied on
every call rather than gated behind a global `OnceLock`, because each
`KhiveRuntime::memory()` in tests creates a fresh in-memory database that needs
its own schema bootstrap. In production the DDL is idempotent and cheap (SQLite
skips `IF NOT EXISTS` tables instantly).

### Why `pub`

Unlike every other helper in this file, `ensure_audit_schema` and
`write_audit_record` are `pub` (not module-private): the ADR-099 `--atomic` CLI
surface's `gtd.transition`/`gtd.complete` prepare functions live in `kkernel` (a
crate that already depends on both `khive-runtime` and `khive-pack-gtd` — see
that crate's `atomic_apply` module doc for the crate-direction rationale), and
the B3 GAP-5 fix applies this exact function as a deferred post-commit effect so
atomic transitions/completes write the same best-effort lifecycle audit row the
canonical handlers do, rather than re-deriving the DDL/INSERT a second time.

## `CompleteParams` / `TransitionParams` — why `pub` with private fields

ADR-099 B3: these structs are `pub` (not module-private) specifically so
`kkernel`'s `--atomic` validation seam (`atomic_apply::validate_atomic_args`) can
deserialize an op's args through the same canonical struct `handle_complete`/
`handle_transition` use, reproducing `deny_unknown_fields` rejection with zero
duplicated field lists. Fields stay private — the atomic seam only needs the
`Result<_, _>` outcome, never field access.

## `fetch_all_matching_tasks` — bounded single-snapshot scan (issue #772, #825)

Fetches every `task` note matching `property_filters` in a single bounded
snapshot query, instead of pre-fetching a fixed-size unfiltered window. The
predicate is pushed into SQL via `query_notes_filtered_bounded`, so the
candidate set returned is bounded by how many tasks actually match — not by how
many task notes of any status exist (issue #772: a fixed unfiltered window
could be entirely filled by newer non-matching churn, hiding older matching
tasks regardless of priority).

`query_notes_filtered_bounded` fetches at most `TASK_SCAN_MAX_ROWS + 1` rows in
one SQL statement with deterministic ordering — one consistent snapshot, not a
`COUNT(*)` followed by independent paged reads that a concurrent insert could
split across (issue #825: the prior page-loop version re-queried the store per
page with no transaction spanning them, so a row inserted between pages could
appear duplicated across a page boundary, or the scan could hit its cap and
still return `Ok` with an incomplete set). If `TASK_SCAN_MAX_ROWS + 1` rows come
back, this returns `Err(InvalidInput)` instead of ever returning a possibly
truncated result — callers must narrow the filters (e.g. add `assignee`) so the
result stays complete.

Source: `crates/khive-pack-gtd/src/handlers.rs`.
