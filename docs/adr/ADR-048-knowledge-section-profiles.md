# ADR-048: Knowledge Section Profiles

**Status**: proposed
**Date**: 2026-05-27
**Authors**: Ocean, lambda:khive

## Context

Knowledge atoms in the corpus tier ([ADR-047](ADR-047-knowledge-pack.md)) store content as
flat text. The atlas lore system structures atom content into typed sections — overview,
core model, boundary conditions, formalism, operational guidance, examples, failure modes,
expert lens — each with a semantic role that determines its value to different agent roles.

An implementer agent needs operational guidance and examples; a theorist needs formalism
and core model. Today, `knowledge.compose` returns the same content regardless of who asks.
There is no mechanism for the system to learn which sections are valuable to which consumers
over time.

The brain pack ([ADR-032](ADR-032-brain-pack.md)) provides Beta-Binomial posterior tracking
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

| Kind       | What it is                           | entity_type sub-classification           |
| ---------- | ------------------------------------ | ---------------------------------------- |
| `resource` | Actionable content agents consume    | atom, domain, skill, tool, template, prompt, runbook |

The distinction from `concept`: a concept models "what IS it" (structural graph position,
edges to other concepts, papers, projects). A resource models "how to USE it" (section-typed
content, embeddings, composition weights). They link via `annotates`: resource annotates
concept.

Resources participate in the full graph — they can have edges, be traversed, appear in
search results alongside concepts and documents. The knowledge pack creates resources
(entity_type=atom, entity_type=domain) and manages their content in the `knowledge_atoms` /
`knowledge_sections` tables. The entity row in `entities` gives them graph position; the
content tables give them deep searchable content.

### Sections as a dedicated table

Sections are sub-records of resource/atom entities, stored in `knowledge_sections`:

```sql
CREATE TABLE knowledge_sections (
    id          TEXT PRIMARY KEY,
    atom_id     TEXT NOT NULL,
    namespace   TEXT NOT NULL,
    section_type TEXT NOT NULL,  -- closed enum: 10 values
    heading     TEXT NOT NULL,
    content     TEXT NOT NULL,
    tokens      INTEGER NOT NULL DEFAULT 0,
    sort_order  INTEGER NOT NULL DEFAULT 0,
    created_at  INTEGER NOT NULL,
    updated_at  INTEGER NOT NULL,
    FOREIGN KEY (atom_id) REFERENCES knowledge_atoms(id)
);
```

Section_type is a closed enum matching the atlas schema v1: `overview`, `core_model`,
`boundary_conditions`, `formalism`, `operational_guidance`, `examples`, `failure_modes`,
`expert_lens`, `references`, `other`.

**Editing a section does not touch other sections.** `knowledge.edit(slug, sections=[...])`
updates only the named section rows. Each section has its own embedding vector (re-embedded
on edit, not the whole atom). The atom's own embedding (from description + keywords) is
separate and only updates when the atom-level metadata changes.

**Sections link to atoms structurally (FK), not via graph edges.** The section→atom
relationship is always 1:N containment — there's no semantic edge type needed. All
cross-entity connections for a section route through its parent atom's graph edges.

### Embedding strategy (three levels)

| Level | Source text | Standard length | Updates when |
|---|---|---|---|
| **Domain** | description + purpose + member slug prose | 100-200 tokens | domain metadata edited |
| **Atom** | description (50-150 tok) + keywords as coherent sentences (50-100 tok) | 150-250 tokens | atom description/tags edited |
| **Section** | section body content | up to 500 tokens (chunk if longer) | that section's content edited |

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

Every `knowledge.edit` call creates an `observation` note annotating the atom:

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

| Scale | What | Graph memory | PQ memory | Vectors | Query latency | Strategy |
|---|---|---|---|---|---|---|
| 342K atoms | Current corpus | 88MB | 16MB | 527MB (RAM) | <1ms | sqlite-vec brute force |
| 2M sections | After section split | 512MB | 96MB | 3GB (RAM) | <5ms | khive-hnsw in-memory |
| 10M atoms | Full lore absorption | 2.5GB | 480MB | 15GB (SSD) | <5ms | DiskANN: graph+PQ in RAM, vectors on SSD |
| 100M sections | Multi-project corpus | 25GB | 4.8GB | 150GB (SSD) | <10ms | Sharded DiskANN + RaBitQ filtering |

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

