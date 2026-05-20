# ADR-047: Frontend Views — KG Explorer and GTD Board

**Status**: superseded — superseded by ADR-049, which moved to khive-cloud ADR-030\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-003 defines the four-layer architecture and places a Next.js 15 + React 19 + Tailwind
frontend in `frontend/` as the visual face of khive. That ADR named the frontend's responsibility
as "visual KG explorer, traverse UI, research session frontend" but left the concrete view
structure, component topology, data flows, and phase plan unspecified.

Two views are immediately useful:

1. A **KG Explorer** (`/kg`) for browsing, searching, and visualizing the entity graph. The
   primary consumers are researchers who want to understand what is in the graph, how dense it
   is, and how specific entities connect. Secondary consumers are agents that surface graph state
   to humans after batch ingestion runs.

2. A **GTD Board** (`/tasks`) for managing the structured task queue. The primary consumers are
   agents reviewing their own work queue and Ocean reviewing agent output. Secondary consumers are
   future team members who need queue visibility without a CLI.

Both views call the Deno HTTP gateway (ADR-003 §Implementation), which in turn calls
`khive-mcp` via the MCP `request` tool (ADR-027). The frontend never calls `khive-mcp` directly.

### Constraints

- The Deno gateway already exposes a REST surface that wraps MCP verb calls. The frontend is a
  thin read-only consumer in phase 1; writes arrive only in phase 2.
- The 6 entity kinds (ADR-001), 13 edge relations (ADR-002), and 5 GTD statuses (ADR-026) are
  all closed sets known at compile time. Component discriminants can be typed against them without
  a runtime lookup.
- Phase 1 ships read-only views. No drag-and-drop, no inline editing, no mutations.
- `deno fmt --check` must pass for all files in `deno/` and `frontend/`.

### What this ADR does not cover

- The Deno HTTP gateway route design (to be addressed in a separate ADR when the gateway is
  scaffolded).
- Authentication and authorization at the frontend layer (deferred; ADR-034 covers the extension
  traits).
- Mobile layout (future; desktop-first for phase 1).
- The memory pack visualization (future; ADR-036 is proposed, not accepted).

## Decision

### Route structure

```
frontend/app/
├── page.tsx          — Dashboard: summary stats + recent entity/task activity
├── kg/
│   └── page.tsx      — KG Explorer root (entity browser, sub-views via URL params)
└── tasks/
    └── page.tsx      — GTD Board root (kanban + swimlanes)
```

URL params carry view-layer state (active entity ID, selected subview, active assignee filter)
so views are bookmark-able and shareable.

---

### View 1: KG Explorer (`/kg`)

The KG Explorer composes four sub-views, switchable via a tab bar. All four share a global
search bar and an entity kind filter strip.

#### 1.1 — Entity Browser

A paginated, filterable table/grid of all entities in the namespace.

**Component: `EntityBrowser`**

```
┌──────────────────────────────────────────────────────────────────┐
│  [Search...] [concept▾] [document▾] [dataset▾] [project▾] …     │
│  [domain: attention ×] [status: implemented ×]                   │
├─────────────────────────────────────────────────────────────────-┤
│  Kind       Name              Domain          Status    Edges    │
│  ────────   ────────────────  ──────────────  ────────  ─────    │
│  concept  · FlashAttention    attention       shipped   12       │
│  document · Flash Paper 2022  attention       —         4        │
│  project  · lattice-embed     inference       shipped   8        │
│  …                                                               │
├──────────────────────────────────────────────────────────────────┤
│  ← prev  Page 1 of 14  next →                                    │
└──────────────────────────────────────────────────────────────────┘
                                                ↓ click row
                                       ┌────────────────────────┐
                                       │  Property Inspector    │
                                       │  (side drawer)         │
                                       │  name, kind, tags,     │
                                       │  properties bag,       │
                                       │  edge count by relation│
                                       │  [Open in graph view]  │
                                       └────────────────────────┘
```

**Filters** (AND-combined):

- Kind: multi-select toggle strip (concept / document / dataset / project / person / org)
- Domain: derived from `properties.domain` tag; rendered as pills
- Status: derived from `properties.status` tag
- Free-text: passed verbatim to `search(kind="entity", query=...)`

**Data flow**:

```
EntityBrowser
  → search(kind="entity", query=<text>, limit=25, offset=<page × 25>)   [search mode]
  → list(kind="entity", filters={entity_kind: <selected kinds>})          [browse mode]
  → get(id=<selected row>) → PropertyInspector
```

`search` is used when the search bar is non-empty; `list` is used otherwise (browse mode).
The two are not mixed in a single request — the active mode is communicated via URL param
`?q=...`.

**PropertyInspector** is a right-side drawer (slide in from right on row click, close on
`Escape` or click-outside). It renders:

- Header: name, kind badge, full UUID (copyable)
- Tags as pill list
- Properties bag as a key-value table (keys sorted alphabetically)
- Edge summary: count grouped by relation type (from `neighbors(node_id=<id>)`)
- A button "Open in graph view" that switches to the Neighborhood sub-view with this entity
  pre-selected

#### 1.2 — Neighborhood Graph

A React Flow canvas showing one entity and its 1-hop neighbors. Designed for understanding local
context, not the whole graph.

**Component: `NeighborhoodGraph`**

```
┌──────────────────────────────────────────────────────────────────┐
│  Entity: [FlashAttention      ▾]  [depth 1 ▾]  [Expand] [Reset] │
├──────────────────────────────────────────────────────────────────┤
│                         (React Flow canvas)                      │
│                                                                  │
│       ┌──────────────┐                                           │
│       │  GQA concept │──competes_with──►  [Softmax Attention]   │
│       └──────────────┘                                           │
│              ↑ instance_of                                       │
│       ┌──────────────┐    introduced_by                          │
│       │ FlashAttn 2  │───────────────►  [Flash Paper 2022]       │
│       │  (selected)  │                                           │
│       └──────────────┘    implements                             │
│              │──────────────────────►  [lattice-embed project]  │
│                                                                  │
│       [Legend: ● concept  ■ document  ▲ project  …]             │
└──────────────────────────────────────────────────────────────────┘
```

**Nodes**:

- Colored by entity kind (fixed palette: concept=blue, document=amber, dataset=teal,
  project=emerald, person=violet, org=slate)
- Label: entity name (truncated to 28 chars with tooltip)
- Selected node: ring outline, slightly larger
- Click: opens PropertyInspector for that entity

**Edges**:

- Labeled with relation name, rendered as directed arrows
- Relation-to-color mapping (derived from the 6 ADR-002 categories):
  - Structure (contains / part_of / instance_of): gray
  - Derivation (extends / variant_of / introduced_by / supersedes): purple
  - Dependency (depends_on / enables): orange
  - Implementation (implements): green
  - Lateral (competes_with / composed_with): red
  - Annotation (annotates): blue-gray

**Interaction**:

- The entity selector (top left) is an autocomplete search field backed by
  `search(kind="entity", query=...)`. Selecting an entity re-fetches the neighborhood.
- Depth selector: 1-hop (default) or 2-hop. At depth 2 the fetch uses
  `traverse(roots=[<id>], max_depth=2)`.
- Expand: clicking a neighbor node loads that node's own neighbors and merges them into
  the canvas.
- Layout: ELK hierarchical for mixed structural/derivation edges; dagre as fallback when ELK
  is unavailable. Force-directed (D3-force via React Flow built-ins) for lateral-only subgraphs.
- Reset: returns to depth-1 view of the originally selected entity.

**Data flow**:

```
NeighborhoodGraph — initial load (parallel batch):
  [neighbors(node_id="<id>", direction="both"), get(id="<id>")]

NeighborhoodGraph — neighbor name resolution (parallel batch):
  [get(id="<n1>"), get(id="<n2>"), …]

NeighborhoodGraph — depth-2 expansion:
  traverse(roots=[<id>], max_depth=2)
```

Neighbor details are resolved lazily (only IDs come back from `neighbors`; names and kinds are
fetched in a second parallel batch of `get` calls).

#### 1.3 — Path Finder

Displays the shortest path between two entities as a linear chain.

**Component: `PathFinder`**

```
┌──────────────────────────────────────────────────────────────────┐
│  From: [FlashAttention      ▾]  →  To: [lattice-embed      ▾]   │
│  [Find path]                                                     │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  FlashAttention  ──implements──►  lattice-embed                  │
│                                                                  │
│  (1 hop)                                                         │
└──────────────────────────────────────────────────────────────────┘
```

Both entity selectors are autocomplete search fields (same pattern as the Neighborhood Graph
selector).

