# khive-pack-formal

Formal-math ontology pack: additive edge endpoint rules for six formal-math
`concept` subtypes — `theorem`, `definition`, `structure`, `instance`, `axiom`,
`goal` (ADR-069). Pure ontology, no verbs.

`FormalPack` contributes zero note kinds, zero entity kinds, and an empty
`HANDLERS` table; its `dispatch` unconditionally returns
`RuntimeError::InvalidInput` for any verb. Its entire contribution is
`EDGE_RULES` — 21 `EdgeEndpointRule` entries that widen which
`(source, relation, target)` triples the closed 17-relation edge ontology
accepts, without adding a relation or tightening the base contract.

## Endpoint rules

Every rule uses `EndpointKind::EntityOfType { kind: "concept", entity_type }` so
a match requires _both_ the base entity `kind == "concept"` and the declared
`entity_type` subtype — the full `(EntityKind, entity_type)` pair ADR-001
requires for granular subtyping. The closed `EdgeRelation` enum is unchanged;
these rules only add legal endpoint pairs for relations that already exist:

| Relation      | Rule count | Examples                                                            |
| ------------- | ---------- | ------------------------------------------------------------------- |
| `depends_on`  | 14         | `theorem -> definition`, `goal -> theorem`, `instance -> structure` |
| `instance_of` | 1          | `instance -> structure`                                             |
| `extends`     | 2          | `structure -> structure`, `definition -> definition`                |
| `variant_of`  | 4          | `theorem -> theorem`, `goal -> theorem`                             |

## Usage

`FormalPack` is registered with the runtime via `inventory` alongside the other
packs; it contributes no callable verb, so there is no request DSL surface to
invoke. Its effect is passive: once loaded, edge validation for `concept`
entities carrying a matching `entity_type` accepts the pairs above in addition
to the base ADR-002 contract. The rule table itself is a plain constant:

```rust
use khive_types::{EdgeEndpointRule, EdgeRelation, EndpointKind};

// from khive-pack-formal's FORMAL_EDGE_RULES — depends_on, theorem -> axiom
EdgeEndpointRule {
    relation: EdgeRelation::DependsOn,
    source: EndpointKind::EntityOfType { kind: "concept", entity_type: "theorem" },
    target: EndpointKind::EntityOfType { kind: "concept", entity_type: "axiom" },
};
```

## Where this sits

`khive-pack-formal` sits in the pack tier on `khive-types` (`EdgeEndpointRule`,
`EndpointKind`) and `khive-runtime` (`Pack`, `PackRuntime`, inventory
registration); it `REQUIRES` [`khive-pack-kg`](https://crates.io/crates/khive-pack-kg)
for the underlying `concept` entity substrate. Unlike the eleven packs force-linked into the `khive-mcp` binary, `khive-pack-formal`
is only force-linked into `kkernel` (the admin/reindex binary) — it is not part of
the agent-facing MCP server's pack registry at all today. A deployment that
ingests formal-math corpora (Lean/mathlib-style theorem/definition/proof graphs)
through `kkernel` opts in via `KHIVE_PACKS` or `--pack formal` on that binary; wiring
it into `khive-mcp` would require adding the force-link `pub use` in
`khive-mcp/src/pack.rs` first. Governing ADR:
[ADR-069 (The Subject Model — Domain-Ontology Ingestion and Map Pipeline)](https://github.com/ohdearquant/khive/blob/main/docs/adr/ADR-069-subject-model.md).

## License

BUSL-1.1. See the repository [LICENSE](https://github.com/ohdearquant/khive/blob/main/LICENSE).