### Namespace injection: session-to-actor mapping

The MCP server already supports `--actor` / `KHIVE_ACTOR` / config file `[actor] id` for
namespace resolution. The missing piece is **per-session injection** — the same MCP
server process serves all Claude Code sessions, but each session has a different lambda
identity.

The current MCP protocol does not carry per-request caller identity. Two approaches:

**Approach A: Hook-injected env (current best option)**

The `UserPromptSubmit` hook detects the lambda from cwd and writes an actor file:

```bash
# Hook detects: cwd=/Users/lion/projects/khive/khive → lambda:khive
echo "lambda:khive" > /tmp/claude_hooks/actor_context
```

The MCP server reads this file on each `request` dispatch and uses it as the actor
for brain.resolve and namespace scoping. This is imprecise (races between concurrent
sessions) but works for single-user local dev.

**Approach B: MCP request-level context (future)**

A future MCP protocol extension could carry caller context in the request envelope:

```json
{"tool": "request", "args": {"ops": "...", "_context": {"actor": "lambda:khive", "session": "abc123"}}}
```

The server would use `_context.actor` for brain resolution and `_context.session` for
feedback correlation. This eliminates the race condition in Approach A but requires MCP
protocol changes.

For v1, Approach A is sufficient. The hook-based actor injection works for the primary
use case (single developer, one active session per project).

### The hook opportunity

