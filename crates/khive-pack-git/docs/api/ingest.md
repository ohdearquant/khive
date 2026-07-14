# Batch git-history ingester — design notes

Long-form rationale extracted from `crates/khive-pack-git/src/ingest.rs`
doc-comments. Everything documented here is a non-public (private or
`pub(crate)`) item — internal notes, not the published API reference (the
crate's public surface — `IngestInclude`, `IngestOptions`, `IngestReport`,
`run_ingest`, `resolve_project_id` — keeps its complete doc-comments in the
source file itself).

## Module overview

One-shot: walks local git history plus (optionally) `gh`-fetched issues and
pull requests, and writes `commit` / `issue` / `pull_request` notes through
the standard `create` verb (so `KindHook` validation and `annotates` edge
creation run exactly as they would for any other caller). Reuses ADR-087's
operational pattern (cursor table, secret masking on ingest, cursor
advances only on success) — NOT a daemon loop, NOT a webhook, NOT a poller:
one pass per invocation.

## `Budget`

Bounds the number of new-record creation attempts across a `run_ingest`
pass (ADR-088 Amendment 1 `max_items`). Only creation attempts (success or
failure) consume budget — cheap natural-key "already exists" skips do not,
since they are not the work the bound exists to limit.

## `NewRecordForRef`

A newly created note this pass, retained so the post-ingestion
reference-extraction sweep (`link_references`) can resolve cross-references
between records created in the *same* pass regardless of ingestion order
(PRs and issues are ingested before commits) without re-reading them from
storage.

## `run_ingest_with_commit_recovery`

Same one-shot ingest pass as `run_ingest`, but a classified
missing-promisor-object failure while loading the commit-history snapshot
(`GitLogError::is_missing_promisor_object`) is retried through `recover`
instead of aborting the whole pass (issue #765). Issues and PRs still run
exactly once regardless of whether recovery is later needed or invoked —
only commit-snapshot acquisition (`walk_commits` + `touched_files`) is
retried, inside this same invocation's `Budget`, `IngestReport`, PR/merge
maps, and reference candidates (`new_records`); a repair never resets or
replays any of them, and there is no second `run_ingest` pass hiding behind
this one.

`run_ingest` itself delegates to this with a recovery callback that never
repairs anything — the CLI and any local-path caller has no disposable
remote cache to repair (issue #765 self-heal is remote-URL mode only,
ADR-088 Amendment 1), so a classified commit-snapshot failure here surfaces
as an ordinary error, exactly as before this pass gained recovery support.
Also: issues + PRs are ingested first (via `gh`, when available), then
commits (via local `git log`) — PRs are ingested before commits so a
commit's `annotates` list can reference an already-created merging-PR note
(the generic `create` verb validates `annotates` targets exist before it
writes — see `khive-runtime::operations::create_note_inner`).

## `resolve_id`

Resolves a full UUID or an 8+ hex prefix to a full UUID, unfiltered by
namespace (matches the by-ID resolution contract used by `get`/`update`).

## `link_references`

Post-ingestion sweep (ADR-088 Amendment 1 ingest enrichment): extracts
GitHub reference-grammar mentions from every note created *this pass*
(commits, issues, PRs — order-independent, since all three are already in
`new_records` by the time this runs) and materializes `annotates` edges to
the referenced issue/PR note, carrying `ref_kind` ("closes" | "mentions")
as edge metadata. Fail-open throughout: a malformed or unresolvable
reference is skipped and counted, never aborts the pass.

## `find_commit_by_sha` / `find_by_number`

Natural-key idempotence lookups (dedupe-before-create). `find_by_number` is
scoped by kind + namespace + `project_id` since GitHub issue/PR numbers are
repository-scoped — a bare `kind`+`number` filter would incorrectly collide
two different repos' `#1`.

## `find_document_for_path`

Finds an existing `document` entity whose `properties.source_uri` or
`name` matches `path` (ADR-086 keying convention). Returns `None` when no
match — v0 never creates documents on the ingester's behalf (skip the
edge).

Exact and suffix-`LIKE` candidates are selected in a single query/snapshot
so a document inserted between an exact-match read and a fallback read can
never be missed by the exact branch, and a wildcard-broadened match can
never shadow an exact one: the `ORDER BY` ranks exact matches first, with
`id` as a deterministic tiebreaker (PR #816 fixed a TOCTOU gap here, and a
sibling bug where an unescaped path let `%`/`_` act as `LIKE` wildcards —
see `find_document_for_path_tests`).

## `write_cursor`

Called once per section (commits/prs/issues) after that section's loop
finishes, with a value that stops advancing at the first per-record create
failure (see the `cursor_stalled` handling in each `ingest_*` loop) — so
the next pass re-walks from before the failure and retries it, while
records that already landed (including ones ingested later in a stalled
pass) are no-ops via natural-key dedupe.

## Issue #765: commit-snapshot recovery

