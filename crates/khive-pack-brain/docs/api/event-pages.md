# Event pages

`brain.event_page` reads stored events in ascending time and ID order, one bounded
page at a time. It complements `brain.event_counts` and the `brain.events` debug view.
The contract is proposed in ADR-005, ADR-022, ADR-032 Amendment 6 and ADR-133 Amendment 8;
their acceptance is required before the implementation merges.

```text
brain.event_page(since="2026-10-06T00:00:00Z", kind="feedback_explicit", limit=100)
```

Use the returned `next_after` as `after` with the same filters to continue. An omitted
`until` is frozen on the first page and reused on later pages. An explicit `until` is
exclusive; `since` is inclusive. `limit` may change between pages. When `has_more` is
false, `next_after` is null. `count` is the current page size, not a window total.

| Parameter            | Meaning                                                                         |
| -------------------- | ------------------------------------------------------------------------------- |
| `since`              | Required starting time, using the existing brain time parser                    |
| `until`              | Optional exclusive end time; defaults to server time on the first page          |
| `kind`, `kinds`      | One kind and/or a list, combined and deduplicated                               |
| `namespaces`         | Up to 16 names; defaults to the current namespace; `[]` selects none            |
| `exclude_namespaces` | Up to 32 names removed by the storage predicate                                 |
| `actor`              | Optional actor selector with the same authorization and aliases as event counts |
| `all_actors`         | Requires the serving runtime's fleet-reader authorization; excludes `actor`     |
| `limit`              | Integer 1–1000; default 100                                                     |
| `after`              | Returned opaque cursor, at most 512 bytes                                       |

Explicit namespaces intersect current visibility. Neither `all_actors` nor a cursor
grants access to another namespace. An invisible valid namespace contributes no rows
and does not disclose whether it exists. Continuations recheck current authority;
changes to filters, identity or authorized scope can invalidate a cursor.

Each event includes its canonical `id`, kind, actor, namespace, verb, outcome, original
payload, metadata and microsecond RFC3339 `created_at`. The event's `id` is distinct
from any ID inside its payload. Current and historical GTD event-plane audit rows
lack task ID and prior/new task status. These fields remain absent; the reader does
not infer them from the current task or join lifecycle records. Future typed GTD
success-audit enrichment is separate work, not a prerequisite for this reader.

The response declares `consistency: "live_ordered_window"`. Pages are independent
live reads, not a snapshot. They order by stored microsecond time and physical ID
text. An event inserted later with a greater key inside the frozen window can appear
after the cursor. A backdated event or an equal-time event with a lower ID can fall
before it and remain unseen. Re-reading the window is necessary if that distinction
matters. A cursor is an unsigned position/filter binding, not an authorization token.
If stored rows have an identical time and physical ID across the selected sources,
the page refuses the ambiguous position instead of silently skipping one. The read
does not change or deduplicate those rows.

Filters apply before the page limit, and a peek row determines whether more results
exist. The implementation does not count the whole window or use offset pagination.
Each leaf read has a 1 MiB cumulative raw-text budget before event decoding; the final
JSON response has a 4 MiB cap. Budget errors refuse the page explicitly rather than
drop payloads or report a false end. These are materialization limits, not measured
process-memory or query-time guarantees. Older daemons that lack the new event-page
operation refuse it explicitly; clients must use a compatible serving runtime.
