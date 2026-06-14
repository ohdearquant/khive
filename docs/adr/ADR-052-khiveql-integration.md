# ADR-052: KhiveQL -- Unified Query Language, Pipeline Compiler, and Declarative Pack Vocabulary

**Status**: Proposed
**Date**: 2026-06-13

## Context

khive's query surface today is `khive-query` (~4.3K LOC): hand-written recursive-descent
parsers for GQL and SPARQL that compile pattern-matching queries to parameterized SQLite SQL,
with recursive CTEs for multi-hop traversal. It is a read-only pattern-matching layer: six to
eight AST node types (`GqlQuery`, `PatternElement`, `NodePattern`, `EdgePattern`, `WhereExpr`,
`ReturnItem`, `Condition`), 29 unit tests.

Everything else in khive's surface lives outside the language. Verb dispatch is the
function-call DSL parsed by `khive-request` ([ADR-016](ADR-016-request-dsl.md)). Pack
vocabulary is declared in Rust (`khive-pack-kg/src/vocab.rs`, [ADR-017](ADR-017-pack-standard.md)).
Edge endpoint rules are Rust `EDGE_RULES` consts. Retrieval configuration (embedder choice,
index parameters, fusion strategy, recall scoring) is hardcoded in `khive-runtime` wiring and
the memory pack's `RecallConfig`.

This split has three costs:

1. **No composable retrieval.** `SEARCH ... THEN TRAVERSE`, the hybrid-then-graph pipeline that
   is khive's core value, cannot be written as a query. It is assembled imperatively inside
   runtime operations.
2. **Pack authoring requires Rust.** Adding a verb means editing a handler match in
   `khive-pack-kg/src/handlers.rs`, recompiling, and shipping a binary. An agent cannot author
   a pack at runtime.
3. **Retrieval config is invisible.** A reader cannot see which embedder, index parameters, or
   scoring weights a pack uses without reading `khive-runtime` Rust source. There is no
   declarative contract.

This ADR proposes **KhiveQL (KQL)**: a single purpose-built query language, implemented in a new
`khive-ql` crate, that absorbs verb dispatch, pack vocabulary, retrieval configuration, and
composable retrieval pipelines into one declarative surface. `khive-query` (GQL/SPARQL) remains
for backward compatibility behind the `query` verb until migration completes.

## Design tenets (binding for all KQL grammar decisions)

1. **Model-writable**: an AI agent authors valid KQL in a single tool call on the first attempt.
   Consistent keywords, exactly one way to express each construct, no position-dependent meaning.
2. **Human-readable**: a `.kql` pack file reads as documentation of itself.
3. **Unambiguous, compiled-language behavior**: the parser rejects ambiguity. Every error
   surfaces at parse / boot / compile time where possible: no runtime surprises from grammar.
   One parse tree per input.
4. **No magic**: a small set of substrate primitives composed explicitly. Every behavior traces
   to a DEFINE statement or a NATIVE binding. No hidden built-ins, no implicit state.

Every syntax decision below is evaluated against these four.

## khive today vs. proposed KhiveQL

| Dimension          | khive-query (today)                           | KhiveQL (proposed)                                                                               |
| ------------------ | --------------------------------------------- | ------------------------------------------------------------------------------------------------ |
| Surface            | GQL, SPARQL                                   | One language; GQL/SPARQL retained behind `query` verb for compat                                 |
| Parser             | char-level recursive descent                  | lexer/tokenizer + token-level recursive descent, span-based errors                               |
| AST node types     | 6-8                                           | 30+ (Pipeline, Stage, DefineStmt variants, CRUD, Graph, Let, Assert, Call, If, For, Transaction) |
| Compiler target    | parameterized SQL strings (CTE for multi-hop) | abstraction-agnostic `ExecutablePlan` (staged); executor layer separate, no SQL lock-in          |
| Statement coverage | MATCH + WHERE + RETURN + LIMIT (read-only)    | retrieval pipelines, schema definitions, CRUD, graph ops, control flow, transactions             |
| DEFINE support     | none                                          | PACK, KIND, RELATION, VERB, INDEX, EMBEDDER, FUSION, RETRIEVAL PROFILE                           |
| Mutation           | none (read-only)                              | CREATE, UPDATE, DELETE, MERGE, LINK, UNLINK                                                      |
| Retrieval config   | hardcoded in Rust                             | declarative `.kql` (DEFINE EMBEDDER/INDEX/FUSION/PROFILE)                                        |
| Pack authoring     | Rust handlers + recompile                     | `.kql` files loaded at boot; NATIVE escape hatch for hot paths                                   |

