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

Counts consume the original operator request-read budget
(`KHIVE_REQUEST_READ_TIMEOUT_SECS`, default 30 seconds, maximum 86400), retaining
any earlier caller deadline across legacy and events stores. Count RPCs are
bound by that original client deadline; append and non-count RPCs retain their
10-second transport timeout. No deadline or clock value crosses the count socket.
The daemon stops and settles the count when the caller's budget expires and the
connection closes, using the existing SQLite interrupt/join path. It does not
enforce an independent deadline or renew the allowance at receipt.

A read timeout remains a typed storage error naming the budget, with no partial
successful `window_event_total`. Admission failures retain their actual wait
duration and pool identity. Both remain subject to the existing retry classifier.
The existing hard-cap exception is unchanged: if a worker ignores interruption,
it can remain unrecovered and retain admission; a timeout alone does not prove
that its pool recovered. The events protocol is version 6, so clients and daemons
must be upgraded together; version skew is refused without replay or downgrade.

## Failed audit classes

Non-success `audit` events (`error` or `denied`) contribute to
`counts_by_error_class`, a map keyed by the exact known `payload.error_class`
token. The vocabulary is `policy_refusal`, `gate_refusal`, `invalid_params`,
`not_found_or_not_addressed`, `deadline_exceeded`, `admission_exhausted`,
`conflict`, `unsupported`, `configuration`, `storage`, `unavailable`, and
`internal`. The classification is informational; it does not change retry or
domain-disposition policy.

Historical rows without that field, malformed values, and unknown future tokens
contribute only to `unclassified_error_count`. They are not relabeled `internal`,
and their stored payloads are not changed. Successful audit events and all
non-audit events contribute to neither field, even if their payload contains an
`error_class` field.

Both fields are omitted when the fetched rows contain no failed audit event.
Otherwise the map may be empty and the unclassified count may be zero. If the
window is truncated, only `counts_by_error_class_page_scoped` and
`unclassified_error_count_page_scoped` are emitted. They cover the same fetched
rows and retain the same namespace, actor, kind, time, and request-budget scope
as the other aggregates; `window_event_total` is not a failed-audit total.
