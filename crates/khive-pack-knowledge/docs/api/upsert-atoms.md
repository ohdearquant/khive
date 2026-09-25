# knowledge.upsert_atoms

`knowledge.upsert_atoms(atoms, chunk_size?, dry_run?)` accepts 1–5000 atom
writes. A content item carries `slug`, `name`, `content` and optional atom fields;
a properties-only item carries exactly a complete UUID `id` and `properties`.
`chunk_size` remains an accepted hint; the server does not split the batch.

With `dry_run=true`, all items are checked and the response contains:

```json
{
  "dry_run": true,
  "would_refuse_batch": false,
  "results": [{
    "index": 0,
    "slug": "retrieval-basics",
    "identity_masked": false,
    "would_refuse": false,
    "reason": null,
    "detector": null,
    "trigger": null,
    "masked": null,
    "location": null,
    "message": null
  }]
}
```

A properties-only verdict has `id` instead of `slug`. Slugs use the write path's
trimmed spelling and UUIDs use its canonical spelling. No ID is generated for a
new slug. If a slug contains a detected secret, the shared GateProbe masker
replaces the secret and `identity_masked` is true. Raw secret text is not returned.

Every refused item sets `would_refuse=true`. `reason` is `secret_detected`,
`invalid_input`, or `not_found`. A secret refusal carries the same detector,
canonical trigger, masked excerpt, atom-field location, and message as the secret
probe; validation refusals carry the write path's refusal class and message.
Messages and locations also use the shared masker. Allowed items have null
refusal details. `would_refuse_batch` is true if any item refuses.

Dry run performs every input and secret check and the read-only target checks:
properties-only IDs must identify live ordinary atoms, while slug writes must
not collide with a domain mirror or tombstone. It acquires no atom writer and
writes no atom row, index, or refusal event. Host dispatch auditing remains separate
and may still record the call. Reader/storage failures remain errors rather than
speculative item verdicts. Concurrent changes that only a later writer can decide
are outside the prediction.

Malformed parameter objects, unknown fields, non-boolean `dry_run`, empty batches,
and batches over 5000 items still return errors. The default is `dry_run=false`;
false and omission retain normal atomic writes and refusal-event recording.
Both modes retain the verb's Write classification and permission requirements.
