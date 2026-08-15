# ADR-152: kg-editor Interaction Grammar — Navigation, History, and Review Conversation

**Status**: Proposed\
**Date**: 2026-08-10\
**Authors**: khive maintainers\
**Related**: [ADR-145](ADR-145-local-first-kg-workbench.md),
[ADR-147](ADR-147-repo-showcase-bundle.md),
[ADR-151](ADR-151-kg-editor-design-language.md)

## Context

The showcase and the workbench are both exploration tools over bounded projections of large
graphs. Exploration UIs fail in predictable ways: navigation trails grow without bound,
history fragments into per-type pages, review data renders as walls of undifferentiated
rows, and half the surface is reachable only by mouse. ADR-151 governs how surfaces look;
this ADR governs how they behave.

The unifying principle: borrow interaction metaphors users already own — the browser's
back/forward stack, the email client's thread list and conversation stream, version-control
history — instead of inventing navigation. An interface a user can predict from muscle
memory beats a novel one that must be learned, and interfaces here are judged by
scanability and predictability before visual novelty.

## Decision

### D1 — Browser-grade navigation, no unbounded trails

Graph and view exploration maintains a history stack with back/forward controls that behave
exactly like the browser's, and integrates with the actual browser history where routing
allows. Breadcrumb trails that append per navigation step are rejected: any trail visible at
once is bounded to the containment path of the current focus (e.g., project → package →
module), never the user's walk history. The walk lives in the stack, reachable through
back/forward.

### D2 — One unified History timeline

Each vertical presents one chronological history surface, not separate pages per record
type. In the workbench, review activity — change-set operations, validation runs, review
events, conversation — interleaves on one timeline with type facets as _filters over the one
timeline_, never as separate destinations. In the showcase, the cadence view is likewise one
timeline faceted by series. Splitting history by type forces the user to reassemble
chronology in their head; that reassembly is the tool's job.

### D3 — Review reads as a thread

The change-set review surface uses the email shape: a scannable list pane (one row per
reviewable unit — bounded, uniform, monospace identifiers) and a conversation pane where the
selected unit's content streams chronologically — operations, findings, evidence, and
review annotations in order. The list answers "what needs my attention"; the thread answers
"what is the story of this one." Both ADR-145 review variants render through this shape.

### D4 — Selection drives a contextual inspector

One selection model spans graph views, lists, and timelines: selecting any addressable
object (node, edge, change row, finding, commit) binds the inspector panel to it. The
inspector is contextual — its content is the selection's type-specific card — never a static
rail that renders the same regardless of focus. Empty selection shows the current scope's
summary card, which per ADR-151 D5 invites the next action.

### D5 — Keyboard-first operation

Every navigational and review action is reachable by keyboard. A command palette
(Cmd/Ctrl-K) exposes navigation targets, view switches, and non-destructive actions by
name. List and timeline surfaces support j/k-style row traversal and Enter-to-open.
Focus order follows visual order; the palette and traversal are tested, not aspirational.

### D6 — Boundedness is a first-class interaction

ADR-145 D9 and ADR-147 D6 make truncation and availability wire-level facts. This ADR makes
them interactions: a truncation badge on any bounded surface states the bound and the total
when known, and where a wider bound exists server-side, the badge is the affordance that
requests the next page. Unavailable capabilities render their reason on demand (per ADR-145,
e.g. the same-family review refusal explains itself). No control silently does nothing: an
action that cannot proceed states why where the user clicked.

## Consequences

- Navigation and history behavior become testable contracts (stack semantics, single
  timeline, keyboard reachability) instead of per-view improvisation.
- The thread shape constrains how future conversation/comment enrichments land in ADR-145's
  `pull_request` variant: they extend the stream, not a new pane.
- Command palette and keyboard traversal add up-front cost to every new surface; the
  compounding return is that power users never leave the keyboard.

## Acceptance criteria

- Back/forward traverse the exploration history across graph, list, and timeline views;
  no UI element accumulates unbounded navigation state.
- Each vertical has exactly one history timeline; type filters filter it in place.
- The workbench review route renders list-plus-thread and both `khive.review.v1` variants
  flow through it.
- Every interactive surface passes a keyboard-only walkthrough; the palette reaches every
  named view.
- Truncated, unavailable, empty, and loading states each surface their reason or bound at
  the point of interaction, covered by component tests.
