# ADR-117b: Session Mirror Deletion and Storage-Cap Retention

**Status**: proposed — pending maintainer hash sign-off before retention code
**Date**: 2026-09-29
**Authors**: khive maintainers
**Implements**: [ADR-117](ADR-117-session-continuity-search.md) D5
**Depends on**: [ADR-080](ADR-080-session-pack-oss-storage-mechanism.md) §6,
[ADR-093](ADR-093-sessions-raw-zstd-compression.md) §4,
[ADR-117a](ADR-117a-session-identity-tenant-isolation.md)

## Context and boundary

The mirror stores provider transcripts in `sessions` and `session_messages`, indexes
`session_messages.text` in an external-content FTS5 table, and tracks source-file progress
in `session_mirror_cursor`. It reads provider files without deleting or modifying them
(ADR-080 §6). The cursor is keyed by file path and stores only one last session id; a
whole-file export can contain many sessions. A cursor reset or a newly discovered copy
of a file can therefore re-stream a deleted session. Source-qualified account identity is
`(namespace, source, provider_session_id)` (ADR-117a Amendment 1).

This ADR governs the **passively mirrored transcript corpus and every derivative of it**.
The caller-authored `kind=session` notes used by `session.store`, `list`, `resume`, and
`export` are a separate store under [ADR-083](ADR-083-session-pack-t1-verbs.md); a mirror
deletion does not purport to delete those notes or the provider's source files. The user
surface must say this plainly. An account wipe here means all of that account's **mirrored
session history**, not every khive record or every upstream transcript. A broader account
erasure workflow must compose the independent note and provider deletion mechanisms.

The current `TopLevelMaintenance` storage capability exposes full `VACUUM` and WAL
checkpoint truncate, not incremental vacuum. Existing files may have
`auto_vacuum=NONE`. This document specifies the retention and deletion mechanism; it
does not claim it has shipped.

## Decision

### D1 — Scope and verbs

The three Directive verbs below operate only in the resolved account scope of the
request's `NamespaceToken`. No caller-supplied `namespace` or account identifier can
widen their scope. Missing scope refuses before any row lookup; shared-hosted use still
requires ADR-096's authenticated connection identity. The authorization gate sees the
verb and unresolved arguments as usual; the handler enforces the scoped row predicate.
The current mirror writer stamps `local`; implementing this contract for any other
account requires an authenticated source-to-account binding at ingest, not a deletion
handler that assumes the writer's hard-coded stamp is tenant-aware.

- `session.delete(source, provider_session_id)` takes exact strings, with `source`
  from ADR-080's closed set and no prefix resolution. It idempotently tombstones
  the scoped identity and removes that mirror session and its derivatives. A
  missing local row still creates the tombstone, so a later import cannot create
  it. The result has an operation id, deleted counts, and `complete` or
  `pending_remote`.
- `session.wipe_account()` takes no account argument. It explicitly pauses this
  account's mirror and remote uploader, tombstones every known scoped session,
  deletes all of its mirrored corpus and derivatives, and establishes a durable
  account wipe fence. It returns counts, operation id, and completion state. A
  repeat while the fence is active returns the same state without reopening
  ingestion.
- `session.age(dry_run?, expected_revision?)` defaults to `dry_run=true`; applying
  requires `dry_run=false`. It reports the current charge, cap, ordered candidate
  session identities, and estimated released charge. Apply rechecks
  `expected_revision` if supplied, selects whole sessions under the writer lock
  until charge is at or below cap, and invokes the same deletion path with reason
  `retention`. It returns actual identities/counts and `complete` or
  `pending_remote`; it runs only by explicit call or the operator-enabled
  schedule-kind action in D3.

`session.deletion_status(operation_id)` reports local and remote completion without
returning deleted content; it is required once remote ingestion exists. A remote-pending
operation is never represented as complete. The verbs are separate from the existing
T1 note verbs and must describe their mirror scope in catalog text and responses.
Migration-only `source=unknown` orphan messages from ADR-117a are included in account
wipe and charge accounting. Aging treats each `(namespace, unknown, session_id)` group
as one synthetic whole session ordered by its latest `created_at`; the user-facing
single-session verb accepts only ingestible sources and cannot guess an orphan's origin.

