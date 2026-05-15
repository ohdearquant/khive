# ADR-019: Note Kind Taxonomy

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

Notes (the `Note` substrate from ADR-004) are how agents and humans record observations, insights,
decisions, and questions alongside the entity-and-edge graph. They are addressable, queryable, and
persist alongside the KG.

The current `Note.kind` field is a free-form `String`. Note creation accepts any string for `kind` —
there's no validation, no normalization, no constraint. This produces the same failure mode the
entity-kind taxonomy (ADR-001) fixed:

1. No compile-time guarantees about valid kinds.
2. Inconsistent naming across agents and sessions ("note" vs "memo" vs "observation" vs "obs").
3. No discriminative power — every search returns mixed shapes.
4. Agents have no taxonomy to write _against_ — they invent kinds ad hoc.

With ADR-001 (entity kinds, 6 closed) and ADR-002 (edge relations, 13 closed) already locking the
rest of the substrate, leaving notes as free-string strays from the pattern.

This ADR closes the gap.

## Decision

**5 note kinds, defined as `NoteKind` enum in `khive-types`:**

| Kind            | What it records                                               | Example                                                              |
| --------------- | ------------------------------------------------------------- | -------------------------------------------------------------------- |
| **Observation** | An empirical capture — what was noticed or measured           | "The benchmark in section 4.2 uses only English data"                |
| **Insight**     | An analytical or synthetic conclusion drawn from observations | "FlashAttention's gains scale with sequence length, not batch size"  |
| **Question**    | An open inquiry, research direction, or unknown               | "Why does this attention mechanism require fp32 accumulation?"       |
| **Decision**    | A committed choice with rationale                             | "Use BGE-base instead of small — multilingual data in target corpus" |
| **Reference**   | An external pointer with context (paper, URL, citation note)  | "See arxiv:2205.14135 §3 for the tile-quantization derivation"       |

These five are **closed and exhaustive**. Adding a sixth requires a new ADR.

```rust
// crates/khive-types/src/note.rs
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteKind {
    Observation,
    Insight,
    Question,
    Decision,
    Reference,
}

impl Default for NoteKind {
    fn default() -> Self {
        Self::Observation
    }
}

impl std::fmt::Display for NoteKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            Self::Observation => "observation",
            Self::Insight => "insight",
            Self::Question => "question",
            Self::Decision => "decision",
            Self::Reference => "reference",
        };
        f.write_str(s)
    }
}

impl std::str::FromStr for NoteKind {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.trim().to_ascii_lowercase().as_str() {
            "observation" | "obs" => Ok(Self::Observation),
            "insight" | "finding" => Ok(Self::Insight),
            "question" | "q" => Ok(Self::Question),
            "decision" | "choice" => Ok(Self::Decision),
            "reference" | "ref" | "citation" => Ok(Self::Reference),
            other => Err(format!(
                "unknown note kind: {other:?}. Valid: observation | insight | question | decision | reference"
            )),
        }
    }
}
```

The `Note` struct in `khive-storage` changes from `pub kind: String` to `pub kind: NoteKind`. The
MCP `create(kind="note", ...)` handler validates the `note_kind` parameter on input and returns a
clear error for unknown values.

## Per-kind property guidance

Like entities, each note kind has typical properties — these are **conventions**, not enforced
schemas. Properties stay free-form JSON.

| Kind        | Suggested properties                                                                        |
| ----------- | ------------------------------------------------------------------------------------------- |
| Observation | `entity_id` (what was observed about), `source` (paper section, URL), `confidence` 0–1      |
| Insight     | `entity_ids[]` (entities the insight connects), `derived_from` (note ids), `confidence` 0–1 |
| Question    | `urgency` (low/medium/high), `entity_ids[]` (what the question concerns), `resolved` bool   |
| Decision    | `alternatives_considered[]`, `rationale` (long text), `entity_ids[]` (decision touches)     |
| Reference   | `source_url`, `cite_key` (BibTeX-style), `paper_section`, `quoted_text`                     |

Agents should populate these when relevant. Recall queries can filter on these properties.

## Rationale

### Why five?

Three felt too coarse ("Note / Question / Reference" loses the distinction between empirical capture
and synthetic insight, which is the foundation of research). Seven felt too granular (`hypothesis`,
`summary`, `critique`, `analogy` are all valid concepts but overlap with `insight` and dilute it).

Five maps cleanly to cognitive functions an agent performs while researching:

1. _I noticed_ → **observation**
2. _I synthesized_ → **insight**
3. _I don't know_ → **question**
4. _I chose_ → **decision**
5. _I read_ → **reference**

These are mutually exclusive and jointly exhaustive for research-KG use cases.

### Why closed?

Same logic as ADR-001 and ADR-002: a closed taxonomy prevents agents from inventing variants. If
agents see five kinds and can pick one, they're forced to _think_ about classification rather than
coining a new term. The discipline pays off in retrieval quality.

If a use case genuinely needs a sixth kind, the right path is an ADR amendment — not a silent string
extension.

### Why `Observation` as the default?

When an agent creates a note without specifying a kind, they're usually capturing something they
just noticed — not a fully formed insight, not a decision, just "this seems worth noting."
`Observation` is the lowest-commitment kind, which makes it the right default.

### What about Note vs Memory?

Memory in this system is implemented as notes. There is no separate "memory" substrate. Notes are
written via `create(kind="note", ...)` and searched via `search(kind="note", ...)` (per ADR-023).
The storage type is `Note`; there is no `remember`/`recall` pair on the agent surface — those words
carry implicit memory semantics that may not match what the system actually does (it stores a typed
Note with optional graph edges).

## Alternatives Considered

