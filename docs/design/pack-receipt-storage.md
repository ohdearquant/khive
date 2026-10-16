# Pack receipt storage

The exec and git packs keep separate receipt persistence models. The shared SQL
interfaces in `khive-storage` and `khive-runtime` remain their common storage
boundary; this decision adds no table, migration, or receipt API.

Both models carry identifiers, actor attribution, optional sessions, and JSON
evidence. Their current schemas and write protocols differ:

- Exec stores the complete receipt as JSON in `exec_runs`, beside indexed
  attribution and listing columns. Its insert allocates a per-session sequence
  number; reads combine that authoritative column with the stored JSON. Separate
  `exec_events` rows record append-only execution events. See
  [ADR-181](../adr/ADR-181-exec-verb-sandboxed-run.md) and
  [`receipts.rs`](../../crates/khive-pack-exec/src/receipts.rs).
- Git stores a typed receipt across constrained columns, including repository,
  verb, inputs, policy evidence, timing, and disposition. A mutation first stores
  an `unknown` receipt, then settles it to `committed` or `not_committed` when the
  outcome is known. Reconciliation uses operation-specific evidence without
  repeating the mutation. Receipt reads are actor-scoped. See
  [ADR-182 Amendment 2](../adr/ADR-182-git-dev-loop-verbs.md#amendment-2-2026-09-08-exact-compares-actor-only-credentials-dispositions-receipts),
  [`git_receipts.sql`](../../crates/khive-pack-git/sql/git_receipts.sql), and
  [`receipts.rs`](../../crates/khive-pack-git/src/receipts.rs).

A common receipt table or CRUD helper would need to encode both session-sequence
allocation and git's pre-effect/settlement protocol, while preserving their
different decoding and filtering rules. Sharing the table shape alone would hide
those requirements. Keep these protocols in their owning packs until a concrete
shared operation can preserve both contracts; any proposed schema or interface
change needs its own design decision before implementation.

This records the receipt-storage decision requested in #4551. Pure helpers such
as byte hashing, embedding warnings, and UUID-or-prefix parsing belong in lower
crates and are shared independently of receipt persistence.
