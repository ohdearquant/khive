# ADR-048: Knowledge Section Profiles

**Status**: accepted
**Date**: 2026-05-27
**Authors**: Ocean, lambda:khive

## Current Implementation Status

This ADR is accepted as the governing record for shipped knowledge sections and section
profile primitives, while explicitly deferring broader resource dual-write, profile-weighted
compose/suggest, hooks, lint, export, and observability phases.

| Area                                                          | Status   | Shipped behavior                                                                                                                                                                                                                                                                                                                                           |
| ------------------------------------------------------------- | -------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `knowledge_sections`                                          | shipped  | Dedicated section rows with 10-value `SectionType`, `content_hash` + `UNIQUE(atom_id, content_hash)`, nullable `embedding`, section indexes, `fts_sections`, and FTS5 triggers. 80-char minimum content.                                                                                                                                                   |
| Section write-side embedding backfill                         | shipped  | `kkernel reindex` and `knowledge.edit` (inline atom-scoped re-embed) populate `knowledge_sections.embedding` via breadcrumb-enriched embed text (ADR-051 phase 1). Direct section-cosine scoring in `knowledge.search` and `knowledge.compose` is shipped (ADR-051 phase 2). Profile-weighted compose and the Vamana section ANN snapshot remain deferred. |
| V22 lifecycle/source fields                                   | shipped  | Status/source columns on atoms, status columns on sections/domains, status indexes, and finalized atom backfill to `reviewed`.                                                                                                                                                                                                                             |
| `knowledge.edit`                                              | shipped  | Upserts sections content-addressed by `content_hash`; identical content is idempotent, distinct content inserts a sibling row, and existing siblings (including verified ones) are left untouched.                                                                                                                                                         |
| `knowledge.import`                                            | shipped  | Supports `atlas_md` files/directories with `chunk_strategy=section                                                                                                                                                                                                                                                                                         |
| `knowledge.challenge` / `knowledge.adjudicate`                | shipped  | Challenge moves eligible sections to `disputed` and increments atom `dispute_count`; adjudicate requires disputed sections and resolves accept -> `verified`, reject -> `reviewed`.                                                                                                                                                                        |
| Brain section posterior primitives                            | shipped  | Brain state, fold, feedback parsing, and `brain.create_profile(seed_priors.section_posteriors)` exist for section posteriors.                                                                                                                                                                                                                              |
| `knowledge.suggest` / `knowledge.compose` profile weighting   | deferred | `suggest` is domain-oriented search with optional Vamana signal; `compose` uses explicit `domain_ids`/`atom_ids` and formats atom-body markdown. Neither resolves `brain` profiles or emits section-weighted manifests.                                                                                                                                    |
| Resource entity dual-write                                    | deferred | `knowledge.upsert_atoms` and `knowledge.upsert_domains` write corpus tables; domain mirror is into `knowledge_atoms` for FTS, not graph `entities`.                                                                                                                                                                                                        |
| `knowledge.lint`, `knowledge.lint_config`, `knowledge.export` | deferred | These verbs are not registered in the shipped knowledge pack.                                                                                                                                                                                                                                                                                              |

## Governance for Shipped Knowledge Sections and Profiles

ADR-048 treats knowledge sections as shipped corpus storage and lifecycle state, while
profile persistence remains owned by the brain pack. The knowledge pack does not ship a
`knowledge_profiles` table or knowledge-local profile verbs. Shipped profile persistence
is the V20 brain profile snapshot/event-log model, and section posterior learning is
driven through brain profile state and feedback events.

`knowledge_sections` is the authoritative table for atom sections. Sections are
**content-addressed**: `knowledge.edit` upserts each supplied section by
`(atom_id, content_hash)`. Byte-identical content is an idempotent metadata refresh
(status and embedding preserved); content not already present is inserted as a new row, so
repeated section types with differing content coexist as sibling rows. Existing sibling
sections — including verified ones — are never overwritten. `knowledge.import` supports
`atlas_md` file/directory import and, with the default section chunk strategy, parses
section headings into section rows. For a section-only document (an empty or short
pre-section body), import synthesizes the atom's `content` from its section bodies so the
atom satisfies the content minimum and remains searchable at the atom level.

### Atom and section content constraints

The `knowledge_atoms` table stores atom body text in a single `content` column. There is
no separate `description` column — content **is** the atom's description. The
`knowledge.upsert_atoms` verb accepts `content` only; there is no `description` input
alias. Atom content must be **at least 20 words**; shorter content is rejected at write
time as a stub.

`knowledge_sections.content` must be **at least 80 characters**; shorter section content is
rejected as a stub. Each section row carries a `content_hash` column holding the first 16
hex characters of `sha256(content)`. The uniqueness key is `UNIQUE(atom_id, content_hash)`
— multiple sections of the same `section_type` are legitimate as long as their content
differs, and exact-duplicate content for a given atom collapses onto the existing row via
the hash key rather than being stored twice. `knowledge.edit` resolves the target section
by `(atom_id, content_hash)`: a hash hit refreshes that row's metadata, a miss inserts a
new row.

Section lifecycle governance is explicit. `knowledge.challenge` marks an eligible section
as disputed and increments the atom dispute counter. `knowledge.adjudicate` requires a
disputed section; accept marks the section verified, reject returns it to reviewed, and the
atom dispute counter is decremented.

`knowledge.suggest` is a base domain-discovery verb. It accepts query/role/limit, searches
domains, may fuse ANN results when the index is warm, reranks with embeddings, and returns
domain IDs, names, and scores. It does not implicitly resolve a brain profile or apply
profile-weighted section scoring in the shipped implementation.

`knowledge.compose` is a base explicit-composition verb. It requires explicit domain IDs
and/or atom IDs plus query context, resolves those records, reranks atom text, and returns
markdown with atom/domain metadata. It does not emit implicit feedback, does not call
`brain.resolve`, and does not perform section-manifest weighting in the shipped
implementation.

## Context

Knowledge atoms in the corpus tier ([ADR-047](ADR-047-knowledge-pack.md)) store content as
flat text. The atlas lore system structures atom content into typed sections — overview,
core model, boundary conditions, formalism, operational guidance, examples, failure modes,
expert lens — each with a semantic role that determines its value to different agent roles.

An implementer agent needs operational guidance and examples; a theorist needs formalism
and core model. Today, `knowledge.compose` returns the same content regardless of who asks.
There is no mechanism for the system to learn which sections are valuable to which consumers
over time.

The brain pack ([ADR-032](ADR-032-brain-profile-orchestration.md)) provides Beta-Binomial posterior tracking
and profile resolution via `(actor, namespace, consumer_kind) → profile_id` bindings. This
machinery is exactly what section-weight learning needs — but it currently only supports
per-entity posteriors for the `recall` consumer kind.

Meanwhile, different projects (khive, lionagi, lattice) share the same knowledge corpus but
have different retrieval needs. A khive lambda working on MCP wiring cares about different
sections than a lionagi lambda doing inference optimization. The profile system should
resolve automatically based on the caller's identity — not require explicit profile naming
in every call.

### Entity kind amendment: `resource` (9th kind)

Knowledge atoms, domains, skills, and tools are concrete resources that agents consume —
distinct from abstract `concept` entities that model ideas and their relationships.
ADR-001 is amended to add a 9th entity kind: **`resource`**.

| Kind       | What it is                        | entity_type sub-classification                       |
| ---------- | --------------------------------- | ---------------------------------------------------- |
| `resource` | Actionable content agents consume | atom, domain, skill, tool, template, prompt, runbook |

The distinction from `concept`: a concept models "what IS it" (structural graph position,
edges to other concepts, papers, projects). A resource models "how to USE it" (section-typed
content, embeddings, composition weights). They link via `annotates`: resource annotates
concept.

Resource is accepted as the governance term for actionable content agents consume, and
the KG pack validator currently includes `Resource` as a pack-side 9th kind. Shipped
knowledge storage, however, is still corpus-table canonical: `knowledge_atoms`,
`knowledge_domains`, and `knowledge_sections` hold knowledge resources. The planned
graph `entities` dual-write for atom/domain resources is deferred; current domain mirroring
is domain-to-`knowledge_atoms` for FTS, not graph entity creation.

### Sections as a dedicated table

Sections are sub-records of resource/atom entities, stored in `knowledge_sections`:

```sql
CREATE TABLE IF NOT EXISTS knowledge_sections (
    id           TEXT PRIMARY KEY,
    atom_id      TEXT NOT NULL,
    namespace    TEXT NOT NULL,
    section_type TEXT NOT NULL,
    heading      TEXT NOT NULL DEFAULT '',
    content      TEXT NOT NULL DEFAULT '',
    content_hash TEXT NOT NULL DEFAULT '',
    tokens       INTEGER NOT NULL DEFAULT 0,
    sort_order   INTEGER NOT NULL DEFAULT 0,
    embedding    BLOB,
    created_at   INTEGER NOT NULL,
    updated_at   INTEGER NOT NULL,
    status       TEXT NOT NULL DEFAULT 'draft',
    FOREIGN KEY (atom_id) REFERENCES knowledge_atoms(id),
    UNIQUE(atom_id, content_hash)
);
```

V21 creates indexes on `atom_id`, `(namespace, section_type)`, and `(namespace, atom_id)`,
plus an external-content FTS5 table `fts_sections` with insert/delete/update triggers.
V22 adds the `status` column and `idx_knowledge_sections_status`.

Section_type is a closed enum matching the atlas schema v1: `overview`, `core_model`,
`boundary_conditions`, `formalism`, `operational_guidance`, `examples`, `failure_modes`,
`expert_lens`, `references`, `other`.

**Editing a section does not touch other sections.** `knowledge.edit(slug, sections=[...])`
updates only the named section rows. Each section has a nullable `embedding` column.

`knowledge.edit` performs an **inline, atom-scoped re-embed** immediately after writing
section rows: it queries `knowledge_sections WHERE atom_id = ? AND embedding IS NULL` and
embeds those rows using the same breadcrumb-enriched text format as `kkernel reindex`
(`{atom_name}\n{heading}\n\n{content}`, truncated to 32 768 bytes). Byte-identical sections
retain their prior embedding (they were updated via the `UNIQUE(atom_id, content_hash)` hit
path, which does not clear `embedding`), so the re-embed pass is incremental by construction.
Section vectors are written to `knowledge_sections.embedding` immediately, making the hybrid
section-cosine read path (ADR-051) fresh without a manual reindex.

The Vamana ANN snapshot rebuild is **deferred**: per-edit ANN rebuilds are prohibitively
costly. Approximate ANN recall over newly-written vectors lags until the next `kkernel reindex`
or scheduled rebuild; the direct section-cosine (non-ANN) path uses the new vectors immediately.

If no default embedder is configured, `knowledge.edit` succeeds without embedding — the same
early-return degradation as `embed_sections` in `kkernel reindex`. `knowledge.upsert_atoms`
remains batch-only and does not perform inline re-embed.

`knowledge.index` indexes atoms only. Full section write-side embedding backfill is shipped
via `kkernel reindex` (ADR-051 phase 1). Direct section-cosine scoring in `knowledge.search`
and `knowledge.compose` is shipped (see `search.rs` section candidate path and
`compose.rs` section rerank path); profile-weighted section compose and the Vamana ANN
snapshot rebuild remain deferred.

**Sections link to atoms structurally (FK), not via graph edges.** The section→atom
relationship is always 1:N containment — there's no semantic edge type needed. All
cross-entity connections for a section route through its parent atom's graph edges.

### Embedding strategy (three levels)

| Level       | Source text                                                            | Standard length                    | Updates when                  |
| ----------- | ---------------------------------------------------------------------- | ---------------------------------- | ----------------------------- |
| **Domain**  | description + purpose + member slug prose                              | 100-200 tokens                     | domain metadata edited        |
| **Atom**    | description (50-150 tok) + keywords as coherent sentences (50-100 tok) | 150-250 tokens                     | atom description/tags edited  |
| **Section** | section body content                                                   | up to 500 tokens (chunk if longer) | that section's content edited |

The atom embedding captures "what is this about" for coarse retrieval. Section embeddings
capture "what specifically does this say" for granular matching. Both use the dual-model
default (all-minilm-l6-v2 + paraphrase-multilingual).

Embedding text for atoms follows a standard template:

```
{name}. {description}. Keywords: {tag1}, {tag2}, {tag3}. Domain: {domain}.
Related: {related_concept_1}, {related_concept_2}.
```

This produces consistent 150-250 token embedding inputs regardless of atom content length.

### Audit trail: notes on graph edits

Observation-note audit for every `knowledge.edit` call is deferred. Shipped `knowledge.edit`
returns section update results and does not create graph notes.

The original spec described:

```
create(kind="note", note_kind="observation",
  content="Updated section:formalism — added convergence proof for entropic regularization",
  annotates=["<atom-entity-id>"])
