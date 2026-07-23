# ADR-102: Tiered validation and merge for KG change-sets

**Status**: Accepted
**Date**: 2026-07-08
**Depends on**: [ADR-101](./ADR-101-kg-changeset-model.md) (change-set artifact),
[ADR-002](./ADR-002-edge-ontology.md) (edge contract),
[ADR-046](./ADR-046-event-sourced-proposals.md) (approval lifecycle),
[ADR-055](./ADR-055-epistemic-edge-relations.md) (epistemic relations), and
[ADR-067](./ADR-067-write-owner-daemon.md) (single-writer path)

## Context

[ADR-101](./ADR-101-kg-changeset-model.md) defines an ordered, stage-time-ID-stable change-set.
Before a change-set reaches the live graph, it must be validated against the closed graph
taxonomies and checked for conflicts with state that changed after staging.

Operations do not all carry the same consequence. Additive operations that satisfy every
structural rule can use a direct validated path. Destructive, identity-changing, epistemic, or
otherwise judgment-bearing operations require an explicit approval record before application.
Using one path for both classes either adds unnecessary delay to routine additive writes or gives
high-impact writes insufficient control.

This ADR defines two caller-neutral paths, one shared rule engine, the approval contract, and the
merge behavior when staged preimages no longer match current state.

## Decision

### D1: Every operation resolves to one of two paths

Validation classifies every staged operation before any operation is applied:

- **Tier 1, validated direct apply.** The operation passes the configured rules and the mandatory
  architectural floor in D2, then applies through the existing live write surface. Application is
  atomic at the change-set boundary when the selected apply surface supports that unit; otherwise
  the change-set is rejected rather than partially applied.
- **Tier 2, approval required.** The change-set remains staged, a structured diff is computed
  against current graph state, and an approved proposal under ADR-046 is required before apply.
  Application revalidates rules and preimages after approval and commits through the same
  single-writer boundary.

Classification is deterministic for a given change-set, rules document, and graph snapshot. A
caller cannot bypass classification by requesting a tier directly.

### D2: Shared rules and a non-configurable safety floor

The rule evaluator is the pure, UI-agnostic component defined by ADR-101. A headless command and a
graphical interface receive the same findings for the same inputs. Rules are supplied as TOML and
can add stricter conditions, but they cannot weaken this minimum:

**Tier 1 eligible**

- `create`;
- `link` using a relation not reserved for Tier 2; and
- `update` of mutable entity or note fields;

provided the operation has no `error` finding and does not meet a Tier 2 condition.

**Tier 2 required**

- a `link` using `supersedes`, `supports`, or `refutes`;
- any `delete` or `merge`;
- any change to an existing edge's relation or weight;
- any resulting edge weight below `0.7`; and
- any operation with an `error` finding.

The evaluator also applies the existing endpoint, direction, referential-integrity, naming, and
citation-date rules. Warning and informational findings are retained with the validation result
but do not independently force Tier 2 unless the TOML policy makes them stricter.

The implementation rejects a rules document that attempts to place a safety-floor operation in
Tier 1. Unknown rule keys, invalid relation names, and malformed thresholds are errors, not ignored
configuration.

### D3: Approval is an authorization decision, not producer routing

Tier 2 uses the proposal lifecycle from ADR-046. The proposal contains the change-set identifier,
its structured diff, validation findings, and a digest of the exact staged bytes. Approval records
the authorized actor, decision time, and reviewed digest.

The approval gate depends on authorization policy and proposal state. It does not inspect client
implementation details or producer category. The approved digest must match the bytes being
applied. Any post-approval change to the envelope or operations invalidates the approval and
requires a new review.

Rejection and withdrawal leave the live graph unchanged. Approval does not skip apply-time
validation or conflict detection.

### D4: Apply-time preimage validation and merge

Application evaluates the staged change-set against a consistent current snapshot:

1. Validate operation syntax, identifiers, endpoint rules, and the D2 safety floor.
2. For `update`, compare each field-scoped preimage with the current value.
3. For `delete` and `merge`, compare the captured record and incident-edge preimages with current
   state.
4. If every preimage matches, apply operations in their staged order.
5. If a preimage differs, return a structured conflict and apply nothing.

A conflict result identifies the operation index, record identifier, field or edge that diverged,
and the base, staged, and current values where disclosure is appropriate. Conflict order is stable
by operation index and then identifier.

For archive-level reconciliation, `khive-merge` provides deterministic three-way merge over base,
current, and proposed snapshots. A clean merge produces a new candidate change-set that must pass
the same validation and approval rules. A conflicting merge returns explicit conflicts and never
selects a side silently.

### D5: Revert semantics

Op-list inversion is the primary revert mechanism:

- `create` inverts to deletion of the same identifier;
- `link` inverts to removal of that edge;
- `update` restores the field-scoped values captured at stage time; and
- `delete` and `merge` reconstruct prior records and edges from their captured preimages.

An inverse is staged as a new change-set and follows the same tier classification. Inversion is
valid only while it does not overwrite intervening changes. If current state diverges from the
expected postimage, the inverse returns a conflict and the operator prepares a compensating
change-set against current state.

### D6: Access to the live graph

Validation and review tooling accesses the live graph through the public request surface and the
warm daemon's single-writer path. It does not open an independent writable handle to the live
SQLite database. Codec, rule, diff, and merge crates remain pure in-memory libraries; callers own
filesystem and transport I/O.

## Verification

Tests must cover:

- deterministic classification for identical change-set, rules, and snapshot inputs;
- rejection of a rules document that weakens the Tier 2 floor;
- identical findings from native and wasm rule-evaluator builds;
- Tier 1 application only after a clean validation result;
- Tier 2 refusal without an approved proposal for the exact staged-byte digest;
- invalidation of approval after any staged-byte change;
- apply-time revalidation after approval;
- field-scoped `update` conflicts and full-preimage `delete` and `merge` conflicts;
- all-or-nothing behavior when any operation conflicts;
- stable conflict ordering and structured base, staged, and current values;
- clean and conflicting three-way merge behavior; and
- inverse application refusing to overwrite an intervening change.

## Alternatives considered

### Require approval for every change-set

Rejected because purely additive operations that pass all structural rules do not need the same
control as destructive or epistemic mutations. The mandatory safety floor preserves the important
boundary without serializing every write through approval.

### Let callers select a tier

Rejected because callers could misclassify high-impact operations. Tier selection is a derived
validation result.

### Encode the tier predicate only in code

Rejected because installations need to add stricter policy without recompilation. The TOML rules
file can tighten the policy while the ADR-defined floor prevents weakening it.

### Resolve merge conflicts automatically

Rejected because selecting a side can discard current or staged graph assertions. Only a clean
three-way merge applies without an explicit compensating decision.

## Consequences

### Positive

- Every staged operation receives deterministic validation and tier classification.
- High-impact mutations require a durable approval record tied to exact staged bytes.
- Producer implementation details do not influence governance.
- Preimage validation prevents stale change-sets from silently overwriting current state.
- The same rule, diff, and merge logic is reusable across command-line and graphical surfaces.

### Negative

- Two application paths require separate regression coverage.
- Tier 2 adds approval latency.
- Rules files become load-bearing configuration and require strict parsing and versioning.
- A stale preimage requires conflict resolution or a new compensating change-set.

## References

- [ADR-002](./ADR-002-edge-ontology.md): edge relations, weights, and endpoint constraints
- [ADR-046](./ADR-046-event-sourced-proposals.md): proposal approval lifecycle
- [ADR-055](./ADR-055-epistemic-edge-relations.md): `supports` and `refutes`
- [ADR-067](./ADR-067-write-owner-daemon.md): single-writer application boundary
- [ADR-101](./ADR-101-kg-changeset-model.md): typed op-list, preimages, codec, rules, and diff