### D2 — One deletion path across all derived surfaces

Within a writer transaction, the deletion path first installs the durable scoped
tombstone/fence and remote-deletion outbox record, then removes matching
`session_messages` and `sessions` rows. The existing `session_messages_fts_ad` trigger
must remove each deleted rowid from `session_messages_fts`; implementation tests must
prove deleted text is no longer matchable, including a query made after a restart.
Future vector rows keyed by the same scoped event identity must be deleted in this
transaction before a vector signal becomes searchable. Missing vector integration
blocks that signal; it cannot silently leave embeddings behind. A deletion transaction
either commits all local row/index/tombstone/outbox changes or none.

The cursor domain gains a **separate durable tombstone relation** keyed by
`(namespace, source, provider_session_id)`. A file-path cursor's single `session_id`
column is not a tombstone and cannot represent a multi-session export. Every ingest
transaction checks the scoped tombstone before creating a session or message; a
tombstoned event is skipped while the file cursor may advance past that event. This
check and cursor advance share the writer transaction. A path rename, cursor reset,
restarted poller, or copy of a whole-file export cannot resurrect the scoped id. Cursor
schema and service state must also carry or partition account scope so an account wipe
cannot delete another account's progress. A missing cursor is never treated as proof
that an id is safe to re-import. The tombstone survives cursor-row cleanup.

Search caches and hydrated results are tagged with the account's deletion revision.
The delete transaction advances that revision; caches are invalidated, and search
rechecks it before publishing a hydrated result. A result from an earlier revision
cannot be served after deletion completes. Already delivered results and user-created
exports cannot be retracted; the response states this boundary.

For ADR-117d remote ingestion, the account-scoped sink applies the same tombstone and
row/index/vector deletion transaction, and rejects later or reordered uploads for that
id. The local durable outbox orders deletion ahead of later sends, cancels queued
events for the id (or all account events for wipe), retries until every configured sink
acknowledges, and records acknowledgements. An account wipe advances a sink-side
account generation; every older in-flight or retried upload is rejected regardless of
whether that session id was known locally. A remote receipt covers index/vector and
cache invalidation as well as base rows. Remote failure leaves `pending_remote`
visible and retryable; local search no longer returns the data, and a remote sink
still above its cap cannot accept fresh uploads while aging is pending. Remote
protocol and receipt details are an ADR-117d dependency, not an assumed existing
capability.

An account wipe fence keeps mirror and uploader disabled for that account after the
local transaction. Re-enrollment is a separate explicit administrative operation that
must establish fresh source positions and retain tombstones for all deleted scoped ids;
simply clearing cursors or toggling the existing enable flag cannot re-enable it. An
old source file can remain on disk, but cannot silently repopulate wiped mirror rows.

Tombstones contain only scope, source, provider id, deletion reason, operation id, and
timestamps/revision — never transcript text or raw bytes. They are retained while any
source or remote retry can replay the id. A separate, explicit source-retirement proof
may eventually remove them; age alone does not. If tombstone metadata itself exhausts
the configured budget, ingestion and aging report capacity unavailable rather than
discarding anti-resurrection state. A full account wipe retains the minimal fence and
remote receipt state necessary to prevent replay; it is not a claim that no account
identifier exists anywhere.

### D3 — Storage cap and oldest-first aging

The proposed default cap is **4 GiB (4,294,967,296 bytes) per resolved account**.
`KHIVE_SESSION_STORAGE_CAP_BYTES` configures it at daemon startup as a positive decimal
byte count; zero, overflow, or invalid input fails configuration rather than disabling
retention. The effective value is reported by the session storage-stats surface and by
`session.age`. There is no time window, age deadline, default automatic age sweep,
or implicit delete on ingest. History under the cap remains indefinitely.

