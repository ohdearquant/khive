# ADR-013: Note Kind Taxonomy

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: khive maintainers

## Context

Notes are how agents and humans record observations, insights, decisions, questions,
and references alongside the entity-and-edge graph. They are addressable, queryable,
and persist across sessions.

The note substrate (ADR-004) supports a kind discriminant that drives:

1. **Validation** — which fields are required, which are optional, which are forbidden.
2. **Lifecycle** — what state transitions are legal for a registered kind.
3. **Search profile** — which fields are indexed, with what weights.
4. **Discrimination** — `search(kind="note", note_kind="question")` returns only open
   questions.

The taxonomy must satisfy:

1. **A canonical cognitive vocabulary.** Without one, agents invent kinds ad-hoc
   ("memo", "obs", "finding") and discrimination collapses.
2. **Pack extensibility.** Future packs may add their own kinds. The taxonomy cannot
   be a closed Rust enum.
3. **Runtime validation.** Unknown kinds must be rejected at write time, not silently
   accepted.
4. **Supersession semantics.** Notes evolve; the earlier observation gets refined.
   Supersession must be history-preserving (the old note stays) and explicit.

## Decision

### Base taxonomy: five cognitive kinds, owned by the `kg` pack

The `kg` pack registers five note kinds. They map to cognitive functions an agent
performs while researching:

| Kind            | What it records                                         | Example                                                              |
| --------------- | ------------------------------------------------------- | -------------------------------------------------------------------- |
| **observation** | An empirical capture — what was noticed or measured     | "The benchmark in §4.2 uses only English data"                       |
| **insight**     | A synthetic conclusion drawn from observations          | "FlashAttention's gains scale with sequence length, not batch size"  |
| **question**    | An open inquiry, research direction, or unknown         | "Why does this attention mechanism require fp32 accumulation?"       |
| **decision**    | A committed choice with rationale                       | "Use BGE-base instead of small — multilingual data in target corpus" |
| **reference**   | An external pointer with context (paper, URL, citation) | "See arxiv:2205.14135 §3 for the tile-quantization derivation"       |

```text
I noticed     → observation
I synthesized → insight
I don't know  → question
I chose       → decision
I read        → reference
```

These five are mutually exclusive and jointly exhaustive for research-KG cognition.
Five was chosen over three (too coarse — loses observation/insight distinction) and
over seven (granularity buys nothing — `hypothesis`, `summary`, `critique`, `analogy`
overlap with `insight` and dilute it).

Adding a sixth base kind requires a new ADR amending this one. The base taxonomy is
small on purpose: agents pick from five, not from twenty.

### Kind is a string validated against `NoteKindSpec` registrations

Note kinds are runtime-validated strings, not a closed Rust enum. The `NoteKindSpec`
framework lives in ADR-004; this ADR specifies which kinds exist and what each is.

```rust
// In khive-pack-kg
fn note_kinds(&self) -> Vec<NoteKindSpec> {
    vec![
        NoteKindSpec::new("observation").with_lifecycle(...).with_fields(...),
        NoteKindSpec::new("insight").with_lifecycle(...).with_fields(...),
        NoteKindSpec::new("question").with_lifecycle(...).with_fields(...),
        NoteKindSpec::new("decision").with_lifecycle(...).with_fields(...),
        NoteKindSpec::new("reference").with_lifecycle(...).with_fields(...),
    ]
}
```

The runtime accumulates registrations from all loaded packs at boot. Collisions across
packs are boot-time errors (ADR-004). Unknown kinds at write time return
`RuntimeError::UnknownNoteKind { kind, registered: Vec<String> }` with the full
registered set in the error message.

### Pack-registered note kinds

Other packs may register kinds additive to the base five. These are not "second-class
kinds." They are full peers of the base five — same
storage substrate, same supersession rules, same edge ontology. The kg pack owns the
cognitive vocabulary; other packs own their domain vocabularies. The runtime composes
all registrations into one note-kind registry per deployment.

KG-only deployments see only the base five.

### Per-kind property conventions

Each kind has typical properties — conventions, not enforced schemas. Properties stay
free-form JSON; `NoteKindSpec.fields` can declare required/optional fields per kind
when stricter validation is needed.

