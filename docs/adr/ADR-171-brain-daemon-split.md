# ADR-171: Brain daemon — profile state leaves the domain process

- Status: Proposed
- Date: 2026-08-24

## Context

The brain pack owns recall-weighting state: per-profile posterior state
(`BrainState`, section posteriors, entity posteriors, implicit mass) held
in-memory per namespace, persisted to three tables in the main store
(`brain_event_log`, `brain_profile_snapshots`, `brain_implicit_mass`), with
folds applied synchronously inside verb dispatch. This couples the domain
process to profile serving twice over: brain persistence rides the main
store's single writer lane, and the in-memory posterior state is bound to
the domain process lifecycle — a daemon restart discards warm state and
every embedded host rebuilds it from snapshot on first touch.

ADR-170 split the audit lane into a dedicated events daemon and validated
the hand-off primitive this ADR reuses: a subcommand of the same binary
owning its own SQLite file behind a Unix socket with length-prefixed frames,
peer-uid admission, a flock guard, supervision from the domain daemon, and a
kill-switch. This ADR is the follow-up its Forward section names.

Two seam facts, verified in-tree, make this decomposition cleaner than the
events one:

- **The brain tables are pack-internal.** A workspace enumeration of raw-SQL
  consumers (`grep -rl` over `brain_event_log`, `brain_profile_snapshots`,
  `brain_implicit_mass`) finds them referenced only inside `khive-pack-brain`
  (`persist.rs`, `fold_gate.rs`). Unlike the legacy `events` table (ADR-170's
  amendment), no external raw-SQL consumer depends on co-residency, so the
  whole state can move.
- **Consumers reach brain state through verbs, not memory.** The recall path
  loads profile coefficients by dispatching `brain.profile` through the verb
  registry (`khive-pack-memory/src/handlers/recall.rs::load_brain_profile`),
  and feedback lands through `brain.feedback`/`brain.auto_feedback`. The
  registry dispatch is already the interface; recall consumes a handful of
  numbers per query (posterior means folded into scoring terms, the ADR-104
  multiplier clamp, reprojection weight), never the state itself.

## Decision

Move brain state into a dedicated brain daemon that owns `brain.db`.

1. **Brain daemon.** A new subcommand of the same binary owns `brain.db`
   beside the main store (the three brain tables plus its in-memory
   posterior state), binds `khive-brain.sock` derived beside that file, and
   reuses the ADR-170 socket pattern verbatim: framing, peer-uid admission,
   flock guard, versioned protocol with typed refusals. It is the only
   resident writer of `brain.db`.

2. **Event feed, not a push stream.** The brain daemon consumes the event
   plane by tailing the ADR-170 events lane with short-lived read-only opens
   (WAL one-writer-many-readers), folding feedback and serve events into
   posterior state on its own schedule and checkpointing its read cursor in
   `brain.db`. No new streaming protocol and no producer-side coupling: an
   events-lane row is folded whether the brain daemon was up when it landed
   or not.

3. **Coefficient queries.** The brain verbs (`brain.profile` and the other
   profile lifecycle/read verbs) are served in the domain process by a thin
   client that round-trips the daemon socket with a bounded timeout. The
   response is the few numbers recall already consumes — not state, not
   snapshots. Verb-level consumers (recall, knowledge suggest) do not change.

4. **Fail-soft is the contract.** If the brain daemon is unreachable, recall
   proceeds un-reweighted (hybrid ranking without the profile terms), stamps
   no serving profile, increments a degradation counter, and logs the reason
   once per outage. A recall must never block on, or fail because of,
   profile-state liveness. Feedback during an outage is not lost: it rides
   the events lane (point 2), so the daemon folds it on return.

5. **Lifecycle, embedded mode, cutover.** The domain daemon supervises the
   brain daemon exactly as it supervises the events daemon. One-shot
   embedded hosts open `brain.db` directly, covered by SQLite's per-file
   cross-process exclusion. State migrates by snapshot: on first boot the
   brain daemon imports the existing snapshots and replays newer feedback
   from the event plane; the legacy rows in the main store age in place until
   the operator purge tool (ADR-170 point 6) covers them too. A kill-switch
   environment variable restores legacy in-process behavior.

## Consequences

**Positive.**

- Brain writes (event log, snapshot upserts, implicit-mass moves) leave the
  main writer lane entirely — the remaining recall-adjacent write traffic on
  the domain store after ADR-170.
- Posterior state gets a process lifetime independent of domain-daemon
  restarts, and one authoritative copy instead of per-process rebuilds.
- The event plane becomes the single feedback transport; the fold path stops
  running inside verb dispatch.

**Negative / accepted.**

- Recall's profile terms become eventually consistent with feedback: a fold
  happens when the brain daemon tails the event, not inside the submitting
  dispatch. The posterior model is already an accumulation of evidence over
  time; a seconds-scale fold lag is within its semantics.
- A coefficient round-trip is added to profile-stamped recalls; bounded by
  the socket timeout and removed entirely by the fail-soft path when the
  daemon is down.
- One more supervised child process and one more database file.

## Alternatives considered

- **Keep folds in-process, split only persistence.** Rejected: removes the
  writer-lane share but keeps warm state bound to the domain process and
  keeps every embedded host rebuilding it; the coefficient-query seam is
  what makes state ownership movable at all.
- **Push stream from the events daemon to the brain daemon.** Rejected:
  invents a subscription protocol and a delivery contract the tail-with-
  cursor model gets from SQLite WAL for free, including catch-up after brain
  downtime.
- **Brain daemon owns the events lane too (one auxiliary daemon).** Rejected
  for now: the two lanes have different durability classes and failure
  domains (audit drops are tolerable; posterior state is not), and the
  four-daemon topology this program is converging on separates them
  deliberately. Revisit only with measurements showing the process count
  itself is a cost.

## Forward

The remaining decomposition candidates on the domain store — the indexing
write path (FTS/vector maintenance) as its own concern — follow the same
primitive and the same analysis discipline: enumerate raw-SQL co-residency
consumers of the tables in question before deciding what moves. That
analysis, not this ADR, decides the next slice.

## References

- ADR-170 (events daemon; the hand-off primitive and its amendment's
  co-residency analysis)
- ADR-104 (posterior serving: multiplier clamp, reprojection weight)
- ADR-133 (durable audit batching — the events-lane producer)
- `crates/khive-pack-brain/src/persist.rs` (state persistence, the three
  tables)
- `crates/khive-pack-brain/src/fold_gate.rs` (implicit-mass fold gate)
- `crates/khive-pack-memory/src/handlers/recall.rs` (`load_brain_profile` —
  the verb-level coefficient seam)
