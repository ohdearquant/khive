# ADR-153: Typed Graph Visual Encoding

**Status**: Proposed\
**Date**: 2026-08-10\
**Authors**: khive maintainers\
**Related**: [ADR-001](ADR-001-entity-kind-taxonomy.md),
[ADR-002](ADR-002-edge-ontology.md),
[ADR-013](ADR-013-note-kind-taxonomy.md),
[ADR-055](ADR-055-epistemic-edge-relations.md),
[ADR-145](ADR-145-local-first-kg-workbench.md),
[ADR-147](ADR-147-repo-showcase-bundle.md),
[ADR-151](ADR-151-kg-editor-design-language.md)

## Context

khive's ontology is closed: 9 entity kinds, 17 edge relations in named families, 5 note
kinds. That closure is the product's central bet, and it is also a rendering opportunity no
generic graph library exploits: a closed vocabulary can have a _complete_ visual encoding,
where every kind and relation has one stable visual identity and a reader who has learned
the legend once can read any khive graph anywhere — showcase, workbench, or any future
surface.

Today each graph view chooses its own colors and line styles ad hoc. The same
`depends_on` edge renders differently across views, derived exporter edges (ADR-147 D5)
are visually indistinguishable from ingested ones, and epistemic relations (ADR-055) carry
no visual semantics at all. Nothing distinguishes an encoding decision from a styling
accident.

## Decision

### D1 — A complete, closed visual vocabulary

The encoding covers the full ontology and only the ontology; it is amended when the
ontology is amended (ADR-001/002/013 process), never extended ad hoc by a view.

- **Entity kinds (9)**: each kind has a stable pairing of icon (per the ADR-151 SVG
  contract) and hue. The pairing is defined once in a shared legend module consumed by
  every graph, list, and inspector surface.
- **Edge relations (17)**: line treatment is assigned per ADR-002 _family_, with
  relation-level differentiation inside a family only where the family has more than one
  member on screen: structure (contains/part_of/instance_of) as quiet solid lines;
  derivation and provenance as directional treatments; dependency
  (depends_on/enables) as the assertive solid family; lateral symmetric relations
  (competes_with/composed_with) as undirected treatments; `annotates` as the recessive
  dashed treatment; epistemic `supports`/`refutes` (ADR-055) as the one place semantic
  color applies to edges — the support/refute tokens from ADR-151 D2, since their meaning
  is evaluative, not structural.
- **Note kinds (5)**: notes render as satellite chips off their anchor, tinted by kind,
  visually subordinate to entities.
- **Derived vs ingested**: an exporter-derived edge (ADR-147 D5) always carries a visible
  derivation mark distinguishing it from ingested assertions. A reader must never mistake
  a computed join for a stored claim.

### D2 — Level-of-detail ladder

Graph views render along a bounded ladder: cluster/overview (packages or kind-clusters,
aggregate counts on super-nodes) → mid (entities with icons and labels for the focus
neighborhood) → detail (full cards in the inspector, not on the canvas). Zoom and expansion
move along the ladder under the wire-level budgets of ADR-145 D9 / ADR-147; a collapsed
super-node states its contained count, and expansion beyond budget surfaces the truncation
interaction of ADR-152 D6. Node sizing encodes exactly one declared quantity per view
(degree, dependent count, or churn — named in the view's legend), never an undeclared mix.

### D3 — Focus and overlay semantics

Selection focuses the subgraph: the selected node, its neighbors at the declared hop bound,
and their edges render at full strength; the remainder dims to context (within ADR-151
contrast floors — dimmed is still readable). Analytical overlays (hotspot quadrant
membership, hidden-coupling pairs, ownership concentration from ADR-147 aggregates) tint
or badge nodes _on top of_ the base encoding and are exclusive: one overlay at a time,
named in the legend while active. Overlays never repaint the kind hue — kind identity
survives every overlay.

### D4 — Deterministic layout

Same bundle, same view parameters, same layout: layout algorithms run with fixed seeds and
stable input ordering (the deterministic ordering the bundles already guarantee). Review
reproducibility depends on it — two reviewers of one change-set see one picture, and
screenshot-based visual regression becomes possible. Force-directed animation may run at
interaction time, but the settled state is the deterministic one.

### D5 — Encoding is never hue alone

Every distinction the encoding makes survives grayscale: kind pairs icon with hue, edge
families pair line treatment with any color, epistemic edges pair color with a directional
glyph, derivation marks are geometric. The legend is a permanent, compact on-canvas
affordance, not documentation.

## Consequences

- The legend module becomes a single point of truth consumed by three lanes (showcase,
  workbench, ADR-146), ending per-view encoding drift; it is also a natural showcase asset —
  the encoding itself demonstrates the ontology.
- Ontology amendments acquire a small additional cost: a new kind or relation ships with
  its visual identity. That coupling is deliberate; an unrenderable kind is an incomplete
  addition.
- Deterministic layout trades some aesthetic freedom of force simulation for
  reproducibility; the trade is correct for a review tool.

## Acceptance criteria

- A shared legend module enumerates all 9 + 17 + 5 identities plus the derived-edge mark;
  a completeness test fails when the ontology and the legend disagree.
- Derived edges are visually distinct from ingested edges in every graph view.
- Rendering the same golden bundle twice yields identical settled layouts (asserted by
  serialized position snapshot).
- A grayscale render of the legend remains fully distinguishable.
- Each analytical overlay renders exclusively, with the active overlay named on-canvas.
