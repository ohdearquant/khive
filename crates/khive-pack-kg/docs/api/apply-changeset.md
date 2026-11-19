# Canonical change-set apply

`apply_changeset` accepts one `delta` string containing the complete supported ADR-101 NDJSON
envelope and ordered operations. The maximum is 2000 operations. The daemon applies the complete
input through one ADR-099 writer transaction, preserving staged create and link identifiers.
An existing live or tombstoned identifier refuses the unit. Merge is `KnownUnimplemented` and
refused before preparation.

Create admission runs the existing strict secret scans with no manifest exemption.

The gate checks the wrapper and every constituent create, link, update and delete under the
applying caller. Before each operation writes, the same writer reads its live record. Updates
compare only touched fields; deletes compare the complete capture except `updated_at`. The first
refusal rolls back the complete unit and identifies its operation index. A preimage mismatch
also identifies the record and field without returning captured or current contents.

A kind whose existing owner hook validated the complete prepared record retains that owner's
revision guard. Its separate refusal is `owner_snapshot_conflict`, with record identifier and
kind. Unhooked rows retain concurrent changes to untouched fields. No refusal automatically
reprepares or retries the unit.

Canonical properties replace the whole map; ordinary live update retains its map merge. For
notes only, the canonical map excludes the storage alias `properties.tags`: canonical `tags`
holds that value, and the alias in a canonical note map is refused. A properties-only patch
preserves the live tags slot read in the same writer transaction. A tags-only patch changes only
that slot. An empty tag vector on create leaves the storage key absent; an update setting an
empty vector writes an empty array. Absent and empty stored tags compare equally as canonical
empty vectors. An entity's custom `properties.tags` remains an ordinary property. These mappings
follow [ADR-084 §3a](../../../../docs/adr/ADR-084-verb-surface-consistency.md#3-first-conformance-applications)
and [ADR-188](../../../../docs/adr/ADR-188-entity-version-fence.md).

The first caller is the daemon-only `kg apply` command. It loads one rules snapshot, reuses the
existing classifier and its validation coverage findings, and submits the original complete
delta only when every operation is Tier 1. A Tier-2 operation refuses the complete CLI input.
The CLI does not open a local runtime or replay a fully submitted write.

Reviewed proposals use a top-level `Canonical { schema_version, delta }` arm and the same
landing implementation. The outer version must match the envelope. Canonical cannot be nested
inside Compound, and proposal creation validates its delta and owning create hooks before
persisting history. New-entry budgets count every canonical create. Legacy proposal arms retain
their existing guards and report `preimage: none`.

Confirmed commit returns `committed: true` and ordered receipts. Captured markers identify
update/delete checking; a create/link-only unit reports `preimage: none`. A post-commit failure
requires reconciliation and cannot be replayed. An unknown writer outcome remains unknown;
the proposal worker leaves the proposal applying, without a Failed event or approval reset.