```

This gives a complete edit history queryable via the notes substrate. Graph traversal from
an atom surfaces both its sections (content) and its edit history (notes).

### Scale: absorbing the lore corpus

The atlas lore corpus has 342K atoms organized into 25K domains. With section-level
embeddings (~5 sections per atom), the total vector count reaches ~2M rows across two
embedding models. The FTS5 trigram index handles substring matching over millions of
rows efficiently.

**Scaling roadmap** (DiskANN-informed, from RuVector `ruvector-diskann` research):

| Scale         | What                 | Graph memory | PQ memory | Vectors     | Query latency | Strategy                                 |
| ------------- | -------------------- | ------------ | --------- | ----------- | ------------- | ---------------------------------------- |
| 342K atoms    | Current corpus       | 88MB         | 16MB      | 527MB (RAM) | <1ms          | sqlite-vec brute force                   |
| 2M sections   | After section split  | 512MB        | 96MB      | 3GB (RAM)   | <5ms          | khive-hnsw in-memory                     |
| 10M atoms     | Full lore absorption | 2.5GB        | 480MB     | 15GB (SSD)  | <5ms          | DiskANN: graph+PQ in RAM, vectors on SSD |
| 100M sections | Multi-project corpus | 25GB         | 4.8GB     | 150GB (SSD) | <10ms         | Sharded DiskANN + RaBitQ filtering       |

DiskANN's Vamana graph (bounded degree R=64, single layer) is SSD-friendly because
neighbors are spatially local after alpha-robust pruning — unlike HNSW's multi-layer
skip connections that cause random page faults. The integration path:

1. **Now**: sqlite-vec brute force + FTS5 recall (2000 candidate pool). Works to ~2M vectors.
2. **Medium term**: `khive-hnsw` in-memory index at startup. RaBitQ compressed fallback
   for the full corpus (18MB per million vectors at D=384). ACORN over-connection for
   filtered queries.
3. **Long term**: Implement Vamana graph construction (from RuVector's algorithm, not as
   dependency) in a new `khive-vamana` crate. PQ codes in memory, vectors on SSD via
   mmap. The `khive-db` multi-backend federation (ADR-009) provides per-shard files.

The practical bottleneck is embedding generation, not search. At 342K atoms with dual
models, backfilling takes ~30 minutes on M-series. At 10M atoms, ~15 hours. Incremental
indexing (`knowledge.index(ids=[...])`) is essential — only embed new/changed content.

### RuVector algorithm reference (study, not dependency)

The RuVector ecosystem (120K-star Rust vector search library, partnership with
lattice-inference) contains battle-tested implementations that inform our architecture.
**We do not depend on RuVector crates** — we study their algorithms and implement
ourselves against khive's storage traits, except `ruvector-rabitq` which is pure math
(rand + serde only).

Key algorithms (source: `ruvector-gnn`, `ruvector-diskann`, `ruvector-rabitq`,
`ruvector-acorn`):

- **RaBitQ** (Gao & Long, SIGMOD 2024): 1-bit quantization via random rotation.
  342K embeddings at D=384 compress from 527MB → 18MB. May use crate directly.
- **DiskANN/Vamana**: bounded-degree graph (R=64) with alpha-robust pruning.
  SSD-friendly single-layer graph handles billions. Generation-counter visited
  set gives O(1) clear between queries.
- **ACORN** (Patel et al., SIGMOD 2024): filtered HNSW that maintains recall at
  low selectivity by over-connecting the graph and exploring through non-matching nodes.
- **GNN hierarchical search**: differentiable search with GRU-gated message passing.
  InfoNCE contrastive loss can use khive's edge ontology as training signal. EWC
  prevents catastrophic forgetting as the graph grows.
- **AdaptiveHotset**: LRU cache with decaying access counts (0.95 decay factor),
  maps to hot/warm/cold tier promotion.

### Namespace injection: session-to-actor mapping (deferred)

> **Deferred**: The shipped MCP `request` tool does not carry per-request caller identity
> in its dispatch path. The two approaches below are design notes for a future
> implementation phase, not shipped behavior. They are preserved here because ADR-048 is
> otherwise accepted; this section is consistent with the "deferred" entries in the
> Current Implementation Status table above.

The MCP server supports `--actor` / `KHIVE_ACTOR` / config file `[actor] id` for
static namespace resolution. Per-session dynamic injection — needed for profile-weighted
`knowledge.compose` — is not yet wired.

The current MCP protocol does not carry per-request caller identity. Two candidate approaches:

**Approach A: Hook-injected env**

A `UserPromptSubmit` hook could detect the lambda from cwd and write an actor file:

```bash
# Hook detects: cwd=/Users/lion/projects/khive/khive → lambda:khive
echo "lambda:khive" > /tmp/claude_hooks/actor_context
```

The MCP server would read this file on each `request` dispatch and use it as the actor
for brain.resolve and namespace scoping. This is imprecise (races between concurrent
sessions) but viable for single-user local dev. Not currently wired.

**Approach B: MCP request-level context**

A future MCP protocol extension could carry caller context in the request envelope:

```json
{
  "tool": "request",
  "args": { "ops": "...", "_context": { "actor": "lambda:khive", "session": "abc123" } }
}
```

The server would use `_context.actor` for brain resolution and `_context.session` for
feedback correlation. This eliminates the race condition in Approach A but requires MCP
protocol changes.

### The hook opportunity (deferred)

If actor injection were wired (Approach A or B above), a `UserPromptSubmit` hook could
inject the resolved profile into the MCP namespace context so that every
`knowledge.compose` call in that session automatically uses the right profile — no agent
cooperation required. This is not current shipped behavior; it is the target design once
the namespace injection mechanism is implemented.

For feedback, a `PostToolUse` hook on `knowledge.compose` / `knowledge.suggest` responses
can buffer section-level usage data. At session end (`/summarize`), the hook correlates
buffered compose calls with task outcomes and emits `brain.feedback` with section signals.
The agent never explicitly calls feedback — the reinforcement is invisible.

The full feedback context captured by hooks:

1. **Task context**: what was the agent working on? (from the prompt / task description)
2. **Query context**: what did the agent search for? (from the compose/suggest args)
3. **Usage signal**: did the agent's response reference the composed content? (from the
   PostToolUse hook observing subsequent tool calls)
4. **Outcome signal**: did the task succeed? (from task completion / session summary)
5. **Section attribution**: which section types were in the returned content? (from the
   compose response's section manifest — as of ADR-051 / #183 the manifest is **opt-in**: the
   hook must call `knowledge.compose` with `explain=true` to receive `sections[]` / `breakdown`;
   the default response omits them)

This gives a complete `(task, query, sections, outcome)` tuple for each compose call.
The brain feedback reduces this to per-section-type Beta updates scoped to the serving
profile.

## Decision

### 1. Section-typed atom content

Atom content is structured into sections with a closed 10-value `SectionType` enum:

| SectionType            | Semantic role                                              |
| ---------------------- | ---------------------------------------------------------- |
| `overview`             | Opening context, motivation, scope                         |
| `core_model`           | Internal structure, mechanisms, invariants, key properties |
| `boundary_conditions`  | When/where the concept applies, preconditions, constraints |
| `formalism`            | Precise rules, theorems, algorithms, complexity bounds     |
| `operational_guidance` | How to apply, implement, diagnose; steps and checklists    |
| `examples`             | Concrete cases, worked examples, counterexamples           |
| `failure_modes`        | How it breaks, edge cases, anti-patterns, silent failures  |
| `expert_lens`          | Trade-offs, hidden assumptions, non-obvious connections    |
| `references`           | Related atoms, bibliography, version history               |
| `other`                | Topic-specific content not matching a canonical type       |

This enum is stored in the atom's `properties` JSON as a section manifest:

```json
{
  "sections": [
    { "type": "overview", "heading": "Overview", "offset": 0, "tokens": 85 },
    { "type": "core_model", "heading": "Core Model", "offset": 312, "tokens": 210 },
    { "type": "operational_guidance", "heading": "Implementation", "offset": 1024, "tokens": 340 }
  ],
  "profile": "computational_engineering"
}
```

The `content` column remains flat markdown. Sections are byte-offset ranges into
the content, parsed at ingest time. This avoids schema changes — the section manifest
is metadata, not a new column.

The `profile` field maps to one of five atom profiles that determine default section
selection (from the atlas taxonomy):

| AtomProfile                 | Default sections                                                                                      |
| --------------------------- | ----------------------------------------------------------------------------------------------------- |
| `formal_mathematical`       | overview, core_model, formalism, examples, failure_modes, expert_lens                                 |
| `mechanistic_empirical`     | overview, core_model, boundary_conditions, examples, failure_modes, expert_lens                       |
| `computational_engineering` | overview, core_model, formalism, operational_guidance, examples, failure_modes, expert_lens           |
| `institutional_decision`    | overview, core_model, boundary_conditions, operational_guidance, examples, failure_modes, expert_lens |
| `interpretive_historical`   | overview, core_model, examples, failure_modes, expert_lens                                            |

### 2. Section posteriors in brain profiles

A new `consumer_kind = "knowledge_compose"` is added to the brain profile system.
Profiles of this kind maintain per-section-type Beta posteriors:

```json
{
  "section_posteriors": {
    "overview": { "alpha": 2.0, "beta": 2.0 },
    "core_model": { "alpha": 4.0, "beta": 2.0 },
    "boundary_conditions": { "alpha": 2.0, "beta": 3.0 },
    "formalism": { "alpha": 1.5, "beta": 4.0 },
    "operational_guidance": { "alpha": 6.0, "beta": 1.5 },
    "examples": { "alpha": 5.0, "beta": 2.0 },
    "failure_modes": { "alpha": 3.0, "beta": 2.0 },
    "expert_lens": { "alpha": 3.0, "beta": 2.0 }
  }
}
```

Seed priors encode the role's starting bias. An implementer profile seeds
`operational_guidance` at `Beta(3, 1)` (mean 0.75); a theorist seeds
`formalism` at `Beta(3, 1)`. Posteriors converge from there via feedback.

### 3. The reinforcement learning loop (detailed mechanics)

The learning loop has four stages: **observe → attribute → update → apply**. Each stage
has a concrete mechanism. The loop runs continuously across sessions — no batch training,
no offline phase.

#### Stage 1: Observe (hooks capture the raw signal)

Three hooks cooperate to build a complete observation record per compose call:

**Hook A: `UserPromptSubmit` — session identity injection**

Fires at the start of every Claude Code turn. Responsibilities:

```bash
#!/bin/bash
# .claude/hooks/knowledge_identity.sh (UserPromptSubmit)

