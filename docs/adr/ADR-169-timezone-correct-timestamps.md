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

**Representation B — RFC 3339 strings stored inside note properties.** `due` (gtd),
`completed_at` (gtd), `sent_at` and `delivered_at` (comm), `cancelled_at` (schedule), and
`last_seen_at` (code) are each written into `properties` as an already-formatted string and
echoed back to the caller verbatim. They never pass through `micros_to_iso`.

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
`parse_due` and every other parser that accepts a bare `YYYY-MM-DD` resolves it to midnight in
the configured zone. The stored value remains a single unambiguous instant; only the zone used
to anchor it changes. A value that already carries an explicit offset or `Z` is unaffected and
continues to be normalized as it is today.

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

## Implementation

1. `[display]` section on the runtime config (`crates/khive-runtime/src/engine_config.rs`,
   alongside the existing sections on `KhiveConfig`), with the documented example updated in
   `docs/khive-config-example.toml` and `docs/configuration.md`.
2. Resolve the configured zone once and make it reachable from the parse sites and from
   `presentation.rs`.
3. `parse_due` (`crates/khive-pack-gtd/src/handlers.rs:499-515`) anchors date-only input to
   midnight in the configured zone. Tests must cover a zone west of UTC, a zone east of UTC,
   and a DST transition date, and must assert the resulting calendar date rather than a fixed
   offset string.
4. Audit the sites that write RFC 3339 strings into `properties` and decide, per field, whether
   the value is a stored fact or a rendered one. This audit is part of the work, not a
   precondition for it.
5. Correct the `micros_to_iso` doc comment (D3).

Sequencing: step 3 is the minimum that resolves the observed defect and can land before the
rest. Steps 1 and 2 are its prerequisites.

## Open question for review

D1 leaves the spelling of the stored string undecided, and the choice is visible to callers
because these values are echoed verbatim.

- **Offset spelling** — store `2026-08-23T00:00:00-04:00`. The verbatim echo already reads as
  the intended date, so the defect is resolved without any rendering work. Stored strings are
  then no longer uniformly `Z`.
- **`Z` spelling** — store `2026-08-23T04:00:00Z`. Stored strings stay uniform, but the
  verbatim echo still shows the wrong-looking day until rendering is in place, which means the
  observable fix waits on the larger half of the work.

Both are the same instant. This ADR does not decide between them.

## References

- ADR-078: output format and shape-aware rendering.
- `crates/khive-runtime/src/presentation.rs` — `micros_to_iso`, `PresentationMode`.
- `crates/khive-pack-gtd/src/handlers.rs` — `parse_due`.
