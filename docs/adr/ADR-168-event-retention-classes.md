# ADR-168: Event Retention Classes and Sealed Archival

- **Status:** Proposed
- **Date:** 2026-08-20
- **Extends:** ADR-162 (adds a per-class contract dimension in the sense of its §4), ADR-022
  (two additive surface behaviours, §7 below)
- **Depends on:** ADR-004, ADR-005, ADR-007, ADR-018, ADR-022, ADR-041, ADR-046, ADR-094,
  ADR-103, ADR-106, ADR-108, ADR-161, ADR-162, ADR-163, ADR-164

## Context

The event plane is append-only by construction, and nothing bounds its lifetime. The
`EventStore` capability (ADR-005) exposes append, get, query, and count; it has no delete or
update surface, and the backing tables (`events`, `event_observations`) have no non-test
deletion path anywhere in the workspace. That is the correct write posture for an
authoritative record of what happened (ADR-004, ADR-162 §1) — and it means growth is
unbounded by design, with no accepted decision stating a lifetime for any event class.

The pressure is no longer hypothetical. One long-running deployment has accumulated over
4.2 million event rows, and the plane's own exhaustive counting surface refuses windows
matching more than 2 million events, so consumers of wide windows already operate against
population limits rather than against the full record. A store whose fact plane grows
without bound eventually taxes every query, checkpoint, and backup that shares the file.

Retention cannot be a single age cutoff, because the plane is not only an audit trail.
Accepted decisions make it load-bearing in four distinct ways:

1. **Event-sourced state.** Proposal lifecycles are reconstructed from events (ADR-046),
   and a scheduled row's replay authority is reconstructed from its creator-provenance
   events, never from mutable metadata (ADR-106). Removing those rows while their referent
   is live does not trim history; it deletes state.
2. **Provenance projection input.** ADR-041 projects provenance edges from the log; the log
   is the source the projection can be re-derived from.
3. **Accounting basis.** ADR-103 builds windowed resource attribution over the plane, and a
   windowed lifecycle comparison is only sound while both ends of the window are readable.
   ADR-018's `RateLimit` obligation is validated but explicitly unenforced in v0; when an
   admission or quota decision ships, it will need an accounting basis that does not reset,
   and a retention rule fixed after that decision would constrain it. Fixed now, retention
   can state the carry-forward rule such a decision inherits.
4. **Audit.** Dispatch audits, gate outcomes, and telemetry — the classes an age horizon
   serves well.

A uniform cutoff that is safe for the fourth silently breaks the first three. The unit at
which safety can be stated is the event class, and the plane already has the pattern for
per-class contract dimensions: attribution (ADR-162 §2), additive evolution (§3), write
posture (§4), and sink (ADR-164 §1). This decision adds the fifth dimension — lifetime —
in the same form: a closed vocabulary, declared per class, with a fail-safe rule for
classes that have not declared.

## Decision

### 1. Retention class is a per-class contract dimension, from a closed three-value set

Every event class carries a retention class, joining attribution, additivity, write
posture, and sink as a per-class contract dimension. The vocabulary is closed:

