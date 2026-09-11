# ADR-184: Telemetry Pack — Channel Table, Emit Outcome, and Read-Time Rollup

- **Status**: Proposed
- **Date**: 2026-09-11
- **Depends on**: [ADR-017](ADR-017-pack-standard.md) (Pack trait, additive pack surface),
  [ADR-023](ADR-023-declarative-pack-format.md) (self-registration, verb visibility),
  [ADR-027](ADR-027-dynamic-pack-loading.md) (per-pack config schemas live in the pack's own record),
  [ADR-174](ADR-174-ordered-streams-append.md) (`stream.append` / `stream.read` / `stream.batch`, the durable log)
- **Relates to**: [ADR-094](ADR-094-lifecycle-telemetry-events.md) (lifecycle telemetry _events_, a
  different subject: that record governs which events the runtime emits about its own lifecycle, this
  one governs a pack that carries arbitrary caller events),
  [ADR-103](ADR-103-resource-attribution-model.md) (`brain.event_counts` and its cost fields; the read-time rollup this pack generalizes to an arbitrary stream),
  [ADR-183](ADR-183-batched-write-disposition.md) (a partial write reports what stopped it, not what
  remained)

## Context

khive already owns the durable half of a telemetry system. `stream.append` is an event-first log with
a dense per-stream sequence, `stream.read` resumes a reader from a cursor, and `brain.event_counts`
demonstrates that windowed counts grouped by kind, actor and verb are computable at read time with no
second store. What khive does not have is the part every consumer rebuilds: a declared assignment of
event kinds to carriers, and the same rollup over an arbitrary stream rather than over one pack's
private plane.

The assignment is the piece that is a design decision rather than a port. Whether a streaming delta
is worth durable storage depends on the deployment, so an emitter that decides it has hard-coded one
deployment's answer into every call site. Moving the decision into operator configuration is the
whole point of the pack; the verbs exist to make that configuration readable, auditable and visible
in the emit result.

This record settles three questions that the implementation cannot settle for itself, because each
one is a contract with future readers rather than a coding choice: whether khive owns an ephemeral
broadcast window, what an unclassified event kind does, and what an emit reports when the durable
append fails with an outcome nobody can determine.

## Decision

### D1: Scope — four verbs, one config table, no new store

Crate `khive-pack-telemetry`, `NAME = "telemetry"`, `REQUIRES = ["kg"]`, opt-in and **not** in the
default pack set, loaded with `KHIVE_PACKS=kg,telemetry` or `--pack telemetry`. It contributes four
verbs and nothing else: no entity kind, no note kind, no edge relation, no new backend, no new
external dependency, no background service.

```
telemetry.channels()                                    -> the effective table, as resolved from config
telemetry.emit(kind, payload, run_id?, actor?)          -> the emit outcome (D4)
telemetry.read(stream, since?, limit?, kinds?)          -> {events, next_cursor, coverage}
telemetry.counts(stream, window, group_by?, kinds?)     -> {rows, window, total}
```

`telemetry.emit` is `Assertive` and writes. `channels`, `read` and `counts` are reads. None of the
four is admission degrade-safe: `emit` writes, and the three reads answer questions whose wrong
answer is a false statement about what was recorded, which is worse than a refusal.

### D2: khive owns a drop, not a broadcast window

The originating ask named this as the fork to rule on first. **khive does not own an ephemeral
broadcast bus.** `carrier = "ephemeral"` means the daemon accepts the event and drops it, not that
the daemon holds a window of it for late subscribers.

A ring is naturally per-process. A cross-process ring is a publish-subscribe bus with its own
liveness, backpressure and delivery semantics, and adopting one would make the daemon a message
broker in addition to a store. The weaker contract is still useful and it is honest, which is the
property that matters: an operator who classifies a kind as ephemeral has said "record nothing", and
a reader is told which kinds that covers (D5) rather than handed a short list that reads complete.

This is reversible. Adding a held window later strengthens the guarantee without changing any
caller's code, because no caller may depend today on an ephemeral event being readable at all.

### D3: An unclassified kind takes a default the operator declared, and the emit says so

Three options were available: default an unclassified kind to durable, default it to ephemeral, or
refuse it. Each silent option hides a classification bug in a different direction. Defaulting to
durable pays storage quietly for a kind nobody meant to store. Defaulting to ephemeral discards data
silently, which is the failure that biases toward looking healthy. Refusing turns adding an event
kind into a config change and breaks an emitting task at runtime for what is a documentation gap.

The ruling takes none of those as a built-in. **`telemetry.default_carrier` has no compiled default
and the configuration fails to load when the table omits it**, naming the key. The operator states
the deployment's answer once, deliberately, and khive's contribution is to refuse to guess.

The cost of a wrong guess is then bounded by making the fallback visible at the call site rather
than only in the config: every `telemetry.emit` result carries **`classified`**, true when the kind
matched a `[[telemetry.channels]]` entry and false when it fell to the declared default. An emitter
that has quietly started producing an unclassified kind can see it in its own result without reading
the operator's file. A fallback that cannot be observed from the path that uses it is a fallback
nobody audits.

`telemetry.channels()` returns the effective table including the declared default and the resolved
kind patterns, because a table that cannot be read back is a table nobody can debug.

### D4: The emit outcome is one required enum, and ambiguity has its own value

This is the question the implementation asked and it is the one worth the most care. `stream.append`
can fail in a way that leaves the domain effect undetermined: the caller's admission deadline can
elapse while the row is still enqueued, so the append may yet commit. A `durable` channel with
`failure_posture = "gap"` therefore cannot truthfully report that the event was dropped.

The proposal put to this record was `dropped: null` with `domain_disposition: "unknown"`. **A
nullable `dropped` is rejected**, and the reason generalizes past this field. A field whose empty
value carries meaning is the field nobody re-derives, and the test for such a field is to ask what a
_broken_ producer writes there. A code path that forgets to set `dropped` writes exactly the same
null as a path that tried and genuinely cannot say. The value cannot carry the distinction it was
chosen to carry.

So the outcome is a single **required** field with three values, and no field's absence means
anything:

```
telemetry.emit(...) -> { carrier, classified, outcome, seq?, error?, receipt_id }

outcome = "recorded"   the event is in the log; seq is present and is its dense per-stream sequence
        | "dropped"    the event was deliberately not stored; no domain effect occurred
        | "unknown"    an append was attempted and its domain effect is undetermined
```

- `seq` is present **if and only if** `outcome == "recorded"` and `carrier == "durable"`.
- `error` is present **if and only if** the result was not the configured behaviour: always for
  `outcome == "unknown"`, and for a `dropped` on a durable channel, which is a refusal and has a
  reason. It is the original structured error, unmodified. The failure path writes it; it is never
  reconstructed by the caller of the failure path, and it is never flattened to a string.
- An ephemeral channel returns `outcome == "dropped"` with no `error`. That is the configured
  behaviour, not a failure, and conflating it with a failed durable append would make the one field
  that readers act on unable to separate policy from incident.
- **`carrier` is the discriminator between policy and incident, and `outcome` alone is not.** A
  durable append refused before any domain effect also returns `dropped`, so the two are told apart
  by the pair: `carrier == "ephemeral"` with `dropped` is the operator's classification working as
  configured, and `carrier == "durable"` with `dropped` is an incident. A consumer alerting on "my
  durable telemetry is being dropped" keys on the pair, never on `outcome` by itself. Arm 13 is what
  makes that pair observable rather than merely true.
- A durable `dropped` additionally carries its refusal reason in `error`; an ephemeral `dropped`
  carries none, because a configured drop has no reason to report. So `error` is present exactly
  when the result was not the configured behaviour, which gives the same separation a second,
  independent reading. (Stated as a delta on the rule above rather than an exception to it: the
  invariant is that `error` is absent only for `recorded` and for a configured ephemeral drop.)
- `outcome == "unknown"` is **never** reported as `dropped`, under any posture. A false "dropped"
  tells a reader the event definitely is not in the log, which is the one thing not known.

`failure_posture` selects what happens to the error, not what the outcome is called:

- `stop` — the original error propagates out of the emitting task unchanged. The verb does not
  return a result.
- `gap` — the verb returns, with `outcome` set to `dropped` or `unknown` as the facts allow, and the
  error preserved in `error` when the outcome is `unknown`.

**Neither posture retries.** A retry of an append whose effect is undetermined is a second write
with the first one's disposition still open, which is how one ambiguous event becomes two recorded
ones.

### D5: Read coverage replaces the ring's `gap`

With D2 removing the ring, the read side's `gap` flag loses the meaning it was given in the original
proposal. There is no window to fall out of, so a `gap: true` would be read by every consumer as "you
resumed too late" when the only thing it could mean is "some kinds are never stored". Keeping the
name and changing the meaning is worse than changing the name.

`telemetry.read` returns **`coverage`** instead: the list of requested kinds whose configured carrier
is `ephemeral` and which therefore have no stored rows, alongside the durable kinds the result covers
in full. An empty `coverage.ephemeral` list means the result is complete for everything asked for.
The reader learns the shape of what it cannot see, which is the property the original `gap` flag was
reaching for.

`next_cursor` follows `stream.read`'s existing cursor contract unchanged.

### D6: `telemetry.counts` is computed at read time

No rollup table, no incremental maintenance, no second store. `brain.event_counts` already proves the
shape over one plane; this generalizes it to a named stream with the same window and grouping
arguments. The scan is bounded and its initial head is pinned at the start of the read, so a stream
being appended to during the count does not produce a total that no point in time ever held.

An incremental rollup is an optimization to reach for when a measurement says read-time aggregation
is too slow. Building it first would add a second source of truth for a number, and a disagreement
between a rollup and the log it summarizes is resolved by re-deriving from the log anyway.

### D7: Attribution is stamped, and reads scope to the caller

`telemetry.emit` takes an optional `actor`. **The resolved acting identity is what gets stamped, and
a `actor` argument naming anything else is ignored rather than refused.** Refusing would turn an
attribution disagreement into a runtime failure inside the emitting task, which is the argument D3
already makes against refusing an unclassified kind, and consistency inside one record is worth more
than either option's marginal merit.

Ignoring it silently would be worse than refusing, so the override is made visible the same way D3
makes its fallback visible: the result carries the `actor` that was stamped, and
`actor_argument_ignored: true` when the call named a different one. An emitter that believes it is
attributing to someone else learns otherwise from its own result.

This matters beyond tidiness. `telemetry.counts` groups by actor, so without a stamping rule a
per-actor utilization figure is a number the measured party wrote about itself.

Reads scope to the caller. `telemetry.read` and `telemetry.counts` return only the calling actor's
events by default. A read naming a foreign actor requires that actor to be visible to the caller,
and an all-actors read requires the caller to be in the configured fleet-readers allowlist. This is
deliberately the same model `brain.event_counts` already enforces rather than a second one invented
here; a stream name is not an authorization, so naming an arbitrary stream grants nothing on its own.

## Acceptance, stated before implementation

Every arm names its control in the same test, because an absence without a same-call positive control
is not a finding.

1. A kind listed in a `[[telemetry.channels]]` entry emits with `classified: true`; a kind listed in
   no entry emits with `classified: false` and the declared default's carrier. Both in one test.
2. A configuration omitting `telemetry.default_carrier` fails to load, naming the key. A configuration
   declaring it loads, as the control in the same test.
3. A durable channel's successful emit returns `outcome: "recorded"` with a `seq`, and `stream.read`
   returns that event at that sequence.
4. An ephemeral channel's emit returns `outcome: "dropped"`, carries no `error` and no `seq`, and the
   stream's length does not change, with a durable emit in the same test moving it by one.
5. **The rule-separating pair.** An append refused _before_ any domain effect returns
   `outcome: "dropped"`. An append that fails with its effect undetermined returns
   `outcome: "unknown"` with the original error preserved. The two fixtures must differ in the
   returned value, or the fixtures are not separating the rules.
6. Under `failure_posture: "stop"`, the original error propagates out unchanged and no result is
   returned; under `gap` for the same injected failure, a result is returned carrying that same error
   in `error`. One injected failure, two postures, one test.
7. No posture retries: in every injected-failure fixture the append-attempt counter is exactly one.
8. `outcome: "unknown"` never appears with `seq`, `outcome: "recorded"` never appears with `error`,
   and an ephemeral `dropped` never appears with `error` while a durable `dropped` always does.
   Asserted structurally over every arm above rather than by inspection.
9. `telemetry.channels()` returns the declared default and every configured entry, and a round trip
   through it reproduces the classification decision that `emit` made for both a classified and an
   unclassified kind.
10. `telemetry.read` over a mixed request returns `coverage.ephemeral` naming exactly the requested
    ephemeral kinds, and an all-durable request returns an empty `coverage.ephemeral` as the control.
11. `telemetry.counts` over a stream being appended to concurrently returns a total consistent with a
    single pinned head: the same window counted twice across an append returns the pinned total, and
    a re-read after the pin is released reflects the new row.
12. The pack is absent from a default-pack boot and present under `--pack telemetry`, verified by the
    verb list in both, not by a config read.
13. **The policy-versus-incident pair.** An ephemeral drop and a durable append refused before any
    domain effect are emitted in one test. Both return `outcome: "dropped"`; the results must differ
    in `carrier`, and the durable one must carry an `error` while the ephemeral one must not. This is
    the control arm 4 otherwise lacks: without it, `dropped` is asserted but never shown to be
    separable.
14. An emit naming an `actor` other than the resolved acting identity stamps the resolved one and
    returns `actor_argument_ignored: true`; an emit naming the resolved identity, and an emit naming
    none, both stamp the same value with the flag absent. All three in one test, and
    `telemetry.counts` grouped by actor attributes all three to the resolved identity.
15. `telemetry.read` and `telemetry.counts` return only the caller's events when no actor is named,
    proved with a second actor's events present in the same stream as the control. A read that
    returned everything and a read that returned nothing must both fail this arm.
16. A read naming a foreign actor the caller cannot see refuses; the same read for a visible actor
    succeeds in the same test. An all-actors read refuses for a caller outside the fleet-readers
    allowlist and succeeds for one inside it, also paired in one test.

## Rationale

The three rulings share one shape: each picks the option that makes a mistake **visible** over the
option that makes it cheap.

D3 refuses to compile in a default because either silent default hides the same bug, and it pairs the
refusal with `classified` so the surviving fallback is observable from the call site that uses it.
D4 refuses a nullable field because null is what a broken producer writes, and an ambiguous outcome
deserves a name rather than an absence. D5 renames a flag rather than reusing a familiar one whose
meaning changed, because a familiar name is read at its old meaning by everyone who does not read
this record.

D2 goes the other way deliberately: it takes the weaker guarantee, because the strong one requires
khive to become a message broker, and a weaker contract that is honestly described costs less than a
stronger one that is operationally wrong.

## Alternatives Considered

**A ring in the daemon (D2).** Rejected for scope, not for value. It makes the daemon a broadcast
bus with delivery and backpressure semantics it does not otherwise have. The drop contract is a
strict subset, so adding the window later breaks nothing.

**Refusing an unclassified kind (D3).** Rejected because it converts a documentation gap into a
runtime failure in the emitting task, and under `failure_posture: "stop"` that failure is loud in
production for a new event kind that nobody deliberately excluded. The declared-default rule gets the
deliberateness without the outage.

**Keeping `dropped` as a nullable field (D4).** Rejected above. A narrower variant, keeping `dropped`
as a non-null boolean beside `outcome`, was also rejected: two fields that must agree will eventually
disagree, and a reader cannot tell which one is authoritative from the payload.

**An incremental rollup table (D6).** Deferred, not rejected. It is the right optimization once a
measurement demands it, and premature adoption creates a second source of truth for a number.

## Consequences

An operator must declare `telemetry.default_carrier` before the pack loads at all, which is a
deliberate one-line cost paid once per deployment.

A consumer porting from a runtime that had a ring loses late-subscriber replay for ephemeral kinds.
That loss is explicit in `coverage` rather than silent, and the port's own classification table is
the place to promote a kind to durable if the replay mattered.

`outcome: "unknown"` is a state consumers must handle. That is the point: it exists because the
underlying operation genuinely has that outcome, and a contract that omitted it would be forcing
every consumer to guess the same thing wrongly in private.

## Open Questions

1. Whether `telemetry.counts` should accept a cursor rather than only a window, so a consumer can
   count exactly the range it has read. Deferred until a consumer asks.
2. Whether the channel table should support per-run overrides. Not in this record; the table is
   deployment configuration and a per-run override is an emitter decision wearing a config costume.

## Implementation

Source-ready ahead of this record, gated on it: branch `codex/telemetry-pack`, implementation commit
followed by a census correction that adds the four verbs to the runtime's exhaustive admission-degrade
classification and asserts the retry-policy distinction between reads and emit. Native gates run
against the pinned toolchain from `crates/`, with `--no-fail-fast`.

Nothing merges before this record is accepted.

## Amendment 1 (2026-09-11): configuration scope, coverage semantics, read reproducibility, disposition mapping

Four questions the record above left answerable two ways. Each is settled here rather than in the
implementation, because an implementation that answers them is making the decision.

**A1.1 — the configuration requirement is pack-scoped.** D3 says a configuration omitting
`telemetry.default_carrier` fails to load. Read globally that refuses a deployment which does not
load the telemetry pack at all, for a key belonging to a pack it does not run. That is wrong and it
is not what D3 decides. The requirement attaches to the pack's own configuration table: a
deployment whose loaded pack set excludes telemetry needs no telemetry table and no key, and loads
clean. A deployment that loads the pack and omits the key fails, naming the key. The absent-pack
case and the present-but-incomplete case are different states and only the second is an error.

**A1.2 — `coverage` carries two facts and labels which is which. This one amends D5 and arm 10
rather than clarifying them.** D5 defined `coverage` as the requested kinds whose configured carrier
is ephemeral _and which therefore have no stored rows_. The second half of that is an inference from
the current table to the contents of the past, and it is wrong whenever the table has changed: a kind
that was durable last week and is ephemeral today has stored rows, and reporting it as empty because
of policy is a fabrication with a plausible shape.

`coverage` therefore carries both facts, each labelled for what it is. The window the read actually
covered and where its visibility ended, which is a statement about this read. And separately, the
requested kinds whose carrier is ephemeral **in the configuration in force now**, which is a
statement about configuration and never a statement about the rows. A caller that wants to know what
happened to a particular past event reads the events that were recorded; no field here answers that
question and none of them should look like it does.

D5 and acceptance arm 10 are amended to this content. Arm 10 keeps its requirement that
`coverage.ephemeral` names exactly the requested kinds whose current carrier is ephemeral, and gains
the arm that separates the two readings: a kind whose carrier is ephemeral now but which has stored
rows from an earlier configuration must appear in `coverage.ephemeral` **and** have its stored rows
returned by the same read. A `coverage` implemented as "no rows because policy" cannot pass both
halves, which is what makes the pair worth running.

**A1.3 — the pinned head buys reproducibility, not atomicity.** D6 pins a head so that a count and a
subsequent read agree. It does not make the read a snapshot. A concurrent hard delete removes rows
beneath the pin, and no pin prevents that. The guarantee stated is therefore the one that holds: the
same head with the same filters returns the same rows unless those rows were deleted, and deletion is
outside the read's control. Nothing in this record may be read as promising a consistent view across
a concurrent delete.

**A1.4 — a refused dispatch is not a telemetry outcome.** The three-value `outcome` enum describes
what happened to an append that was attempted. A dispatch-level refusal is a different event and
propagates as an error result with no `outcome` at all, exactly as any other verb's refusal does. A
hole in the durable log maps into the grid: refused with no effect is `dropped` carrying its reason,
undetermined is `unknown` carrying the original structured error. That error is carried through as
the structured value it already is. Serializing it a second time inside this pack would produce a
second rendering of the same failure, and two renderings of one error is how a consumer ends up
matching on the wrong one.

**Acceptance arms added, each naming its control in the same test.**

17. A configuration with the telemetry pack absent from the loaded pack set and no telemetry table
    loads clean; the same binary with the pack loaded and the key omitted fails, naming the key.
    Both in one test. A check written globally fails the first half, which is what makes this arm
    separating rather than decorative.
18. A dispatch refused before the pack is reached returns an error result carrying no `outcome`
    field; an append refused inside the pack returns `outcome: "dropped"` with its reason. The two
    in one test, because the shapes are easy to conflate and only the pair shows they differ.
19. An `unknown` outcome's `error` is byte-identical to the structured error the append layer
    produced, asserted by equality against the source value rather than by matching a message.
20. **Arm 10's separating half, stated here because it amends arm 10 rather than adding to it.** A
    kind is emitted while its carrier is durable, the configuration is then changed to make that kind
    ephemeral, and one read over a window spanning both returns the stored rows AND lists the kind in
    `coverage.ephemeral`. A second kind, ephemeral throughout, appears in `coverage.ephemeral` with no
    rows, as the control in the same test. An implementation that treats `coverage.ephemeral` as
    "empty by policy" passes the second and fails the first.

## References

- Originating issue: telemetry pack, the channel table and the rollup (#2575)
- [ADR-174](ADR-174-ordered-streams-append.md) — the durable log this pack carries events on
- [ADR-103](ADR-103-resource-attribution-model.md) — `brain.event_counts`, the read-time rollup generalized here
- [ADR-183](ADR-183-batched-write-disposition.md) — the sibling ruling that a partial write is
  described by what stopped it
