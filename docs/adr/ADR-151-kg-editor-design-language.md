# ADR-151: kg-editor Design Language and Token Contract

**Status**: Proposed\
**Date**: 2026-08-10\
**Authors**: khive maintainers\
**Related**: [ADR-145](ADR-145-local-first-kg-workbench.md),
[ADR-147](ADR-147-repo-showcase-bundle.md)

## Context

`apps/kg-editor` now carries two product verticals over one component base: the ADR-147
repository showcase (default route, ten analysis views) and the ADR-145 KG review workbench
(`/review`). ADR-147 explicitly counts component transfer between the two lanes as a consequence
of its design, and ADR-146 (in flight) will add a third consumer of the same rendering
components.

Both governing ADRs are data and trust contracts. Neither says a word about visual language.
Today every view invents its own spacing, color, icon treatment, and empty-state shape, and
nothing stops the app from drifting toward the generic admin-template look that data-heavy
tools default to. With three lanes sharing components, visual drift compounds: the same entity
rendered two ways reads as two different products.

A design language is enforceable only if it is written down as a contract with testable
clauses. This ADR is that contract. It intentionally binds aesthetics the way ADR-145 binds
trust boundaries: specific, checkable, and fail-closed where a machine can check, with the
judgment calls named as review criteria rather than left implicit.

## Decision

### D1 — One design language, dark-first, across both verticals

The showcase and the workbench share a single design language. Component-level styling is
expressed exclusively through the shared token set (D2); a view may compose tokens but MUST NOT
introduce literal color, radius, shadow, or type values. The base theme is dark; a light theme
is a token remap, not a component rewrite.

The intended character, stated as a review criterion: restrained, warm, and material. Premium
through meaning and restraint, not gloss. Dense where the user scans, generous where the user
reads or edits. The failure modes this clause exists to reject are equally specific: flat
generic admin-template surfaces, sterile minimalism, decorative gradients without informational
content, and stock-dashboard visual noise.

### D2 — Token contract

All visual constants live in one token layer (CSS custom properties, mapped into Tailwind
theme config so utilities resolve to tokens):

- **Surfaces**: a near-black warm neutral base (not pure `#000`, not a cold blue-gray), with
  at most three elevation steps. Elevation is expressed by surface value and hairline borders,
  not drop shadows.
- **Accent**: one warm accent family (stone/amber range) used for primary actions, focus, and
  selection. Semantic colors (success, warning, error, epistemic support/refute) are their own
  tokens and are never reused for decoration.
- **Text contrast floors**, validated against the base surface: primary text at full strength;
  secondary text at a minimum of 0.7 alpha-equivalent; muted/metadata text at a minimum of
  0.5 alpha-equivalent. These floors are validated on dense data tables at 1x zoom, never on
  isolated swatches — a value that passes on a swatch and fails on a table fails.
- **Type**: one UI family and one monospace family. Identifiers, hashes, paths, counts, and
  code render in monospace. Type scale is a token ramp; components do not set raw sizes.
- **Spacing and radius**: a single spacing ramp and a single radius ramp; tables and dense
  lists use the compact end, reading and editing surfaces use the generous end.

Enforcement: a lint gate fails any component stylesheet or class string carrying a literal
color value outside the token layer, and an automated contrast check computes the effective
contrast of text tokens over surface tokens in both themes.

### D3 — One SVG contract

Every icon in the application follows one contract: `viewBox="0 0 24 24"`, 1.5px stroke,
round caps and joins, literal shapes over abstract glyphs. Icons from different sources that
disagree in stroke weight or corner treatment read as two different products inside one
window; a lint test asserts the contract over the icon directory, and rail icons and body
icons come from the same set.

### D4 — Density, scanability, and generous editing space

Dense surfaces (tables, change lists, graph legends) optimize for scanability: uniform row
shape, monospace identifiers, columns before prose. Reading and editing surfaces (an entity
card, a rule finding's evidence, a review comment) get generous space — the dominant element
of an editing view fills the view, full-height where the content is the point. Cramped modal
editors over dense backgrounds are rejected; editing happens in space, not in a keyhole.

### D5 — Zero states invite action

Every empty state names what belongs there and carries the primary action to create or load
it, phrased as an invitation, with exactly one primary CTA. A bare glyph and a sentence is a
failure. Distinct states remain distinct per ADR-145/147: _empty_ ("known to contain no
items"), _unavailable_ (capability absent), _truncated_ (bounded), and _loading_ each have
their own visual treatment, and none of them is collapsed into another.

### D6 — Acceptance is visual, on the running app

Automated gates (token lint, contrast computation, icon contract test, `next build`) are
necessary and not sufficient. A change that touches visual surface area ships with
screenshots of the running app in the PR — dense view and reading view, both themes — and
the review criterion is D1's stated character. Green CI with an unstyled or off-character
page is a known failure class; the screenshot is the gate that catches it.

## Consequences

- Aesthetic drift becomes a reviewable defect with named criteria instead of a matter of
  taste re-litigated per PR.
- The token layer is a one-time migration cost for existing views; after it, theme work and
  the ADR-146 lane inherit the language for free.
- The contract constrains contributors who would prefer a component library's defaults; that
  is the point, and the escape hatch is amending this ADR, not overriding a token locally.

## Acceptance criteria

- Token layer exists; no component carries literal color values (lint-enforced).
- Contrast floors hold computationally in both themes and visually on the densest table.
- Icon lint passes over the full icon set.
- All four empty-state classes render distinctly in both verticals.
- A visual review with screenshots accompanies every surface-touching PR.