- **`pinned_while_referenced`** — the class declares a referent and a terminal condition.
  Rows are ineligible for archival while their referent is live, and become age-eligible
  only after the referent reaches its terminal state. Examples of the shape: creator-
  provenance events for a schedulable row (referent: the row; terminal: the row is
  deleted or permanently deactivated per ADR-106's contract), proposal lifecycle events
  (referent: the proposal; terminal: a terminal lifecycle state per ADR-046), process
  lineage events (referent: the process record; terminal: a terminal process state per
  ADR-161).
- **`aggregate_then_archivable`** — the class declares a durable aggregate that must exist,
  and be verified, before rows in a window become eligible. The aggregate is itself a
  durable record with a stated home, written before eligibility and never reset by the
  archival that follows it. Any class serving quota or admission accounting takes this
  value: an aggregate that survives archival is what makes a non-resetting quota
  implementable over a bounded live store. The verification contract for an aggregate —
  what is checked, against what, and recorded where — is defined at the first class
  assignment that takes this value, not inherited silently from this decision.
- **`age_archivable`** — rows are eligible on horizon age alone (§4). The value for pure
  audit and telemetry classes with no referent and no accounting consumer.

**A class with no declared retention class is retained indefinitely.** That is the absence
state, not a fourth vocabulary value, and it fails in the safe direction: nothing already
recorded becomes eligible for archival by default. §2 assigns a value to every class that
exists when this ADR is accepted; the absence state remains the fallback for any class a
later decision adds without declaring one. A newly introduced class must declare one of the
three values; omission is a rejection of the class definition, exactly as ADR-164 §2 treats
a missing sink declaration.

A class may additionally declare a **correlation key** — a payload field grouping rows into
units whose counts are compared against each other (ADR-163's restart-scan events pair an
opening count with a closing count keyed by boot identity, and its per-record terminal
events join the same unit; see §2's ADR-163 rows). Eligibility and segment assignment are
evaluated at correlation-unit granularity: a unit's rows archive together or not at all, so
a paired-count comparison never spans the horizon. This has a consequence beyond the
individual rows' own retention precondition: **a correlation-keyed unit is archive-eligible
only once it is complete** (its closing event exists on the plane), regardless of the age of
its individual rows. An incomplete unit, one whose opening has no matching closing, is
retained indefinitely by this rule alone, independent of any horizon, because there is no
closing count to compare against and archiving part of an incomplete unit would manufacture
the false completeness ADR-163 §2 exists to prevent.

**Class identity for a kind emitted by more than one site.** Most `EventKind` variants map
to exactly one emitting site with one set of retention needs. Two kinds today do not:
`Audit` and `FeedbackExplicit` are each constructed from multiple call sites whose rows
carry materially different referents, and an implementer who classifies by `kind` alone
collapses those referents into one class, which is exactly the failure this decision exists
to avoid. Every `Event` already carries a `verb` field (`crates/khive-types/src/event.rs`,
the verb that produced the event) independent of `payload`, and every constructor for these
two kinds passes a distinct, stable verb string (a constant such as
`schedule.creator_provenance`, or the dispatched `pack.verb` id). **For `Audit` and
`FeedbackExplicit`, `verb` is the class-key discriminator**: each distinct verb (or, for the
generic dispatch-audit case, the class of dispatched verbs with no dedicated kind of their
own) is its own row in §2's table, with its own retention value. No payload field is used as
the discriminator, because none of the constructors write one for this purpose; `verb` is
the field that already serves it in practice.

### 2. The normative retention mapping

This table enumerates every `EventKind` variant, one row per emitting site class, from the
production constructor sweep `rg -n "EventKind::" crates --glob '*.rs' --glob
'!**/tests/**'` plus a manual exclusion pass over `#[cfg(test)]` blocks the glob does not
reach (module-level `mod tests` inside otherwise-production files). A row marked
**no current production emitter** means the sweep found no non-test constructor for that
kind; the declared value is prospective, for whichever future change wires the kind, and the
kind's absence today does not exempt the row: omitting a declared kind from this table
would leave it in the indefinite-retention absence state by accident rather than by
decision.

This table is **prospective**: it governs which rows written from this ADR's acceptance
onward become archive-eligible under each class's precondition. It does not reach rows
already on the plane before a given class's assignment took effect; §9 states that
separately and is the governing contract for the existing 4.2-million-row population.

Rows marked **(provisional)** carry the safe-direction default. Their retention value is
normative as written, but their referent or terminal contract is not independently derived
from an owning ADR the way the proposal, schedule, and restart-boundary rows are. The owning
subsystem tightens a provisional row by amending this table; until such an amendment lands,
the stated default governs.

| #  | EventKind                                                  | Site / verb discriminator                                                                                                                                                                                                                                                                                                                                                                                                 | Retention class                                    | Correlation key | Referent / terminal condition                                                                                                                                                                                                                                        |
| -- | ---------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------- | --------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| 1  | `Audit`                                                    | dispatch-audit: `verb` = the canonical dispatched `pack.verb` id, the fallback for any dispatch not given a dedicated kind of its own (`kg.create`, `gtd.transition`, `link` denial/error paths, and the generic per-dispatch audit trail that fires alongside a dedicated structural kind for verbs that have one). Site: `crates/khive-runtime/src/pack.rs` (`build_audit_storage_event`, `persist_intercepted_audit`). | `age_archivable`                                   | none            | Pure per-dispatch audit trail (ADR-018); no referent.                                                                                                                                                                                                                |
| 2  | `Audit`                                                    | schedule creator-provenance: `verb="schedule.creator_provenance"`. Site: `crates/khive-pack-schedule/src/handlers.rs:207-218`. Write posture: precondition.                                                                                                                                                                                                                                                               | `pinned_while_referenced`                          | none            | Referent: the scheduled row (note). Terminal: the row is deleted or permanently deactivated per ADR-106.                                                                                                                                                             |
| 3  | `Audit`                                                    | schedule reminder delivery-failure: `verb="schedule.remind.fire"`. Site: `crates/khive-mcp/src/pending_events.rs:2284-2300`.                                                                                                                                                                                                                                                                                              | `age_archivable`                                   | none            | Best-effort telemetry of a delivery failure; no referent.                                                                                                                                                                                                            |
| 4  | `Audit`                                                    | git write audit: `verb` ∈ `{git.commit, git.branch, git.push}`. Site: `crates/khive-pack-git/src/write_handlers.rs:640-675` (`emit_write_audit`, ADR-108).                                                                                                                                                                                                                                                                | `age_archivable`                                   | none            | Decoupled dispatch-style audit of a git write action; the action's own effect lives in the git repository, this row is the khive-side record.                                                                                                                        |
| 5  | `Audit`                                                    | `git.digest` receipt: `verb="git.digest"`. Site: `crates/khive-runtime/src/pack.rs` (ADR-088 Amendment 1). Write posture: coupled-outcome (ADR-162 §4 third bullet).                                                                                                                                                                                                                                                      | `pinned_while_referenced` (provisional)   | none            | Referent: the ingested project/digest record (`project_id`). Terminal: not yet named; a superseding receipt for the same `project_id` may or may not retire the prior one, and confirmation belongs to a git-pack amendment of this row.               |
| 6  | `Audit`                                                    | moodboard serve record: `verb=SERVE_RECORD_VERB`. Site: `crates/khive-pack-moodboard/src/preference_handlers.rs:983-1001` (`.with_aggregate("moodboard_serve", ...)`). Write posture: precondition (event-sourced aggregate).                                                                                                                                                                                             | `pinned_while_referenced` (provisional)   | none            | Referent: the moodboard serve aggregate (`serve_id` / `board_entity_id`). Terminal: not named by ADR-148/149; confirmation belongs to a moodboard-pack amendment of this row.                                                                                                                    |
| 7  | `Audit`                                                    | moodboard model record: `verb=MODEL_RECORD_VERB`. Site: `crates/khive-pack-moodboard/src/preference_handlers.rs:~1330`.                                                                                                                                                                                                                                                                                                   | `pinned_while_referenced` (provisional)   | none            | Referent: the moodboard model aggregate (`model.id`). Terminal: not named; confirmation belongs to a moodboard-pack amendment of this row.                                                                                                                                                                     |
| 8  | `FeedbackExplicit`                                         | brain feedback: `verb="brain.feedback"`. Site: `crates/khive-pack-brain/src/handlers.rs:1670-1712`, folded by `crates/khive-pack-brain/src/fold.rs`.                                                                                                                                                                                                                                                                      | `aggregate_then_archivable` (provisional) | none            | Aggregate: the brain profile posterior snapshot these events are folded into. Verification contract (what is checked against what, recorded where) is deferred to a khive-pack-brain-owning follow-up per §1's rule that it is not inherited silently from this ADR. |
| 9  | `FeedbackExplicit`                                         | moodboard judgment: `verb=JUDGMENT_RECORD_VERB`. Site: `crates/khive-pack-moodboard/src/preference_handlers.rs:1053-1072` (`.with_aggregate("moodboard_judgment", ...)`). Write posture: precondition.                                                                                                                                                                                                                    | `pinned_while_referenced` (provisional)   | none            | Referent: the moodboard judgment aggregate. Terminal: not named; confirmation belongs to a moodboard-pack amendment of this row.                                                                                                                                                                               |
| 10 | `RecallExecuted`                                           | `crates/khive-pack-memory/src/handlers/recall.rs:1120-1143`.                                                                                                                                                                                                                                                                                                                                                              | `age_archivable`                                   | none            | Pure telemetry of a recall call; feedback targets the recalled entity/note by id, never this event, so nothing downstream holds a reference to this row.                                                                                                             |
| 11 | `RerankExecuted`                                           | No current production emitter (`RerankExecutedPayload` is typed and tested but has no non-test constructor).                                                                                                                                                                                                                                                                                                              | `age_archivable`                                   | none            | Prospective, matching its sibling telemetry kinds (10, 12).                                                                                                                                                                                                          |
| 12 | `SearchExecuted`                                           | `crates/khive-pack-kg/src/handlers/search.rs:502-518`.                                                                                                                                                                                                                                                                                                                                                                    | `age_archivable`                                   | none            | Same shape as row 10.                                                                                                                                                                                                                                                |
| 13 | `ChannelPollStarted`                                       | `crates/khive-mcp/src/serve.rs:~896`.                                                                                                                                                                                                                                                                                                                                                                                     | `age_archivable`                                   | none            | ADR-094 sequencing telemetry.                                                                                                                                                                                                                                        |
| 14 | `ChannelPollSucceeded`                                     | `crates/khive-mcp/src/serve.rs:~938`.                                                                                                                                                                                                                                                                                                                                                                                     | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 15 | `ChannelPollFailed`                                        | `crates/khive-mcp/src/serve.rs:~1070`.                                                                                                                                                                                                                                                                                                                                                                                    | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 16 | `ChannelBackoffArmed`                                      | `crates/khive-mcp/src/serve.rs:~1090`.                                                                                                                                                                                                                                                                                                                                                                                    | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 17 | `ChannelBackoffReset`                                      | `crates/khive-mcp/src/serve.rs:~949`.                                                                                                                                                                                                                                                                                                                                                                                     | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 18 | `ChannelHeartbeatPersistFailed`                            | `crates/khive-mcp/src/serve.rs:~1232`.                                                                                                                                                                                                                                                                                                                                                                                    | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 19 | `ConfigLocked`                                             | `crates/khive-runtime/src/pack.rs:~1738`.                                                                                                                                                                                                                                                                                                                                                                                 | `age_archivable`                                   | none            | Process-lifetime diagnostic; deliberately excluded from per-verb receipt counts (see the code comment at the site).                                                                                                                                                  |
| 20 | `CheckpointOutcomeRecorded`                                | `crates/khive-db/src/checkpoint.rs:~2177`.                                                                                                                                                                                                                                                                                                                                                                                | `age_archivable`                                   | none            | WAL checkpoint telemetry.                                                                                                                                                                                                                                            |
| 21 | `PhaseStarted`                                             | `crates/khive-pack-kg/src/dispatch.rs:76`; `crates/khive-pack-knowledge/src/pack.rs:162`; `crates/khive-pack-memory/src/ann.rs:914`; `crates/khive-runtime/src/phase_events.rs:149`; `crates/khive-pack-brain/src/handlers.rs:2206`.                                                                                                                                                                                      | `age_archivable`                                   | none            | ADR-103 Stage 1 background-phase telemetry.                                                                                                                                                                                                                          |
| 22 | `PhaseCompleted`                                           | `crates/khive-pack-kg/src/dispatch.rs:113`; `crates/khive-pack-knowledge/src/pack.rs:201`; `crates/khive-pack-memory/src/ann.rs:950`.                                                                                                                                                                                                                                                                                     | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 23 | `PhaseCancelled`                                           | `crates/khive-pack-kg/src/dispatch.rs:98`; `crates/khive-pack-knowledge/src/pack.rs:186`; `crates/khive-pack-memory/src/ann.rs:935`.                                                                                                                                                                                                                                                                                      | `age_archivable`                                   | none            | Same.                                                                                                                                                                                                                                                                |
| 24 | `EmbedderInitialized`                                      | `crates/khive-runtime/src/runtime.rs:1334-1341`.                                                                                                                                                                                                                                                                                                                                                                          | `age_archivable`                                   | none            | Process lifecycle diagnostic, distinct from `EmbeddingModelChanged` (row 41).                                                                                                                                                                                        |
| 25 | `EntityCreated`                                            | No current production emitter (`kg.create(kind=<entity kind>)` records generically under row 1 instead).                                                                                                                                                                                                                                                                                                                  | `pinned_while_referenced`                          | none            | Prospective, matching rows 26-28. Referent: the created entity. Terminal: the entity is hard-deleted.                                                                                                                                                                |
| 26 | `EntityUpdated`                                            | `crates/khive-runtime/src/atomic_prepare.rs:862-875`; `crates/khive-runtime/src/curation.rs:910-920`.                                                                                                                                                                                                                                                                                                                     | `pinned_while_referenced`                          | none            | Referent: the target entity (`target_id`). Terminal: the entity is hard-deleted (cascade removes incident edges).                                                                                                                                                    |
| 27 | `EntityDeleted`                                            | `crates/khive-runtime/src/atomic_prepare.rs:1169-1176`; `crates/khive-runtime/src/operations.rs:~4829`.                                                                                                                                                                                                                                                                                                                   | `pinned_while_referenced`                          | none            | Referent: the deleted entity. Terminal: the entity is hard-deleted; for a hard-delete's own event, the referent is already terminal at write time, while for a soft-delete's event, the row stays pinned until a later hard delete.                                  |
| 28 | `EntityMerged`                                             | `crates/khive-runtime/src/curation.rs:1189-1199`.                                                                                                                                                                                                                                                                                                                                                                         | `pinned_while_referenced`                          | none            | Referent: the kept entity (`summary.kept_id`). Terminal: the kept entity is hard-deleted.                                                                                                                                                                            |
| 29 | `NoteCreated`                                              | `crates/khive-pack-memory/src/handlers/remember.rs:220-226`. Emitted only by `memory.remember`; generic `kg.create(kind=<note kind>)` records under row 1 instead.                                                                                                                                                                                                                                                        | `pinned_while_referenced`                          | none            | Referent: the created memory note. Terminal: the note is hard-deleted (ordinary `memory.prune` only soft-deletes per ADR-021 and does not reach terminal).                                                                                                           |
| 30 | `NoteUpdated`                                              | No current production emitter (generic note update records under row 1 instead).                                                                                                                                                                                                                                                                                                                                          | `pinned_while_referenced`                          | none            | Prospective, matching row 29/31. Referent: the updated note. Terminal: the note is hard-deleted.                                                                                                                                                                     |
| 31 | `NoteDeleted`                                              | `crates/khive-runtime/src/atomic_prepare.rs:1249-1258`; `crates/khive-runtime/src/operations.rs:4494-4503`.                                                                                                                                                                                                                                                                                                               | `pinned_while_referenced`                          | none            | Same shape as row 27, for notes.                                                                                                                                                                                                                                     |
| 32 | `NoteMerged`                                               | `crates/khive-runtime/src/curation.rs:~1844`.                                                                                                                                                                                                                                                                                                                                                                             | `pinned_while_referenced`                          | none            | Referent: the kept note. Terminal: the kept note is hard-deleted.                                                                                                                                                                                                    |
| 33 | `LinkCreated`                                              | No current production emitter (`link` verb dispatch records under row 1's `link_audit_success_from_result` path instead).                                                                                                                                                                                                                                                                                                 | `pinned_while_referenced`                          | none            | Prospective, matching rows 34-35. Referent: the created edge. Terminal: the edge is deleted.                                                                                                                                                                         |
| 34 | `EdgeUpdated`                                              | `crates/khive-runtime/src/atomic_prepare.rs:1029-1038`; `crates/khive-runtime/src/operations.rs:5450-5460`.                                                                                                                                                                                                                                                                                                               | `pinned_while_referenced`                          | none            | Referent: the target edge. Terminal: the edge is deleted.                                                                                                                                                                                                            |
| 35 | `EdgeDeleted`                                              | `crates/khive-runtime/src/atomic_prepare.rs:1346-1352`; `crates/khive-runtime/src/operations.rs:~5540`.                                                                                                                                                                                                                                                                                                                   | `pinned_while_referenced`                          | none            | Referent: the deleted edge. Terminal: already reached at write time.                                                                                                                                                                                                 |
| 36 | `TaskTransitioned`                                         | No current production emitter (`gtd.transition` records under row 1 instead).                                                                                                                                                                                                                                                                                                                                             | `pinned_while_referenced`                          | none            | Prospective. Referent: the task note. Terminal: the task note is hard-deleted (reaching a GTD-terminal status such as `done`/`cancelled` is not itself retention-terminal; the note is still a live KG record).                                                      |
| 37 | `ProposalCreated`                                          | `crates/khive-pack-kg/src/handlers/proposal.rs:~202`.                                                                                                                                                                                                                                                                                                                                                                     | `pinned_while_referenced`                          | none            | Referent: the proposal. Terminal: a terminal lifecycle state per ADR-046.                                                                                                                                                                                            |
| 38 | `ProposalReviewed`                                         | `crates/khive-pack-kg/src/handlers/proposal.rs:~326`.                                                                                                                                                                                                                                                                                                                                                                     | `pinned_while_referenced`                          | none            | Same.                                                                                                                                                                                                                                                                |
| 39 | `ProposalApplied`                                          | `crates/khive-pack-kg/src/apply_worker/worker.rs:~639`.                                                                                                                                                                                                                                                                                                                                                                   | `pinned_while_referenced`                          | none            | Same.                                                                                                                                                                                                                                                                |
| 40 | `ProposalWithdrawn`                                        | `crates/khive-pack-kg/src/handlers/proposal.rs:~456`.                                                                                                                                                                                                                                                                                                                                                                     | `pinned_while_referenced`                          | none            | Same.                                                                                                                                                                                                                                                                |
| 41 | `EmbeddingModelChanged`                                    | No current production emitter.                                                                                                                                                                                                                                                                                                                                                                                            | `pinned_while_referenced` (provisional)   | none            | Referent: the embedding model version this describes. Terminal: superseded by the next `EmbeddingModelChanged` naming the same model subject, or a future embedding-subsystem ADR names one.                                                                         |
| 42 | `EmbeddingMigrationCompleted`                              | No current production emitter.                                                                                                                                                                                                                                                                                                                                                                                            | `pinned_while_referenced` (provisional)   | none            | Same shape as row 41.                                                                                                                                                                                                                                                |
| 43 | `EmbeddingMigrationFailed`                                 | No current production emitter.                                                                                                                                                                                                                                                                                                                                                                                            | `pinned_while_referenced` (provisional)   | none            | Same shape as row 41.                                                                                                                                                                                                                                                |
| 44 | `EmbeddingDriftDetected`                                   | No current production emitter.                                                                                                                                                                                                                                                                                                                                                                                            | `pinned_while_referenced` (provisional)   | none            | Same shape as row 41.                                                                                                                                                                                                                                                |
| 45 | `ProfileResolutionRecommended`                             | No current production emitter.                                                                                                                                                                                                                                                                                                                                                                                            | `pinned_while_referenced` (provisional)   | none            | Referent: the brain profile this recommends resolving. Terminal: superseded by the next event of the same kind for the same profile, or a follow-up names one.                                                                                                       |
| 46 | `ProfileMerged`                                            | No current production emitter.                                                                                                                                                                                                                                                                                                                                                                                            | `pinned_while_referenced` (provisional)   | none            | Referent: the kept profile. Terminal: same shape as row 45.                                                                                                                                                                                                          |
| 47 | `RestartScanOpened` (ADR-163; not yet in `EventKind::ALL`) | Boot scan, emitted before any per-record event.                                                                                                                                                                                                                                                                                                                                                                           | `age_archivable`                                   | `boot_id`       | Runtime-system-actor telemetry once complete; see §1's completeness rule: this unit is ineligible until row 49 exists for the same `boot_id`.                                                                                                                        |
| 48 | `RecordTerminatedAtRestart` (ADR-163)                      | Boot scan, one per terminated record.                                                                                                                                                                                                                                                                                                                                                                                     | `age_archivable`                                   | `boot_id`       | Same unit as rows 47/49.                                                                                                                                                                                                                                             |
| 49 | `RestartScanClosed` (ADR-163)                              | Boot scan, emitted after per-record events.                                                                                                                                                                                                                                                                                                                                                                               | `age_archivable`                                   | `boot_id`       | Same unit; this event's presence is what makes the unit complete.                                                                                                                                                                                                    |
| 50 | `ArchiveSegmentSealed` (this ADR, §5)                      | Archival worker, one per sealed segment.                                                                                                                                                                                                                                                                                                                                                                                  | `pinned_while_referenced`                          | none            | Referent: the segment. Terminal: destruction of the segment (§6).                                                                                                                                                                                                    |
| 51 | `ArchiveRowsPruned` (this ADR, §5)                         | Archival worker, one per prune.                                                                                                                                                                                                                                                                                                                                                                                           | `pinned_while_referenced`                          | none            | Same shape as row 50.                                                                                                                                                                                                                                                |
| 52 | `ArchiveSegmentAccessAttempted` (this ADR, §5, §6)         | Operator unseal/restore/replace action, one per attempt.                                                                                                                                                                                                                                                                                                                                                                  | `pinned_while_referenced`                          | none            | Same shape as row 50.                                                                                                                                                                                                                                                |

### 3. Archival is copy-then-seal-then-verify; the plane as a whole stays append-only

Archival moves eligible rows into **sealed segments** outside the live store:

1. **Copy.** Rows eligible under §1/§2 and older than the horizon (§4) are copied, with their
   `event_observations` rows, into a new segment — an append-only artifact outside the live
   database file. Segment format and placement are operator-owned configuration; the
   contract is on the manifest and the verification, not the container.
2. **Seal.** The segment closes with a manifest recording: segment identity, the classes it
   contains, row count, the covered `created_at` range, and a content digest computed over
   the segment's rows.
3. **Verify.** Before any pruning, the sealed segment is independently re-read: row count
   and content digest are recomputed from the segment and compared against the manifest.
   A mismatch voids the segment; nothing is pruned against it.

Only after verification may the live store be pruned, and pruning is bounded to exactly the
rows proven present in the verified segment — the deletion's row set is derived from the
segment readback, not from re-evaluating the eligibility predicate, so a row that became
eligible after the copy cannot be deleted without having been archived.

This is the one conscious amendment to the plane's append-only posture, and it is scoped:
the **live table** loses rows only to a verified sealed copy, and the **plane as a whole**
— live store plus sealed segments — remains append-only. §6 states the one narrow exception
this decision allows for a sealed segment's own destruction; short of that exception, no
information is destroyed.

### 4. The horizon is operator configuration, and archival is off by default

Eligibility requires `created_at` older than a configured horizon in addition to the
class's own precondition. The horizon's value is operator-owned configuration; this
decision fixes the mechanism, not the number. **Unset means archival does not run.** A
deployment that never configures a horizon keeps today's behaviour exactly, and no
migration or backfill occurs on upgrade. §9 states the historical-row contract this
implies in full.

### 5. Archival actions are themselves events, recorded ahead of the destructive step

The retention machinery emits its own event classes, declared here with all five
dimensions (attribution, additivity, write posture, sink, retention). All three are
additive `EventKind` variants per ADR-162 §3: never removed, never repurposed, no payload
field redefined once shipped.

- **`ArchiveSegmentSealed`**: one per sealed segment. Payload: `segment_id`, `classes`
  (the retention classes represented in the segment), `row_count`, `covered_range` (the
  segment's `created_at` span as `{from, to}`), `content_digest`, `manifest_version`.
  Attribution: the daemon principal (runtime background work, the ADR-162 §2 form). Sink:
  `caller_event_store`. Write posture: **precondition**: the event is appended and
  durable before any prune against that segment may run; if the append fails, the prune
  does not run. Retention class: `pinned_while_referenced`, referent the segment
  itself; terminal condition: destruction of the segment (§6). Manifests are therefore
  effectively permanent, which is intended, since the manifest must outlive everything it
  vouches for.
- **`ArchiveRowsPruned`**: one per prune. Payload: `segment_id` (the segment pruned
  against), `row_count_removed`, `manifest_digest` and `recomputed_digest` (the
  verification pair from §3's verify step), `manifest_row_count` and
  `recomputed_row_count`. Same attribution and sink. Write posture: precondition,
  appended before the delete executes. Retention class: `pinned_while_referenced`,
  referent the segment; terminal condition: destruction of the segment.
- **`ArchiveSegmentAccessAttempted`**: one per unseal, restore, or replace attempt against
  a sealed segment (§6), success or failure. Payload: `segment_id`, `action`
  (`"unseal"` | `"restore"` | `"replace"`), `gate_decision` (`"allow"` | `"deny"`),
  `digest_verified` (boolean, present for `unseal`/`restore`; absent for `replace`, which
  does not read the segment's content before acting), `reason` (optional). Attribution:
  the operator principal the Gate resolved for the request (ADR-162 §2's dispatched-actor
  form: this is an explicit operator action, not background daemon work, so it does not
  take the daemon-principal form rows 50/51 use). Sink: `caller_event_store`, for
  consistency with rows 50/51 and because these rows are data-plane evidence about a
  segment rather than the ADR-161-style caller-visibility-sensitive structural set ADR-164
  §3 routes to the operator sink. Write posture: **precondition** for all three actions,
  including the read-only `unseal` case: the event must be durably appended before the
  action (even a read) is allowed to proceed, so that no access to a sealed segment can
  happen without a corresponding audit row, per §6. Retention class:
  `pinned_while_referenced`, referent the segment; terminal condition: destruction of the
  segment.

Recording the verification pair (manifest value and recomputed value) in the prune event
is deliberate: a reader auditing retention can check the comparison from the plane alone,
without access to the segment media.

### 6. Sealed segment lifecycle: WORM, unseal, restore, and replacement

Once sealed (§3, step "Seal"), a segment is **write-once**. Nothing in this decision, and
no path this decision provides, mutates a sealed segment's rows or manifest in place. Three
distinct operator actions exist over a sealed segment, each requiring the Gate's
affirmative `Allow` (ADR-018) under its own canonical verb id: no default configuration
grants any of them implicitly, none is covered by a generic verb-wildcard policy, each must
be named explicitly by verb id in the operator's capability grant (consistent with ADR-018
Amendment 1's canonical-verb-id rule), and a gate `Err` refuses the action per ADR-018
Amendment 3 exactly as it refuses any other dispatch.

- **Open (`archive.unseal`).** Resolves a segment for read access and is **read-only by
  default**: it never removes, reorders, or rewrites the segment's rows, and it grants no
  write capability over the segment or the live store. Every open recomputes the segment's
  content digest and compares it against the manifest `ArchiveSegmentSealed` recorded. A
  mismatch is a digest-verification failure, handled below.
- **Restore (`archive.restore`).** An operator action that copies archived rows back into a
  location the runtime can serve. This decision does not define a re-federation path back
  into the live `events` table: the "Transparent federation" alternative considered below
  remains rejected for the query surface, so restore's destination and the guarantees it
  carries are scoped to the operator surface, not to ADR-022's caller-facing `list`/`get`.
  Restore is a distinct, more consequential action than open and is gated by its own
  canonical verb id rather than folded into `archive.unseal`.
- **Replace or delete (`archive.replace_segment`).** Overwriting or removing a sealed
  segment's media is a separate, explicitly authorized destructive act, gated by its own
  canonical verb id, distinct from open and restore. This is the one action this ADR
  permits that can destroy a segment: every other section states no such path exists, and
  this is the exception, named here rather than left implicit. A digest check does not
  apply before a replace or delete the way it does for open/restore, because the action
  does not depend on reading the segment's current content first.

Every attempt at any of the three actions, whether the Gate allows or denies it and, for
open/restore, whether the digest comparison passes, emits `ArchiveSegmentAccessAttempted`
(§5) before the action's effect, if any, is allowed to proceed. This is what makes the
audit mandatory rather than advisory: an implementation that performs the action first and
records the attempt afterward, or only on success, does not implement this section.

A digest-verification failure on open or restore is a **mandatory alert, not a silent
skip**: the event's outcome is `EventOutcome::Error` with `digest_verified: false`, the
failure is additionally logged at error level through the host tracing sink (the same
two-sink discipline ADR-018 established for gate audit: structured tracing plus
`EventStore` persistence), and the call returns an error to the caller rather than serving
the segment's rows from an unverified read. Nothing in this path substitutes a partial or
unverified read for a verified one.

### 7. The query surface states what retention did — two additive behaviours on ADR-022

Retention must not silently change what a query means. Two additive contract behaviours:

- **Horizon disclosure on `list`.** A `list(kind="event")` whose effective time window
  extends past the oldest live row's horizon reports that the window is horizon-clipped —
  a completeness fact on the result envelope, present only when clipping occurred.
  Consumers that never query past the horizon see no change. This discriminant is scoped
  to retention state only; the general availability discriminant (whether the plane
  answered fully in the presence of write or read failure) belongs to ADR-162 Open
  section B, which should take this behaviour as an input rather than find it decided.
- **Archived-stub resolution on `get`.** Pruning writes a stub — event id and segment
  identity — into a live index, and a by-ID `get` of a pruned event resolves the stub to
  a typed archived answer naming the segment, rather than the absence answer an unknown
  id gets. Absence of any record and archival of a real record are different facts, and a
  surface that returns the same answer for both converts retention into silent data loss
  for every holder of an event id. The stub index grows with archived rows but carries
  two identifiers per row, which is the price of keeping by-ID resolution honest. The
  stub is data, not authorization: ADR-007's namespace-agnostic by-ID posture is
  unchanged, and stubs for `operator_audit`-sinked classes do not exist because those
  rows were never on this surface (ADR-164 §4).

### 8. Population, stated exactly

This decision governs the runtime event plane: the `events` and `event_observations`
tables, with observations always archiving alongside their event in the same segment.
The archived-stub index (§7) is a table this decision creates and is in population: its
growth is bounded to two identifiers per archived row, and stubs are never themselves
archived.

Explicitly out of scope:

- **The operator audit sink.** ADR-164 names its retention a separate operational
  decision; nothing here reaches it.
- **Pack-owned logs.** `brain_event_log` is a pack-owned append-only table on a different
  surface; a pack owning a private log owes its own retention decision, for which this
  ADR's class pattern is available but not imposed.
- **Non-event tables.** Notes, entities, edges, and their curation lifecycle are governed
  by ADR-014; nothing here adds a deletion path to any other substrate.

### 9. Historical rows: grandfathered by default, reachable only through a versioned backfill

**Pre-existing rows are never archive-eligible under this ADR.** A row's eligibility under
§1/§2 is evaluated against the retention class in effect at the row's own write time, not
retroactively against a class assigned after the row was written. This is a stronger rule
than "an unassigned class defaults to indefinite retention" (§1): it holds even for a class
this ADR itself assigns a value to in §2, because assigning a value prospectively does not
answer how the rows written before the value existed should be treated, and this ADR does
not extend eligibility backward without a stated procedure for doing so safely.

The consequence is stated plainly: **until a versioned backfill procedure defined below
lands and runs, the historical population is unbounded, and the bounded-store objective
this ADR exists to serve applies only to rows written after each class's assignment took
effect.** That is the intended failure direction: the historical population keeps growing
the store, but nothing already recorded is silently made eligible for a process this ADR
did not validate against it.

A future backfill decision that wants to reach the historical population must state, at
minimum:

- **Versioned classifier identity.** Which version of which class-assignment rule the
  backfill run classified rows under, recorded so a later audit can tell which rows were
  reachable by which rule.
- **Unresolved-reference handling.** What happens to a row whose referent (§1) cannot be
  resolved: the entity, note, edge, proposal, or scheduled row it names no longer exists
  or was never captured. A backfill that treats an unresolved reference as terminal risks
  archiving a row whose referent is actually still live under a different identity; a
  backfill that treats it as non-terminal risks the historical population never becoming
  eligible at all. The rule must be stated, not inferred per row at backfill time.
- **Correlation-unit reconstruction.** For any correlation-keyed class (§1, §2 rows 47-49),
  how the backfill groups historical rows into units when the unit's completeness was
  never evaluated against this ADR's rule at write time, and how an incomplete historical
  unit is distinguished from one whose closing event exists but predates the backfill's own
  bookkeeping.
- **Stub/index creation.** How the archived-stub index (§7) is populated for historical
  rows the backfill archives, so by-ID `get` resolution stays honest for pre-existing event
  ids exactly as it does for rows archived under this ADR's live path.
- **Audit evidence.** The backfill run itself is an operator action in the shape of §6: it
  must emit its own mandatory audit trail (reusing or extending the machinery in §5) rather
  than running as an unaudited maintenance script.
- **Post-backfill verification.** The same copy-then-seal-then-verify discipline (§3)
  applies to backfilled segments; a backfill is not exempt from verification because its
  source rows are old.
- **Rollback.** A stated procedure for undoing a backfill run that classified rows
  incorrectly, before those rows are pruned from the live store. Verification (§3) already
  gates pruning, so a rollback window exists by construction as long as the backfill does
  not skip the verify step.

Until that decision lands, §2's table governs new rows only, and the historical population
is retained in full, which is the safe direction this ADR chooses throughout.

## Non-goals

- **No durability or availability decision.** ADR-162's Open sections A and B remain open;
  §7's horizon disclosure is scoped input to B, not a resolution of it.
- **No coverage change.** Retention governs recorded events. What gets recorded — including
  known gaps where mutations produce no event-plane observation — is the emission
  contract's problem, not retention's, and archival neither widens nor narrows it.
- **No segment transport or remote storage policy.** Where segments live beyond "outside
  the live database file" is operational.
- **No automatic scheduling policy.** When the archival pass runs is operational; the
  contract governs what it may touch and what it must record.
- **Vocabulary change is scoped to this decision's own machinery.** Outside of the three
  variants §5 adds for archival and access bookkeeping, this ADR adds no event kind, and
  kinds remain additive per ADR-162 §3 for every other decision's domain. Retention removes
  rows, never kinds, and a class whose rows are all archived remains a defined class.

## Consequences

- The plane gains a bounded live store without giving up its append-only character: the
  amendment is scoped to the live table, conditioned on a verified sealed copy, and
  recorded on the plane itself before it happens.
- Every future event-emitting decision now declares five dimensions instead of four, and
  the fifth has a fail-safe absence state, so the cost of not deciding is growth rather
  than loss.
- A future admission or quota decision inherits `aggregate_then_archivable` as the stated
  carry-forward rule, instead of discovering after the fact that its accounting basis was
  age-pruned.
- By-ID holders of archived event ids keep an honest answer (§7), at the cost of a stub
  index that grows with archived volume.
- Every existing `EventKind` variant, including the multi-site `Audit` and
  `FeedbackExplicit` classes and the kinds with no current production emitter, now carries
  a stated retention value, with provisional rows visibly designated as such (§2),
  rather than being deferred wholesale to a follow-up decision. The provisional rows are
  the residual follow-up work, scoped to specific rows with a stated reason each, not to
  the whole mapping.
- Three new event classes exist with their five dimensions declared (§5), exercising the
  full declaration this family of decisions now requires, and a sealed segment's lifecycle
  (§6) is fully specified: read-only opening, gated restore, separately gated and audited
  replacement, and a mandatory, precondition-posture audit trail for every attempt.
- The historical population (§9) is explicitly, permanently out of this ADR's reach absent
  a future versioned backfill decision meeting the stated requirements: the bounded-store
  objective applies to post-assignment rows only until that decision lands, and that scope
  limitation is stated rather than discovered later.

## Alternatives considered

### A single age horizon for all classes

Rejected. Safe for audit classes, wrong for event-sourced state (ADR-046, ADR-106),
provenance sources (ADR-041), and accounting bases (ADR-103, future quota work) — and
wrong silently, since deletion of load-bearing rows presents as data loss long after the
prune.

### Delete-only retention, no archive

Rejected. It destroys information the plane exists to keep, forecloses re-projection
(ADR-041) and late audit, and makes the append-only claim of ADR-004/162 false rather
than consciously amended. The marginal cost of copy-then-verify over delete is the
archive media itself, which is the cheapest component in the system.

### Export without pruning

Considered, and preserved as the default state: with no horizon configured, this decision
changes nothing. Rejected as the end state because it does not bound the live store,
which is the operational pressure motivating retention.

### Transparent federation of live and archived rows on the query surface

Rejected. Making `list` read sealed segments transparently reintroduces the unbounded
query population retention exists to bound, couples caller-facing availability to archive
media, and hides the horizon instead of disclosing it. An explicit discriminant and a
typed stub answer keep the surface honest at bounded cost; an operator-side tool reading
segments directly (§6) needs no runtime surface at all.

### Per-namespace retention policy

Deferred. Namespace is attribution, not an authorization or lifecycle boundary (ADR-007);
a per-namespace horizon would make retention a per-tenant contract before any decision
establishes tenancy semantics for the plane. The per-class dimension composes with such a
decision later without being blocked on it.

### Backfilling historical rows as part of this decision

Rejected for this ADR. §9 states the requirements a backfill decision must meet, but
meeting them (unresolved-reference handling, correlation-unit reconstruction for a
population that predates this contract, rollback) is substantial design work in its own
right, and coupling it to the mechanism decision here would either delay the mechanism or
under-specify the backfill. Grandfathering the historical population by default keeps the
two decisions independently reviewable, at the stated cost that the bounded-store objective
does not reach existing rows until the follow-up lands.
