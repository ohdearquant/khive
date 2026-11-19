# khive-pack-charter

Charter workflows for [ADR-193](../../docs/adr/ADR-193-charter-runs.md) and
[ADR-208](../../docs/adr/ADR-208-charter-workflows.md). Both hosts link the pack into
the inventory; select `charter` explicitly alongside the other packs the deployment
needs. It is not in the default pack list.

Loading it applies a pack-owned, idempotent schema plan for `charter_definitions`,
`charter_vocabularies`, `charter_subjects`, `charter_runs`, `charter_phases`,
`charter_evidence`, `charter_attempts`, `charter_commands` and
`charter_waiting_changes`. No core migration or generic note kind is added. The
definition identity includes the policy domain, charter identifier and version.
Runs have unique replay keys, and an unresolved attempt, including an uncertain
outcome, excludes a second attempt for the same subject.

The pack registers twelve verbs. `charter.compile`, `charter.publish` and
`charter.vocabulary` compile charter sources against the phrase registry and store
charter versions and vocabularies. `charter.start`, `charter.observe`,
`charter.sample` and `charter.cancel` record runs and advance them on observations.
`charter.waiting`, `charter.get`, `charter.runs` and `charter.list` read them back.
`charter.claim` refuses without writing.

The pack records; it performs nothing. Claims are not admitted and no verb has an
external effect. An observation's producer is the caller's actor. Producer
registrations, which limit the kinds and subjects a producer may submit, and the
authenticated grant and executor integrations are not implemented; ADR-193 M2
enforcement requires them.

The Rust transition helpers check both the expected revision and state. A stale
revision or state returns a conflict. They operate on the caller's existing SQL
writer transaction; they do not authorize a transition or open and commit a
separate transaction. They are concurrency guards, not the complete lifecycle
transition matrix. The caller must check subject, definition and authority
state, and commit the related evidence and command records in that same unit.
An error must escape the unit so earlier writes roll back.

Namespace moves do not yet have a charter-specific disposition. A namespace
move encountering nonempty charter tables refuses rather than rewriting their
recorded attribution. No relocation or deletion policy is implied by installing
this schema.