# 1. Detect lambda from cwd
CWD="$PWD"
case "$CWD" in
  */khive/khive*)  ACTOR="lambda:khive" ;;
  */lionagi*)      ACTOR="lambda:lionagi" ;;
  */lattice*)      ACTOR="lambda:lattice" ;;
  *)               ACTOR="local" ;;
esac

# 2. Detect role from agent context (if spawned as subagent)
AGENT_TYPE="${CLAUDE_AGENT_TYPE:-}" # Claude Code exposes this for subagents
if [ -n "$AGENT_TYPE" ]; then
  ACTOR="${ACTOR}:${AGENT_TYPE}"  # e.g. "lambda:khive:implementer"
fi

# 3. Write actor context for MCP server to read
mkdir -p /tmp/claude_hooks
echo "$ACTOR" > /tmp/claude_hooks/actor_context

# 4. Write session ID for feedback correlation
echo "${CLAUDE_SESSION_ID:-unknown}" > /tmp/claude_hooks/session_id
```

The MCP server reads `/tmp/claude_hooks/actor_context` on each `request` dispatch.
This replaces the static `--actor` flag with dynamic per-turn identity.

**Hook B: `PostToolUse` on `mcp__khive__request` — compose observation**

Fires after every khive MCP call. Filters for knowledge.compose and knowledge.suggest:

```bash
#!/bin/bash
# .claude/hooks/knowledge_observe.sh (PostToolUse, match: mcp__khive__request)

# Parse the tool input — is this a knowledge.compose or knowledge.suggest call?
OPS="$TOOL_INPUT_OPS"
if ! echo "$OPS" | grep -qE 'knowledge\.(compose|suggest)'; then
  exit 0  # not a knowledge call, skip
fi

