# ADR-173: Weighted incidence as the graph's storage primitive

- Status: Proposed
- Date: 2026-09-01

## Context

`graph_edges` today hardwires the binary directed edge:
`(namespace, id, source_id, target_id, relation, weight, …)` — two
endpoint columns and a single `weight REAL` on the edge row
(`crates/khive-db/sql/schema.sql`). Three modeling pressures have outgrown
this shape:

1. **Weight is conceptually a property of the (node, edge) pair, not of
   the edge.** The same relation matters differently to each participant:
   a citation edge is one-of-fifty references to the citing document and
   the defining application to the cited one. Hypergraph literature makes
   this exact distinction — a weighted incidence `γ(v, e)` per member,
   separate from any global edge weight `w(e)` (edge-dependent vertex
   weights; Chitra & Raphael, ICML 2019). One `weight` column per edge
   cannot represent it.
2. **Hyperedges are unrepresentable.** A relation over three or more
   records (a composition, a multi-party derivation, an n-ary event) has
   no home. Client-side prototyping confirmed the workaround's defect: an
   edge carrying its member list in `metadata` is invisible to
   `neighbors`/`traverse` — the topology engine cannot see participants
   that are not endpoint columns, so the hyperedge exists as data but not
   as graph.
3. **Roles are schema, not data.** `source`/`target` are the only
   participation modes expressible. Qualified relations (premise /
   conclusion / context; input / output / catalyst) need either bogus
   intermediate entities or property conventions no query plane
   understands.

The edge record itself is already general — `(id, kind, namespace,
properties, created_at, updated_at, deleted_at)` matches every other
record. Only its membership encoding is special-cased.

## Decision

**Topology moves to an incidence table; the binary edge becomes the
two-row special case rather than the schema.**

1. **New table:**

   ```sql
   graph_incidences (
     namespace   TEXT    NOT NULL,
     edge_id     TEXT    NOT NULL,   -- FK graph_edges(id)
     node_id     TEXT    NOT NULL,   -- entity or note id
     role        TEXT    NOT NULL DEFAULT 'member',
     weight      REAL    NOT NULL DEFAULT 1.0,
     PRIMARY KEY (namespace, edge_id, node_id, role)
   )
   -- indexes: (namespace, node_id) and (namespace, edge_id)
   ```

2. **`graph_edges` drops `source_id`, `target_id`, `weight`.** The edge
   row keeps identity, `kind` (the relation), lifecycle timestamps, and
   its property map. An edge must have ≥ 2 incidences; enforcement lives
   in the write path (`link` creates edge + incidences atomically).
3. **Binary directed edges** are edges with exactly two incidences,
   roles `source` and `target`. Symmetric relations use `member` for all
   participants — direction stops being a schema fact and becomes a role
   fact.
4. **Traversal is an incidence join.** `neighbors(x)` =
   `incidences[node_id = x] ⋈ incidences[same edge, node_id ≠ x]`; the
   result pairs each co-member with _its own_ weight in the shared edge.
   Degree = incidence count. Relation filters apply on the edge row's
   `kind` as before.
5. **Weights are stored absolute; normalization is the algorithm's.**
   Random-walk-style consumers normalize per edge
   (`γ(v,e) / Σ γ(·,e)`) at computation time. Storing shares would
   couple every write to its siblings and forbid meaningful absolute
   magnitudes.
6. **Endpoint validity rules re-key from (source kind, target kind) to
   (role, node kind) pairs per relation.** Existing binary rules
   translate mechanically: today's source constraint becomes the
   `source`-role constraint, and relations that declare no hyperedge
   form reject a third incidence outright — the closed-vocabulary
   posture is unchanged.

## Migration

Backfill is mechanical and lossless: each existing edge row emits two
incidence rows (`source`, `target`), both carrying the edge's current
`weight` — the degenerate case where per-node weights coincide, which is
exactly what a single edge weight asserted. A compatibility view can
serve the old flat shape to readers during the transition; writers cut
over atomically with the write-path change. Existing edge ids, kinds,
properties, and timestamps are untouched.

## Consequences

- Per-node weights, hyperedges, and qualified n-ary relations all land
  from one change, because they are one modeling fact: membership is a
  first-class row.
- `neighbors` becomes a two-index-hop join instead of an endpoint-column
  scan. Both access paths are indexed; benchmark before/after on the
  standard traversal suite is part of acceptance.
- Ranking and recall consumers that read edge weight must choose a
  perspective: "weight as seen from the anchor node" replaces "the
  edge's weight". This is a semantic improvement but a real call-site
  sweep.
- Wire results for edges change shape (`members: [{node_id, role,
  weight}]` replacing `source_id`/`target_id`/`weight`). The Python
  client already speaks the target shape and bridges the current one, so
  the cutover deletes translation code rather than adding it.

## Alternatives considered

- **Member list in edge metadata.** Rejected on prototype evidence: not
  topology — invisible to every traversal and index; queries degrade to
  full-edge scans in the client.
- **Separate hyperedge table beside binary `graph_edges`.** Rejected: two
  topologies mean every algorithm, index, and validity rule implemented
  twice, and a relation that grows a third participant must migrate
  tables.
- **Reify relations as intermediate entities.** Rejected: pollutes the
  entity space with records that are not domain objects, and pushes
  relation semantics into naming conventions the query planes cannot
  enforce.
- **Per-direction weight columns (`source_weight`, `target_weight`).**
  Rejected: patches the binary case only; hyperedges and roles remain
  unrepresentable, and a third column request arrives with the first
  three-party relation.
