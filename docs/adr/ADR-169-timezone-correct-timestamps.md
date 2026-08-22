# ADR-169: Timezone-correct timestamps: date-only values and a configured display timezone

**Status**: proposed **Date**: 2026-08-22 **Authors**: OceanLi

## Context

Every timestamp khive returns is rendered in UTC, and there is no way to configure that.
For a caller in any zone west of UTC this is not only inconvenient: it silently changes what
a date-only value means.

khive holds timestamps in two representations, and they do not share a rendering path.

**Representation A — integer microseconds in entity and note columns.** These are converted
by `micros_to_iso` (`crates/khive-runtime/src/presentation.rs:328`), which hard-codes UTC.
Its doc comment describes it as "the single conversion point before any field reaches the MCP
boundary."

**Representation B — RFC 3339 strings stored inside note properties.** `due`, `completed_at`,
`sent_at`, `delivered_at`, `cancelled_at`, and `last_seen_at` are each written into `properties`
as an already-formatted string and echoed back to the caller verbatim. They never pass through
`micros_to_iso`. Their producing call sites are enumerated in the Mechanism section below, and
they do not all live in the pack whose field the name suggests.

So the doc comment quoted above is not an accurate description of the system, and the
inaccuracy is load-bearing: it makes the rendering surface look like one function when it is
in fact one function plus every site that writes a timestamp string into `properties`.

A single `gtd.tasks` response row demonstrates both paths at once. It returns `created_at`
as `2026-08-21T20:01` — presentation-layer output, minute granularity — and `due` as
`2026-08-19T00:00:00+00:00` — the stored string, returned unchanged. Two formats in one row.

The consequence shows up most sharply on date-only input. `parse_due`
(`crates/khive-pack-gtd/src/handlers.rs:499-515`) accepts `YYYY-MM-DD` and promotes it with
`NaiveDate::parse_from_str` followed by `DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc)`,
so the date is anchored to midnight **UTC**. `gtd.assign(due="2026-08-23")` stores
`2026-08-23T00:00:00+00:00`. For a caller at UTC-4 that instant is 20:00 on 2026-08-22: the
date the caller typed is not the date the system now holds, and every subsequent read of that
field is off by one evening.

This is a write-path defect, which is the reason it matters for the ordering of any fix. A
display-side timezone conversion applied on its own would take the stored instant and render
it in local time, producing `2026-08-22 20:00` — the previous evening, displayed even more
explicitly. It would make the observable symptom worse while appearing to address it.

The same class of misreading is not limited to humans. A date-prefix comparison against a
stored UTC timestamp selects a different day than the caller intended, which makes this a
correctness concern in query and filter paths rather than a presentation preference.

## Decision

**D1. A date-only value is anchored to the configured display timezone, not to UTC.**
`parse_due` and every other parser that accepts a bare `YYYY-MM-DD` resolves it to **the earliest
instant that belongs to that calendar date in the configured zone**. The stored value remains a
single unambiguous instant; only the zone used to anchor it changes. A value that already carries
an explicit offset or `Z` is unaffected and continues to be normalized as it is today.

"The earliest instant of that date" rather than "midnight" because midnight is not a total
function of date and zone, and the cases where it fails are exactly the cases a date-anchoring
rule has to survive. Some zones transition at 00:00, so on a transition date local midnight can
fail to exist or can occur twice:

- **Midnight does not exist** (the clock jumps from 23:59:59 to 01:00:00). The anchor is the first
  instant that exists on that date, which is the instant immediately after the gap.
- **Midnight occurs twice** (the clock repeats the hour). The anchor is the earlier of the two.

Both cases are the same rule, not two exceptions to one: take the least instant whose local date
is the requested date. An implementation on `chrono` reads this directly off
`from_local_datetime`, whose `LocalResult` is `None` in the first case and `Ambiguous` in the
second; neither may be resolved by unwrapping to UTC, which would silently reintroduce the defect
this ADR exists to remove.

These are not hypothetical, and the distinction is easy to test against the wrong zone. Measured
against the IANA database:

| zone               | date       | local midnight                            |
| ------------------ | ---------- | ----------------------------------------- |
| `America/Havana`   | 2021-03-14 | does not exist                            |
| `America/Santiago` | 2021-09-05 | does not exist                            |
| `America/Havana`   | 2020-11-01 | occurs twice (offsets -04:00 then -05:00) |
| `America/New_York` | any date   | always exists                             |

A conformance test must therefore use a zone that actually transitions at 00:00. `America/New_York`
observes DST and still cannot reach either case, so a test written against it passes without
exercising the rule at all — which is the failure mode this table exists to prevent.

**D2. A `[display]` configuration section sets the timezone used for rendering.**

```toml
[display]
timezone = "America/New_York"   # IANA name; default: the host's local zone
```

