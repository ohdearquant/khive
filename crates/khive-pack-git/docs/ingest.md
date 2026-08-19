# Git ingester — walker exit states and commit-walk stall semantics

Companion to `crates/khive-pack-git/docs/api/ingest.md` (the crate's existing
design-notes doc for `ingest.rs`, covering budget accounting, secret-gate
refusal accounting, the per-source tri-state, masking boundaries, paging,
and changed-paths/code-module annotation). This file holds narrative that
doc did not yet cover: how a walker's exit state is classified as
walked-then-failed versus never-walked, the instrumentation-gap tripwire
that guards that classification, and the commit walk's ancestor-divergence
check and cursor-stall guarantee.

## Seed invariant for `pin_stopped_early`

`pin_stopped_early` overwrites a walker's source slot with
`StoppedEarly(reason)` from an end-of-walk arm. Every end-of-walk arm
(`!window_complete` / `cursor_stalled`) is reachable only after at least one
successful page fetch, and each walk seeds its slot on its first successful
fetch — so the slot is `Some` on every path today. The function's `None`
arm is a release-mode soft fallback: rather than panicking, it constructs
the state directly, with a `debug_assert!` tripwire in debug builds for the
instrumentation gap that would have to exist for the slot to still be
`None` at that point.

## Walker exit states

`run_ingest` distinguishes a **never-walked** source (its first page/first
fetch failed) from a **walked-then-failed** source (a continuation-page
fetch, a per-record existence lookup, or the final cursor write failed
after the walk was already underway) by reading the source's own state
slot: `None` means the walker never got as far as recording anything, so
the source was never walked; `Some` means the walker had already recorded a
state — usually `Completed` or `StoppedEarly` — before this failure
occurred, so the walk did happen.

For pull requests and issues this is checked directly (`report.sources.pull_requests.is_none()` /
`report.sources.issues.is_none()`). For commits, `ingest_commits` records
its source slot at every walker exit, so a `Some` slot beside an `Err` means
the walk ran (possibly to completion) and the pass failed *after* it — the
final cursor write is the canonical case for that. A `None` slot for
commits is a pre-walk failure (snapshot recovery, cursor read) and stays a
hard error, since the recovery contract depends on those surfacing.

When the source did walk before failing:

- A completed walk whose pass then fails is downgraded from `Completed` to
  `StoppedEarly`, so `completed` never outlives a pass whose durability
  turned out to be unproven (e.g. the final cursor write never landed).
- The resume loop must not treat the source as finished: `report.done` is
  forced to `false`.

When the source never walked, the failure pins nothing — a first-fetch
failure like "no git remotes found" is deterministic, and `done` keeps its
ordinary budget-cursor meaning for that case.

### Instrumentation-gap tripwire

After all three sources have been walked, `run_ingest` folds the
walk-recorded completion flags (`commits_complete` / `prs_complete` /
`issues_complete`) into the per-source tri-state. Every walker is expected
to record its own state by the time it returns: the PR/issue walkers
pre-seed `StoppedEarly` when a walk begins and upgrade it on completion,
and `ingest_commits` writes its state at every exit. The three fallback
arms that fire when a source's slot is still `None` here are therefore a
release-mode belt-and-braces fallback for a walker that returned without
recording anything — an instrumentation gap, not a normal stop — and the
reason string they synthesize says so loudly instead of dressing the gap up
as a real stop reason.

**Fabrication risk**, named here for the next maintainer touching a walker:
in release builds the `debug_assert!` guarding these arms is a no-op, so a
future walker that sets its completion flag *without* also recording a
source state would silently produce a fabricated `Completed` /
`StoppedEarly` here with no real reason behind it. The invariant every
walker must uphold is that it records its own exit state; these fallback
arms exist only so a gap degrades to a loud, clearly-labeled synthetic
reason instead of leaving the slot silently empty.

## Commit walk: ancestor-divergence and cursor-stall

### Ancestor-divergence check

An empty `{cursor}..HEAD` commit range is a genuine completion only when
the cursor is an ancestor of the tip being walked. A cursor that is *not*
an ancestor means this source's history lags or diverged from whatever
advanced the cursor — measured directly: a scratch clone whose `HEAD`
trailed the cursor by weeks walked nothing and reported a clean pass
(issue #1644). `ingest_commits` checks this explicitly
(`is_ancestor_of_head`) and, when the cursor is not an ancestor, records a
`StoppedEarly` state with the divergence reason and refuses to claim
completion — rather than the walk silently reporting a clean pass over
history it never actually covered.

This state is recorded at the point of detection rather than left for the
end-of-pass instrumentation-gap fill described above: the diverged-cursor
case is a stop-early (the walk cannot claim it covered this source's
history), while the ancestor case is a genuine completion.

### Cursor-stall guarantee

Before the per-commit loop starts, the source slot is seeded with
`StoppedEarly("walk began but did not report completion")` so that a
mid-walk database error is reported in-band as walked-then-failed rather
than as a pre-walk hard error (cursor and snapshot failures earlier in the
function remain pre-walk errors).

The commit snapshot (`walk_commits`) is oldest-first and always includes
`HEAD` whenever this phase has work, so the last record is the exact
repository snapshot the ADR-085 module index binds against. The walk
itself is never truncated — `walk_commits` issues one unbounded
`git log {since}..HEAD`, and `max_items` bounds only the create loop that
follows, never the snapshot — so `snapshot_head` is always the true
repository `HEAD` of the pass regardless of how many commits the budget
lets it create.

`cursor_stalled` freezes `last_sha` at the last contiguous successfully
processed commit: once a record fails to create, later records in the same
pass are still attempted (so a run surfaces every failure it can, not just
the first), but the persisted cursor no longer advances past the failure.
That guarantees a failed record is retried — and its warning re-surfaced —
on every subsequent pass until it is fixed upstream, rather than being
silently skipped forever because the cursor moved past it. Records that do
succeed after a stall are still written; they are idempotent via the `sha`
natural key, so a retried pass never double-creates them.

`local_sha_to_id` maps parent SHA to note id for commits created earlier in
the same pass. Combined with `find_commit_by_sha`'s database lookup, it
resolves `precedes` parent edges regardless of which pass the parent
landed in. The stall guard applies to every `last_sha` advance in the loop:
once a commit create fails, advancing the cursor past a later
refused/failed record — including past a later *existing* record, whose
natural-key lookup proves only its own landing, not the failed record's —
would strand the failed commit behind the floor and skip it forever instead
of retrying it on the next pass.