An optional schedule-kind action invokes `session.age(dry_run=false)` for its fixed
resolved account scope on an operator-configured cadence. It is OFF by default:
only an explicit operator enable creates the action, and install, upgrade, and
migration never activate it. Its action and scope are visible at creation and in
schedule status. Every run surfaces either the complete `session.age` apply
result, including every removed scoped identity and any `pending_remote` state,
or an explicit failure; a summary alone cannot omit identities.
Without this opt-in, unattended operation above the cap remains stalled with
`age_required` until an explicit `session.age` apply.

The cap measures **account-attributable stored mirror content**, rather than the
database file's shared physical page count. Charge version 1 sums the actual stored
byte lengths of every non-null variable-width column in that account's `sessions`,
`session_messages`, cursor/tombstone/outbox, and future vector rows (including legacy
TEXT and compressed BLOB `raw` as stored), adds 128 bytes per such row for fixed/index
overhead, and adds one more stored `text` byte length plus 128 bytes per indexed
message as an FTS proxy. Byte lengths, not Unicode scalar counts, are used. The
charge version and components are reported with the stats result and computed by the
storage writer, not from client estimates. The account's searchable sink is the cap
authority: the local DB in single-machine mode and the account sink under ADR-117d;
per-producer caps cannot replace one aggregate account budget. FTS5 and SQLite page
fragmentation cannot be assigned exactly to an account in a shared DB, so the cap is
**not a hard bound on `sessions.db` + WAL bytes**. File, WAL, freelist, and per-table
bytes are reported separately; physical reclaim is D4 below. The retention
implementation round reports database-wide main-file, WAL, and freelist bytes
alongside aggregate logical charge and a labelled `(main-file + WAL) / logical
charge` ratio (`null` when aggregate charge is zero); the ratio is not a
requirement on frozen Cluster J R1b. This logical interpretation of D5's
storage cap requires explicit sign-off before implementation.

The oldest eligible whole session is the smallest `last_seen_at`, then
`first_seen_at`, `source`, and `provider_session_id` as deterministic ties. A session
currently receiving a bounded mirror pass is not partly deleted: aging serializes
with the writer, then removes that entire session or retries selection against the
new revision. To apply, `session.age` repeatedly deletes the oldest whole session
until the measured account charge is at or below the cap; it never trims messages
inside a retained session. The dry run is advisory. If an account is over cap because
the configured cap was lowered or a new ingest would cross it, ingestion refuses the
new write without advancing its cursor and reports `age_required` with current charge
and cap. Reads, deletion, aging, and maintenance remain available. If one session or
tombstone floor alone exceeds the cap, aging returns `capacity_unavailable` with an
explanation, never a false success. Retention deletions use D2's identical local,
remote, cursor, cache, and tombstone path.

At daemon startup, an upgraded store already over its cap enters the same
`age_required` admission state, without deleting sessions or introducing a
separate report-only mode. Session storage stats, session status, and a warning
startup log each surface `age_required`, the measured charge, cap, and
would-delete whole-session count from the consistent oldest-first dry run, and
the exact `session.age(dry_run=true)` command. Ingest remains stalled without
cursor advance until explicit apply or the enabled schedule-kind action; reads,
deletion, aging, and maintenance remain available.

### D4 — Physical reclaim is an explicit fork

Deleting rows frees SQLite pages for reuse; it does not necessarily shrink the DB
file. ADR-093 §4 already calls for explicit `VACUUM` to reclaim space. The current
`TopLevelMaintenance::Vacuum` path is the **baseline**: `session.vacuum` can run after
deletion or aging, outside the request transaction under the serialized writer, and
reports file/WAL bytes before and after. It may block serving and need substantial
temporary disk space, so aging does not secretly invoke it. An operator can see
`reclaim_pending` and run it deliberately. Deletion completion means the data is gone
from live local/remote query surfaces; it does not claim that backups, source files,
already exported results, WAL frames, or forensic copies have been erased.