**Data flow**:

```
PathFinder
  → query("<GQL shortest-path pattern>")
```

The GQL shortest-path query uses the `query` verb with the path pattern specified per ADR-008.
The returned path is rendered as a horizontal chain of nodes connected by labeled directed
arrows. If no path exists, the component displays "No path found between these two entities."

If the query crate's shortest-path operator is not yet implemented when this view ships, the
component falls back to client-side BFS over the `traverse` result (depth up to 5) and caches
the result locally. The PathFinder sub-view is hidden behind a feature flag until the operator
ships.

#### 1.4 — Density Heatmap

A bar chart showing entity concentration by domain, intended to reveal sparse areas of the graph
that need more ingestion work.

**Component: `DensityHeatmap`**

```
┌──────────────────────────────────────────────────────────────────┐
│  Density by domain (entities per domain)                        │
│  [group by: domain ▾]  [min entities: 0 ─────● 5]               │
├──────────────────────────────────────────────────────────────────┤
│                                                                  │
│  attention         ███████████ 42                                │
│  inference         ████████ 31                                   │
│  fine-tuning       █████ 18                                      │
│  optimal-transport ██ 7                                          │
│  retrieval         █ 3                                           │
│  embedding         ░ 1                                           │
│                                                                  │
│  ░ sparse  ██ dense                                              │
└──────────────────────────────────────────────────────────────────┘
```

**Grouping dimension**: `properties.domain` tag (canonical values from `AGENTS.md`:
attention / inference / training / fine-tuning / optimal-transport / embedding / quantization /
pruning / distillation / retrieval / serving / verification). Entities without a `domain` tag
fall into an "untagged" bucket.

**Data flow**:

```
DensityHeatmap
  → list(kind="entity", limit=1000)   [paginated until exhausted for <1K namespaces]
  → client-side group-by on properties.domain
```

For namespaces with more than 1,000 entities, a server-side aggregation endpoint is required.
That endpoint is tracked as a follow-up; this ADR does not define it. Until it ships, the
component paginates `list` exhaustively and aggregates client-side. The DensityHeatmap sub-view
is hidden when the namespace entity count exceeds 1,000.

Color scale: white (0 entities) → amber-100 (1–5) → amber-400 (6–20) → amber-600 (21–50) →
amber-900 (51+). The scale is relative to the maximum domain count in the current namespace.

---

### View 2: GTD Board (`/tasks`)

The GTD Board renders the task queue as a kanban layout. Phase 1 is read-only. Phase 2 adds
drag-and-drop transitions and bulk actions.

#### 2.1 — Kanban Board

**Component: `KanbanBoard`**

Six columns, one per GTD status. Column order matches ADR-026's lifecycle direction
(left-to-right: inbox → next → active → waiting → done → cancelled). The `someday` status is
excluded from the board columns (non-actionable; surfaced in a future Backlog tab).

```
┌────────────────────────────────────────────────────────────────────┐
│  [Assignee: all ▾]  [Priority: all ▾]                              │
├─────────┬────────┬────────┬─────────┬──────┬──────────────────────┤
│ inbox   │ next   │ active │ waiting │ done │ cancelled            │
│ (12)    │ (4)    │ (2)    │ (3)     │ (89) │ (7)                  │
├─────────┼────────┼────────┼─────────┼──────┼──────────────────────┤
│ [card]  │ [card] │ [card] │ [card]  │ …    │ …                    │
│ [card]  │ [card] │ [card] │ [card]  │      │                      │
│ [card]  │ …      │        │         │      │                      │
│ …       │        │        │         │      │                      │
└─────────┴────────┴────────┴─────────┴──────┴──────────────────────┘
```

When an assignee is selected, the board switches to **swimlane mode** — one horizontal row per
assignee, columns remaining as status. The swimlane toggle is automatic when a specific assignee
filter is active.

**TaskCard** (inside each kanban cell):

```
┌──────────────────────────────────────────────┐
│  [P1]  Fix lore retrieval v2 alignment issue │
│  assignee: lambda:khive                       │
│  tags: lore  retrieval  feat/lore-revival     │
│  due: 2026-05-22                             │
└──────────────────────────────────────────────┘
```

