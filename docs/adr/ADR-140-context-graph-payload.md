# ADR-140: Add a bounded graph payload to context responses

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

ADR-089 defines `context` as a one-call, entity-anchored graph-context read that combines semantic anchors with bounded graph expansion. (Source: ADR-089, lines 9-28 and 72-79 at `origin/main`.)

Measured on a development deployment, the `context` response contained `anchors`, `dropped`, and `truncated`, but no response-level edge payload. (Source: `origin/main:crates/khive-pack-kg/src/handlers/context.rs:499-510`.)

The existing nested neighbor records expose a neighbor identifier, name, relation, direction, weight, hop, and `via`, but do not express both endpoints as a complete edge row. (Source: `origin/main:crates/khive-pack-kg/src/handlers/context.rs:467-489`.)

ADR-089 already bounds expansion by `hops` and a per-node neighbor cap (`fanout` in ADR-089), and bounds serialized output with a deterministic character budget and dropped-record counts. (Source: ADR-089, lines 72-97 at `origin/main`.)

## Decision

ADR-089 is amended so that `context` adds an `edges` section containing the graph relationships selected for the returned context. (Source: ADR-089, lines 80-97 and 99-133 at `origin/main`.)

Each `edges` row contains `source`, `source_name`, `target`, `target_name`, `relation`, and `weight`; `source` and `target` are endpoint identifiers, and the paired name fields are endpoint display names. (Source: ADR-089, lines 80-90 and 99-133 at `origin/main`.)

Each `edges` row also retains `direction`, `hop`, and `via` when those fields are needed to preserve the existing expansion interpretation. (Source: ADR-089, lines 80-90 and 99-133 at `origin/main`.)

The `edges` section is assembled from the same candidate relationships already selected by `context`; it does not add an index, storage type, expansion hop, or per-node neighbor cap beyond ADR-089. (Source: ADR-089, lines 72-79 and 135-140 at `origin/main`.)

### Assembly order and neighbor/edge atomicity

Every `edges` row is the edge that discovered the corresponding neighbor record already present in an anchor's `neighbors` list. An `edges` row and its neighbor record are therefore the same underlying assembly step viewed from two response locations, not two independently selected pieces of data.

`edges` rows serialize in exactly the deterministic order ADR-089 already establishes for `neighbors`: anchors in selection order; within an anchor, hop-1 before hop-2; within a stratum, edge weight descending; ties broken by UUID. (Source: ADR-089, lines 80-90 at `origin/main`, the assembly order the neighbor list already uses.)

Emission is atomic per discovered node: the budget walk appends a neighbor record and its corresponding `edges` row together, as one unit, in that walk position. A neighbor is never emitted without its edge, and an edge is never emitted without its neighbor. Consequently `dropped.edges` is defined to equal `dropped.neighbors` under this rule — both count the same set of budget-cut discovery steps — and a client can rely on the two counts staying equal without reconciling them independently.

The existing deterministic budget walk must count every emitted edge row, including endpoint display names, against the same character budget as the neighbor record it pairs with, and must set `truncated` plus `dropped.edges` (equal to `dropped.neighbors`) when the remaining budget cannot hold the next neighbor/edge pair in the established order. (Source: ADR-089, lines 86-97 at `origin/main`.)

## Consequences

A caller receives semantic anchors and endpoint-complete graph context from one `context` invocation. (Source: ADR-089, lines 9-28 and 99-133 at `origin/main`.)

The response remains bounded by the existing `hops`, per-node neighbor cap, and character-budget mechanics. (Source: ADR-089, lines 72-97 at `origin/main`.)

The additional endpoint names consume response budget, so a budget-constrained response can contain fewer graph rows and reports that condition through `truncated` and `dropped.edges`, which always equals `dropped.neighbors` under the atomic-emission rule above. (Source: ADR-089, lines 91-97 at `origin/main`.)

## Alternatives considered

1. **Require a follow-up `neighbors` or `traverse` call.** This was rejected because ADR-089 identifies caller-side graph assembly as an N+1 round-trip path that cannot apply one global server-side budget. (Source: ADR-089, lines 9-14 and 149-158 at `origin/main`.)

2. **Return only endpoint identifiers.** This was rejected because the existing context contract includes names in its anchor and neighbor representations. (Source: ADR-089, lines 86-90 and 99-133 at `origin/main`.)

3. **Expand `hops` or the per-node neighbor cap to compensate for missing graph rows.** This was rejected because ADR-089 deliberately bounds expansion work independently of response budget. (Source: ADR-089, lines 72-79 at `origin/main`.)

4. **Add a second unbounded graph payload.** This was rejected because ADR-089 makes deterministic budget enforcement part of the `context` contract. (Source: ADR-089, lines 91-97 at `origin/main`.)

5. **Emit `edges` and `neighbors` as independently budgeted lists.** This was rejected because independent truncation could return a neighbor without its edge or an edge without its neighbor, making `dropped.edges` incomparable to `dropped.neighbors` and the response internally inconsistent about what context was actually returned.
