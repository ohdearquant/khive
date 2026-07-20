# Specialized Packs

khive's default install loads eleven production packs
(`kg, gtd, memory, brain, comm, schedule, session, git, code, workspace, blob`, per
`RuntimeConfig::default()` in `crates/khive-runtime/src/config.rs`). The `code` pack contributes one verb, `code.ingest` (L1 manifest + L1.5
import-scan source ingestion into a dedicated map database, see
[ADR-085](../adr/ADR-085-code-pack.md)), alongside its `finding` note kind and
edge rules; `findings.json` ingestion remains an admin CLI path. `workspace`
registers the `workspace` entity kind and five `contains` endpoint rules only,
with no verbs. Beyond the
default set, khive also ships niche packs that extend the graph for a
specific domain without adding verbs of their own. This guide covers the
formal-math pack, the first of these, and how pack loading works in general.

## Pack composition model

Every pack implements the `Pack` trait (`crates/khive-types/`) and declares,
additively, what it contributes: note kinds, entity kinds, verb handlers,
and edge endpoint rules. A pack can declare zero verbs and still be useful,
contributing purely to the edge ontology. Packs declare a `REQUIRES` list of
other packs that must already be loaded; the runtime resolves this at
startup. See [ADR-017](../adr/ADR-017-pack-standard.md) for the full
standard, including how pack-declared edge endpoint rules combine with the
base ADR-002 contract: rules are additive only, never tightening what the
base contract already allows.

### Loading a pack

Packs are selected via the `--pack` CLI flag (repeatable) or the
`KHIVE_PACKS` environment variable (comma- or whitespace-separated):

```bash
kkernel mcp --pack kg --pack gtd --pack formal
# or
KHIVE_PACKS="kg,gtd,formal" kkernel mcp
```

`formal` declares `REQUIRES = &["kg"]`, so `kg` must also be in the load set.

## The formal pack

`crates/khive-pack-formal/` is a pure ontology extension for formal
mathematics, targeting Lean-style proof developments, built around six
concept subtypes: `theorem`, `definition`, `structure`, `instance`, `axiom`,
and `goal`. It is not part of the default pack set; opt in explicitly.

### What it contributes

`FormalPack` declares:

- `NOTE_KINDS = &[]`, `ENTITY_KINDS = &[]`, `HANDLERS = &[]`: no new note
  kinds, entity kinds, or verbs.
- `EDGE_RULES = &FORMAL_EDGE_RULES`: 21 additive edge endpoint rules.

Every rule is expressed via `EndpointKind::EntityOfType { kind: "concept",
entity_type: <subtype> }`: all six subtypes are `concept` entities
distinguished by their `entity_type` property, not by a new `EntityKind`
variant. Because `dispatch()` unconditionally returns an error naming the
verb, loading `formal` cannot be used to call any verb. Its only effect is
widening which typed edges the graph accepts.

### Endpoint rules by relation

| Relation      | Rule count | Pairs                                                                                                                                                                                           |
| ------------- | ---------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `depends_on`  | 14         | theorem to {theorem, definition, structure, axiom}; definition to {definition, structure, theorem, axiom}; instance to {structure, definition}; goal to {theorem, definition, structure, axiom} |
| `instance_of` | 1          | instance to structure                                                                                                                                                                           |
| `extends`     | 2          | structure to structure; definition to definition                                                                                                                                                |
| `variant_of`  | 4          | theorem to theorem; definition to definition; goal to theorem; goal to definition                                                                                                               |

`depends_on` models the prerequisite chain, so the source uses or builds on a
target: a theorem may depend on other theorems, definitions, structures, or
axioms it invokes, and a `goal` (an unproved target) may depend on the same
four subtypes it will eventually need. `instance_of` models an instance
implementing a structure. `extends` models structural or definitional
inheritance. `variant_of` models a restatement, including a `goal` framed as
a variant of an existing theorem or definition, which is useful as an
anti-duplicate signal when the same result is proposed as a fresh goal.

### Example

```
request(ops="create(kind=\"concept\", name=\"Cauchy-Schwarz\", properties={\"entity_type\": \"theorem\"})")
request(ops="create(kind=\"concept\", name=\"Inner product space\", properties={\"entity_type\": \"structure\"})")
request(ops="link(source_id=\"<theorem_id>\", target_id=\"<structure_id>\", relation=\"depends_on\")")
```

With only `kg` loaded (no `formal`), the same `link` call is rejected. The
base ADR-002 contract does not admit a `concept`-to-`concept` `depends_on`
edge between two arbitrary subtypes on its own; the `formal` pack's rules
are what makes this specific `(theorem, depends_on, structure)` triple legal.

## See also

- [Knowledge Graph Modeling](knowledge-graph.md): the base entity kind and
  edge relation taxonomy that specialized packs extend.
- [Agent Sessions and Data Ingest](sessions-and-ingest.md): another optional
  pack (`session`), included in the default set but with its own opt-in
  background service.