- Priority badge: P0 (red), P1 (orange), P2 (blue), P3 (gray)
- Title: truncated to 2 lines with tooltip for full text
- Assignee: displayed as a pill
- Tags: up to 3 displayed; "+N more" pill if overflow
- Due date: displayed in relative format ("tomorrow", "in 3 days", "overdue 2d")
- Overdue tasks: red left border

Click on a card opens the TaskDetailPanel (§2.2).

**Data flow** — all six column queries issued in a single parallel `request` batch:

```
[
  tasks(status="inbox",     limit=50, assignee=<filter>),
  tasks(status="next",      limit=50, assignee=<filter>),
  tasks(status="active",    limit=50, assignee=<filter>),
  tasks(status="waiting",   limit=50, assignee=<filter>),
  tasks(status="done",      limit=50, assignee=<filter>),
  tasks(status="cancelled", limit=50, assignee=<filter>)
]
```

When no assignee filter is active, the `assignee` argument is omitted. The `done` and
`cancelled` columns are collapsed by default (showing only the count header and the first 5
cards) to reduce visual noise.

#### 2.2 — Task Detail Panel

A right-side slide-out drawer that opens on card click.

**Component: `TaskDetailPanel`**

```
┌────────────────────────────────────────────────────────┐
│  Fix lore retrieval v2 alignment issue          [×]   │
│  ────────────────────────────────────────────────────  │
│  Status:    active        Priority: P1                 │
│  Assignee:  lambda:khive  Due: 2026-05-22             │
│  Created:   2026-05-15    Updated: 2026-05-19         │
│                                                        │
│  Description                                           │
│  Lore alpha sweep found ranking inconsistencies…      │
│                                                        │
│  Tags: lore  retrieval  feat/lore-revival             │
│                                                        │
│  Depends on (2)                                        │
│  ► Fix embed model config (done)                       │
│  ► Pin lore sweep params (next)                        │
│                                                        │
│  Originating notes (1)                                 │
│  ► [observation] Lore retrieval inconsistency noticed │
│                                                        │
│  Timeline                                              │
│  2026-05-15  Created (inbox)                          │
│  2026-05-17  → next                                   │
│  2026-05-18  → active                                 │
└────────────────────────────────────────────────────────┘
```

**Sections**:

- **Header**: title + close button. Status badge + priority badge in the subheader.
- **Metadata**: assignee, due date, created at, updated at.
- **Description**: full content from `properties.description`.
- **Tags**: all tags as pills.
- **Depends on**: resolved from `neighbors(node_id=<id>, relations=["depends_on"])`. Each
  dependency is a link that opens that task's own detail panel.
- **Originating notes**: resolved from `neighbors(node_id=<id>, relations=["annotates"],
  direction="incoming")`. Each note is a link that opens the note in a secondary panel.
- **Timeline**: status transitions reconstructed from the `events` substrate (ADR-038) when
  available; falls back to "Created" + "Last updated" from note timestamps when ADR-038 is
  not yet shipped.

**Data flow** — parallel batch on panel open:

```
[
  get(id=<task id>),
  neighbors(node_id=<task id>, relations=["depends_on"]),
  neighbors(node_id=<task id>, relations=["annotates"], direction="incoming")
]
```

#### 2.3 — Bulk Actions (phase 2 only)

Multi-select checkboxes appear on each card in phase 2. A floating action bar appears at the
bottom of the screen when two or more cards are selected.

**Available actions**:

- **Transition**: move all selected tasks to a target status (only statuses valid for all
  selected cards per the ADR-026 lifecycle table are offered).
- **Reassign**: change the assignee on all selected tasks.
- **Cancel**: move all selected tasks to `cancelled`.

Bulk actions call `transition(id=<id>, status=<target>)` in a parallel batch (one call per
selected task).

---

### Shared component patterns

#### Loading and error states

Every data-fetching component uses a consistent three-state model:

```tsx
type ViewState<T> =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ok"; data: T };
```