| Alternative                                            | Pros                 | Cons                                                                                              | Why rejected                                    |
| ------------------------------------------------------ | -------------------- | ------------------------------------------------------------------------------------------------- | ----------------------------------------------- |
| Keep as free String                                    | Maximum flexibility  | No discipline, drift, inconsistent kinds across sessions                                          | Same failure mode as ADR-001 fixed for entities |
| 3 kinds (Note/Question/Reference)                      | Smaller surface      | Loses observation/insight/decision distinction — those are the most useful for research workflows | Too coarse                                      |
| 7+ kinds (add hypothesis, summary, critique, analogy)  | More expressive      | Agents would have to disambiguate "insight" vs "hypothesis" vs "summary" — likely to drift        | Granularity buys nothing                        |
| Open enum with deprecation policy                      | Forward-compat       | Requires governance machinery; OSS doesn't have a steward                                         | Premature                                       |
| Zettelkasten model (fleeting / literature / permanent) | Established taxonomy | Built for human note-taking workflow, not research agents                                         | Wrong domain                                    |

## Consequences

### Positive

- MCP `create(kind="note", ...)` gets a typed `note_kind` parameter with clear documentation of the
  five values.
- Agents have a finite set to choose from — eliminates "what kind should I use?" decision fatigue.
- Searches become discriminating — `search(kind="note", note_kind="question", ...)` returns only
  open questions, not mixed shapes.
- Cross-session and cross-agent consistency — every "observation" means the same thing.

### Negative

- Breaking change for any existing notes with non-canonical kind strings — migration required.
  Acceptable since the OSS is pre-alpha and no real user data exists yet.
- The 5 kinds may not fit every research workflow perfectly. Mitigation: `properties` are still
  free-form JSON, agents can encode finer distinctions there.

### Neutral

- Adds ~30 LOC to `khive-types` and minor changes downstream (storage, runtime, MCP).
- The Display + FromStr impls expose aliases (obs, q, ref) for ergonomic agent input —
  case-insensitive parsing.

## Supersession

A note can be superseded by a later note when the agent's understanding evolves: an earlier
`observation` is refined into a more accurate one, an earlier `decision` is revised, etc.
Supersession is **history-preserving** — the original note stays in the store, and a later note is
marked as its successor. Search excludes superseded notes by default.

**Supersession is an edge, not a field.** Notes are first-class graph nodes (ADR-024); the
`supersedes` relation in the closed edge ontology (ADR-002) already expresses "new replaces old".
Notes use the same mechanism as entities:

```text
new_note --supersedes--> old_note
```

The `supersede` verb (per ADR-023) is shorthand for
`link(source=new_id, target=old_id, relation="supersedes", weight=1.0)`. There is no `superseded_by`
column on the `notes` table — the edge is the single source of truth.

**Chains are graph walks.** If A is superseded by B and B by C, the graph contains:

```text
C --supersedes--> B --supersedes--> A
```

To find the latest version in a chain, traverse `direction="in"` on `supersedes` edges from A until
no incoming `supersedes` edge exists. To check "is this note current?", check that no `supersedes`
edge has it as target. The runtime exposes this via a helper, but it's just a one-step graph query.

**There is no "unsupersede".** If a supersession was wrong, the agent creates a new note that
supersedes the superseding one (the chain stays auditable and forward-only).

**Same mechanism for entities.** `supersede(kind="entity", new_id, old_id)` does the same thing for
entity nodes — one verb, one relation, one mechanism, two substrate kinds.

## Implementation Plan

1. **`khive-types`**: add `NoteKind` enum with `Default`, `Display`, `FromStr` (case-insensitive,
   alias-supporting). ~50 LOC + tests.
2. **`khive-storage::Note`**: change `kind: String` → `kind: NoteKind`. Update
   `Note::new(ns, kind: NoteKind, content)`. ~10 LOC.
3. **`khive-db::stores::note`**: SQL column stays TEXT; serialize via `Display`, parse via `FromStr`
   on read. Add migration only if any existing rows need re-typing (pre-alpha = no rows to migrate).
   ~15 LOC.
4. **`khive-runtime::operations::create_note`**: signature
   `create_note(ns, kind: NoteKind, content, salience)`. Callers pass `NoteKind::Observation` for
   the default. ~5 LOC.
5. **MCP `create` handler (note branch)**: the `note_kind: Option<String>` wire field is parsed
   through `NoteKind::from_str` at the boundary; invalid values return `invalid_params` with the 5
   valid kinds enumerated. Update tool description accordingly. ~20 LOC.
6. **MCP `list` and `search` handlers (note branch)**: same — `note_kind: Option<String>` parsed and
   validated.
7. **Tests**: 1 unit test per kind (parse + display roundtrip + default), 1 integration test for the
   MCP error path with an unknown kind.

## Open Questions

1. **Should `Reference` notes also produce a structured Edge?** When an agent writes a reference
   note about an entity, conceptually that's a "this entity has a citation" relationship. We could
   auto-create an edge of relation `introduced_by` from the entity to a Document entity representing
   the cited paper. Defer — needs ADR-002 expansion or a citation-specific relation.
2. **Salience by kind?** Should `Decision` and `Reference` notes default to higher salience than
   `Observation` (since they're harder to recreate)? Or stay uniform at 0.5? Stay uniform; let
   callers tune.
3. **Auto-link on insight?** When an `Insight` note mentions entity IDs in its properties, should
   the runtime auto-create edges connecting those entities? Defer; this is a "smart notes" feature,
   not a substrate concern.

## References

- ADR-001: Entity Kind Taxonomy (same pattern, 6 entity kinds)
- ADR-002: Closed Edge Ontology (13 canonical relations)
- ADR-004: Substrate Observables (Note is one of the three observables)
- ADR-014: KG Curation Operations (notes participate in update/delete; `merge(kind="note")` is
  defined in ADR-023)
