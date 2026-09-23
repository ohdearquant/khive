# Task timestamp evidence census

`gtd.census()` counts live task rows on the runtime backend bound to the GTD pack.
It uses the same visible namespace scope as task queries; an explicit
`namespace` selects that namespace. Terminal and unknown lifecycle statuses are
included without changing their status or their default task-list visibility.
The default response contains no task IDs or payloads.

The default response remains `schema_version: 1`, `scope`, `total_tasks`, two
histograms (`created_at` and `archived_at`), `created_at_gt_archived_at_raw`, and an
`interpretation` sentence. Both histograms include every bucket, including zeros,
and each sums to `total_tasks`. Passing `include_candidates=false` returns this
same aggregate response.

| Bucket | Meaning |
| --- | --- |
| `null` | SQL NULL, JSON null, or an absent JSON property |
| `nonnumeric` | Other nonnumeric types, including JSON numeric strings and booleans |
| `epoch_zero` | Numeric zero |
| `magnitude_10_digits` | Absolute numeric magnitude at least 1,000,000,000 and below 10,000,000,000 |
| `magnitude_13_digits` | Absolute numeric magnitude at least 1,000,000,000,000 and below 10,000,000,000,000 |
| `magnitude_16_digits` | Absolute numeric magnitude at least 1,000,000,000,000,000 and below 10,000,000,000,000,000 |
| `other` | All remaining numeric values |

The same ranges apply to integers, real values, and negative values by magnitude.
The 10/13/16-digit magnitudes can help investigate common epoch representations;
they do not prove seconds, milliseconds, or microseconds. Current notes storage
requires non-null `created_at` and `updated_at`; the classifier still represents
legacy nulls explicitly. SQLite column affinity can convert a numeric string
before storage; the census reports the resulting stored type.

`created_at_gt_archived_at_raw` compares only pairs of numeric values. It is
**NOT temporal ordering**: the two numbers may use different or unknown units.
No unit is inferred, chosen, echoed, or accepted as a parameter. There is no
repair option and no creation-date reconstruction from an archival date.

## Optional candidate rows

```text
gtd.census(include_candidates=true, limit=100)
gtd.census(include_candidates=true, limit=100, cursor={"namespace":"local","id":"00000000-0000-0000-0000-000000000001"})
```

`include_candidates` is a boolean. When true, the response adds `candidates` with
`rows`, `next_cursor`, and
`expected_buckets: {"created_at":"magnitude_16_digits","updated_at":"magnitude_16_digits"}`.
The aggregate response fields still describe the entire selected population,
independently of the candidate page.

A live task qualifies when either its stored `created_at` or `updated_at` falls
outside `magnitude_16_digits`. This expectation follows the current note writer's
microsecond storage convention; it does not establish a historical row's original
units or a correct replacement date. An archival value has no unit expectation
and cannot qualify a row by itself; absent and null archival properties are ordinary
evidence values. All three fields retain their own magnitude bucket.

`limit` defaults to 100 and must be an integer from 1 through 200. Candidate rows
are ordered by `(namespace, id)` ascending. `next_cursor` is null when there are
no further qualifying rows, or an object containing the last returned row's
`namespace` and full canonical lowercase dashed UUID `id`. Pass that object with
the same namespace scope to continue. Rows at or before the cursor are excluded.
The cursor namespace must belong to the current request scope; it is not authority
to widen that scope. A cursor need not name a row that still exists. Separate pages
are separate reads, not a frozen snapshot across concurrent changes.

Supplying `limit` or `cursor` requires `include_candidates=true`. Explicit null
values, malformed cursors, UUID prefixes or noncanonical UUID spellings, an
out-of-scope cursor namespace, and unknown options are rejected. Offset pagination
is not supported.

Each row has this shape:

```json
{
  "id": "00000000-0000-0000-0000-000000000001",
  "namespace": "local",
  "stored_status": "\"archived\"",
  "raw": {
    "created_at": "1772323200",
    "updated_at": "1772323200000000",
    "archived_at": "1772323200000"
  },
  "buckets": {
    "created_at": "magnitude_10_digits",
    "updated_at": "magnitude_16_digits",
    "archived_at": "magnitude_13_digits"
  }
}
```

`stored_status` and the three `raw` fields contain **JSON source text as strings**,
not parsed numeric values. For example, the stored status string `archived` is
reported as the text `"archived"`, including its JSON quotation marks. A numeric
archival value beyond the JSON library's integer range, or a long decimal, keeps
its source lexeme instead of being rounded. Core timestamp text values are JSON
quoted; their integers and finite real values are encoded from the actual SQLite
scalar. A core SQL NULL is the text `null`.

For JSON properties, absence is JSON null in the response, while an explicitly
stored JSON null is the string `"null"`. This distinguishes an absent
`properties.archived_at` or `properties.status` from a present null. The census
preserves raw status evidence without mapping unknown or legacy values to a GTD
lifecycle state. Unsupported core scalar types and nonfinite real values return
an error rather than fabricated or lossy evidence.

The aggregate query returns at most 15 rows to the handler but can scan the whole
matching population. The optional candidate query limits materialized rows to
`limit + 1` to determine continuation; that bound does not promise constant scan
work. Backend errors are returned, never disguised as an empty census. The handler
uses SQL readers; normal dispatch audit effects are unchanged.

The status portion of #2394 was already covered by ADR-019 Amendment 2. This
census supplies evidence for investigation and does not claim that historical
data has been repaired. Original row provenance and independently established
source timestamps are still required before any correction.
