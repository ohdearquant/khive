# Task timestamp evidence census

`gtd.census()` counts live task rows on the runtime backend bound to the GTD pack.
It uses the same visible namespace scope as task queries; an explicit
`namespace` selects that namespace. Terminal and unknown lifecycle statuses are
included in these counts without changing their status or their default task-list
visibility. No task IDs or payloads are returned.

The response has `schema_version: 1`, `scope`, `total_tasks`, two histograms
(`created_at` and `archived_at`), `created_at_gt_archived_at_raw`, and an
`interpretation` sentence. Both histograms include every bucket, including zeros,
and each sums to `total_tasks`.

| Bucket                | Meaning                                                                                    |
| --------------------- | ------------------------------------------------------------------------------------------ |
| `null`                | SQL NULL, JSON null, or absent `properties.archived_at`                                    |
| `nonnumeric`          | Other nonnumeric types, including numeric strings and JSON booleans                        |
| `epoch_zero`          | Numeric zero                                                                               |
| `magnitude_10_digits` | Absolute numeric magnitude at least 1,000,000,000 and below 10,000,000,000                 |
| `magnitude_13_digits` | Absolute numeric magnitude at least 1,000,000,000,000 and below 10,000,000,000,000         |
| `magnitude_16_digits` | Absolute numeric magnitude at least 1,000,000,000,000,000 and below 10,000,000,000,000,000 |
| `other`               | All remaining numeric values                                                               |

The same ranges apply to integers, real values, and negative values by magnitude.
The 10/13/16-digit magnitudes can help investigate common epoch representations;
they do not prove seconds, milliseconds, or microseconds. Current notes storage
requires a non-null `created_at`; the shared classifier still reports its null
bucket explicitly.

`created_at_gt_archived_at_raw` compares only pairs of numeric values. It is
**NOT temporal ordering**: the two numbers may use different or unknown units.
No unit is inferred, chosen, echoed, or accepted as a parameter. There is no
repair option and no creation-date reconstruction from an archival date.

The query returns at most 15 aggregate rows to the handler but can scan the
whole matching population. Backend errors are returned, never disguised as an
empty census. The handler uses only a SQL reader; normal dispatch audit effects
are unchanged.

The status portion of #2394 was already covered by ADR-019 Amendment 2. This
census supplies evidence for the outstanding timestamp question and does not
claim that historical data has been repaired.