SESSION_ID=$(cat /tmp/claude_hooks/session_id 2>/dev/null || echo "unknown")
ACTOR=$(cat /tmp/claude_hooks/actor_context 2>/dev/null || echo "local")
TIMESTAMP=$(date -u +%Y-%m-%dT%H:%M:%SZ)

# Extract from tool output: event_id, sections returned, scores
# The compose response includes a section manifest
TOOL_OUTPUT="$TOOL_OUTPUT"

# Buffer the observation
mkdir -p /tmp/claude_hooks/compose_buffer
cat >> "/tmp/claude_hooks/compose_buffer/${SESSION_ID}.jsonl" << JSONEOF
{"timestamp":"$TIMESTAMP","actor":"$ACTOR","ops":"$OPS","response_hash":"$(echo "$TOOL_OUTPUT" | md5 -q)","session":"$SESSION_ID"}
JSONEOF
```

**Hook C: `PostToolUse` on all tools — usage tracking**

Fires after every tool call in the session. Tracks whether the agent references
knowledge content in subsequent actions (code edits, file writes, messages):

```bash
#!/bin/bash
# .claude/hooks/knowledge_usage.sh (PostToolUse, all tools)

# Check if the agent's output references content from a recent compose call
# This is a lightweight heuristic: did the agent's action use keywords from
# the composed sections?
SESSION_ID=$(cat /tmp/claude_hooks/session_id 2>/dev/null || echo "unknown")
BUFFER="/tmp/claude_hooks/compose_buffer/${SESSION_ID}.jsonl"
[ -f "$BUFFER" ] || exit 0

# Track tool call count since last compose (usage decay signal)
COUNTER="/tmp/claude_hooks/compose_buffer/${SESSION_ID}.counter"
COUNT=$(cat "$COUNTER" 2>/dev/null || echo "0")
echo $((COUNT + 1)) > "$COUNTER"
```

#### Stage 2: Attribute (map observations to section-level signals)

Attribution happens at two points:

**Immediate attribution (within-session)**:

When the agent calls `knowledge.compose` and the response includes sections, then
within the next N tool calls (N=5 window), if the agent:

- Writes code that references concepts from `operational_guidance` → that section is "useful"
- Quotes text from `formalism` in a message → that section is "useful"
- Ignores `boundary_conditions` entirely (no reference in 5 turns) → "not_useful"

This is imprecise but directionally correct. The attribution window prevents stale
correlations from polluting the signal.

**Deferred attribution (session-end)**:

At `/summarize` or session end, a dedicated pass reviews the compose buffer:

```
For each buffered compose call:
  1. Was the task that triggered this compose marked as completed? (gtd.complete)
  2. Did the agent produce artifacts (commits, PRs, messages) after consuming the content?
  3. Which section types appeared in the compose response?
  4. Which of those were referenced in the agent's subsequent output?

  Map to signals:
  - Section referenced + task succeeded → "useful"
  - Section not referenced + task succeeded → "not_useful" (section was noise)
  - Section referenced + task failed → no signal (task failure may be unrelated)
  - Section not referenced + task failed → no signal
```

The conservative attribution rule: **only emit "not_useful" when the task succeeded
but the section wasn't used**. This avoids punishing sections for unrelated task
failures. "useful" requires both presence and reference.

#### Stage 3: Update (Beta posterior conjugate update)

`brain.feedback` is extended with an optional `section_signals` map:

```
brain.feedback(
  target_id=<compose_event_id>,
  signal="useful",
  served_by_profile_id="khive-knowledge-v1",
  section_signals={
    "operational_guidance": "useful",
    "formalism": "not_useful",
    "examples": "useful"
  }
)
```

The fold/reduce path inside the brain pack:

```
For each (section_type, signal) in section_signals:
  posterior = profile.section_posteriors[section_type]
  match signal:
    "useful"     → posterior.alpha += 1.0    # Beta success
    "not_useful" → posterior.beta  += 1.0    # Beta failure
    "wrong"      → posterior.beta  += 2.0    # stronger penalty
```

**Convergence properties**:

- Beta(α, β) has mean α/(α+β) and variance αβ/((α+β)²(α+β+1))
- After N observations, variance ≈ 1/(4N) — halves every 4x more data
- With seed priors of Beta(2,2), ~20 feedback events per section type are
  sufficient for the posterior mean to reflect actual usage patterns (±0.1)
- A profile with 7 active section types receiving feedback from 3 compose calls
  per session converges in ~7-10 sessions

**Exploration vs exploitation**:

The profile's `exploration_epoch` field controls the explore/exploit tradeoff:

- `exploration_epoch = 0` → **exploit**: use posterior means as weights.
  Deterministic, reproducible, no surprises.
- `exploration_epoch > 0` → **explore**: Thompson sampling — sample from
  `Beta(α, β)` for each section type, use samples as weights. Stochastic,
  may discover better configurations. Epoch decrements each feedback event;
  returns to exploit when epoch reaches 0.

New profiles start with `exploration_epoch = 50` (explore for ~50 feedback events,
then settle). `brain.reset` can restart exploration by re-seeding priors and setting
a new exploration epoch.

#### Stage 4: Apply (compose uses the learned weights)

Profile-weighted `knowledge.compose` is deferred. Shipped `knowledge.compose` requires
explicit `domain_ids` and/or `atom_ids`, reranks atom text, and returns atom-body markdown.
The shipped function does not call `brain.resolve` and does not assemble section-weighted
manifests.

**Inline re-embed on `knowledge.edit`** (shipped, issue #11): after section rows are written,
`knowledge.edit` embeds the atom's newly-inserted sections inline (those whose `embedding` is
NULL). This ensures the hybrid section-cosine read path is fresh without a manual reindex.
The Vamana ANN snapshot rebuild remains deferred (per-edit cost too high). If no embedder is
configured the edit still succeeds. `knowledge.upsert_atoms` is not changed.

The original spec described a profile-weighted compose step:

1. Read actor from `/tmp/claude_hooks/actor_context` (set by Hook A)
2. `brain.resolve(actor=<actor>, consumer_kind="knowledge_compose")` → profile
3. Read `section_posteriors` from profile state snapshot
4. **Weight derivation**:
   - If `exploration_epoch > 0`: Thompson sample from each Beta(α, β)
   - Else: use posterior mean α/(α+β) as weight
5. For each candidate atom, compute section scores:
   ```
   atom_score = Σ (section_weight[type] * section_tokens[type] / total_tokens)
   ```
   Atoms whose section mix matches the profile's learned preferences score higher.
6. Budget-constrained selection (fold) packs highest-scored atoms first
7. Within each selected atom, sections are ordered by weight (highest first)
   and truncated to fit the token budget

The result: an implementer profile that has learned `operational_guidance=0.82,
formalism=0.21` will:

- Prefer atoms rich in operational guidance sections
- Within those atoms, lead with the guidance sections
- Truncate formalism sections first when budget is tight

### 4. Profile resolution hierarchy (automatic, not manual)

Profiles are resolved via the brain binding table. The hierarchy supports
three dimensions of specificity:

```
# Dimension 1: Project-level (which codebase)
brain.bind(actor="lambda:khive",   namespace="*", consumer_kind="knowledge_compose", profile_id="khive-knowledge-v1")
brain.bind(actor="lambda:lionagi", namespace="*", consumer_kind="knowledge_compose", profile_id="lionagi-knowledge-v1")

# Dimension 2: Role-level (what kind of work)
brain.bind(actor="implementer",    namespace="*", consumer_kind="knowledge_compose", profile_id="impl-knowledge-v1")
brain.bind(actor="theorist",       namespace="*", consumer_kind="knowledge_compose", profile_id="theory-knowledge-v1")
brain.bind(actor="researcher",     namespace="*", consumer_kind="knowledge_compose", profile_id="research-knowledge-v1")

# Dimension 3: Compound (project + role, most specific)
brain.bind(actor="lambda:khive:implementer", namespace="*", consumer_kind="knowledge_compose", profile_id="khive-impl-v1")
brain.bind(actor="lambda:lionagi:theorist",  namespace="*", consumer_kind="knowledge_compose", profile_id="lionagi-theory-v1")

