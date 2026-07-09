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

1. **Pick an arm namespace.** Any string that parses as a valid namespace
   (alphanumeric segments, `.` or `::` separators, no spaces) works. Prefix it
   so it is obviously scratch data, e.g. `bench-arm-a`.

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

5. **Tear down** by deleting the arm's memories (`delete(type="memory",
   id=..., hard=true)`) once the measurement is done, or simply let the arm
   namespace age out unread — it costs nothing beyond the storage of its own
   rows.

## What is NOT yet isolated

- **Feedback events.** `brain.auto_feedback` / `brain.feedback` write into the
  _profile's_ live posterior state, not the namespace. If a measurement arm
  pins an existing (non-scratch) profile via `profile_id`, feedback recorded
  during the arm still trains that profile's posteriors going forward — there
  is no namespace-scoped posterior isolation. Use a freshly created,
  arm-specific profile (step 3) to avoid contaminating a shared profile's
  state. Tracked as issue #733 remainder.
- **`knowledge.compose`.** The knowledge/lore corpus has no namespace filter
  at all — composed knowledge is global regardless of which namespace the
  caller's token carries. A measurement arm that includes compose calls is
  not isolated from concurrent compose traffic elsewhere.
- **The serve ledger.** `brain.record_serve` stamps the _effective_ namespace
  used for the fetch (the arm namespace when `namespace=` was passed), so
  ledger rows are attributable to the arm — but the ledger itself is a single
  shared table, not partitioned per arm; querying it for arm-specific
  analysis means filtering by namespace after the fact, not scoping the
  write.
