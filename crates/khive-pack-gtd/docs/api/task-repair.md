# Explicit historical task repair

`gtd.repair` previews caller-specified corrections to existing task rows. It never
infers timestamp units, chooses replacement dates, or repairs data implicitly on
read or during ordinary lifecycle writes. First inspect the source data, for example
with [`gtd.census(include_candidates=true)`](task-timestamp-census.md), then state
both the observed value and the replacement.

## Request

Pass `items` containing between 1 and 100 distinct, canonical full task UUIDs.
Each item supplies a nonempty `changes` object. For example, these are the verb's
JSON arguments:

```json
{
  "items": [{
    "id": "00000000-0000-0000-0000-000000000001",
    "changes": {
      "created_at": {"observed": "1770000", "value": 1770000000000000},
      "updated_at": {"observed": "1770000", "value": 1770000000000000},
      "status": {"observed": "\"archived\"", "value": "cancelled"}
    }
  }],
  "apply": false
}
```

This example proposes dates; it does not establish a conversion rule. Timestamp
replacements are signed 64-bit integers in the storage's microsecond representation.
Only `created_at`, `updated_at`, and `status` may be changed. Unrequested timestamps
stay unchanged, including `updated_at` during a status-only repair.

For timestamps, `observed` is the exact JSON source string returned under census
`raw.created_at` or `raw.updated_at`. For status it is the exact `stored_status`
value. An absent status member is represented by JSON `null`; an explicit null
member is the string `"null"`; a stored string includes its JSON quotes. Equivalent
values with different source spellings do not satisfy the observed-value check.

A status repair accepts only a stored string outside the canonical vocabulary,
and can set only `done` or `cancelled`. Missing, null and non-text stored status
values keep their existing `inbox` fallback and use `gtd.transition` or
`gtd.complete`; this repair path refuses to change their status. Canonical stored
statuses also remain the responsibility of lifecycle verbs, including already-terminal
rows. Timestamp repair is eligible regardless of stored status. There is no automatic
mapping from `archived` to either terminal state. `archived_at` is preserved, and
cannot be changed through this verb.

IDs resolve without a namespace filter, as with task lifecycle operations. Census
namespace scope controls discovery only; authorization remains at the Gate.
The Gate classifies this verb as a write even for a dry run, since `apply=true`
can mutate domain data.

## Preview, application and refusals

Omitting `apply` is equivalent to `apply=false`. A preview returns each row's stored
source values, proposed replacements and acceptance decision without changing the
row or writing a repair audit. Set `apply=true` to apply the same request; values
are checked again at application time. A preview does not reserve a row.

The response contains `apply`, aggregate `accepted`, `applied`, and `refused` counts,
and an input-ordered `results` array. Each result includes `id`, `accepted`,
`applied`, `stored`, `proposed`, and `reason`. `stored` maps requested fields to
source text; `proposed` maps them to replacement values. An accepted preview has
`accepted=true`, `applied=false`; a successful application has both true.
A refused item has a reason such as `stale_observed`, `not_found`, `not_task`,
`deleted`, `canonical_status`, `lifecycle_status`, `unsupported_field`, or
`invalid_target`. A refusal also includes a human-readable `message`. Invalid or
non-object property documents are refused as `invalid_properties`; SQL NULL
properties are treated as an empty object. Stored timestamp values without a
faithful JSON scalar representation are refused as `unsupported_stored_value`.

Malformed request shapes, duplicate IDs, and an out-of-bound batch are rejected
before any row is applied. Otherwise application is per row: an accepted item can
commit even if another item is refused. A refused row writes nothing. Each applied
row and its lifecycle-audit entry commit together; an audit failure rolls back that
row. No earlier successful row is undone by a later refusal. Unexpected storage
errors stop the call instead of claiming a definite per-row refusal; earlier
committed rows can remain applied. Re-read affected rows before retrying when
the error leaves the commit outcome uncertain.

Replay with an old observed value is stale once that field has changed. If another
writer changes the decision snapshot, the repair cannot replace that writer's
properties or resurrect its deleted row; it is refused as a concurrent change.

## Preserved evidence

Applied repairs retain evidence in `properties.gtd_repair`:

- `originals` maps each repaired field to `{value, at, actor}`. `value` is the
  exact previous source text (or null for an absent member), `at` is the repair
  time in microseconds, and `actor` identifies the acting caller.
- A later repair keeps the first original for that field. It can add a first
  original for another field without overwriting existing evidence.
- `last` records the latest `{at, actor, changes}` with explicit observed values
  and proposed replacements. This is separate from the row's `updated_at`.

The lifecycle-audit note identifies `operation="gtd.repair"` and the changed fields.
Malformed preserved originals or unknown repair-history keys are refused as
`invalid_repair_history` instead of overwritten. The previous `last` entry is
replaced by the new repair's entry; audit records retain earlier repairs.
This history is evidence, not a new lifecycle state or automatic migration policy.
