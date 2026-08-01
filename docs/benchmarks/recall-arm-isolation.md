# Isolating a recall measurement arm

`memory.recall` supports an exact-match `namespace` parameter (issue #733) that
scopes the candidate fetch — FTS, vector search, and the ANN over-fetch retry
loop — to exactly one namespace, instead of the caller's default visible set.
Combined with `memory.remember`'s existing `namespace` write override and
`memory.recall`'s `profile_id` serving-profile override (ADR-104 §4), this is
enough to run an isolated A/B measurement arm without touching production
data: write to a scratch namespace, read from that same namespace, and pin
the serving profile so scoring weights don't drift mid-measurement.

## Recipe

1. **Pick an arm namespace.** A valid namespace is one or more segments
   separated by a single `:`, each segment matching `[a-zA-Z0-9\-_.]+` (no
   spaces, no empty segments, no trailing `:`; `::` is rejected — it produces
   an empty segment between the two colons). Prefix it so it is obviously
   scratch data, e.g. `bench-arm-a`.

2. **Write the arm's corpus** with an explicit namespace override:

   ```text
   memory.remember(content="...", namespace="bench-arm-a")
   memory.remember(content="...", namespace="bench-arm-a")
   ```

   Every memory written this way lands in `bench-arm-a` regardless of the
   caller's actor or default namespace.

3. **(Optional) create and pin a serving profile** for the arm, so the same
   posterior state serves every read in the measurement window:

   ```text
   brain.create_profile(namespace="bench-arm-a", name="bench-arm-a-recall-v1", consumer_kind="recall")
   ```

4. **Read back through the same namespace**, with the profile pinned via the
   ADR-104 `profile_id` override (bypasses binding resolution, so no
   `brain.bind` is required for a scratch arm):

   ```text
   memory.recall(query="...", namespace="bench-arm-a", profile_id="bench-arm-a-recall-v1")
   ```

   `namespace` here is an exact match, not a widened visible set — the
   candidate fetch never sees memories from any other namespace, including
   `local`. An invalid namespace string is a hard per-op error, not a silent
   fallback.

5. **Keep feedback and knowledge composition in the arm** when those paths are
   part of the measurement:

   ```text
   brain.auto_feedback(query="...", results=[{"id":"..."}], namespace="bench-arm-a")
   knowledge.compose(query="...", atom_ids=["..."], namespace="bench-arm-a")
   ```

   Auto-feedback stamps its event and folds only the arm's posterior state;
   compose uses the arm for corpus, section, KG-blend, and profile-weight reads.

6. **Tear down** by deleting the arm's memories (`delete(id="...",
   hard=true)` — one call per memory id; `delete` takes `id`/`kind`/`hard`,
   not a `type=` param) once the measurement is done, or simply let the arm
   namespace age out unread — it costs nothing beyond the storage of its own
   rows.

## Remaining shared machinery

- **The serve ledger.** `brain.record_serve` stamps the _effective_ namespace
  used for the fetch (the arm namespace when `namespace=` was passed), so
  ledger rows are attributable to the arm — but the ledger itself is a single
  shared table, not partitioned per arm; querying it for arm-specific
  analysis means filtering by namespace after the fact, not scoping the
  write.