KQL is **not** a SPARQL superset, SQL dialect, or Cypher clone. It borrows familiar syntax where
ergonomic (WHERE, LIMIT, ORDER BY, arrow patterns `a -> rel -> b`) but claims no standard
compatibility. Non-goals: full SPARQL 1.1, SQL DML, Cypher MERGE semantics, Turing-complete
scripting (FOR is pack-verb-only).

## Decision

Add a `khive-ql` crate (lexer + parser + AST + pipeline compiler, ~9-10K LOC). It depends on
`khive-types` + `khive-score` (the existing `DeterministicScore` fixed-point i64 model). It has
no external parser dependencies: hand-written recursive descent for performance, clear span
errors, and easy statement-type extension.

```
Input string
  -> Lexer       (tokens with spans)
  -> Parser      (recursive descent -> AST)
  -> Validator   (type-check against loaded pack vocabulary)
  -> Compiler    (AST -> ExecutablePlan)
  -> Executor    (plan -> storage operations -> results)
```

### 1. Statement categories

```
Schema:     DEFINE PACK | KIND | RELATION | VERB | INDEX | EMBEDDER | FUSION | RETRIEVAL PROFILE
CRUD:       CREATE | GET | UPDATE | DELETE | MERGE | LIST
Graph:      LINK | UNLINK | NEIGHBORS | TRAVERSE | MATCH
Retrieval:  SEARCH [HYBRID|KEYWORD|VECTOR] <query> [ON <kind>] [FUSE <strategy>] [PROFILE <name>] [LIMIT <n>]
Control:    LET | IF/ELSE | FOR | ASSERT | RETURN | BEGIN/COMMIT
Compose:    <stage> THEN <stage>   (SEARCH...THEN TRAVERSE, TRAVERSE...THEN SEARCH)
```

### 2. Pipeline compiler (the differentiator)

Retrieval statements compile to a staged `ExecutablePlan` rather than to SQL strings. This is the
load-bearing design choice: it removes the SQL lock-in that constrains `khive-query`, and it
gives an explicit optimization surface.

Key plan IR types:

- `ExecutablePlan`: ordered list of `StagePlan` entries
- `StagePlan`: enum: `Search`, `Traverse`, `Fuse`, `Stats`
- `SearchPlan`: `query_text`, `query_vec`, `k`, `rrf_k`, `mode`, `inline_fusion`, `min_score`,
  `profile_name`, `decay`, `weights`, `candidates_explicit`
- `CompiledDecay`: decay model + parameters for post-retrieval scoring
- `ScoringWeights`: three-signal weighting (relevance, salience, temporal)

The execution engine uses a fixed 9-opcode `Op` enum: `EvalExpr`, `ExecQuery`, `ExecCommand`,
`Assert`, `JumpIfFalse`, `Jump`, `ForInit`, `ForNext`, `Return`. **Every new statement form
lowers to these existing opcodes** via `CommandPayload` variants. The opcode set does not grow.
This is the central invariant of the execution layer: the language surface expands by adding
statement payloads, never opcodes, keeping the interpreter small and auditable.

The composed pipeline `SEARCH "attention" FUSE rrf LIMIT 5 THEN TRAVERSE DEPTH 2 RELATIONS
[extends, implements]` becomes two `StagePlan` entries that hand candidate sets forward, replacing
the imperative search-then-traverse logic currently inlined in `khive-runtime`.