# Global fallback
brain.bind(actor="*", namespace="*", consumer_kind="knowledge_compose", profile_id="balanced-knowledge-v1")
```

Resolution is longest-match-wins (most specific actor > less specific > wildcard):

| Session context              | Hook sets actor to         | Resolves to                                |
| ---------------------------- | -------------------------- | ------------------------------------------ |
| khive implementer subagent   | `lambda:khive:implementer` | `khive-impl-v1` (exact compound match)     |
| khive session, no role       | `lambda:khive`             | `khive-knowledge-v1` (project match)       |
| lionagi theorist subagent    | `lambda:lionagi:theorist`  | `lionagi-theory-v1` (exact compound match) |
| unknown project, implementer | `local:implementer`        | `impl-knowledge-v1` (role match)           |
| completely generic           | `local`                    | `balanced-knowledge-v1` (wildcard)         |

Each profile learns independently. The khive implementer's posteriors reflect what
khive implementation work needs. The lionagi theorist's posteriors reflect what
formal verification work needs. They share the same corpus but get different views.

### 5. Profile lifecycle and cross-learning

**Profile creation**: Seed profiles are created at system setup via `brain.create_profile`.
Each gets role-appropriate priors:

```
brain.create_profile(
  id="impl-knowledge-v1",
  description="Section weights for implementer-role knowledge retrieval",
  consumer_kind="knowledge_compose",
  seed_priors={
    "section_posteriors": {
      "overview":              {"alpha": 2.0, "beta": 2.0},
      "core_model":            {"alpha": 3.0, "beta": 2.0},
      "boundary_conditions":   {"alpha": 2.0, "beta": 2.0},
      "formalism":             {"alpha": 1.5, "beta": 3.0},
      "operational_guidance":  {"alpha": 4.0, "beta": 1.0},
      "examples":              {"alpha": 3.5, "beta": 1.5},
      "failure_modes":         {"alpha": 3.0, "beta": 1.5},
      "expert_lens":           {"alpha": 2.5, "beta": 2.0}
    }
  }
)
```

**Cross-learning** (future): When a compound profile (`khive-impl-v1`) receives
feedback, the evidence could propagate to its parent profiles (`khive-knowledge-v1`,
`impl-knowledge-v1`) with a discount factor. This is the Beta posterior merge
operation already in `BetaPosterior::merge()`:

```
parent.alpha += (child.alpha - prior.alpha) * discount
parent.beta  += (child.beta  - prior.beta)  * discount
```

Discount factor 0.3 means: 30% of child evidence flows to parent. This lets the
global implementer profile benefit from khive-specific implementer experience
without being dominated by it. Deferred to v2.

### 6. Hook-injected profile context (deferred)

The hooks form a three-stage pipeline:

```
┌──────────────────┐     ┌──────────────────┐     ┌──────────────────┐
│  Hook A:         │     │  Hook B:         │     │  Hook C:         │
│  Identity        │────▶│  Observe         │────▶│  Usage Track     │
│  (UserPromptSub) │     │  (PostToolUse)   │     │  (PostToolUse)   │
│                  │     │  on khive calls  │     │  on all tools    │
│  Writes:         │     │  Writes:         │     │  Writes:         │
│  • actor_context │     │  • compose_buf   │     │  • usage_counter │
│  • session_id    │     │  • section_list  │     │  • ref_keywords  │
└──────────────────┘     └──────────────────┘     └──────────────────┘
                                                           │
                                                           ▼
                                              ┌──────────────────────┐
                                              │  Session End:        │
                                              │  Attribution Pass    │
                                              │  (/summarize hook)   │
                                              │                      │
                                              │  Reads buffer →      │
                                              │  correlates with     │
                                              │  task outcomes →     │
                                              │  emits brain.feedback│
                                              └──────────────────────┘
```

**Failure modes and mitigations**:

| Failure mode                              | Consequence                              | Mitigation                                                                  |
| ----------------------------------------- | ---------------------------------------- | --------------------------------------------------------------------------- |
| Hook A doesn't fire (no actor_context)    | Compose uses wildcard profile            | Acceptable fallback; balanced profile still works                           |
| Hook B doesn't fire (no buffer)           | No feedback for this session             | Posterior unchanged; no harm, just slower learning                          |
| Hook C misattributes usage                | Wrong section gets "useful"/"not_useful" | Beta priors dampen noise; needs ~5 consistent wrong signals to shift by 0.1 |
| Session crashes (no deferred attribution) | Buffer is orphaned                       | Cron job cleans buffers older than 24h; immediate attribution still fires   |
| Two concurrent sessions write same actor  | Race on actor_context file               | Per-session actor file keyed by session_id (not shared path)                |
| Agent explicitly calls brain.feedback too | Double-counting                          | Dedup by event_id in compose buffer; same event_id → skip hook feedback     |

**Smart attribution heuristics**:

The hooks don't just track "was the content referenced." They apply domain-aware
heuristics:

1. **Code-write signal**: if the agent writes code (Edit/Write tool) within 3 turns
   of a compose, and the code contains identifiers from the `operational_guidance`
   or `formalism` sections → those sections are "useful"
2. **Explanation signal**: if the agent produces a text response (no tool call) that
   paraphrases content from `overview` or `core_model` → those sections are "useful"
3. **Ignore signal**: if the agent calls another knowledge.compose with a refined
   query within 2 turns → the first compose was insufficient; sections that appeared
   in the first but not the second are "not_useful"
4. **Expert escalation signal**: if the agent spawns a subagent (Agent tool) after
   a compose → the compose wasn't sufficient on its own; reduce confidence but
   don't mark as "not_useful" (the content may have informed the subagent prompt)

### 7. File import and agent editing

Two new verbs support corpus maintenance:

**`knowledge.import`** — ingest atoms from markdown files:

```
knowledge.import(
  path="/path/to/atoms/",
  format="atlas_md",      # atlas markdown with ## section headers
  chunk_strategy="section" # one section per chunk, or "atom" for whole-file
)
```

Parses markdown into section-typed atoms using the atlas header normalization map.
Supports glob patterns for batch import.

**`knowledge.edit`** — agent-driven atom editing:

```
knowledge.edit(
  slug="sinkhorn-algorithm",
  sections=[
    {"type": "operational_guidance", "content": "## Operational Guidance\n\n..."},
    {"type": "examples", "action": "append", "content": "### Rust Example\n\n..."}
  ]
)
```

Agents can add, replace, or append to specific sections of an atom. The section
manifest in `properties` is updated atomically. This enables agents to improve
corpus quality during their normal workflow — after reading a paper, an agent can
`knowledge.edit` the relevant atom's `formalism` section with new theorems.

### 8. Hybrid retrieval pipeline

All search paths fuse results from multiple channels via RRF:

```
query "attention pruning for inference"
    │
    ├── FTS5 (fts_knowledge trigram)    → atom candidates (2000 pool)
    ├── FTS5 (fts_entities trigram)     → entity candidates
    ├── FTS5 (fts_sections trigram)     → section candidates
    ├── Vector search (atom embeddings)  → description+keyword similarity
    ├── Vector search (section embeddings) → body content similarity
    ├── Vector search (entity embeddings)  → existing entity search
    │
    └── RRF fusion (khive-fusion crate)
        │
        ├── section-level results (most granular, carry section_type)
        ├── atom-level results (grouped sections, weighted by profile)
        └── entity-level results (graph-connected, carry edge context)