Loading: skeleton placeholders matching the dimensions of the loaded content.
Error: inline error banner with retry button and the API error message.
Empty: an empty-state illustration with a contextual prompt ("No entities yet. Run
`/kg-digest` to ingest your first paper.").

#### Client-side caching

All entity and task data is cached in a lightweight in-process store (React Query or SWR) with
a 60-second stale time. The cache is keyed by the full request payload (verb + args). A manual
refresh button is present in each view header.

#### URL-driven state

View-layer state is serialized to URL search params:

| View               | Param               | Values                                 |
| ------------------ | ------------------- | -------------------------------------- |
| Entity Browser     | `?kind=concept,doc` | Comma-separated entity kind filter     |
| Entity Browser     | `?q=<text>`         | Free-text search query                 |
| Entity Browser     | `?entity=<id>`      | Open PropertyInspector for this entity |
| Neighborhood Graph | `?center=<id>`      | Selected center entity                 |
| Neighborhood Graph | `?depth=1\|2`       | Hop depth                              |
| Path Finder        | `?from=<id>`        | Source entity                          |
| Path Finder        | `?to=<id>`          | Target entity                          |
| GTD Board          | `?assignee=<name>`  | Active assignee filter                 |
| GTD Board          | `?task=<id>`        | Open TaskDetailPanel for this task     |
| GTD Board          | `?priority=p0,p1`   | Comma-separated priority filter        |

---

### Phase plan

| Phase   | Scope                                                       | Constraint                              |
| ------- | ----------------------------------------------------------- | --------------------------------------- |
| Phase 1 | EntityBrowser + PropertyInspector (read-only)               | No mutations                            |
| Phase 1 | KanbanBoard (6 columns, read-only) + TaskDetailPanel        | No drag-and-drop                        |
| Phase 1 | NeighborhoodGraph (depth 1, dagre layout)                   | ELK optional; dagre acceptable          |
| Phase 2 | NeighborhoodGraph expand/collapse nodes + depth-2 traversal | After phase 1 stable                    |
| Phase 2 | ELK layout integration (`react-flow-elk`)                   | After depth-2 ships                     |
| Phase 2 | PathFinder sub-view                                         | Requires query crate shortest-path op   |
| Phase 2 | DensityHeatmap                                              | Requires server-side aggregation or <1K |
| Phase 2 | Bulk actions on KanbanBoard                                 | Requires gate enforcement (ADR-035)     |

The phase 1 frontend ships as a static Next.js export served by the Deno gateway's static file
handler.

## Rationale

### Why React Flow for graph visualization?

Three alternatives were evaluated:

| Library      | Layout engines       | Interaction model        | Bundle size | Verdict        |
| ------------ | -------------------- | ------------------------ | ----------- | -------------- |
| React Flow   | ELK, dagre, D3-force | Node/edge as React nodes | ~200 KB     | Selected       |
| D3.js direct | Force simulation     | Low-level SVG            | ~80 KB      | Too imperative |
| Cytoscape.js | Dagre, CoSE, Klay    | Canvas-based; no React   | ~400 KB     | React mismatch |

React Flow's node-as-component model matches the React 19 component tree. Layout engine
pluggability (ELK/dagre) covers both hierarchical and force layouts. D3 direct would require
re-implementing the interaction layer from scratch; Cytoscape's canvas model does not compose
with React's reconciler.

### Why ELK for hierarchical layout (with dagre fallback)?

The 13-relation ontology maps to two layout archetypes:

- **Hierarchical**: structural (contains/part_of/instance_of) and derivation (extends/
  variant_of/introduced_by/supersedes) edges have a natural parent-child direction. ELK's
  `LAYERED` strategy respects these directions without user-controlled positions.
- **Force-directed**: lateral edges (competes_with/composed_with) have no inherent direction
  and cluster naturally under spring physics.

ELK is the better-maintained hierarchical layout library. Dagre is the fallback because it
ships with `@dagrejs/dagre` and requires no WASM load. If the `react-flow-elk` adapter fails
to load (SSR edge case), the component falls back to dagre gracefully.

### Why six kanban columns and not the full seven statuses?

The GTD lifecycle (ADR-026) has 7 statuses: inbox, next, active, waiting, someday, done,
cancelled. `someday` is excluded from the board columns because items in that state are not
actionable and pollute the board in typical agent workflows. They remain queryable via
`tasks(status="someday")` and can be surfaced in a dedicated Backlog tab in phase 3. Six
columns fit in a standard 1440px widescreen without horizontal scroll.

### Why URL-driven state instead of client-state only (Zustand/Jotai)?

Deep-linking and shareable URLs are structural requirements for the research workflow — a
researcher must be able to link a colleague to a specific entity or task. These cannot be
satisfied by client-only state. Client state (Zustand/Jotai) is additive for transient UI
state (which accordion is expanded), but it does not replace URL state for primary view
parameters.

### Why 60-second SWR cache?

The KG and task queue are updated by agent batch runs that complete over minutes, not seconds.
A 60-second stale time prevents redundant re-fetches during normal browsing without serving
stale data for longer than one typical agent task cycle.

### Why batch all column queries in one `request` call?

The `request` verb (ADR-020) supports parallel batch dispatch in a single MCP call. Sending six
sequential task queries would take 6× the round-trip latency. A single batched call delivers all
six column payloads in one HTTP request to the Deno gateway, which forwards them to the MCP
server in one batch. This is the designed use case for the batch syntax.

## Alternatives Considered

| Alternative                                    | Pros                                | Cons                                                                                          | Why rejected                                                    |
| ---------------------------------------------- | ----------------------------------- | --------------------------------------------------------------------------------------------- | --------------------------------------------------------------- |
| Single combined route (KG + tasks on one page) | Less navigation                     | Two very different mental models in one view; no deep-linkable focus on either                | Two separate routes better match user intent                    |
| D3.js direct for graph visualization           | Smaller bundle; full layout control | Re-implements React node lifecycle; forces SVG manipulation; clashes with Tailwind styling    | React Flow component model is the right abstraction             |
| Cytoscape.js for graph visualization           | Mature; proven at scale             | Canvas-based; no React reconciler integration; CSS styling impossible                         | Architectural mismatch with Next.js 15                          |
| Real-time WebSocket instead of SWR polling     | Instant refresh when agent finishes | Requires server-side SSE from Deno gateway; adds infrastructure complexity before v1          | Deferred to phase 3; 60s SWR sufficient for batch agent cadence |
| Zustand/Jotai for all view state               | Simpler component code              | URL state is a requirement; client-only state cannot be bookmarked or shared                  | URL-driven state is non-negotiable                              |
| Include `someday` as a seventh kanban column   | Complete view of all statuses       | Non-actionable items pollute the board; seven columns exceed 1440px without horizontal scroll | Backlog tab in phase 3                                          |
| D3 treemap for density heatmap                 | Visual hierarchy at a glance        | Treemap shape changes with data; bar chart is more readable for named domain categories       | Bar chart chosen for clarity and DOM simplicity                 |

## Consequences

### Positive

- Every view is accessible via a bookmarkable URL — links can be shared between sessions and
  between collaborators.
- The batch query pattern (6 status columns in one `request` call) exercises the MCP `request`
  batch surface as designed — this serves as a proof-of-concept for the batch syntax in a UI
  context.
- React Flow's component model means node rendering and styling live in the same Tailwind/React
  tree as the rest of the frontend — no separate canvas styling system to maintain.
- Phase 1 ships read-only with minimal complexity; interactive features arrive in phase 2 without
  requiring a component architecture change (the URL-driven state model accommodates mutations
  without redesign).

### Negative

- ELK requires a WASM binary in the browser bundle (~500 KB). The dagre fallback is available
  but layout quality degrades on deep structural hierarchies. Mitigated: ELK loads asynchronously
  after first paint; the graph renders with dagre first, then re-layouts with ELK when loaded.
- `DensityHeatmap` requires exhaustive `list` pagination for namespaces with more than 1,000
  entities. A dedicated server-side aggregation endpoint is needed to scale beyond that. Tracked
  as a follow-up issue; the sub-view is hidden when the entity count exceeds the threshold.
- `PathFinder` depends on the query crate's shortest-path operator (ADR-008), which is deferred
  to v0.2. The client-side BFS fallback has a max depth of 5 and degrades on long paths. The
  sub-view is feature-flagged until the operator ships.
- The timeline section of `TaskDetailPanel` depends on the Events substrate (ADR-038, proposed).
  Until ADR-038 ships, the timeline renders only "Created" and "Last updated" from note
  timestamps.

### Neutral

- The frontend adds TypeScript as a second language in the repository (Rust + TypeScript). This
  is already established by the Deno gateway (ADR-003). No new language is introduced.
- Phase 2 interactive features (drag-and-drop, bulk actions) are fully specified here but not
  yet implemented. Implementation follows the same component structure — no redesign required.

## Implementation Plan

### Phase 1 — Static read-only views

| Step | Component / File                                 | Depends on             |
| ---- | ------------------------------------------------ | ---------------------- |
| 1    | `frontend/` scaffold: Next.js 15, Tailwind, SWR  | ADR-003 layer contract |
| 2    | Deno gateway: REST routes wrapping MCP `request` | ADR-011 gateway design |
| 3    | `EntityBrowser` + `PropertyInspector`            | Step 2                 |
| 4    | `KanbanBoard` + `TaskCard` (6-column read-only)  | Step 2                 |
| 5    | `TaskDetailPanel` (get + neighbors)              | Step 4                 |
| 6    | `NeighborhoodGraph` (depth 1, dagre layout)      | Step 3                 |

### Phase 2 — Interactive and analytical views

| Step | Component / File                                    | Depends on                 |
| ---- | --------------------------------------------------- | -------------------------- |
| 7    | `NeighborhoodGraph`: expand/collapse, depth 2       | Phase 1 step 6             |
| 8    | ELK layout integration (`react-flow-elk`)           | Step 7                     |
| 9    | `PathFinder` sub-view                               | Query crate path operator  |
| 10   | `DensityHeatmap` + server-side aggregation endpoint | Deno route addition        |
| 11   | Drag-and-drop on `KanbanBoard` (transition call)    | Gate enforcement (ADR-035) |
| 12   | `BulkActionBar` (multi-select + batch transition)   | Step 11                    |

## Open Questions

1. **Deno gateway REST routes**: the frontend calls the Deno HTTP gateway for all data. The exact
   route design (e.g., `POST /api/request` forwarding raw DSL vs. per-verb typed routes) is
   unspecified. A separate ADR is needed when the gateway is scaffolded.

2. **Authentication in the frontend**: the frontend has no auth surface in phase 1 (localhost
   only). When deployed as a SaaS dashboard (khive-cloud), authentication must be threaded from
   the Deno gateway session through to namespace resolution. ADR-034's `ActorStore` and
   `SessionStore` extension traits are the hook; an amendment to this ADR will be filed at that
   point.

3. **Mobile layout**: desktop-first is the stated constraint. A mobile layout for the GTD Board
   (cards stacked vertically, one status visible at a time) is useful but out of scope for phase 1.
   Filed for later.

4. **Real-time updates**: agents complete tasks and create entities during background runs. A
   Server-Sent Events stream from the Deno gateway would allow the board to update without polling.
   The 60-second SWR refresh is sufficient for phase 1. Phase 3 can introduce SSE from the Deno
   gateway if the use case is validated.

5. **`someday` status in the board**: excluded from the kanban columns here. Whether a Backlog
   tab (showing `someday` items grouped by tag or assignee) belongs in the GTD Board or on the
   main Dashboard is deferred to phase 3 implementation.

## References

- [ADR-001](ADR-001-entity-kind-taxonomy.md): Entity Kind Taxonomy (6 kinds, their properties)
- [ADR-002](ADR-002-edge-ontology.md): Closed Edge Ontology (13 relations, 6 categories)
- [ADR-003](ADR-003-four-layer-architecture.md): Four-Layer Architecture (frontend layer context)
- [ADR-008](ADR-008-query-layer-separation.md): Query Layer Separation (path finder dependency)
- [ADR-011](ADR-011-deno-mcp-only-server.md): Deno User Surfaces (HTTP gateway; frontend calls Deno)
- [ADR-020](ADR-020-request-dsl.md): Request DSL (batch call syntax used by all data flows)
- [ADR-023](ADR-023-verb-consolidated-mcp-surface.md): Verb-Consolidated MCP Surface (verbs used)
- [ADR-026](ADR-026-gtd-pack.md): GTD Pack (task lifecycle, 7 statuses, 4 priority tiers, 5 verbs)
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single Tool MCP Surface (`request` is the only tool)
- [ADR-034](ADR-034-identity-session-metering-hooks.md): Identity/Session hooks (future auth path)
- [ADR-035](ADR-035-hard-enforcement-and-audit-persistence.md): Hard Authorization Enforcement
- [ADR-038](ADR-038-events-surface.md): Events Surface (task timeline dependency)
- [React Flow](https://reactflow.dev/): Graph canvas library
- [ELK.js](https://eclipse.dev/elk/): Eclipse Layout Kernel, hierarchical graph layout
- [@dagrejs/dagre](https://github.com/dagrejs/dagre): Fallback hierarchical layout
