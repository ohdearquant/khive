# Specialized Packs

khive's default install loads twelve production packs
(`kg, gtd, memory, brain, comm, schedule, knowledge, session, git, code, workspace, blob`, per
`RuntimeConfig::default()` in `crates/khive-runtime/src/config.rs`). The `code` pack contributes one verb, `code.ingest` (L1 manifest + L1.5
import-scan source ingestion into a dedicated map database, see
[ADR-085](../adr/ADR-085-code-pack.md)), alongside its `finding` note kind and
edge rules; `findings.json` ingestion remains an admin CLI path. `workspace`
registers the `workspace` entity kind and five `contains` endpoint rules only,
with no verbs. Beyond the
default set, khive also ships opt-in packs for narrower domains. Some are
pure ontology extensions; others expose specialized verbs. This guide covers
the formal-math and Moodboard packs and how pack loading works in general.

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

Packs are selected, in descending precedence, by the repeatable `--pack` CLI flag,
the comma- or whitespace-separated `KHIVE_PACKS` environment variable, or
`[runtime].packs` in the discovered configuration file. Each non-empty layer replaces
the complete set; with no selection, khive loads the built-in production set.

```bash
kkernel mcp --pack kg --pack gtd --pack formal
# or
KHIVE_PACKS="kg,gtd,formal" kkernel mcp
# or in khive.toml
# [runtime]
# packs = ["kg", "gtd", "formal"]
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

## The Moodboard pack

`crates/khive-pack-moodboard/` is an opt-in experimental visual-media pack. Load it with its
required KG vocabulary:

```bash
kkernel mcp --pack kg --pack moodboard
# or
KHIVE_PACKS="kg,moodboard" kkernel mcp
```

It contributes the additive `artifact` subtypes `visual_asset`, `moodboard`, and
`moodboard_model`. Its ADR-148 visual path publishes original raster bytes to BlobStore, derives
an identity-bound Lattice descriptor, and performs exact descriptor-space retrieval through
`moodboard.model`, `moodboard.ingest`, and `moodboard.search`.

ADR-149 adds explicit interaction learning through `moodboard.serve`, `moodboard.judge`,
`moodboard.train_preference`, and `moodboard.preference`. These four verbs require a canonically
attributed non-`local` actor. Training uses immutable randomized pairwise judgments, deterministic
unordered-pair train/calibration/test splits, a frozen ten-feature contract, and minimum support
gates. It fits deterministic logistic binary cross-entropy in the pack, then persists and serves
the exact zero-intercept `10 -> 1` head through `lattice-fann` 0.7.1. FANN bytes, the calibrated
model bundle, and their provenance live in BlobStore, an `artifact/moodboard_model`, and immutable
events.

The learned result is a conditional pairwise-preference probability. It is deliberately returned
separately from conformal evidence, retrieval similarity, and any later board-level coherence
measure; wrong identity or insufficient calibration fails closed. See
[ADR-148](../adr/ADR-148-moodboard-visual-retrieval-pack.md) and
[ADR-149](../adr/ADR-149-moodboard-preference-learning.md) for the exact contracts.

## See also

- [Knowledge Graph Modeling](knowledge-graph.md): the base entity kind and
  edge relation taxonomy that specialized packs extend.
- [Agent Sessions and Data Ingest](sessions-and-ingest.md): another optional
  pack (`session`), included in the default set but with its own opt-in
  background service.