```

Notes are also searchable — an `observation` note saying "this algorithm fails at
batch sizes > 1024" surfaces alongside the entity/atom it annotates. The note's
`annotates` edges connect it to the relevant graph context.

Graph traversal enriches search results: when an entity appears in results, its
immediate neighbors (via `neighbors`) provide context — related concepts, implementing
projects, citing documents. This is the "graph retrieval" layer that pure vector search
misses.

### 9. KG Lint — configurable graph hygiene rules (deferred)

The graph needs a linting system analogous to `clippy` for Rust or `eslint` for
JavaScript. Static rules catch structural problems; configurable rules encode
project-specific conventions. The output is machine-readable and actionable.

**`knowledge.lint`** verb — run lint rules and report violations:

```
knowledge.lint(rules?: [...], fix?: false, severity?: "warn") → {
  violations: [
    {
      rule: "min-edge-density",
      severity: "error",
      entity_id: "abc123",
      entity_name: "Sinkhorn Algorithm",
      entity_kind: "concept",
      message: "concept entity has 2 edges, minimum is 4",
      suggestion: "Add instance_of, introduced_by, or competes_with edges",
      auto_fixable: false
    },
    ...
  ],
  summary: {
    total: N,
    errors: N,
    warnings: N,
    fixed: N,    // when fix=true
    by_rule: {"min-edge-density": 3, "orphan-entity": 1, ...}
  },
  stats: {
    total_entities: N,
    total_edges: N,
    avg_density: f64,
    entity_kinds: {"concept": N, "project": N, ...},
    edge_relations: {"implements": N, "depends_on": N, ...}
  }
}
```

**Built-in lint rules** (always available, severity configurable):

| Rule ID                  | Default | What it checks                                                                                    |
| ------------------------ | ------- | ------------------------------------------------------------------------------------------------- |
| `orphan-entity`          | error   | Entities with 0 edges                                                                             |
| `dangling-edge`          | error   | Edges where source or target is soft-deleted                                                      |
| `min-edge-density`       | error   | Entity below kind-specific minimum (concept ≥ 4, project ≥ 3, person ≥ 1)                         |
| `direction-violation`    | error   | Edge where (source_kind, relation, target_kind) is not in the ADR-002 contract or pack EDGE_RULES |
| `missing-entity-type`    | warn    | project/resource entity without `entity_type` in properties                                       |
| `missing-introduced-by`  | warn    | concept with `properties.type=paper` but no `introduced_by` edge                                  |
| `missing-implements`     | warn    | project entity with no `implements` edge to any concept                                           |
| `duplicate-name`         | warn    | Multiple entities of the same kind with identical names                                           |
| `superseded-not-marked`  | warn    | Entity with incoming `supersedes` edge but no `properties.status=deprecated`                      |
| `concept-no-parent`      | warn    | concept with no `instance_of`, `extends`, or `variant_of` edge                                    |
| `paper-missing-metadata` | info    | concept with type=paper missing authors/year/source in properties                                 |
| `low-weight-edge`        | info    | Edge with weight < 0.3 (possibly speculative)                                                     |
| `note-no-annotates`      | info    | Note with no `annotates` edge                                                                     |

**Custom lint rules** (configurable via `knowledge.lint_config`):

```
knowledge.lint_config(rules=[
  {
    "id": "khive-crate-needs-adr",
    "severity": "warn",
    "description": "Every khive crate entity should link to at least one ADR entity",
    "match": {"kind": "project", "properties.crate": {"$exists": true}},
    "require": {"edge": {"relation": "implements", "target.properties.type": "adr"}}
  },
  {
    "id": "retrieval-concept-needs-benchmark",
    "severity": "info",
    "description": "Retrieval domain concepts should have benchmark properties",
    "match": {"kind": "concept", "properties.domain": "retrieval"},
    "require": {"property": "benchmark"}
  }
])
```

The rule config is stored in `knowledge_atoms` as a domain-type resource (meta —
the lint config is itself a knowledge artifact). Changes to lint rules are tracked
as knowledge edits with observation notes.

**Auto-fix** (`fix=true`): Some rules support auto-fix:

- `dangling-edge` → soft-delete the edge
- `duplicate-name` → propose merge (creates a `question` note, doesn't auto-merge)
- `superseded-not-marked` → set `properties.status = "deprecated"`

Non-fixable violations return `auto_fixable: false` with a `suggestion` string.

**Integration with agent workflows**:

Agents should run `knowledge.lint(severity="error")` after any batch KG operation
(entity creation, edge wiring, polish). The `/kg-polish` skill already uses health
checks — lint replaces and extends that surface. The lint result is structured for
programmatic consumption: an orchestrator can dispatch fix agents per violation type.

**State introspection** (`knowledge.lint(rules=["stats-only"])`):

Returns only the `stats` section — entity/edge/note counts by kind, density
histogram, relation distribution, property coverage. This is the "dashboard view"
for understanding graph state without running any violation checks.

**`knowledge.export`** verb — version-controllable graph dump:

```
knowledge.export(format="jsonl") → writes to stdout or file:
  // One line per entity, sorted by id for stable diffs
  {"type":"entity","id":"...","kind":"concept","name":"...","properties":{...},"edges":[...]}
  {"type":"entity","id":"...","kind":"resource","entity_type":"atom","name":"...","sections":[...]}
  {"type":"note","id":"...","kind":"observation","content":"...","annotates":["..."]}
```

JSONL format diffs cleanly in git. The export includes edges inline with their source
entity (no separate edge file). Import is idempotent — `knowledge.import` from an
export file upserts by ID.

### 10. Entity-atom-citation linking pattern

The standard linking pattern between KG concepts, knowledge resources, and citations:

```
project "lattice-transport"
    │ implements
    ↓
concept "Sinkhorn Algorithm"
    ↑ annotates                    ↑ introduced_by
    │                              │
resource/atom "sinkhorn-algorithm" document "Cuturi 2013"
    │ (section FK, not edge)
    ├── section:overview
    ├── section:core_model
    ├── section:formalism
    └── section:operational_guidance
