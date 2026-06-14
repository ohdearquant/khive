# ADR-052: KhiveQL Integration

**Status**: Proposed
**Date**: 2026-06-13
**Origin**: khivedb ADR-101/110/111 (salvage)

## Context

khive-query provides GQL and SPARQL parsers that compile to SQL. This works
for pattern matching but cannot express retrieval pipelines (SEARCH + FUSE +
TRAVERSE), pack schema declarations (DEFINE PACK/KIND/VERB), or embedder
infrastructure (DEFINE EMBEDDER/INDEX/FUSION/PROFILE).

khivedb developed KhiveQL, a purpose-built query language with 9.9K LOC of
lexer, parser, and pipeline compiler (khivedb-ql). Head-to-head comparison
shows khivedb-ql is the successor design: it covers data definition, retrieval
pipelines, and control flow where khive-query only covers read-only pattern
matching.

## Decision

Port khivedb-ql into khive as a new crate (`khive-ql`), coexisting with
khive-query until migration is complete. KhiveQL becomes the primary language
for pack definitions, verb bodies, and retrieval pipelines.

### What transfers

1. **Lexer + parser** (lex/, parse/): recursive-descent, span-based errors,
   30+ AST node types including DEFINE statements.
2. **Pipeline compiler** (exec/): SearchPlan, ExecutablePlan, StagePlan,
   CompiledDecay, ScoringWeights. Compiles SEARCH/FUSE/TRAVERSE into
   executable stage plans.
3. **DEFINE statements**: PACK, KIND, RELATION, VERB (AS-block + NATIVE),
   EMBEDDER, INDEX, FUSION, RETRIEVAL PROFILE.
4. **Vocabulary system**: `load_packs()` from .kql files, VerbRegistry
   integration, native verb binding.

### What stays in khive-query

GQL and SPARQL parsing remain available for backward compatibility. The
`query` verb continues to route through khive-query. New pack verbs use
KhiveQL exclusively.

### Adaptation for khive

- khivedb-ql depends on khivedb-core; ported crate depends on khive-types +
  khive-score (same DeterministicScore, same type foundation).
- khive's closed EntityKind/EdgeRelation enums remain authoritative; KQL
  DEFINE KIND/RELATION declarations validate against them.
- khive's Pack trait coexists with .kql declarations: existing Rust packs
  keep their trait impls, new packs are .kql-first.

### Binding tenets (from khivedb ADR-110)

1. **Model-enterable**: an AI agent can write valid KQL in a single tool call.
2. **Human-readable**: a developer can read .kql pack files without docs.
3. **Unambiguous/compiled-like**: the parser rejects ambiguity; no runtime
   type coercion or implicit defaults.
4. **No magic**: every behavior traces to a DEFINE statement or a NATIVE
   binding.

## Migration path

1. Add khive-ql crate (port khivedb-ql, swap core dependency).
2. Write .kql files for kg, memory, gtd packs (coexist with Rust packs).
3. Add NATIVE binding support to VerbRegistry.
4. Migrate pack-by-pack: .kql replaces Rust handler registration.
5. Deprecate khive-query when all consumers use khive-ql.

## Consequences

- Pack authors write .kql instead of Rust for vocabulary and verb bodies.
- NATIVE escape hatch preserves Rust hot paths (memory.remember, brain verbs).
- Pipeline compilation replaces ad-hoc search dispatch in khive-runtime.
- ~10K LOC addition (khive-ql) but eventual removal of ~4K (khive-query) and
  significant reduction in per-pack handler boilerplate.
