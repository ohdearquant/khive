# khive-pack-charter

The recording-schema foundation for [ADR-193](../../docs/adr/ADR-193-charter-runs.md).
Both hosts link the pack into the inventory; select `charter` explicitly alongside
the other packs the deployment needs. It is not in the default pack list.

Loading it applies a pack-owned, idempotent schema plan for `charter_definitions`,
`charter_subjects`, `charter_runs`, `charter_phases`, `charter_evidence`,
`charter_attempts`, and `charter_commands`. No core migration or generic note kind
is added. The definition identity includes the policy domain, charter identifier
and version. Runs have unique replay keys, and an unresolved attempt, including
an uncertain outcome, excludes a second attempt for the same subject.

The Rust transition helpers check both the expected revision and state. A stale
revision or state returns a conflict. They operate on the caller's existing SQL
writer transaction; they do not authorize a transition or open and commit a
separate transaction. They are concurrency guards, not the complete lifecycle
transition matrix. The caller must check subject, definition and authority
state, and commit the related evidence and command records in that same unit.
An error must escape the unit so earlier writes roll back.

This is an additive schema foundation, not the complete M1 recording milestone.
It registers no public verbs. Definition publication, activation and revocation,
observation validation, evaluation, replay and waiting views follow separately.
It provides no action admission or external effect. The missing authenticated
grant and executor integrations are required for ADR-193 M2 enforcement.

Namespace moves do not yet have a charter-specific disposition. A namespace
move encountering nonempty charter tables refuses rather than rewriting their
recorded attribution. No relocation or deletion policy is implied by installing
this schema.