```

Rules:

- **project --implements--> concept**: code realizes algorithm
- **resource --annotates--> concept**: resource provides actionable content about concept
- **concept --introduced_by--> document**: concept was first described in this paper
- **sections link to atoms via FK only**, not graph edges. All semantic connections
  for a section route through its parent atom's graph edges.
- **resource --introduced_by--> document**: the atom's content is sourced from this paper
  (when the atom itself needs provenance, not just its concept)

This avoids the combinatorial explosion of section-level edges while keeping the graph
navigable. A query for "Sinkhorn implementation" finds the concept via graph search,
follows `annotates` to the resource/atom, then reads the `operational_guidance` section.

## Consequences

### Positive

- Retrieval quality improves over time per role without manual tuning
- Different projects sharing the same corpus get tailored results automatically
- Section-level feedback is more informative than entity-level (the same atom
  can be useful for its examples but useless for its formalism)
- Hook-based feedback requires zero agent cooperation
- File import enables batch corpus building from existing atlas content
- Agent editing enables continuous corpus improvement during normal work

### Negative

- Section parsing adds complexity to atom ingest (header normalization map)
- Brain profile state grows by 10 floats per section type per profile (negligible)
- Hook dependency means the feedback loop only works in Claude Code sessions
  (direct MCP callers would need to call brain.feedback explicitly)
- Thompson sampling exploration can occasionally produce worse results than
  posterior means (by design — exploration has a cost)

### Accepted trade-offs

- Section types are a closed enum. New section types require an ADR amendment.
  This matches the closed-taxonomy principle of entity kinds and edge relations.
- The hook approach is Claude Code-specific. Non-Claude-Code callers get the
  compose weighting (from existing posteriors) but not the automatic feedback.
  This is acceptable because Claude Code is the primary consumer.
- Atom profiles (formal_mathematical, etc.) are assigned at ingest time and are
  not updated by the feedback loop. They control default section selection, not
  per-role weighting. The brain profile handles the per-role adaptation.

## Implementation

### Phase 0: Vamana ANN index (khive-vamana) — P0, unblocks lore absorption

New crate `crates/khive-vamana/` — batch-built Vamana graph for approximate nearest
neighbor search. Separate from `khive-hnsw` (different lifecycle: batch-build vs OLTP
insert/delete).

**Architecture decisions** (from DiskANN feasibility study):

- **Flat binary + mmap persistence**: `{data_dir}/vamana/{namespace}/vectors.bin` +
  `graph.bin`. Graph (~88MB for 342K) stays in RAM. Vectors (526MB) mmap'd. Zero
  SQLite dependency for index storage.
- **Explicit rebuild**: `knowledge.index(rebuild_ann=true)`. Not at startup (5-30s
  cold start unacceptable), not per-insert (Vamana is not incremental). Build time:
  90-180s for 342K×384d with rayon.
- **Parallel ANN signal**: FTS5 candidates (lexical) + Vamana ANN (semantic) fused
  via RRF. Additive to existing pipeline — sqlite-vec stays as fallback.
- **Pre-normalize vectors**: L2²(a,b) = 2−2·cos(a,b) for unit vectors. Graph uses
  L2 (fast), output converts to cosine for RRF fusion.
- **No PQ at 342K**: 526MB fits in RAM. PQ config field present but disabled until 2M+.

Files:

- `crates/khive-vamana/src/{lib,config,distance,graph,index}.rs` — core implementation
- `crates/khive-pack-knowledge/src/knowledge/vamana.rs` — ~150 LOC bridge

### Phase 1: Brain persistence + section state — P0, unblocks learning

**Brain profile persistence** (currently ALL in-memory, lost on restart):

- V20 migration: `brain_profile_snapshots` table in SQLite
- Save on every feedback event (or batched every N events)
- Load on startup from latest snapshot
- Without this, all learned preferences are throwaway

**`SectionPosteriorState`** (does not exist yet):

- New state type for `consumer_kind="knowledge_compose"`
- 10 `BetaPosterior` fields (one per section type)
- ESS cap at ~50 per parameter for temporal decay: when `α+β > cap`,
  rescale toward prior (`α = α_prior + (α-α_prior) * cap/ESS`)
- Weight floor: `max(0.05, mean)` prevents section collapse to zero
- Exploration epoch decrement on each feedback event (currently broken —
  epoch never decrements, Thompson sampling never transitions to exploit)

**`brain.create_profile` seed_priors**: currently ignores the param.
Implement section posterior initialization from caller-provided priors.

### Phase 2: Resource entity kind + sections table

- ADR-001/resource governance: accepted as pack-side KG validator behavior; shared
  `khive_types::EntityKind` still has 8 base kinds, so graph-wide taxonomy harmonization
  is a follow-up.
- `knowledge.upsert_atoms` / `knowledge.upsert_domains`: corpus tables only; graph
  entity dual-write is deferred.
- `knowledge_sections` table with section type enum, nullable section embeddings, FK to
  atom, `content_hash` + `UNIQUE(atom_id, content_hash)`, indexes, and FTS5 triggers.
- `knowledge.edit`: section-level upsert keyed by `content_hash`; identical content is
  idempotent, distinct content inserts a sibling row, and siblings are left untouched.
- `knowledge.import`: shipped atlas markdown ingestion with section parsing.

### Phase 3: Compose + suggest verbs with profile resolution

- `knowledge.suggest`: shipped as domain discovery with plain search, optional Vamana
  ANN signal when warm, and embedding rerank; profile-weighted scoring is deferred.
- `knowledge.compose`: shipped as explicit domain/atom markdown composition over atom
  bodies; section-weighted output and implicit brain profile resolution are deferred.
- Brain section posterior primitives ship, but knowledge compose/suggest do not consume
  them yet.

### Phase 4: Hook wiring + implicit feedback

- `UserPromptSubmit` hook: actor injection from cwd/lambda identity
- `PostToolUse` hook: buffer compose calls with section manifests
- Session-end attribution: correlate buffered compose calls with task outcomes
- Emit `brain.feedback` with `section_signals` map
- ESS cap + decay ensures preferences stay adaptive

### Phase 5: KG lint, export, observability

- `knowledge.lint` verb: configurable rule engine for graph hygiene (§9). 13+ built-in
  rules with severity levels. Custom rules via `knowledge.lint_config`. Auto-fix for
  dangling edges, superseded-not-marked, duplicate proposals. Stats-only mode for
  dashboard introspection.
- `knowledge.lint_config` verb: CRUD for custom lint rules stored as knowledge atoms.
  Rules specify match predicates + required properties/edges.
- `knowledge.export` verb: JSONL format for git-diffable graph snapshots
- `brain.diagnostics` verb: ESS per parameter, weight vector entropy, delta-mean
  over last N events, convergence trend
- `reclassify` verb: change entity kind preserving UUID + edges (currently blocked —
  entity_kind is immutable, delete+recreate loses edge references)

## Benchmarks required (test-driven, bench-driven)

Every phase ships with benchmarks that gate merge:

| Phase        | Benchmark                                                          | Pass criteria                                |
| ------------ | ------------------------------------------------------------------ | -------------------------------------------- |
| 0 (Vamana)   | recall@10 on 5K/384d random dataset                                | ≥ 85%                                        |
| 0 (Vamana)   | build time for 342K × 384d                                         | < 180s on M-series                           |
| 0 (Vamana)   | query latency at 342K (single query)                               | < 5ms p99                                    |
| 1 (Brain)    | profile save/load round-trip                                       | snapshot == restored state                   |
| 1 (Brain)    | ESS cap convergence: 200 events then 200 opposing                  | mean shifts by ≥ 0.3                         |
| 2 (Sections) | section edit does not alter sibling sections                       | property test                                |
| 3 (Compose)  | compose with implementer profile returns > 60% ops_guidance tokens | weight test                                  |
| 4 (Hooks)    | end-to-end: compose → feedback → posterior shift                   | integration test                             |
| 5 (Lint)     | lint 1000 entities with 13 rules                                   | < 500ms, zero false positives on clean graph |
| 5 (Lint)     | custom rule evaluation on 100 entities                             | < 50ms per rule                              |

---

## Shipped Schema Reference

### `knowledge_sections` (V21 + V22)

V21 creates `knowledge_sections`; V22 adds the `status` lifecycle column. The shipped
columns, constraints, and indexes are:

- `id TEXT PRIMARY KEY`
- `atom_id TEXT NOT NULL`
- `namespace TEXT NOT NULL`
- `section_type TEXT NOT NULL`
- `heading TEXT NOT NULL DEFAULT ''`
- `content TEXT NOT NULL DEFAULT ''`
- `content_hash TEXT NOT NULL DEFAULT ''` (sha256(content)[:16])
- `tokens INTEGER NOT NULL DEFAULT 0`
- `sort_order INTEGER NOT NULL DEFAULT 0`
- `embedding BLOB` (nullable)
- `created_at INTEGER NOT NULL`
- `updated_at INTEGER NOT NULL`
- `status TEXT NOT NULL DEFAULT 'draft'`
- `FOREIGN KEY (atom_id) REFERENCES knowledge_atoms(id)`
- `UNIQUE(atom_id, content_hash)`

Indexes: `idx_knowledge_sections_atom`, `idx_knowledge_sections_ns_type`,
`idx_knowledge_sections_ns_atom`, `idx_knowledge_sections_status`.

Full-text search: `fts_sections`, an external-content FTS5 table over `heading` and
`content`, with `id`, `namespace`, `atom_id`, and `section_type` as unindexed metadata.
FTS insert, delete, and update triggers maintain the FTS table on section changes.

### Profile Persistence (brain-owned, not knowledge-local)

ADR-048 does not ship a `knowledge_profiles` table. Profile persistence is brain-owned;
the authoritative shipped tables are `brain_profile_snapshots` and `brain_event_log`
(see V20 DDL note in the Amendment section below and ADR-032).

## Amendment: Research-Informed Design Corrections (2026-05-27)

**Authors**: Ocean, lambda:khive
**Source**: 10 targeted ChatGPT research consultations (Q1-Q10), digested into KG as 17
entities + 72 edges + 9 notes.

### Correction 1: Normalized-Beta → Combinatorial TS (design-breaking)

**Problem**: §4 Stage 4 specifies weight derivation as "Thompson sample from each
Beta(α, β), then normalize to sum to 1." Research Q2 proves this has **Ω(T) linear
regret** under a budgeted document composition objective. The normalized vector
converges to `μ/Σμ` (proportional to means), but the optimal allocation under a fixed
token budget is a vertex — put all weight on the highest-value section. The per-round
regret gap is permanent and does not shrink with more data.

**Fix — phased**:

| Phase              | Weight derivation                                   | Why                                                                                                                                                                                                                        |
| ------------------ | --------------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Phase 1 (ship now) | `argmax` over sampled θ̃ᵢ with softmax temperature τ | Correct TS action for linear utility over simplex. τ controls exploration breadth. As τ→0, becomes pure exploit (vertex). As τ→∞, becomes uniform.                                                                         |
| Phase 3 (future)   | Dirichlet-tree prior with 4-outcome feedback        | Replaces Beta-Binomial entirely. Tree structure: root = positive/negative, children = strong/weak. Natural scalar utility derivation via `u⊤E[p]`. Conjugate, handles multi-outcome feedback without collapsing to binary. |

**Phase 1 weight derivation (replaces §4 Step 4-5)**:

```
1. Sample θ̃ᵢ ~ Beta(αᵢ, βᵢ) for each section type i
2. If exploration_epoch > 0:
     wᵢ = softmax(θ̃ / τ)  where τ = τ₀ * (epoch / epoch_max)
     # τ shrinks as epoch decreases → sharpens toward argmax
   Else:
     wᵢ = softmax(μᵢ / τ_exploit)  where μᵢ = αᵢ/(αᵢ+βᵢ), τ_exploit = 0.1
     # Near-deterministic, heavily favors highest-mean section