| Kind        | Suggested properties                                                                        |
| ----------- | ------------------------------------------------------------------------------------------- |
| observation | `entity_id` (what was observed about), `source` (paper section, URL), `confidence` 0–1      |
| insight     | `entity_ids[]` (entities the insight connects), `derived_from` (note ids), `confidence` 0–1 |
| question    | `urgency` (low/medium/high), `entity_ids[]` (what the question concerns), `resolved` bool   |
| decision    | `alternatives_considered[]`, `rationale` (long text), `entity_ids[]` (decision touches)     |
| reference   | `source_url`, `cite_key` (BibTeX-style), `paper_section`, `quoted_text`                     |

Agents populate these when relevant. Recall queries can filter on them.

### Supersession via edge, not field

A note can be superseded by a later note when the agent's understanding evolves: an
earlier observation is refined, an earlier decision is revised. Supersession is
**history-preserving** — the original note stays in the store, a later note is marked
as its successor.

Supersession is expressed via the `supersedes` edge relation from ADR-002, not via a
column on the `notes` table:

```text
new_note --supersedes--> old_note
```

The link is created via `link(source_id=new_id, target_id=old_id, relation="supersedes",
weight=1.0)`. There is no `superseded_by` column on `notes`. The edge is the single
source of truth.

**Chains are graph walks.** If A is superseded by B and B by C:

```text
C --supersedes--> B --supersedes--> A
```

To find the latest version, traverse `direction="in"` on `supersedes` edges from A
until no incoming edge exists. To check "is this note current?", check that no
`supersedes` edge has it as target.

**Search filters superseded notes by default.** Retrieval (ADR-012, `search_notes`)
excludes notes that have an incoming `supersedes` edge. Callers can opt in to seeing
the full chain with an explicit flag (e.g., `include_superseded: true`).

**There is no "unsupersede."** If a supersession was wrong, the agent creates a new
note that supersedes the superseding one. The chain stays auditable and forward-only.

**Same mechanism for entities.** Entities also use `link(..., relation="supersedes")`.
One relation, one mechanism, two substrate kinds.

**Annotations do not transfer through supersession;** they are per-note and
explicit; a superseding note annotates only what its own `annotates` declares. The
old note's `annotates` edges are never copied or inherited. "Show only current" is a
query/view decision, never a reason to mutate or transfer data (per the data-vs-view
principle).

### Default kind

When a note is created without an explicit kind, the kg pack defaults to `observation`.
This is the lowest-commitment kind — "I just noticed something worth recording." Other
packs may define their own defaults when called through their own verbs.

### Search and discrimination

Note retrieval (ADR-012) accepts a `note_kind` filter:

```text
search(kind="note", note_kind="question") → only open questions
search(kind="note", note_kind="decision") → only decisions
search(kind="note")                       → all kinds, mixed
```

Multi-kind filters (e.g., `note_kind=["observation", "insight"]`) are supported when
the caller wants a focused subset.

### Aliases

`NoteKindSpec` does not standardize aliases. Each pack documents the canonical kind
string only — `observation`, `insight`, `question`, `decision`, and `reference` for the
base five. The MCP wire layer accepts only the canonical form.

This rejects the earlier alias surface (`obs`, `q`, `ref`) because:

- Aliases multiply across packs because every pack can invent its own.
- Cross-pack alias collisions become a governance problem.
- The cost of typing the full kind name is negligible.
- Documentation stays simple — one name per kind.

If alias parsing is needed for ergonomics, callers can implement it client-side. The
runtime contract is exact-match.

## Rationale

### Why five base kinds?

Three felt too coarse — "Note / Question / Reference" loses the distinction between
empirical capture (observation) and synthetic conclusion (insight), which is the
foundation of research. Seven felt too granular — `hypothesis`, `summary`, `critique`,
`analogy` all overlap with `insight` and dilute it.

Five maps cleanly to cognitive functions and forces classification discipline. Agents
that see five kinds and pick one are forced to think about classification rather than
coining a new term. The discipline pays off in retrieval quality.

### Why pack-extensible (not closed enum)?

ADR-004 establishes that note kinds are pack-registered through `NoteKindSpec`. This
ADR specifies what the kg pack registers and acknowledges what other packs register.
Keeping kinds in a closed Rust enum in `khive-types` would force every new pack to
amend `khive-types` — a foundational crate that should not change for pack additions.

The base five stay stable. New packs add their own kinds without touching the kg pack
or the substrate definition.

### Why supersession as edge, not field?

A column would couple supersession to the notes table. Edges work for entities too
(ADR-002), so the same mechanism extends across substrates. Storing supersession as an
edge means:

- Same query mechanism for "what supersedes this?" and "what does this depend on?"
- Same cascade behavior (per ADR-002 supersession cascade rules)
- Same versioning behavior (snapshot covers edges)
- No need for special-purpose lookups on the notes table

Supersession is a relationship, and relationships are edges.

### Why no aliases?

Aliases were tempting for ergonomics but become a governance problem at pack scale.
The kg pack could allow `obs`, `q`, `ref` — but every pack would want its own
abbreviations, collisions appear, and the wire format gets ambiguous. Documentation
stays simple when there's one canonical string per kind.

### Why `observation` as default?

When an agent creates a note without specifying a kind, they're usually capturing
something they just noticed — not a fully formed insight, not a decision. `observation`
is the lowest-commitment kind, which makes it the right default for kg-pack notes.
Other packs define their own defaults when called through their own verbs.

### Why notes and entities share the supersession relation?

Both substrates have the same need: "this newer thing replaces that older thing." One
relation, one mechanism, two substrates is simpler than two parallel mechanisms. The
edge ontology (ADR-002) already handles cross-substrate edges via endpoint rules.

## Alternatives Considered

| Alternative                                            | Why rejected                                                                           |
| ------------------------------------------------------ | -------------------------------------------------------------------------------------- |
| Keep `NoteKind` as a closed Rust enum in `khive-types` | Forces foundational-crate changes for every pack kind.                                 |
| Open string with no validation                         | Same failure mode that ADR-001 fixed for entity kinds.                                 |
| 3 kinds (Note / Question / Reference)                  | Loses observation/insight/decision — the most useful research distinctions.            |
| 7+ kinds (add hypothesis, summary, critique, analogy)  | Granularity buys nothing; agents have to disambiguate near-synonyms.                   |
| Supersession as `superseded_by: Uuid` column on notes  | Couples to the notes table; doesn't extend to entities. Edge mechanism already exists. |
| Aliases (`obs`, `q`, `ref`)                            | Multiplies across packs; collision risk; saves negligible typing.                      |
| Zettelkasten model (fleeting / literature / permanent) | Built for human note-taking, not research agents.                                      |

## Consequences

### Positive

- Five canonical cognitive kinds give agents a finite vocabulary — no decision fatigue.
- Pack-extensible: future packs add kinds without changing foundational crates.
- Supersession is graph-native — same mechanism for entities and notes.
- Search discrimination works: `note_kind="question"` returns only open questions.
- Cross-session, cross-agent consistency — every "observation" means the same thing.
- The view-layer rule holds: "show only current" filters superseded notes in queries;
  the underlying data preserves the chain.

### Negative

- Removing `khive-types::NoteKind` enum is a breaking change for any direct consumers.
  Mitigated: the type can remain as a convenience constant set for the base five
  (`NoteKindSpec::OBSERVATION`, etc.) without being the validation authority.
- No aliases means agents must use canonical strings.
  Mitigated: five strings to memorize is not a meaningful burden.
- Supersession as edge means "is this current?" requires an edge query, not a column
  read.
  Mitigated: the query is one indexed lookup; runtime helpers wrap it.

### Neutral

- Per-kind property conventions remain conventions, not enforced schemas — unless a
  pack opts into stricter validation via `NoteKindSpec.fields`.
- `default()` returns `observation` for kg pack notes; other packs define their own
  defaults.
- The set of allowed `supersedes` endpoint pairs (entity→entity, note→note) is owned
  by ADR-002 and pack-extensible per **ADR-017** (Pack Standard).

## Implementation

- `crates/khive-pack-kg/src/vocab.rs`: registers the five base note kinds via
  `NoteKindSpec`.
- `crates/khive-runtime/src/registry.rs`: aggregates note kind registrations from
  loaded packs; rejects collisions at boot.
- `crates/khive-runtime/src/operations.rs`: `create_note` validates kind against the
  runtime registry. `search_notes` filters by `supersedes` edge presence.
- `crates/khive-types/src/note.rs`: `Note.kind` field is `String`. The convenience
  enum `NoteKind` may remain for the base five but is not the validation authority.

## References

- ADR-002: Closed Edge Ontology — `supersedes` relation, endpoint rules.
- ADR-004: Substrate Observables — `NoteKindSpec` framework, lifecycle/search profile.
- ADR-012: Retrieval Composition — `search_notes`, supersession filtering, multi-kind
  filters.
- ADR-017: Pack Standard — how packs declare kind registrations.
- ADR-017: Pack Standard (§EDGE_RULES) — how packs add legal endpoint pairs.
