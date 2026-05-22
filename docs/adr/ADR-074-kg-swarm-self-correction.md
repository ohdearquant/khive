# ADR-074: KG Swarm Self-Correction Mechanism

**Status**: proposed\
**Date**: 2026-05-21\
**Authors**: Ocean, lambda:khive

## Context

The kg plugin's agent swarm (digester → polisher → gap-analyst → expander) operates in a loop:
find gaps → expand → polish → repeat. Without self-correction, this loop amplifies noise:

- **Gap-analyst** flags missing edges as gaps, even when the absence is intentional.
- **Expander** creates speculative entities to fill gaps, lowering graph precision.
- **Polisher** validates structure but not semantic accuracy — a well-linked wrong entity passes.

The net effect: each cycle adds more entities and edges, but graph quality degrades. The swarm
needs a feedback signal that distinguishes "the graph is growing" from "the graph is improving."

## Decision

### Quality score per entity

Each entity gains a `quality_score` in properties (0.0-1.0), computed from:

- **Edge density**: ratio of actual edges to expected edges for its kind (from ADR-002 minimums).
- **Source attribution**: does the entity cite a source (paper, URL, codebase)?
- **Recency**: when was it last verified or updated?
- **Usage**: how often is it retrieved in searches or traversals?

The score is recomputed by the polisher on each pass and stored in `properties.quality_score`.

### Expansion gate

The expander MUST check quality scores before creating new entities:

1. If the average quality_score of the expansion target's neighborhood is below 0.5, **stop
   expanding and polish first**. Low-quality neighbors suggest the local graph is noisy.
2. New entities start with `quality_score = 0.3` (unverified). They must reach 0.5 within
   two polish cycles or get flagged for review.
3. The gap-analyst weights gaps by the quality of surrounding entities — a gap in a
   high-quality region is more valuable than a gap in a low-quality region.

### Convergence detection

The swarm tracks per-cycle metrics:

- `entities_added`, `entities_removed`, `edges_added`, `edges_removed`
- `avg_quality_before`, `avg_quality_after`
- `gaps_found`, `gaps_closed`

When `avg_quality_after - avg_quality_before < 0.01` for two consecutive cycles, the swarm
declares convergence and stops. This prevents infinite loop expansion.

### Human-in-the-loop escape

Entities with `quality_score < 0.3` after two cycles get flagged with
`properties.needs_review = true`. The `kg:polish` skill surfaces these for human verification
rather than auto-expanding around them.

## Consequences

- The swarm loop becomes self-terminating (convergence detection).
- Graph quality is measurable and trackable over time.
- The gap→expand→polish loop has a quality gate that prevents noise amplification.
- Adds `quality_score` and `needs_review` to entity properties (no schema change needed —
  properties is a JSON map).

## Alternatives considered

1. **Fixed iteration count** — run exactly N cycles. Rejected: doesn't adapt to graph state.
   A sparse graph needs more cycles; a dense one needs fewer.
2. **Human approval per expansion** — require approval for each new entity. Rejected: defeats
   the purpose of autonomous swarm operation. The quality gate is the automated equivalent.
3. **Rollback mechanism** — undo the last cycle if quality dropped. Attractive but complex:
   requires transactional entity creation. Deferred to KG versioning (ADR-042) where branch +
   merge provides natural rollback.