`GitLogPhase` distinguishes which `git log` pass a classified failure came
from: the two passes fail independently (`walk_commits`'s plain metadata
pass can succeed via cached commit data while `touched_files`'s
`--name-only` pass needs a tree that the promisor cache dropped, or vice
versa), so recovery needs to know which one to retry.

`GitLogError` carries its phase and raw stderr so `is_missing_promisor_object`
can classify it without losing the underlying diagnostic (surfaced verbatim
in the final error when recovery is unavailable or exhausted). The
classification is deliberately narrow (ASCII-case-insensitive `promisor`
plus either `not in the object database` or `missing object`) so ordinary
auth/network/`bad object`/spawn/local-source failures are never treated as
corrupt-cache and never trigger a destructive repair.

`walk_commits` shells out to `git log` with a stable, machine-parseable
format (v0 choice per ADR-088 §5 — `git2`/`gix` are not workspace
dependencies today, so shelling out avoids a new heavy dependency). Raw
control-byte separators are embedded directly in the format string (not
git's `%xHH` escape syntax) — passed as a single argv element (never
through a shell), so the literal bytes survive intact and git's
pretty-format engine emits any non-`%` character verbatim.

`touched_files` is a separate `--name-only` pass, kept apart from
`walk_commits`'s custom `--pretty=format` — interleaving file-name lines
with the metadata format has no clean, unambiguous delimiter.

`CommitSnapshot` bundles both passes so a classified failure in either one
can be retried as a single unit. `load_commit_snapshot` mirrors
`ingest_commits`'s original inline sequencing: `touched_files` (a second,
unscoped `git log --name-only` pass over the whole history) is skipped
entirely when `walk_commits` found no new commits, since there is nothing
new to annotate with touched paths.

`CacheRepairStrategy` records which repair `RemoteCommitRecovery`
(`handlers.rs`) performed, so `recover_commit_snapshot` can report exactly
one truthful success warning once the commit phase completes.
`RecoveredRepo` carries the repo path and strategy a `recover` callback
used to repair a classified `GitLogError` — `recover_commit_snapshot`
retries the snapshot load against that path (the same cache slot for both
strategies in `cache.rs`, but callers are not required to keep it
identical).

`recover_commit_snapshot` is bounded entirely by `recover`'s own return
value: `Ok(Some(_))` retries the snapshot load against the recovered repo
path, `Ok(None)` surfaces the original classified error (no more repair
available), and any other error (including an unclassified `GitLogError` or
a non-`GitLogError` failure) is returned immediately without ever calling
`recover`. A later repair attempt's strategy replaces the pending warning
rather than accumulating one per attempt, so exactly one success warning is
ever returned — describing the *last* repair that was needed, not every
one tried.

## Masking boundaries (`MaskedCommitFields`, `MaskedIssueFields`, `MaskedPrFields`)

Every raw git/`gh` record funnels through one constructor before it can
reach `properties`/`content`/the note `name`/paging cursors. Each `new`
destructures its source struct exhaustively (no `..`), so a future new
field forces a compile error here before it can silently skip the masking
boundary — the same one-boundary-per-record-type discipline across
`MaskedCommitFields`, `MaskedIssueFields` (issue #841, following #835's
pattern), and `MaskedPrFields` (issue #841: PR author, base ref, and head
ref were the residual raw fields after #835's title fix).

For `MaskedCommitFields`: `sha`/`short_sha`/`committed_at`/`parents` are
git-computed hashes and an RFC3339 timestamp — not attacker-authored free
text — so they pass through unchanged. `author`, `author_email`,
`subject`, and `body` are git-config- and commit-message-controlled prose
and go through the same `mask_secrets` gate `content` already used —
closing the gap where the commit note `name` (built from the raw subject)
and its `author`/`author_email` properties skipped masking entirely.

For `MaskedPrFields`: `number`, the four timestamp fields, and
`merge_commit`'s `oid` are GitHub-generated (not attacker-authored free
text) and pass through unchanged. `title`, `body`, the author login, and
both ref names are contributor-controlled prose and go through the same
`mask_secrets` gate.

