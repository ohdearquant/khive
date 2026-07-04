# Case Study: The Mathlib Proof Graph

This page describes a khive-at-scale deployment: a knowledge graph of the
[mathlib](https://leanprover-community.github.io/mathlib4_docs/) formal-math
library, built to support automated redundancy detection across proofs. It
focuses on the khive mechanics (ingestion, typed edges, traversal at scale),
not the mathematical adjudication built on top.

## Scale

Mathlib v4.30.0 was ingested as a khive-native graph:

| Metric               | Value     |
| -------------------- | --------- |
| Entities             | 320,810   |
| `depends_on` edges   | 4,387,592 |
| Avg edges per entity | ~13.7     |

Every mathlib declaration (theorem, definition, structure, instance, axiom)
became an entity; every dependency between declarations became a typed
`depends_on` edge. This is the same edge relation khive uses elsewhere for
build/runtime/data/artifact/tooling dependency modeling. Proof dependency is
one more instance of the same closed relation, not a schema extension.

## Ingestion path

At this scale, going through the MCP `request` surface one `create`/`link`
call at a time is not the right tool: 4.7 million individual round trips
would dominate wall time and add no value over a direct write. The ingestion
instead writes straight to a khive-native SQLite database:

1. **Bootstrap the canonical schema.** The same DDL khive's migrations apply
   (`crates/khive-db/sql/`) is applied directly to a fresh database file, so
   the resulting graph is a valid khive database: every verb (`get`,
   `neighbors`, `traverse`, `query`) works against it exactly as it would
   against a database built through the MCP surface.
2. **Deterministic IDs.** Each declaration gets a UUIDv5 id derived from its
   fully-qualified mathlib name, and every insert is `INSERT OR IGNORE`. This
   makes the ingestion idempotent: re-running the ingestion script against an
   updated mathlib snapshot (or after a crash) does not create duplicate
   entities or duplicate edges, since re-deriving the same name always
   produces the same id.
3. **Skip vector tables for a graph-only workload.** This corpus is used for
   structural traversal, not semantic search over declaration text, so the
   ingestion does not populate `sqlite-vec` `vec0` virtual tables. This keeps
   the ingestion path simpler and the resulting database smaller, at the cost
   of not supporting `search`'s vector-similarity leg over this data. Pure
   graph reads (`get`, `neighbors`, `traverse`, `query`) do not depend on the
   vector store at all.

This direct-write path trades the MCP round-trip and its request-level
validation for raw insert throughput, and only makes sense at this scale.
For anything an agent produces interactively (reading a paper, forming a
concept, linking two ideas), the normal `create`/`link` verbs over `request`
are the right tool; see [Prompt Cookbook](prompt-cookbook.md).

## Traversal at scale

The resulting database is queried with the same verbs used against any
khive graph. `neighbors` answers "what does this theorem depend on, or what
depends on it" one hop at a time; `traverse` walks multi-hop dependency
chains and lineage (see [Tips and Tricks](tips-and-tricks.md#traverse-vs-context)
for when to reach for `traverse` versus a query-anchored `context` call).
Multi-hop BFS over a graph this size behaves the same way it does over a
small research graph: bounded by `max_depth` and `relations` filters, not by
a separate large-graph code path. Nothing about the verb surface changes at
4.4 million edges versus 4,000.

## Structural signals built on the graph

Two auditable signals were built on top of the ingested graph, both derived
from graph structure rather than from a language model's judgment of a
proof's content:

- **Statement-template isomorphism.** Two theorem statements that are
  structurally isomorphic up to variable renaming and metavariable
  substitution are flagged as candidate restatements of the same result,
  independent of naming or literal source-text similarity.
- **Specialized-machinery scoring.** A proof's dependency footprint (traced
  through `depends_on`) is scored for how much specialized machinery it pulls
  in versus how directly it follows from more general, widely-depended-on
  results, surfacing proofs that lean on narrow lemmas built for that one
  result alone.

Both signals are auditable: a mathematician can walk the same `depends_on`
edges the signal used and check the claim directly, rather than trusting an
opaque similarity score. That auditability, not the underlying math, is the
khive-relevant point: it is a direct consequence of the graph being typed
and the edges being traversable, not a property of any embedding.

## Evidence

- Live visualization: <https://swarm.unsorry.agentics.org.nz/math/proof-graph>
  positions 4,770 credited proofs by mathlib territory and redundancy
  classification, browsable interactively.
- Public discussion of the redundancy-detection results:
  <https://github.com/agenticsnz/unsorry/discussions/6645>

## See also

- [Knowledge Graph Modeling](knowledge-graph.md): entity kinds and edge
  relations, including `depends_on`
- [Search and Retrieval](search.md): `neighbors`, `traverse`, and `query`
- [Tips and Tricks](tips-and-tricks.md): practical verb usage patterns