### 3. DEFINE RELATION with PAIRS and SYMMETRIC

khive's edge endpoint contract today is the ADR-002 base plus pack-declared `EDGE_RULES` consts
in Rust ([ADR-017](ADR-017-pack-standard.md)). KQL moves the endpoint contract into the
vocabulary grammar.

The `PAIRS` clause replaces Cartesian-product `FROM/TO` with per-pair endpoint rules, so the
vocabulary itself expresses "concept->concept OR artifact->artifact but not concept->artifact"
without any Rust validation logic:

```kql
DEFINE RELATION supersedes PACK kg
  PAIRS (
    concept   -> concept,
    document  -> document,
    artifact  -> artifact,
    service   -> service,
    dataset   -> dataset,
    NOTE      -> NOTE
  );
```

`NOTE -> NOTE` expands to all 25 note-kind pairs at boot (substrate-level). Entity-side entries
cover only the five same-kind pairs. Cross-substrate and cross-kind pairs are absent, therefore
rejected, exactly today's `supersedes` semantics, with zero handler code.

`source_kind`/`target_kind` positions accept a named kind, the substrate keywords `ENTITY` / `NOTE`
(expand at boot), or wildcard `ANY`. `FROM/TO` remains as sugar for the full product (no grammar
break). The registry representation:

```rust
enum EndpointRuleForm {
    Product { sources: BTreeSet<Kind>, targets: BTreeSet<Kind> },
    PairSet { allowed: BTreeSet<(Kind, Kind)> },
}
```

`validate_edge` becomes the single enforcement point: Product checks `src in sources && tgt in
targets`; PairSet checks `(src, tgt) in allowed`.

The `SYMMETRIC` modifier replaces the manual canonical-ordering rules for `competes_with` /
`composed_with` with a declarative property:

```kql
DEFINE RELATION competes_with PACK kg
  SYMMETRIC
  PAIRS (concept -> concept, project -> project, service -> service);
```

When set: storage canonicalizes edge creation to `source_uuid < target_uuid` (byte order);
`validate_edge` accepts `(A,B)` and `(B,A)`; read paths (NEIGHBORS, TRAVERSE) return all incident
edges regardless of direction, deduplicated by edge id. The v1 contract is a bare `SYMMETRIC`
keyword (no boolean argument): one form only (tenet 1).

### 4. Engine statement extensions

Five generic statement forms the interpreter executes for any pack, each lowering to
`Op::ExecCommand`:

| Statement | Grammar                                                             | Backing store method                                                                                |
| --------- | ------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| LIST      | `LIST <kind> [WHERE] [ORDER BY] [LIMIT] [OFFSET] [INCLUDE DELETED]` | `record_list_filtered(ns, substrate, kind, include_deleted, limit, offset) -> (Vec<Record>, total)` |
| NEIGHBORS | `NEIGHBORS <id> [DIRECTION] [RELATIONS] [MIN WEIGHT] [LIMIT]`       | `edges_for_sources/targets` + Rust-side relation/weight filter                                      |
| MERGE     | `MERGE <from> INTO <to>`                                            | transaction: rewire edges onto target + soft-delete source                                          |
| STATS     | `STATS;`                                                            | `record_counts_by_kind(ns)` + `edge_count(ns)`                                                      |
| VERBS     | `VERBS;`                                                            | read-only `&VerbRegistry` introspection (no store access)                                           |

LIST return shape: `{ items, total, limit, offset }` where `total` is the full match count across
pages. STATS return shape mirrors today's stats response: `{ entities, notes, edges, by_kind }`.

MERGE preserves khive's entity-only, edge-rewiring semantics ([ADR-014](ADR-014-curation-operations.md)):
resolve both UUIDs, namespace-check both, reject non-entity substrate and self-merge, rewire
outgoing + incoming edges with dedup, soft-delete the source. It requires a new
`ScopedStore::transaction()` API (closure or begin/commit shape) so the rewire + soft-delete are
atomic. MERGE must not ship as non-transactional interpreter logic. A half-merged entity is
worse than a deferred MERGE.