The alternative is bounded `PRAGMA incremental_vacuum(N)`, but it is **not** a switch
the session pack can turn on for an existing DB. SQLite requires
`auto_vacuum=INCREMENTAL` before tables are created, or a conversion using full
`VACUUM`; `incremental_vacuum` on a `NONE` database is a no-op. Choosing this branch
requires a new `khive-db` versioned initialization/migration contract, capability
extension beyond today's closed `TopLevelMaintenance` set, an existing-file conversion
plan, and tests for `PRAGMA auto_vacuum` on fresh and upgraded DBs. It must budget
writer time and WAL/checkpoint effects and cannot claim to compact partly filled
pages. Until that separate contract is approved and landed, full `VACUUM` is the only
specified physical-shrink operation. The two branches and their costs follow
[SQLite's auto-vacuum documentation](https://www.sqlite.org/pragma.html#pragma_auto_vacuum)
and [VACUUM documentation](https://www.sqlite.org/lang_vacuum.html).

FTS5 deletion must be verified independently of file shrinkage. SQLite's default
FTS5 delete keys can leave old index material in segments; its
[secure-delete option](https://www.sqlite.org/fts5.html#the_secure_delete_configuration_option)
has a file-format compatibility cost. A later physical-erasure claim needs an explicit
FTS5/core secure-delete and WAL/backup policy, with compatible SQLite versions; this
ADR does not silently equate an unmatchable index entry with forensic erasure.

## Sign-off decisions

Maintainers have conditionally accepted three explicit choices, subject to D3's physical
bytes and ratio disclosure and signature of this R2 hash before implementation: a
4 GiB **logical account charge** rather than a physical SQLite-file quota; the
mirror-only meaning of the two deletion verbs, with T1 notes and upstream files
outside their scope; and full `VACUUM` as the available reclaim baseline while an
incremental branch waits for a versioned DB initialization/conversion contract.
Changing any choice requires this ADR to be revised before retention source lands.

## Acceptance and rollout gates

1. Same provider id under another source or account remains intact after a scoped
   delete. Missing scope refuses; a caller-provided account string cannot widen the
   operation. A missing local id still receives a durable scoped tombstone.
2. Delete one session and wipe an account, then prove matching session/message rows,
   FTS hits, and (when present) vectors are gone. Other accounts' rows and file
   cursors remain. A cached/hydrated hit from the old revision is not returned.
3. Reset a cursor, rename/copy a line-tail file, and reparse a whole-file export
   containing multiple sessions. Deleted ids stay absent while undeleted sessions
   remain ingestible; a crash between tombstone and cursor publication cannot replay.
4. A remote retry queued before deletion cannot restore content. Simulate a failed
   sink: the verb reports `pending_remote` until an idempotent sink receipt arrives;
   a delayed upload after the receipt is refused. Wipe covers all remote copies.
5. With a small cap, dry-run order is deterministic and apply removes the oldest
   whole sessions through the same path until under cap. Ingest above cap does not
   advance the cursor, and no background pass deletes anything unless the explicit
   operator-enabled schedule-kind action runs `session.age` and reports every
   removed identity. Invalid cap config
   fails closed. Tombstone-floor exhaustion is reported, not concealed.
6. Existing `auto_vacuum=NONE` is never passed to incremental reclaim as if it
   worked. Full `VACUUM` is explicitly requested and reports physical before/after;
   an incremental branch requires its own fresh/upgrade schema proof first.
7. The schedule-kind action is absent after fresh install and upgrade, appears
   only after explicit operator enable, exposes its action and account scope,
   and reports every removed identity and remote-pending state per run.
   With the default OFF, unattended over-cap ingestion still returns
   `age_required` and leaves its cursor unchanged.
8. Start an upgraded store above cap: stats, session status, and startup warning
   each show `age_required`, charge, cap, would-delete count, and the dry-run
   command; none applies aging. Retention-round stats report main-file, WAL,
   freelist, aggregate logical charge, and the labelled physical-to-logical
   ratio together, without assigning shared physical bytes to one account.

The deletion path must be live and proven before `session.search` is advertised as
available under ADR-117's capability-consumption rule. Remote cases become mandatory
when ADR-117d enables a remote sink; vector cases become mandatory when vectors ship.
This proposed ADR authorizes no retention implementation until maintainers
sign this revision's hash.
