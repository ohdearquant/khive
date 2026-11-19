# Edge governance storage

Migration 061 installs the storage layer from
[ADR-159](../../../../docs/adr/ADR-159-edge-governance-provenance.md). It starts
inactive. Installing these objects does not authorize an edge or enable memory
supersession filtering.

| Object                          | Stored evidence                                                                                                              | Contract                                                        |
| ------------------------------- | ---------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------- |
| `edge_governance_decisions`     | Append-only authorization/rejection and bound edge preimage; authorizer, provider, action and review evidence                | ADR-159 §1, lines 124–140                                       |
| `edge_governance_invalidations` | Append-only events naming a decision, edge, cause and time                                                                   | ADR-159 §1, lines 142–161                                       |
| `edge_governance_active`        | One current projection per edge and decision; target/source/edge covering index; source index for the endpoint-note triggers | ADR-159 §1, lines 163–175; §3, lines 252–253; §4, lines 301–304 |
| `edge_governance_state`         | One inactive row; complete activation receipts required before an active row can be stored                                   | ADR-159 §1, lines 177–181; §7, lines 424–431                    |

Identifiers use the existing graph TEXT representation; timestamps use Unix
microseconds in INTEGER columns. The invalidation ledger allocates a local
INTEGER primary key. Native trigger timestamps have one-second resolution,
expressed in microseconds. History deliberately has no foreign key to graph or
note rows, so hard deletion cannot erase its evidence.

Decisions accept the four proposal reason codes from
[ADR-046](../../../../docs/adr/ADR-046-event-sourced-proposals.md) plus the internal
`late_arrival_unreviewed` cutover-fence code from ADR-159 §6. That internal code
is not added to the public proposal payload vocabulary. The migration constrains
relation to `supersedes` and target backend to local (NULL). Reviewed decisions
require a review-event identifier.

Inactive state has NULL receipt fields. An active state requires epoch, time,
all inventory counts, provider mode and version, and trigger manifest. This
constraint checks completeness; it is not an activation API or verification of
authority. The expected trigger names and normalized SQL SHA-256 digests are
versioned with migration 061. SQL normalization collapses whitespace and trims
it; the terminal statement delimiter is absent from SQLite's stored trigger SQL.

## Invalidation

Null-safe transitions of an edge namespace, source, target, relation or backend
append an invalidation before clearing its active projection. Edge and endpoint
note liveness transitions work in both directions. Hard deletion records history
before projection cascades; subsequent incident-edge deletion cannot duplicate
an already-cleared marker. Unchanged assignments and weight/metadata updates
preserve a marker. These rules implement ADR-159 §3, lines 237–269.

Every spent decision remains unusable even when an edge returns to its former
preimage. Projection insertion or decision replacement checks the invalidation
ledger. Reconstruction must independently exclude spent decisions, as required
by ADR-159 §1; the migration tests exercise that filtering with a fixture-only
reconstruction query. No production reconstruction API is introduced here.

Raw merge rewrites produce `endpoint_or_relation_or_backend_rewrite`; the
`merge_rewire` cause is reserved but not emitted because a database trigger does
not know the caller's intent. Endpoint note changes produce
`endpoint_note_liveness_transition`; edge liveness uses
`edge_liveness_transition`. Hard deletion uses `hard_delete` and a future
revocation writer uses `revoked_by_decision`.

## Generic writes and follow-up

Generic create, update and link reject governance-named destination parameters.
JSON import continues to process only its fixed entity and edge substrates;
unknown archive members and ordinary metadata do not write governance evidence.
These rules do not restrict intentional operator SQL access.

Authority receipts, the governed writer, classification/reconciliation, write
fences, activation and governed recall are separate steps. Their consumer is
ADR-157 memory supersession (issue #5584). Before enabling it, reconcile the
ADR-046/159 reason-list wording and decide whether the merge API should emit its
reserved dedicated cause. Activation also needs the boot manifest verification
and projection reconstruction required by ADR-159 §7.

The [write comparison benchmarks](../benchmarks.md#edge-invalidation-trigger-comparison)
use identical dormant-state workloads before and after migration installation.
They cover edge inserts, updates and deletes, product merges, and contended
writer latency; active-marker populations require a separate comparison.