The value is an IANA zone name so that DST is resolved by the zone database rather than by a
fixed offset. The default is the host's local zone. Storage semantics are unchanged: an
instant is an instant, and this setting selects how it is spelled to a caller.

**D3. The rendering surface is stated explicitly and the inaccurate doc comment is corrected.**
The ADR names both representations above as the surface a display setting must reach. The
`micros_to_iso` doc comment is corrected to describe what it actually covers, because a
comment that overstates a function's reach is what allows a future change to be wired at one
seam and believed complete.

**D4. Existing stored values are not rewritten.** A stored timestamp is data; a zone setting is
a view concern. Nothing in this ADR mutates rows that already exist. This is a forward-only
change, and the window of values written under the previous anchoring stays as it was written.

**D5. The anchored value is stored in offset spelling, and this is not timezone-dependent
storage.** A date-only `2026-08-23` written by a caller anchored at UTC-4 is stored as
`2026-08-23T00:00:00-04:00` rather than `2026-08-23T04:00:00Z`.

The distinction matters and is easy to misread, so it is stated directly. **The stored value is
still an absolute instant.** RFC 3339 with a numeric offset denotes exactly one point on the
timeline; it is machine-comparable against any other RFC 3339 value, sorts correctly, and
converts to UTC without loss. The offset is not a statement that storage now depends on a
timezone setting. It is a record of the zone in which the date was anchored at parse time, and
that information is genuinely part of what the caller expressed: a bare date is a day in some
calendar, and which calendar was used is a fact about the value, not a rendering preference.

The invariant this preserves is the one that matters: **display configuration never changes a
stored value.** Changing `[display] timezone` after the fact re-renders existing timestamps and
alters no byte of them. What D5 changes is the anchoring performed at write time on input that
did not specify an instant at all.

**Relationship to the originating directive.** The requirement this ADR implements was stated as
"storage stays UTC". D5 preserves that requirement in the sense that carries the weight: a stored
timestamp is still one absolute point on the timeline, still directly comparable and sortable
against every other stored timestamp, and still convertible to UTC without loss. What D5 declines
to preserve is the narrower reading in which the requirement governs the SPELLING of the stored
string, so that `2026-08-23T00:00:00-04:00` would be disallowed in favour of
`2026-08-23T04:00:00Z`. Those two denote the same instant. The offset-versus-`Z` fork was put
directly and ruled on 2026-08-22 in favour of offset spelling, on the grounds that a bare date
carries a calendar and which calendar was used is part of what the caller expressed. This
paragraph exists so that a reader holding both the directive and this ADR does not have to
reconcile them by inference.

The practical consequence is that a stored value echoed back verbatim already reads as the date
the caller wrote, so D1 closes the observed defect on its own, without waiting for D2.

## Rationale

D1 before D2 is the whole point of the ordering. The defect the caller actually observes is
produced at the write path, so fixing rendering first does not fix it and does not leave the
system in a better state on the way there.

Anchoring a date to the display zone is the interpretation that matches what a date-only value
means when someone types it. `2026-08-23` is a day in the writer's calendar, not an instant in
Greenwich, and the current code chooses the second reading without saying so.

D3 exists because this ADR was written after reading a doc comment that claimed a property the
code does not have. That claim is exactly what a later implementer would rely on to conclude
that wiring one function is sufficient.

## Alternatives Considered

| Alternative                                                           | Pros                                                   | Cons                                                                                                                                                           | Why rejected                                                                                 |
| --------------------------------------------------------------------- | ------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------------------- |
| Display-side conversion only, leaving the write path unchanged        | Single seam; no change to stored values                | Renders the already-misanchored instant in local time, so the reported symptom gets worse rather than better; date-only input keeps losing a day at write time | Does not address the defect, and degrades the observable behaviour on the way past it        |
| Store date-only values as a distinct date type rather than an instant | Most faithful to the input; no anchoring choice needed | Requires a schema and wire-shape change across every consumer of the field, and a migration story for existing rows                                            | Correct in the long run but far larger than the defect requires; may be revisited separately |
| Fixed UTC offset in config instead of an IANA name                    | Trivial to implement; no zone database dependency      | Wrong across DST boundaries twice a year, silently, and the failure mode is again an off-by-one on dates                                                       | Reintroduces the same class of error the ADR exists to remove                                |
| Per-call timezone parameter only, with no global setting              | No global state; explicit at each call site            | Every caller must remember it on every call; the default stays wrong, so the common path stays broken                                                          | Useful as an addition, insufficient as the mechanism                                         |

## Consequences

### Positive

- A date-only value round-trips as the date that was written.
- Rendered timestamps can be read directly without a mental UTC conversion, which removes a
  documented source of date-comparison errors in filter and query paths.
- The rendering surface is written down, so a future change can be checked for reach rather
  than assumed complete.

### Negative