3. Apply weight floor: wᵢ = max(0.05, wᵢ), renormalize
```

The softmax-over-samples approach is standard in combinatorial TS literature (Combes et
al. 2015). It preserves exploration when τ is large but converges to the optimal vertex
allocation as τ→0. The weight floor prevents complete section starvation.

**Phase 3 Dirichlet-tree storage (replaces §2 section_posteriors schema)**:

```json
{
  "section_posteriors": {
    "overview": {
      "form": "dirichlet_tree",
      "positive": { "strong": 1.0, "weak": 1.0 },
      "negative": { "strong": 1.0, "weak": 1.0 },
      "utility_vector": [1.0, 0.5, -0.2, -1.0]
    }
  }
}
```

Until Phase 3, store as Beta(α, β) but compute weights via softmax, not normalization.

### Correction 2: ESS cap implementation constraints

**Problem**: Q1 reveals ESS capping is non-commutative — replaying events in different
order produces different posteriors. This breaks event-sourced snapshot recovery.

**Constraints added to Phase 1**:

1. **Store posteriors as (m, s) canonical form**: `m = α/(α+β)`, `s = α+β`. Prevents
   float drift across hundreds of updates. Convert back: `α = m*s`, `β = (1-m)*s`.
2. **Snapshot stores `last_event_seq`**: replay from snapshot must process events in
   strict sequence order. Out-of-order replay produces different posteriors.
3. **ESS cap = 100** (not 50): half-life of ~140 feedback events. 50 was too aggressive
   for sections that stabilize slowly (formalism, expert_lens). The research-derived
   optimal: `C_opt ≈ 1/(2ε²)` where ε is acceptable mean-estimation error. For ε=0.07
   (our target), C_opt ≈ 102.

**V20 migration DDL (updated)**:

```markdown
V20 brain persistence DDL in this ADR is superseded by ADR-032 and ADR-015 V20. The
authoritative shipped tables are defined in the brain pack. There is no shipped
`knowledge_profiles` table; profile persistence is brain-owned.

`brain_profile_snapshots`:

- `profile_id TEXT NOT NULL`
- `namespace TEXT NOT NULL DEFAULT 'default'`
- `snapshot_json TEXT NOT NULL`
- `updated_at INTEGER NOT NULL`
- `PRIMARY KEY (profile_id, namespace)`

`brain_event_log`:

- `id INTEGER PRIMARY KEY AUTOINCREMENT`
- `profile_id TEXT NOT NULL`
- `namespace TEXT NOT NULL DEFAULT 'default'`
- `event_kind TEXT NOT NULL`
- `payload TEXT NOT NULL`
- `created_at INTEGER NOT NULL`
- `idx_brain_events_profile` on `(profile_id, namespace, created_at)`
```

### Correction 3: Filtered ANN — StitchedVamana, not ACORN

**Problem**: §10 (scaling roadmap) and §Phase 0 reference ACORN for filtered queries.
Q8 shows ACORN requires R≈768-1536 for 96 kind×domain filter combinations — a 12-24x
regression on unfiltered search latency and memory. Not viable.

**Fix**: Replace ACORN with FilteredVamana/StitchedVamana:

```
Old (§scaling roadmap, medium term):
  "ACORN over-connection for filtered queries"

New:
  "StitchedVamana: per-predicate subgraph build + cross-partition stitching"
```

**StitchedVamana architecture** (from Filtered-DiskANN paper):

1. Build per-label Vamana subgraphs: one for each entity_kind (8), one for each
   domain (12). Each subgraph uses the global R=64 budget.
2. Stitch: for each node, merge its subgraph neighbors with its global graph
   neighbors. RobustPrune the combined set to R=64.
3. Query: filtered greedy search starts from a label-valid entry point (not the
   global medoid). Candidate expansion only follows edges to label-valid nodes.

This keeps R=64 while maintaining >85% recall for selective predicates (kind+domain
combinations). Memory overhead: ~20% over unfiltered graph (stitching edges are a
subset of global candidates, not additive).

**Updated scaling roadmap**:

| Scale         | Strategy                                            |
| ------------- | --------------------------------------------------- |
| 342K atoms    | sqlite-vec brute force + FTS5 (current)             |
| 2M sections   | khive-vamana in-memory, StitchedVamana for filtered |
| 10M atoms     | DiskANN: graph+PQ in RAM, vectors on SSD via mmap   |
| 100M sections | Sharded DiskANN + RaBitQ + partition-index          |

### Correction 4: Embedding Drift Detection via lattice-transport

**Problem**: When the base embedding model changes (new version of all-minilm-l6-v2, or
switching models entirely), all stored embeddings are in the old space. The profile's
learned preferences — section weights, expertise map, reranking — were calibrated against
the old geometry. Without drift detection, the profile silently degrades. Content drift
(KG entity updates) and behavioral drift (agent query pattern changes) are also undetected.

**Fix**: Integrate `lattice-transport` (our OT crate) for three drift detection modes:

| Scenario         | Detector                                 | Placement                       | Feed                                  | Trigger                                                |
| ---------------- | ---------------------------------------- | ------------------------------- | ------------------------------------- | ------------------------------------------------------ |
| Model drift      | `detect_drift_records` (batch)           | Global, on model-change event   | Sample of old vs re-embedded content  | `wasserstein_distance > 0.3` for L2-normalized 384-dim |
| Content drift    | `OnlineDriftDetector` (streaming)        | Per-model                       | Section embeddings from compose calls | Debiased Sinkhorn divergence exceeds threshold         |
| Behavioral drift | `detect_drift_records` (batch, periodic) | Per-profile, every ~50 sessions | Accumulated query embeddings          | Wasserstein distance between windows                   |

**Recalibration protocol — Transport Plan Warping**: On model drift detection, compute
the OT transport plan T between old and new embedding distributions. Transfer learned
preference weights from old entities to new entities proportional to T's sparse coupling
entries (`SparseTransportPlan.entries[i].mass`). This preserves accumulated signal —
preferred over reset-to-priors (loses all signal) and Wasserstein barycenter bridging
(lattice-transport `barycenter` module is Unstable).

**Performance budget**: `OnlineDriftDetector` at W=128, D=384 costs 1.18MB per instance
(2 windows 393KB + 3 workspaces 786KB). Each check fires 3 Sinkhorn solves (debiased
divergence), ~4.9M FP ops. At check_interval=16 and ~3 compose calls per session, fires
roughly every 5 sessions — acceptable on M-series.

**Known gap**: `MIN_ONLINE_DRIFT_WINDOW_SIZE = 128` (hardcoded in lattice-transport)
blocks streaming behavioral drift at low query volumes. Reference window needs ~42 sessions
at 3 queries/session to fill. Batch workaround is acceptable for Phase 1-2. Consider
lowering the minimum or adding a separate low-volume detector in lattice-transport v0.3.

**Cross-crate dependency**: Add `lattice-transport = "0.2.5"` to khive workspace deps.
Integration point: `kkernel engine drift-check` stub (ADR-043 §5). Requires publishing
lattice-transport 0.2.5 to crates.io (currently only 0.2.1 published).

**Phase mapping**:

- Phase 2: Model drift detection (batch, on model-change event)
- Phase 3: Content drift detection (streaming OnlineDriftDetector in compose pipeline)
- Phase 4: Behavioral drift detection + Transport Plan Warping recalibration
- Phase 5+: Neural adapter invalidation on drift (ties to Q10 schema migration)

### Non-changes (confirmed by research)

| ADR-048 design                            | Research finding                                                                         | Status              |
| ----------------------------------------- | ---------------------------------------------------------------------------------------- | ------------------- |
| ParlayANN prefix-doubling build (Q3)      | Confirmed as best Vamana parallelism strategy                                            | No change           |
| Two-pass alpha (1.0 then 1.2) (Q4)        | Confirmed; adaptive alpha is Phase 3 optimization                                        | No change           |
| mmap + MADV_RANDOM for Phase 1 (Q10)      | Confirmed for ≤1.5GB; io_uring for Phase 2 at 10M+                                       | No change           |
| AND-composition of lint rules (Q7)        | Confirmed; auto-fix should NOT chain                                                     | No change           |
| ESS cap as primary temporal strategy (Q1) | Confirmed but as approximation, not principled posterior; combine with BOCD in Phase 2-3 | Deferred            |
| Binary feedback (Q9)                      | Under-specified; Dirichlet-tree recommended                                              | Deferred to Phase 3 |

## References

- [ADR-032](ADR-032-brain-pack.md): Brain pack — profiles, posteriors, feedback
- [ADR-047](ADR-047-knowledge-pack.md): Knowledge pack — corpus tier, atom schema
- [ADR-002](ADR-002-edge-ontology.md): Edge ontology — closed relation set
- Atlas `khive_domains/atom/types/section.py`: Canonical section type enum
- Atlas `khive_domains/eval/weight_tuner.py`: Three tuning modes (empirical,
  perturbation, Thompson sampling)
- Atlas `khive_domains/eval/weights.py`: Per-role section weights
