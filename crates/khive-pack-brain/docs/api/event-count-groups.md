# Optional event-count cross

`brain.event_counts(group_by=["verb","actor"], since=..., until=...)` adds
`counts_by_verb_and_actor`, a nested `{verb: {actor: count}}` map. Only that
ordered pair is accepted. Omission or null emits no cross; a requested empty
window emits `{}`. Reversed, duplicate or other dimensions, unknown names,
wrong lengths and non-array values are invalid.

The outer key is the event verb. Inner actor keys follow `counts_by_actor`:
default caller scope coalesces its permitted historical aliases, while explicit
actor and aggregate reads preserve raw stored labels. Dots and colons remain
ordinary characters in separate keys; there is no delimiter encoding.

All existing authorization and filtering happens before grouping. A foreign
actor must be visible; all-actor access still requires the serving runtime's
fleet-reader allowlist and cannot be combined with an actor filter. Only the
selected namespace and half-open time window contribute.

When the existing event window is truncated, the response contains only
`counts_by_verb_and_actor_page_scoped`, not the normal cross key. The group map
uses the same fetched events as every marginal. No additional cell cap or
budget exists: limits count events, and occupied cells cannot outnumber events.
The cross can have more keys than a marginal without being independently
truncated. `window_event_total` is still counted independently of the row read.

Use `kind="audit"` for a dispatch-audit census. The kind filter applies before
the cap; the unfiltered audit/non-audit budget split is not needed for that
single-kind path. Use `exhaustive=true` when a sampled aggregate is insufficient;
it retains the existing safety limit and live-view consistency caveat.

```text
request(ops='brain.event_counts(since="2026-09-01T00:00:00Z", until="2026-09-02T00:00:00Z", kind="audit", group_by=["verb","actor"], exhaustive=true)')
```