- Resolving IANA zone names requires a zone database in the binary, which is a new dependency
  and a size cost that should be measured before this lands.
- The anchoring change is observable: two callers in different zones writing the same
  date-only string produce different instants. That is the intended reading, but it is a
  behaviour change and must be stated in release notes.
- Values written before this change keep their previous anchoring, so a store will contain
  both. D4 accepts this rather than rewriting data to make a view look uniform.

### Neutral

- Storage remains instant-based; nothing here changes the stored type or the column layout.
- Callers that pass an explicit offset or `Z` see no change at all.

## Mechanism: the rendering surface is two paths, not one

This section exists for whoever implements D2. The display setting has a reach problem, and the
codebase currently documents the opposite, so an implementation that trusts the documentation
will ship believing it is complete.

`micros_to_iso` (`crates/khive-runtime/src/presentation.rs:328`) describes itself as "the single
conversion point before any field reaches the MCP boundary." It is not. It covers exactly one of
two paths.

**Path 1 — integer microseconds in entity and note columns.** These reach a caller through
`micros_to_iso`, which is a single seam and can be made zone-aware in one place.

**Path 2 — RFC 3339 strings already formatted and stored inside note `properties`.** These are
written as strings at their producing site and returned to the caller unchanged. They never enter
`micros_to_iso`. Each field below was located at source rather than inferred from the pack it
conceptually belongs to, because the two do not always agree:

| Field          | Written at                                                                                                               |
| -------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `due`          | `crates/khive-pack-gtd/src/task_create.rs:383` (via `parse_due`, defined in `crates/khive-pack-gtd/src/handlers.rs:499`) |
| `completed_at` | `crates/khive-pack-gtd/src/handlers.rs:845,946`                                                                          |
| `sent_at`      | `crates/khive-pack-comm/src/handlers.rs:337,1413`                                                                        |
| `delivered_at` | `crates/khive-mcp/src/serve.rs:1562,1872`                                                                                |
| `cancelled_at` | `crates/khive-pack-schedule/src/handlers.rs:1060`                                                                        |
| `last_seen_at` | `crates/khive-pack-code/src/source_ingest.rs:739,869`                                                                    |

Query results add a further producer: `SqlValue::Timestamp` is rendered at
`crates/khive-pack-kg/src/handlers/common.rs:862`.

Note that `sent_at` has two distinct producers with different roles: the comm handler sites above
write the stored value, while `crates/khive-mcp/src/serve.rs:994,1756` render a `sent_at` into a
response from an envelope. A field name is therefore not a reliable key for this work; the call
site is.

The observable proof is a single `gtd.tasks` response row, which returns `created_at` in
presentation-layer form and `due` as a raw stored string. Two formats in one row means two paths.

Consequences for D2:

- Wiring the zone setting into `micros_to_iso` alone changes nothing about any Path 2 field. A
  reviewer checking only that function will see a complete-looking change.
- Path 2 fields are not all the same kind of value. Some are stored facts and some are rendered
  ones, and the difference has to be decided per field rather than assumed uniform. That decision
  is listed as implementation step 4 and is not settled here.
- D5 is what keeps this from blocking the fix: because a date-only value is stored in offset
  spelling, the Path 2 echo is already correct for the case that motivated this ADR, so D2's reach
  problem does not have to be solved before the defect is closed.

## Implementation

1. `[display]` section on the runtime config (`crates/khive-runtime/src/engine_config.rs`,
   alongside the existing sections on `KhiveConfig`), with the documented example updated in
   `docs/khive-config-example.toml` and `docs/configuration.md`.
2. Resolve the configured zone once and make it reachable from the parse sites and from
   `presentation.rs`.
3. `parse_due` (`crates/khive-pack-gtd/src/handlers.rs:499-515`) anchors date-only input to the
   earliest instant of that date in the configured zone, per D1. Tests must cover a zone west of
   UTC, a zone east of UTC, and both midnight-transition cases from D1 — a date whose local
   midnight does not exist, and a date whose local midnight occurs twice — in a zone that
   actually transitions at 00:00 rather than any zone that merely observes DST. Tests must assert
   the resulting calendar date rather than a fixed offset string: a test hard-coding `-04:00`
   passes in summer and fails in winter, which is the same class of defect this work removes.
4. Audit the sites that write RFC 3339 strings into `properties` and decide, per field, whether
   the value is a stored fact or a rendered one. This audit is part of the work, not a
   precondition for it.
5. Correct the `micros_to_iso` doc comment (D3).

Sequencing: step 3 is the minimum that resolves the observed defect and can land before the
rest. Steps 1 and 2 are its prerequisites.

## References

- ADR-078: output format and shape-aware rendering.
- `crates/khive-runtime/src/presentation.rs` — `micros_to_iso`, `PresentationMode`.
- `crates/khive-pack-gtd/src/handlers.rs` — `parse_due`.
