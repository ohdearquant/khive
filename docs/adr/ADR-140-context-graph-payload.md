# ADR-140: Add a bounded graph payload to context responses

- **Status:** Proposed
- **Date:** 2026-08-03

## Context

ADR-089 defines `context` as a one-call, entity-anchored graph-context read that combines semantic anchors with bounded graph expansion. (Source: [ADR-089](ADR-089-context-verb.md), §Context and §Decision.)

At the base commit this proposal is written against, the handler's response assembly emits `anchors`, `truncated`, and `dropped`, and no response-level edge payload. (Source: [context.rs lines 499-510 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-kg/src/handlers/context.rs#L499-L510).)

The existing nested neighbor records expose a neighbor identifier, name, relation, direction, weight, hop, and `via`, but do not express both endpoints as a complete edge row. (Source: [context.rs lines 467-489 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-kg/src/handlers/context.rs#L467-L489).)

ADR-089 already bounds expansion by `hops` and a per-node neighbor cap (`fanout`), and bounds serialized output with a deterministic character budget and dropped-record counts. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics.)

## Decision

ADR-089 is amended so that the `context` response adds a top-level `edges` array — a sibling of `anchors`, `truncated`, and `dropped` — containing the graph relationships selected for the returned context. (Source: [ADR-089](ADR-089-context-verb.md), §Response shape.)

### Edge-row schema

`edges` is always present and always an array; it is `[]` when `hops` is `0` (anchors only), when the expansion selects no relationships, or when the budget drops them all. Every row carries the complete field set below; no field is conditional and none may be omitted:

| Field         | Type                    | Contents                                                                                        |
| ------------- | ----------------------- | ----------------------------------------------------------------------------------------------- |
| `source_id`   | string (UUID)           | Edge source per the orientation rules below.                                                    |
| `source_name` | string                  | Display name of the `source_id` entity.                                                         |
| `target_id`   | string (UUID)           | Edge target per the orientation rules below.                                                    |
| `target_name` | string                  | Display name of the `target_id` entity.                                                         |
| `relation`    | string                  | Relation name, identical to the paired neighbor record's `relation`.                            |
| `weight`      | number                  | Edge weight, identical to the paired neighbor record's `weight`.                                |
| `direction`   | string                  | `"outgoing"`, `"incoming"`, or `"both"`, identical to the paired neighbor record's `direction`. |
| `hop`         | integer                 | `1` or `2`, identical to the paired neighbor record's `hop`.                                    |
| `via`         | string (UUID) or `null` | `null` exactly when `hop` is `1`; the parent entity's id exactly when `hop` is `2`.             |

The `source_id`/`target_id` spelling follows the endpoint vocabulary the link surface already emits. (Source: [link.rs lines 222-223 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-kg/src/handlers/link.rs#L222-L223).)

### Endpoint orientation

The parent of a row is the anchor entity for a hop-1 row and the `via` entity for a hop-2 row.

For non-symmetric relations, `source_id` and `target_id` always reproduce the stored assertion direction:

- `direction: "outgoing"` means the stored edge points parent to neighbor; the row sets `source_id` to the parent and `target_id` to the neighbor.
- `direction: "incoming"` means the stored edge points neighbor to parent; the row sets `source_id` to the neighbor and `target_id` to the parent.

A row therefore always reads as the stored assertion `source_id → relation → target_id`, and a client must never render an `incoming` row as the inverse assertion.

Symmetric relations (`competes_with`, `composed_with`) always emit `direction: "both"`, on every filter path. This is normative, and on one path it amends current behavior rather than describing it: at the pinned base, symmetric hits are tagged `"both"` only on the all-symmetric relations-filter fast path (Source: [context.rs lines 77-116 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-kg/src/handlers/context.rs#L77-L116)), while the absent- and mixed-filter path tags every hit — symmetric relations included — `"outgoing"` or `"incoming"` from the stored row's direction (Source: [context.rs lines 156-181 at 9442ec2](https://github.com/ohdearquant/khive/blob/9442ec2c52290120c5bf4a4c8a1dc771102658dd/crates/khive-pack-kg/src/handlers/context.rs#L156-L181)). An implementation of this ADR normalizes every symmetric-relation hit to `direction: "both"` regardless of the relations filter, and the paired neighbor record changes with the edge row — the two always carry the same `direction` field, preserving the identical-fields rule in the schema above. A symmetric row therefore never exposes stored endpoint order through a direction tag. Focused tests must cover the all-symmetric, absent, and mixed filter paths. For a `"both"` row the endpoint order is deterministic but carries no assertion semantics: `source_id` is the parent, `target_id` is the neighbor, and a client must not infer stored endpoint order from a `"both"` row.

### Response example

```json
{
  "anchors": ["… anchor objects exactly as ADR-089 defines them, unchanged …"],
  "edges": [
    {
      "source_id": "5f0e0d5c-6f0a-4c6e-9a3a-2f6f8f1c2ab1",
      "source_name": "vamana",
      "target_id": "9c1d2e3f-4a5b-4c7d-8e9f-0a1b2c3d4e5f",
      "target_name": "hnsw",
      "relation": "competes_with",
      "weight": 0.8,
      "direction": "both",
      "hop": 1,
      "via": null
    },
    {
      "source_id": "9c1d2e3f-4a5b-4c7d-8e9f-0a1b2c3d4e5f",
      "source_name": "hnsw",
      "target_id": "1d4c7b2a-3e5f-4a6b-8c9d-0e1f2a3b4c5d",
      "target_name": "skip-list",
      "relation": "derived_from",
      "weight": 0.9,
      "direction": "outgoing",
      "hop": 2,
      "via": "9c1d2e3f-4a5b-4c7d-8e9f-0a1b2c3d4e5f"
    }
  ],
  "truncated": false,
  "dropped": {
    "anchors": 0,
    "neighbors": 0,
    "edges": 0,
    "stage": "budget"
  }
}
```

The first row is a hop-1 symmetric row anchored at `vamana`: `direction` is `"both"`, the endpoint order is the non-semantic parent-then-neighbor order, and `via` is `null` because `hop` is `1`. The second row is a hop-2 row discovered through `hnsw`: its `via` is its parent's id, and its `"outgoing"` direction states that the stored edge points `hnsw` to `skip-list`.

`dropped.edges` is always present alongside the existing dropped counts, and always equals `dropped.neighbors` under the atomicity rule below.

The `edges` section is assembled from the same candidate relationships already selected by `context`; it does not add an index, storage type, expansion hop, or per-node neighbor cap beyond ADR-089. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics and §Latency budget.)

### Assembly order and neighbor/edge atomicity

Every `edges` row is the edge that discovered the corresponding neighbor record already present in an anchor's `neighbors` list. An `edges` row and its neighbor record are therefore the same underlying assembly step viewed from two response locations, not two independently selected pieces of data.

`edges` rows serialize in exactly the deterministic order ADR-089 already establishes for `neighbors`: anchors in selection order; within an anchor, hop-1 before hop-2; within a stratum, edge weight descending; ties broken by UUID. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics and the 2026-07-04 neighbor-ordering amendment.)

Emission is atomic per discovered node: the budget walk appends a neighbor record and its corresponding `edges` row together, as one unit, in that walk position. A neighbor is never emitted without its edge, and an edge is never emitted without its neighbor. Consequently `dropped.edges` is defined to equal `dropped.neighbors` under this rule — both count the same set of budget-cut discovery steps — and a client can rely on the two counts staying equal without reconciling them independently.

The existing deterministic budget walk must count every emitted edge row, including endpoint display names, against the same character budget as the neighbor record it pairs with, and must set `truncated` plus `dropped.edges` (equal to `dropped.neighbors`) when the remaining budget cannot hold the next neighbor/edge pair in the established order. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics.)

## Consequences

A caller receives semantic anchors and endpoint-complete graph context from one `context` invocation. (Source: [ADR-089](ADR-089-context-verb.md), §Context and §Response shape.)

The response remains bounded by the existing `hops`, per-node neighbor cap, and character-budget mechanics. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics.)

The additional endpoint names consume response budget, so a budget-constrained response can contain fewer graph rows and reports that condition through `truncated` and `dropped.edges`, which always equals `dropped.neighbors` under the atomic-emission rule above. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics.)

## Alternatives considered

1. **Require a follow-up `neighbors` or `traverse` call.** This was rejected because ADR-089 identifies caller-side graph assembly as an N+1 round-trip path that cannot apply one global server-side budget. (Source: [ADR-089](ADR-089-context-verb.md), §Context and §Alternatives rejected.)

2. **Return only endpoint identifiers.** This was rejected because the existing context contract includes names in its anchor and neighbor representations. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics and §Response shape.)

3. **Expand `hops` or the per-node neighbor cap to compensate for missing graph rows.** This was rejected because ADR-089 deliberately bounds expansion work independently of response budget. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics.)

4. **Add a second unbounded graph payload.** This was rejected because ADR-089 makes deterministic budget enforcement part of the `context` contract. (Source: [ADR-089](ADR-089-context-verb.md), §Semantics.)

5. **Emit `edges` and `neighbors` as independently budgeted lists.** This was rejected because independent truncation could return a neighbor without its edge or an edge without its neighbor, making `dropped.edges` incomparable to `dropped.neighbors` and the response internally inconsistent about what context was actually returned.