### 5. Binding slots in verb bodies

Verb AS-block bodies bind `$variable` references with `?? fallback` syntax in kind, direction,
relation, weight, and query-text positions:

```kql
DEFINE VERB neighbors(id: uuid, direction?: string, relations?: array, min_weight?: float)
  PACK kg
  DESCRIPTION "Immediate neighbors of a record"
AS {
  NEIGHBORS $id
    DIRECTION $direction ?? "outgoing"
    RELATIONS $relations ?? ANY
    MIN WEIGHT $min_weight ?? 0.0;
};
```

A `Var` slot resolves at runtime by environment lookup; absent + fallback uses the fallback;
absent + no fallback is a `RuntimeTypeError`. Resolved values validate against the vocabulary
registry (unknown kind / relation / direction → error listing valid values). Tenet 3 prohibits
computed vocabulary membership: values are literal strings or validated variable references,
never expressions.

### 6. Retrieval vocabulary

Four DEFINE forms move retrieval configuration out of Rust into `.kql`:

```kql
DEFINE EMBEDDER memory_embed
    MODEL "sentence-transformers/all-MiniLM-L6-v2"
    DIMENSIONS 384
    PROVIDER local;

DEFINE INDEX memory_vamana
    ON memory
    TYPE vamana(dimensions: 384, max_degree: 64, search_list_size: 128, alpha: 1.2)
    EMBEDDER memory_embed;

DEFINE FUSION memory_fuse STRATEGY weighted(k0: 0.7, k1: 0.3);

DEFINE RETRIEVAL PROFILE memory_recall
    FUSION memory_fuse
    K 150
    MIN_SCORE 0.0
    DECAY exponential(decay_factor: 0.02)
    WEIGHTS relevance: 0.70, salience: 0.20, temporal: 0.10;
```

`TYPE <family>(<named-params>)` covers `vamana`, `hnsw`, `bm25`, exposing every knob from the
existing `VamanaConfig` / `HnswConfig` / `Bm25Config` Rust structs with the same defaults and
constraints (e.g. `search_list_size >= max_degree`, `alpha >= 1.0`, `b in [0,1]`). EMBEDDER is
required for `vamana`/`hnsw`, prohibited for `bm25`.

All four are boot-time vocabulary declarations. **They fail closed at boot** if any reference is
unresolvable (unknown embedder, dimension mismatch with the model's actual output size, missing
`lattice-embed` feature for `PROVIDER local`). Silent degradation (e.g. falling back to
keyword-only) would violate tenet 4.

`SEARCH` gains a `PROFILE <name>` clause. Inline clauses (`FUSE`, `CANDIDATES`, `LIMIT`, `USING`,
`WHERE`, `ON`) override the corresponding profile fields. `MIN_SCORE`, `DECAY`, `WEIGHTS` are
profile-only in v1. To use different values, use a different profile. Profiles are never
implicit; the binding is always visible in the verb body (tenet 4).

The decay models map to khive's memory-pack `DecayModel` formulas:

| Model         | Formula                                          |
| ------------- | ------------------------------------------------ |
| `exponential` | `salience * exp(-decay_factor * age_days)`       |
| `hyperbolic`  | `salience / (1 + decay_factor * age_days)`       |
| `power_law`   | `salience * hl / (hl + age_days)` (hl=half_life) |
| `none`        | identity                                         |

The composite recall score uses the linear decomposition `total = w_rel * relevance + w_sal *
salience_decayed + w_temp * temporal_recency`. (khive's memory pack also has a legacy
multiplicative form; the profile evaluation standardizes on the linear form, and parity QA must
quantify the ranking delta since the two rank differently.)

### 7. KQL-first packs

Packs become `.kql` files loaded at boot via `KHIVE_PACKS` / `--pack`, with all verbs as thin
AS-block wrappers over engine statements:

```kql
DEFINE VERB link(source: uuid, target: uuid, relation: string, weight?: float)
  PACK kg
  DESCRIPTION "Create a typed directed edge between two records"
AS { LINK $source -> $relation -> $target WEIGHT $weight ?? 1.0; };

DEFINE VERB search(query: string, kind?: string, limit?: integer) PACK kg
  DESCRIPTION "Hybrid full-text + vector search"
AS { SEARCH HYBRID $query ON $kind ?? ENTITY FUSE rrf LIMIT $limit ?? 20; };
```

A small number of verbs need the **NATIVE escape hatch** until the parser matures: verbs whose
semantics current grammar cannot express (a kind in `$variable` position, a `SET description =`
clause that collides with the `DESCRIPTION` keyword, a soft/hard-delete boolean branch needing
IF/ELSE, iterative BFS traversal with accumulator state). NATIVE binds a Rust handler to a
declared verb. It is the documented exception for host-integration and hot-path verbs
(memory.remember, brain verbs), not the default; each NATIVE verb carries a concrete unblocking
condition for its eventual migration to an AS block.

### Adaptation for khive

- khive's closed `EntityKind` (8) and `EdgeRelation` (15) enums remain authoritative.
  ([ADR-001](ADR-001-entity-kind-taxonomy.md), [ADR-002](ADR-002-edge-ontology.md)). KQL
  `DEFINE KIND` / `DEFINE RELATION` declarations validate against the compile-time enums; KQL
  does not introduce an open string-typed kind/relation vocabulary. The closed taxonomy is a
  deliberate khive contract, not a limitation to relax (see [ADR-055](ADR-055-storage-modernization.md)
  for the foundational-types rationale).
- khive's `Pack` trait ([ADR-017](ADR-017-pack-standard.md)) coexists with `.kql` declarations:
  existing Rust packs keep their trait impls; new packs are `.kql`-first. Migration is
  pack-by-pack, with verb-parity tests gating each.
- The pack-extensible endpoint mechanism (additive-only, packs cannot tighten the base) is
  preserved: PAIRS declarations are the additive surface; cross-pack relation extension (e.g. gtd
  adding `task -> task` to `depends_on`) follows the same additive rule.

## Migration path

1. Add `khive-ql` crate (lexer, parser, AST, pipeline compiler).
2. Add `ScopedStore::transaction()` to `khive-storage` / `khive-db` (MERGE prerequisite).
3. Wire `ExecutablePlan` execution into `khive-runtime` alongside existing operations.
4. Author `.kql` files for kg, memory, gtd packs; add NATIVE binding support to the
   `VerbRegistry`.
5. Migrate pack-by-pack: `.kql` replaces Rust handler registration, with per-pack verb-parity
   tests.
6. Deprecate `khive-query` (and the `khive-request` function-call DSL) once all consumers route
   through KQL.

## Consequences

- Composable retrieval (`SEARCH ... THEN TRAVERSE`) becomes a first-class query, replacing
  imperative pipeline assembly in `khive-runtime`.
- Pack authors (including AI agents) write `.kql`, not Rust, for vocabulary and verb bodies.
- Edge endpoint validation moves entirely into PAIRS declarations with `validate_edge` as the
  single enforcement point, retiring the Rust `EDGE_RULES` workaround pattern.
- Retrieval configuration (embedder, index params, fusion, scoring) becomes a declarative,
  readable contract in the pack file instead of hardcoded Rust defaults.
- The staged `ExecutablePlan` model is a real optimization surface (per-stage planning,
  multi-index routing) that SQL-string compilation cannot offer.
- ~9-10K LOC added (`khive-ql`), eventual removal of ~4K (`khive-query`) and a large reduction in
  per-pack Rust handler boilerplate. `khive-query` GQL/SPARQL stays behind the `query` verb until
  migration completes.
- Schema/grammar changes require an ADR (this remains an ADR-governed surface).