Claude Code sessions have a session ID and are invoked from a known working directory with
a known lambda identity. A `UserPromptSubmit` hook already runs at the start of each turn.
If the hook injects the resolved profile into the MCP namespace context, every
`knowledge.compose` call in that session automatically uses the right profile — no agent
cooperation required.

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
   compose response's section manifest)

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
    {"type": "overview", "heading": "Overview", "offset": 0, "tokens": 85},
    {"type": "core_model", "heading": "Core Model", "offset": 312, "tokens": 210},
    {"type": "operational_guidance", "heading": "Implementation", "offset": 1024, "tokens": 340}
  ],
  "profile": "computational_engineering"
}
```

The `content` column remains flat markdown. Sections are byte-offset ranges into
the content, parsed at ingest time. This avoids schema changes — the section manifest
is metadata, not a new column.

The `profile` field maps to one of five atom profiles that determine default section
selection (from the atlas taxonomy):

| AtomProfile                  | Default sections                                                        |
| ---------------------------- | ----------------------------------------------------------------------- |
| `formal_mathematical`        | overview, core_model, formalism, examples, failure_modes, expert_lens   |
| `mechanistic_empirical`      | overview, core_model, boundary_conditions, examples, failure_modes, expert_lens |
| `computational_engineering`  | overview, core_model, formalism, operational_guidance, examples, failure_modes, expert_lens |
| `institutional_decision`     | overview, core_model, boundary_conditions, operational_guidance, examples, failure_modes, expert_lens |
| `interpretive_historical`    | overview, core_model, examples, failure_modes, expert_lens              |

### 2. Section posteriors in brain profiles

A new `consumer_kind = "knowledge_compose"` is added to the brain profile system.
Profiles of this kind maintain per-section-type Beta posteriors:

```json
{
  "section_posteriors": {
    "overview":              {"alpha": 2.0, "beta": 2.0},
    "core_model":            {"alpha": 4.0, "beta": 2.0},
    "boundary_conditions":   {"alpha": 2.0, "beta": 3.0},
    "formalism":             {"alpha": 1.5, "beta": 4.0},
    "operational_guidance":  {"alpha": 6.0, "beta": 1.5},
    "examples":              {"alpha": 5.0, "beta": 2.0},
    "failure_modes":         {"alpha": 3.0, "beta": 2.0},
    "expert_lens":           {"alpha": 3.0, "beta": 2.0}
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

`knowledge.compose` gains an implicit profile resolution step:

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

| Session context | Hook sets actor to | Resolves to |
| --- | --- | --- |
| khive implementer subagent | `lambda:khive:implementer` | `khive-impl-v1` (exact compound match) |
| khive session, no role | `lambda:khive` | `khive-knowledge-v1` (project match) |
| lionagi theorist subagent | `lambda:lionagi:theorist` | `lionagi-theory-v1` (exact compound match) |
| unknown project, implementer | `local:implementer` | `impl-knowledge-v1` (role match) |
| completely generic | `local` | `balanced-knowledge-v1` (wildcard) |

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

### 6. Hook-injected profile context (detailed architecture)

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

| Failure mode | Consequence | Mitigation |
| --- | --- | --- |
| Hook A doesn't fire (no actor_context) | Compose uses wildcard profile | Acceptable fallback; balanced profile still works |
| Hook B doesn't fire (no buffer) | No feedback for this session | Posterior unchanged; no harm, just slower learning |
| Hook C misattributes usage | Wrong section gets "useful"/"not_useful" | Beta priors dampen noise; needs ~5 consistent wrong signals to shift by 0.1 |
| Session crashes (no deferred attribution) | Buffer is orphaned | Cron job cleans buffers older than 24h; immediate attribution still fires |
| Two concurrent sessions write same actor | Race on actor_context file | Per-session actor file keyed by session_id (not shared path) |
| Agent explicitly calls brain.feedback too | Double-counting | Dedup by event_id in compose buffer; same event_id → skip hook feedback |

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

### 9. Graph health and export

**`knowledge.health`** verb — actionable diagnostic:

```
knowledge.health() → {
  orphan_entities: [{id, name, kind}],        // 0 edges
  dangling_edges: [{edge_id, source, target}], // target deleted
  under_linked: [{id, name, kind, edge_count, min_required}],
  direction_violations: [{edge_id, relation, source_kind, target_kind}],
  missing_entity_type: [{id, name, kind}],     // project/resource without entity_type
  total_entities: N,
  total_edges: N,
  avg_density: f64
}
```

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

### Phase 1: Section schema (khive-pack-knowledge)

- Add `SectionType` enum and `SectionManifest` struct to `schema.rs`
- Add section parsing to `upsert_atoms` (detect `##` headers, normalize via
  header map, compute byte offsets and token counts)
- Store section manifest in `properties.sections` JSON
- Add `knowledge.import` verb for file-based ingest
- Add `knowledge.edit` verb for section-level editing
- Update `knowledge.compose` (new verb) to assemble sections with weights

### Phase 2: Brain integration (khive-pack-brain)

- Add `SectionPosteriorState` alongside `BalancedRecallState`
- Register `consumer_kind = "knowledge_compose"` in brain profile system
- Extend `brain.feedback` params with optional `section_signals` map
- Add fold/reduce path for section-level Beta updates
- Create seed profiles for common roles (implementer, theorist, researcher, etc.)

### Phase 3: Hook wiring (.claude/hooks/)

- `UserPromptSubmit` hook: resolve profile from session identity
- `PostToolUse` hook: buffer compose calls
- Session-end hook: correlate with task outcomes, emit feedback
- Map Claude Code session IDs to lambda identity via cwd heuristic

### Phase 4: knowledge.suggest and knowledge.compose verbs

- `knowledge.suggest` — domain discovery with profile-weighted scoring
- `knowledge.compose` — two-stage: suggest → assemble sections with weights
  from brain profile posteriors. Budget-constrained by token limit.
  Returns section-typed markdown with per-section scores.

## References

- [ADR-032](ADR-032-brain-pack.md): Brain pack — profiles, posteriors, feedback
- [ADR-047](ADR-047-knowledge-pack.md): Knowledge pack — corpus tier, atom schema
- [ADR-002](ADR-002-edge-ontology.md): Edge ontology — closed relation set
- Atlas `khive_domains/atom/types/section.py`: Canonical section type enum
- Atlas `khive_domains/eval/weight_tuner.py`: Three tuning modes (empirical,
  perturbation, Thompson sampling)
- Atlas `khive_domains/eval/weights.py`: Per-role section weights