`StateReasonField` is the classified outcome of parsing a raw `stateReason`
string against the governed enum (`hook::ISSUE_STATE_REASONS`, ADR-088 §3)
at the masking boundary. `Rejected` never carries the raw string forward —
the ingest loop must reject the record with a warning that names only the
field, never its value (a credential-shaped `stateReason` must never reach
`report.warnings` or the hook's own error path).

`canonical_issue_state_reason` normalizes case before the membership check
(GitHub reports `stateReason` as `""` for open issues and an UPPERCASE enum
value, e.g. `NOT_PLANNED`, for closed ones). A value that is present,
non-empty, and not one of the four governed values is classified
`Rejected`.

`canonical_issue_timestamp` parses a GitHub issue timestamp into canonical
RFC3339 form. GitHub's API always returns valid RFC3339 timestamps; a value
that fails to parse is untrusted/malformed input (a credential-shaped
string is exactly this case) and must never reach `properties` or the
paging cursor as a raw string. On parse failure the field is rejected
(becomes absent) and a warning is recorded — without the raw value, which
may itself be secret-shaped. The issue itself is still ingested; only this
one field is dropped.

## Paging (`PageOutcome`, `decide_page_outcome`, `PAGE_LIMIT`)

`PAGE_LIMIT` (1000) is the per-page fetch cap for both PR and issue paging.
`gh {pr,issue} list --search` is backed by GitHub's search API, which never
returns more than this many results for a single query regardless of
`--limit` — paging works around that ceiling by advancing an `updated:>=`
floor between calls, not by requesting more than one page can hold.

`PageOutcome` is pure and unit-testable independent of `gh`, the database,
or async machinery — the entire "was the remote window proven exhausted"
decision lives here (ADR-088 Amendment 1): a single hard-coded `--limit
1000` fetch could previously report `done: true` while a repo's remaining
PRs/issues past position 1000 were never seen.

- `WindowComplete`: the page held fewer than `PAGE_LIMIT` items — the
  remote window is proven exhausted regardless of local budget state.
- `StopBudgetExhausted`: the page was full and the local budget is
  exhausted — stop paging, but the window is NOT proven exhausted.
- `StopFloorStalled`: the page was full and the last item's `updated_at`
  did not advance past the current floor (more than `PAGE_LIMIT` records
  share one timestamp — an unresolvable pathological case) — stop paging
  rather than loop forever. The window is NOT proven exhausted.
- `Continue(floor)`: the page was full, the budget is not exhausted, and
  the floor advanced — fetch the next page starting at this floor.

Only `WindowComplete` lets `done` stay `true` on the local-budget question
alone; every other outcome means more remote records may exist past the
last fetched page.

## `ingest_prs` / `ingest_issues` cursor semantics

`cursor_stalled` mirrors `ingest_commits`: once one record fails to create,
later records in this pass are still attempted (so every failure surfaces
in this pass's warnings), but `max_updated` no longer advances past the
stall point — the next pass re-fetches from before the failure and retries
it, while already-landed records are no-ops via the natural key.

Each page is already `sort:updated-asc` server-side, but `--search` makes
no hard ordering guarantee across ties — both loops re-sort defensively so
the frozen-cursor invariant (records walked in nondecreasing `updated_at`
order) holds regardless. `is_new` is inclusive (`updated >= cursor`) for
exactly the tie reason: a successful and a failing record sharing one
`updated_at` must both be re-examined next pass until the cursor moves past
that tie.

In `ingest_issues`, the entire fetched page is classified (masked strings,
canonicalized timestamps, governed-enum `state_reason`) before anything
else — including the sort and the paging cursor derivation — touches it. A
raw `GhIssue.updated_at` must never reach the sort comparator,
`last_updated_at`, or (via `decide_page_outcome`'s `Continue`) a future `gh
--search updated:>=` argument (a credential-shaped `updatedAt` could
otherwise sort last and leak into process arguments through the paging
floor). An ungoverned `stateReason` is rejected before the record is ever
built or dispatched — the warning names only the field, never the raw
(possibly credential-shaped) value, matching ADR-088's
fail-closed/no-silent-coercion contract.

Reported `done = false` whenever the remote window is not proven complete
(`StopBudgetExhausted` / `StopFloorStalled`) — the local budget alone is
not a complete signal.

## Test module notes

- `recovery_classifier_tests`: pure/synchronous `GitLogError`
  classification + `recover_commit_snapshot` retry-loop tests. Lives here
  (not in the sibling `recovery_tests` module) because these fields are
  private to this module; `recovery_tests` drives the DB-backed acceptance
  scenarios through the `pub(crate)` surface instead. Tests spawning real
  `git` via bare `Command::new("git")` must hold the crate-wide
  `cache::ENV_MUTEX` the `PATH`-shimming tests in `cache.rs` and
  `recovery_tests.rs` hold while they swap `PATH` to a fake `git`, or a
  test can spawn that shim instead of real git and misclassify a healthy
  repo as corrupt.
- `truncation_tests`: `truncated_embedding_head` boundary tests, including
  a multibyte scalar (3-byte `€`) straddling the byte cap, which must roll
  the boundary back to the nearest valid char boundary rather than
  panicking or splitting the scalar.
- `compact_prefix_resolver_tests` (PR #816): `resolve_id`/`resolve_project_id`
  call the public `resolve_prefix_unfiltered` resolver without their own
  all-hex gate, so a `%`-bearing (or otherwise non-hex) `project` argument
  reached the bound `LIKE` pattern unfiltered. The runtime resolver
  boundary (`resolve_prefix_inner`) now rejects non-hex/non-hyphen input
  itself, so these callers inherit the fix without needing their own gate.
- `find_document_for_path_tests` (PR #816): `find_document_for_path` bound
  an unescaped path into a `LIKE` pattern (`%`/`_` in a filename became
  pattern wildcards) and picked whichever candidate the unordered `LIMIT 1`
  scan happened to return first, even when an exact match existed.
  Resolution now happens in one query/snapshot: both candidates are
  visible to the same `SELECT`, and the `ORDER BY` — not read ordering —
  decides the exact match wins.
