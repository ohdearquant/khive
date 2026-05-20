# ADR-049: Project-First Workspace Frontend

**Status**: moved — see khive-cloud ADR-030; supersedes ADR-045 and ADR-047\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

ADR-047 defined two standalone routes: `/kg` (entity browser + graph) and `/tasks` (GTD kanban).
ADR-045 defined a third standalone route: `/swarm` (agent telemetry). These three routes share no
unifying structural concept. A user browsing the KG has no reason to believe that the entities in
view are scoped to a particular project, nor does the task board indicate which project's tasks it
is showing.

ADR-048's git-native versioning design introduced the namespace as a project-scoped concept: every
project with a `.khive/` directory has its own namespace, and the underlying SQLite database holds
the union of all namespaces. The NDJSON files in `.khive/kg/` are the project's exported graph
state. The `schema.yaml` in that directory declares that project's ontology and its cross-repo
references.

ADR-048 §Frontend also sketched a "namespace picker" and per-namespace explorer, but left the full
frontend architecture — including Tasks, Swarm, Packs, and Schema tabs — unspecified, noting that
a separate ADR would fill in the gaps. This ADR fills those gaps.

Three problems motivate a redesign of the frontend's organizational model:

1. **Namespace ambiguity.** Flat entity and task lists contain records from all namespaces. Without
   a project scope, a user browsing entities cannot tell which project they belong to. Cross-repo
   references (`lattice:<uuid>`) appear in the edge list with no indication of their provenance.

2. **Feature isolation.** KG, Tasks, Swarm, Packs, and Schema are five coherent concerns — but
   each is only useful in the context of a specific project. A task board for "all projects" is
   noise; an agent telemetry view without a project scope is uninterpretable.

3. **The ADR-047 and ADR-045 views are designed for a world without namespaces.** They call
   `/api/entities` and `/api/tasks` without namespace filtering. Now that namespaces are the
   structural unit (ADR-048), the frontend must model them explicitly.

This ADR supersedes ADR-047 (KG Explorer + GTD Board) and ADR-045 (Swarm Telemetry Dashboard),
absorbing both into a unified project workspace model. It also extends ADR-044 (HTTP API Layer)
with namespace-aware endpoints and new project-level routes.

### Constraints inherited from prior ADRs

- The Deno HTTP gateway (ADR-044) is the only backend the frontend talks to. Browsers cannot speak
  stdio MCP; the frontend calls the Deno REST surface.
- The `request` DSL (ADR-020 + ADR-027) is the canonical verb surface on the MCP layer. Gateway
  routes translate REST parameters into DSL strings before forwarding to `khive-mcp`.
- Technology stack is Next.js 15 + TanStack Query + React Flow + Recharts + Tailwind CSS — the
  same stack established across ADR-047, ADR-045, and the KG versioning frontend design in
  ADR-048. No new major dependencies are introduced.
- `deno fmt --check` must pass for all files in `deno/` and `frontend/`.

## Decision

### D1: Navigation model

The frontend is organized as a **project workspace**, not a collection of standalone views.

A project corresponds to one namespace in the khive database. The user first selects a project
from a top-bar picker; all five tabs then show content filtered to that project's namespace.

```
┌─ Project picker ─────────────────────────────────────────────────┐
│  [khive-oss ▾]                                    [All Projects] │
└──────────────────────────────────────────────────────────────────┘
                         ↓ namespace = "khive-oss"
┌─ khive-oss ──────────────────────────────────────────────────────┐
│  KG    Tasks    Swarm    Packs    Schema                         │
├──────────────────────────────────────────────────────────────────┤
│  [content filtered to namespace="khive-oss"]                     │
└──────────────────────────────────────────────────────────────────┘
```

"All Projects" is a world-view cluster map that shows all namespaces and their inter-namespace
connections (cross-repo edges). It is a separate view above individual project workspaces, not a
tab inside one.

The URL structure:

```
/                           — redirects to /projects
/projects                   — "All Projects" world view
/projects/:namespace        — project workspace (default tab: KG)
/projects/:namespace/kg     — KG tab
/projects/:namespace/tasks  — Tasks tab
/projects/:namespace/swarm  — Swarm tab
/projects/:namespace/packs  — Packs tab
/projects/:namespace/schema — Schema tab
```

URL params carry per-tab view state so workspaces are bookmarkable. The `:namespace` segment is
the exact namespace string (e.g., `khive-oss`, `lambda:khive`, `local`).

### D2: Project picker

The project picker (rendered in the top bar at all times) fetches the list of known namespaces
from the gateway. Its data source is `GET /api/projects`.

```tsx
interface ProjectSummary {
  namespace: string;
  entityCount: number;
  taskCount: number;
  activeAgentCount: number;
  lastActivityAt: number | null; // Unix ms
}
```

The picker renders a dropdown of `ProjectSummary` items. Selecting a namespace navigates to
`/projects/:namespace` and persists the selection in `localStorage` under the key
`khive.lastProject`. On initial load, if a persisted project exists and is still valid, the picker
pre-selects it.

"All Projects" appears as a fixed entry at the top of the dropdown list and navigates to
`/projects`. It is not a namespace — it is the world view.

### D3: KG tab (`/projects/:namespace/kg`)

The KG tab absorbs the four sub-views from ADR-047 (`EntityBrowser`, `NeighborhoodGraph`,
`PathFinder`, `DensityHeatmap`), extended with namespace awareness.

**Namespace scoping.** All data calls pass `namespace=<namespace>` on every request. The
`EntityBrowser` renders only entities in the selected project. The `NeighborhoodGraph` renders
local entities as filled nodes and remote entities (those with `lattice:<uuid>`-style edge targets)
as dashed outline nodes labeled with their remote prefix.

**Cross-repo references in the graph.** When an edge has a `<remote>:<uuid>` target, the graph
renders the target as a "remote ref" node. The node label is `<remote>:` + the short UUID. A
tooltip shows the full remote reference and the remote name from `schema.yaml`. Clicking a remote
ref node switches to the entity's remote namespace if that namespace exists locally; otherwise it
opens the remote's GitHub URL (resolved from `schema.yaml#remotes[<remote>].repo`).

**Component tree:**

```
KgTab
├── KgContext            — namespace-scoped shared query state
├── KgTabBar             — sub-view switcher (EntityBrowser / NeighborhoodGraph / PathFinder / DensityHeatmap)
├── EntityBrowser        — paginated entity list with kind/domain/status filters
│   └── PropertyInspector (right drawer)
├── NeighborhoodGraph    — React Flow canvas, local + remote nodes
│   └── RemoteRefNode    — dashed outline node for cross-repo targets
├── PathFinder           — shortest-path between two entities (feature-flagged)
└── DensityHeatmap       — entity concentration by domain (hidden above 1K entities)
```

The `EntityBrowser`, `NeighborhoodGraph`, `PathFinder`, and `DensityHeatmap` component APIs and
data flows are inherited from ADR-047, with the following additions:

- All `list`/`search`/`get`/`neighbors`/`traverse` DSL calls gain `namespace=<namespace>`.
- Remote entity resolution is read from the gateway's new `GET /api/projects/:namespace/schema`
  endpoint; the `remotes` section of the returned `schema.yaml` provides the GitHub URL and
  pinned commit SHA for each remote name.

**URL params for the KG tab:**

| Param          | Values                                     | Sub-view       |
| -------------- | ------------------------------------------ | -------------- |
| `?kind=...`    | Comma-separated entity kind filter         | EntityBrowser  |
| `?q=...`       | Free-text search query                     | EntityBrowser  |
| `?entity=<id>` | Open PropertyInspector for this entity     | EntityBrowser  |
| `?center=<id>` | Selected center entity                     | NeighborhoodGraph |
| `?depth=1\|2`  | Hop depth                                  | NeighborhoodGraph |
| `?from=<id>`   | Source entity                              | PathFinder     |
| `?to=<id>`     | Target entity                              | PathFinder     |

### D4: Tasks tab (`/projects/:namespace/tasks`)

The Tasks tab absorbs the GTD kanban from ADR-047, namespace-scoped.

**Namespace scoping.** All `tasks` and `assign`/`transition`/`complete` DSL calls pass
`namespace=<namespace>`. The board shows only tasks created in the selected project's namespace.

**Component tree:**

```
TasksTab
├── TasksContext          — namespace-scoped task state
├── KanbanBoard          — 6-column kanban (inbox / next / active / waiting / done / cancelled)
│   └── TaskCard[]
├── TaskDetailPanel      — right-side drawer (open on card click)
└── BulkActionBar        — phase 2: floating bar for multi-select transitions
```

The component APIs and data flows are inherited verbatim from ADR-047. The only changes are:

- All batch queries in `KanbanBoard` gain `namespace=<namespace>` on each `tasks(...)` call.
- `TaskDetailPanel` `assign`/`transition`/`complete` mutations pass `namespace=<namespace>`.
- Phase 2 bulk actions pass `namespace=<namespace>` on each batched `transition` call.

**URL params for the Tasks tab:**

| Param            | Values                          |
| ---------------- | ------------------------------- |
| `?assignee=<n>`  | Active assignee filter          |
| `?task=<id>`     | Open TaskDetailPanel            |
| `?priority=p0,p1` | Comma-separated priority filter |

### D5: Swarm tab (`/projects/:namespace/swarm`)

The Swarm tab absorbs the agent telemetry dashboard from ADR-045, namespace-scoped.

**Namespace scoping.** All `tasks` and `list(kind="event")` queries pass `namespace=<namespace>`.
Swarm telemetry is derived from GTD tasks within the project's namespace. Cross-project swarm
comparisons are available in the world view (D8), not in individual project workspaces.

**Component tree:**

```
SwarmTab
├── SwarmContext         — namespace-scoped polling state (inherited from ADR-045)
├── SwarmOverview
│   ├── AgentCard[]
│   │   └── ThroughputSparkline
│   ├── HandoffDag
│   ├── CycleTimeline
│   └── DriftAlerts
└── BottleneckHeatmap
```

Per-agent drill-down navigates to `/projects/:namespace/swarm/:agent` (a sub-route, not a tab).

The `SwarmContext`, `AgentCard`, `ThroughputSparkline`, `HandoffDag`, `CycleTimeline`,
`BottleneckHeatmap`, and `DriftAlerts` component APIs are inherited verbatim from ADR-045. The
polling queries gain `namespace=<namespace>` on all `tasks(...)` and `list(kind="event")` calls.

Polling cadence is inherited from ADR-045: 5 seconds when any agent has `activeTasks > 0`,
30 seconds when idle.

### D6: Packs tab (`/projects/:namespace/packs`)

The Packs tab is a new view with no precedent in prior ADRs. It shows the packs that are loaded in
the `khive-mcp` instance serving this project and what each pack contributes.

**Data source:** `GET /api/projects/:namespace/packs` (new gateway route, see D9).

**Component:**

```tsx
interface PackInfo {
  name: string;
  description: string;
  noteKinds: string[];
  entityKinds: string[];
  verbs: { name: string; description: string }[];
  edgeRules: { relation: string; sourceKind: string; targetKind: string }[];
  enabled: boolean;
}
```

```
PacksTab
└── PackCard[] — one card per loaded pack
    ├── Pack name + enabled badge
    ├── Verb list (name + one-line description)
    ├── Note kind pills
    ├── Entity kind pills
    └── Edge rule rows (relation, source kind → target kind)
```

**Pack card layout:**

```
┌─ kg ──────────────────────────────────────────────────────────────┐
│  [enabled]                                                        │
│                                                                   │
│  Verbs (11)                                                       │
│  create · get · list · update · delete · merge                    │
│  search · link · neighbors · traverse · query                     │
│                                                                   │
│  Entity kinds:  concept  document  dataset  project  person  org  │
│  Note kinds:    observation  insight  question  decision  reference│
│                                                                   │
│  Edge endpoint rules: (none — base ADR-002 contract)              │
└───────────────────────────────────────────────────────────────────┘

┌─ gtd ─────────────────────────────────────────────────────────────┐
│  [enabled]                                                        │
│                                                                   │
│  Verbs (5)                                                        │
│  assign · next · complete · tasks · transition                    │
│                                                                   │
│  Entity kinds: (none)                                             │
│  Note kinds:   task                                               │
│                                                                   │
│  Edge endpoint rules:                                             │
│  depends_on: task → task                                          │
└───────────────────────────────────────────────────────────────────┘
```

Phase 1: read-only (list packs, show what each contributes). Phase 6 adds pack toggle
(`POST /api/projects/:namespace/packs/:name/toggle`) for enable/disable.

### D7: Schema tab (`/projects/:namespace/schema`)

The Schema tab renders and (in phase 4) edits the project's `schema.yaml` from ADR-048.

**Data source:** `GET /api/projects/:namespace/schema` (new gateway route, see D9).

**Component:**

```tsx
interface ProjectSchema {
  version: string;
  entityKinds: string[];
  edgeRelations: EdgeRelationSchema[];
  properties: Record<string, PropertySchema[]>;
  remotes: RemoteSchema[];
}

interface EdgeRelationSchema {
  relation: string;
  category: "structure" | "derivation" | "dependency" | "implementation" | "lateral";
  endpoints: [string, string][]; // [source_kind, target_kind] pairs; empty = any
}

interface PropertySchema {
  key: string;
  values?: string[]; // allowed value set; absent = free text
}

interface RemoteSchema {
  name: string;
  repo: string;
  path: string;
  commit: string; // 40-char SHA
}
```

```
SchemaTab
├── SchemaVersionBadge        — schema format version (e.g., 1.0.0)
├── EntityKindsSection        — pill list of entity kinds
├── EdgeRelationsTable        — table: relation, category, endpoint pairs
├── PropertiesSection         — per-kind expandable property key list
└── RemotesSection            — table: name, repo, commit (linked to GitHub)
```

**RemotesSection** renders each remote with:
- Name (the key in `schema.yaml#remotes`)
- Repo link: `https://github.com/<repo>` (opens in new tab)
- Commit: first 7 chars, linked to `https://github.com/<repo>/commit/<full-sha>`
- Entity count in the remote namespace (from `GET /api/projects/:namespace/stats`)

Phase 4 adds inline editing. Each section has an "Edit" button that opens a modal editor with
a JSON or YAML input (toggleable). On save, the gateway issues `PUT /api/projects/:namespace/schema`
with the updated `schema.yaml` content. The gateway writes the file to disk and runs
`khive kg validate` before confirming. A validation error returns HTTP 422 with the error message
from the validator.

### D8: All Projects world view (`/projects`)

The world view shows all namespaces in a single cluster map, implemented as a React Flow canvas
with `d3-force` layout.

**Data source:** `GET /api/projects` (see D9).

**Component:**

```tsx
interface WorldViewProps {
  projects: ProjectSummary[];
  crossRepoEdges: CrossRepoEdge[];
}

interface CrossRepoEdge {
  sourceNamespace: string;
  targetNamespace: string;
  relation: string;
  count: number; // number of edges of this relation crossing the two namespaces
}
```

```
WorldView
├── WorldViewContext
├── ProjectCluster[]    — one node per namespace, React Flow node
│   ├── ClusterLabel    — namespace name
│   └── ClusterStats   — entity count, task count, active agent count
└── CrossRepoEdge[]    — React Flow edges between namespace clusters
```

Each namespace renders as a React Flow node. The node body shows:

```
┌─ khive-oss ──────────────────────────────┐
│  148 entities   23 tasks   2 agents      │
│  Last activity: 12 min ago               │
└──────────────────────────────────────────┘
```

Cross-repo edges (those with `lattice:<uuid>`-style targets in any namespace) are aggregated into
inter-namespace edges. Each inter-namespace edge renders as a directed arrow labeled with the edge
relation and count (e.g., `implements (3)`).

Clicking a namespace cluster navigates to `/projects/:namespace`.

Layout: `d3-force` spring simulation with a repulsion force between clusters. Cluster nodes have a
fixed minimum separation to prevent overlap. Layout is computed once on mount; the user can drag
nodes to reposition them (React Flow's built-in node drag).

**URL params:**

| Param              | Values                              |
| ------------------ | ----------------------------------- |
| `?highlight=<ns>`  | Highlight edges incident to this namespace |

### D9: Gateway route additions (extends ADR-044)

The Deno HTTP gateway gains the following routes. All existing routes (`/api/entities`,
`/api/tasks`, `/api/edges`, `/api/search`, `/api/traverse`, `/api/request`) gain a `namespace`
query parameter that is passed through to the `khive-mcp` DSL call.

#### Project routes (new)

| Method  | Path                                          | DSL / behavior                                                                                                            |
| ------- | --------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------- |
| `GET`   | `/api/projects`                               | Derives namespace list from `list(kind="entity", limit=1)` distinct-namespace aggregation; returns `ProjectSummary[]`     |
| `GET`   | `/api/projects/:namespace/stats`              | `[list(kind="entity", namespace=<ns>, limit=1), tasks(namespace=<ns>, status="active", limit=1)]` → entity/task counts   |
| `GET`   | `/api/projects/:namespace/schema`             | Reads `.khive/kg/schema.yaml` from the project directory; returns parsed `ProjectSchema` JSON                             |
| `PUT`   | `/api/projects/:namespace/schema`             | Writes body to `.khive/kg/schema.yaml`; runs `khive kg validate`; returns 204 on success, 422 on validation error        |
| `GET`   | `/api/projects/:namespace/packs`              | Introspects `VerbRegistry.all_packs()` (via DSL or gateway config); returns `PackInfo[]`                                  |
| `POST`  | `/api/projects/:namespace/packs/:name/toggle` | Phase 6: enable/disable a pack in the running `khive-mcp` instance; returns updated `PackInfo`                            |

#### Namespace parameter on existing routes (extends ADR-044 D2)

All existing routes accept `?namespace=<ns>` as a query parameter. When present, the DSL call
includes `namespace=<ns>` as a named argument. When absent, the gateway passes no namespace
argument, and `khive-mcp` uses the session-default namespace (the actor's own).

| Existing route     | Namespace parameter behavior                                             |
| ------------------ | ------------------------------------------------------------------------ |
| `GET /api/entities`       | `list(kind="entity", namespace=<ns>, ...)`                        |
| `GET /api/entities/:id`   | `get(id=":id")` — namespace enforcement is internal to the runtime |
| `POST /api/entities`      | `create(kind="entity", namespace=<ns>, ...)`                       |
| `PATCH /api/entities/:id` | `update(id=":id", ...)` — runtime enforces namespace              |
| `DELETE /api/entities/:id`| `delete(id=":id")` — runtime enforces namespace                   |
| `GET /api/edges`          | `list(kind="edge", namespace=<ns>, ...)`                           |
| `POST /api/edges`         | `link(source_id=..., target_id=..., namespace=<ns>, ...)`          |
| `GET /api/tasks`          | `tasks(namespace=<ns>, ...)`                                       |
| `POST /api/tasks`         | `assign(namespace=<ns>, ...)`                                      |
| `POST /api/tasks/:id/...` | Transition/complete — runtime enforces namespace                   |
| `GET /api/search`         | `search(namespace=<ns>, ...)`                                      |
| `GET /api/traverse`       | `traverse(namespace=<ns>, ...)`                                    |

#### MCP verb namespace passthrough (verb surface change)

The DSL verbs already accept `namespace` as a parameter on most operations internally. This ADR
makes the `namespace` parameter explicit and documented on the DSL wire surface (ADR-020
amendment), enabling the gateway to pass through the project's namespace on every call.

This is the preferred option over the alternatives considered in the context section (spawning one
`khive-mcp` per namespace, or using a `set_namespace` handshake). The gateway is a multi-tenant
caller; it passes namespace per-call, and `khive-mcp` enforces isolation internally.

#### `/api/projects` implementation note

The `GET /api/projects` route cannot be served directly by a `khive-mcp` verb because `khive-mcp`
operates within a single namespace. The gateway derives the namespace list by:

1. Calling `list(kind="entity", limit=0)` to get the total entity count (to detect empty instances).
2. Reading the list of distinct namespace strings from the database via a raw query (or, if the
   verb surface exposes it, via a future `namespaces()` verb — tracked as a follow-up issue).

In phase 1, the gateway reads the namespace list from the `~/.khive/projects.json` config file (a
simple JSON array of namespace strings, written by `khive kg init` alongside the SQLite database).
This is a pragmatic bootstrap; a proper `namespaces()` verb is filed as follow-up.

### D10: Data flow summary

```
User selects project "khive-oss" from picker
  ↓
GET /api/projects?namespace=khive-oss/stats
  → [list(kind="entity", namespace="khive-oss", limit=1),
     tasks(namespace="khive-oss", status="active", limit=1)]
  → { entityCount: 148, taskCount: 23, activeAgentCount: 2 }

User clicks KG tab
  ↓
GET /api/entities?namespace=khive-oss&entity_kind=concept&limit=25
  → list(kind="entity", namespace="khive-oss", entity_kind="concept", limit=25)
  → { items: [...], total: 87, ... }

User clicks entity "FlashAttention"
  ↓
[GET /api/entities/:id, GET /api/entities/:id/neighbors]
  → [get(id="a1b2c3d4"), neighbors(node_id="a1b2c3d4")]
  → PropertyInspector opens

Neighbor has target "lattice:c9e4b3f2"
  ↓
GET /api/projects/khive-oss/schema
  → schema.yaml parsed; remotes["lattice"].repo = "ohdearquant/lattice"
  → RemoteRefNode rendered with GitHub link
```

### D11: TypeScript module layout

```
frontend/
├── app/
│   ├── page.tsx                           — redirect to /projects
│   ├── projects/
│   │   ├── page.tsx                       — WorldView (All Projects cluster map)
│   │   └── [namespace]/
│   │       ├── layout.tsx                 — ProjectLayout: tab bar + project picker
│   │       ├── page.tsx                   — redirect to /projects/:namespace/kg
│   │       ├── kg/
│   │       │   └── page.tsx              — KgTab
│   │       ├── tasks/
│   │       │   └── page.tsx              — TasksTab
│   │       ├── swarm/
│   │       │   ├── page.tsx              — SwarmTab (overview)
│   │       │   └── [agent]/
│   │       │       └── page.tsx          — AgentDrilldownPage
│   │       ├── packs/
│   │       │   └── page.tsx              — PacksTab
│   │       └── schema/
│   │           └── page.tsx              — SchemaTab
│   └── _components/
│       ├── ProjectPicker.tsx             — top-bar namespace dropdown
│       └── TabBar.tsx                    — KG / Tasks / Swarm / Packs / Schema
├── lib/
│   ├── api/
│   │   ├── projects.ts                   — fetchProjects(), fetchProjectStats(), fetchSchema(), fetchPacks()
│   │   ├── entities.ts                   — fetchEntities(), fetchEntity(), fetchNeighbors()
│   │   ├── tasks.ts                      — fetchTasks(), createTask(), transitionTask()
│   │   └── request.ts                    — khiveRequest() helper: POST /api/request
│   ├── kg/                               — KG tab components (from ADR-047)
│   │   ├── EntityBrowser.tsx
│   │   ├── PropertyInspector.tsx
│   │   ├── NeighborhoodGraph.tsx
│   │   ├── RemoteRefNode.tsx
│   │   ├── PathFinder.tsx
│   │   └── DensityHeatmap.tsx
│   ├── tasks/                            — Tasks tab components (from ADR-047)
│   │   ├── KanbanBoard.tsx
│   │   ├── TaskCard.tsx
│   │   ├── TaskDetailPanel.tsx
│   │   └── BulkActionBar.tsx
│   ├── swarm/                            — Swarm tab components (from ADR-045)
│   │   ├── SwarmContext.tsx
│   │   ├── SwarmOverview.tsx
│   │   ├── AgentCard.tsx
│   │   ├── ThroughputSparkline.tsx
│   │   ├── HandoffDag.tsx
│   │   ├── CycleTimeline.tsx
│   │   ├── BottleneckHeatmap.tsx
│   │   └── DriftAlerts.tsx
│   ├── packs/                            — Packs tab components (new)
│   │   └── PackCard.tsx
│   ├── schema/                           — Schema tab components (new)
│   │   ├── EntityKindsSection.tsx
│   │   ├── EdgeRelationsTable.tsx
│   │   ├── PropertiesSection.tsx
│   │   └── RemotesSection.tsx
│   └── world/                            — World view components (new)
│       ├── WorldView.tsx
│       ├── ProjectClusterNode.tsx
│       └── CrossRepoEdge.tsx
└── types/
    ├── project.ts                        — ProjectSummary, PackInfo, ProjectSchema, RemoteSchema
    ├── entity.ts                         — Entity, Edge (from ADR-047)
    └── task.ts                           — Task, TransitionEvent (from ADR-047/045)
```

### D12: Shared component patterns

**Loading and error states** — inherited from ADR-047:

```tsx
type ViewState<T> =
  | { status: "loading" }
  | { status: "error"; message: string }
  | { status: "ok"; data: T };
```

**Client-side caching** — TanStack Query (replaces SWR from ADR-047/045 for consistency). Cache
keys include the namespace:

```ts
queryKey: ["entities", namespace, filters]
queryKey: ["tasks", namespace, status, assignee]
queryKey: ["schema", namespace]
queryKey: ["packs", namespace]
queryKey: ["projects"]
```

Stale time: 60 seconds for entities and schema; 5 seconds for tasks and swarm state (active swarm
polling inherits ADR-045 cadence control from `SwarmContext`).

**Why TanStack Query instead of SWR.** ADR-047 and ADR-045 both chose SWR. This ADR switches to
TanStack Query for two reasons: (1) TanStack Query's per-query `staleTime` configuration avoids
the global override pattern needed to give swarm polling its 5-second refresh while keeping schema
at 60 seconds; (2) TanStack Query's structured `queryKey` arrays make namespace-keyed cache
invalidation explicit — invalidating `["tasks", "khive-oss"]` when the user switches projects
requires no custom logic. The component APIs and data shapes from ADR-047 and ADR-045 are
unchanged; only the data-fetching library changes.

## Phasing

| Phase | Scope | Target |
| ----- | ----- | ------ |
| F1 | Project picker + `GET /api/projects` + `namespace` param on existing gateway routes + per-namespace EntityBrowser | v0.3 |
| F2 | Per-namespace Tasks kanban + Swarm telemetry tab (inherit ADR-047 + ADR-045 components, add namespace scoping) | v0.3 |
| F3 | Packs tab (read-only introspection via `GET /api/projects/:namespace/packs`) | v0.3 |
| F4 | Schema tab: render `schema.yaml` via `GET /api/projects/:namespace/schema` + RemotesSection with GitHub links | v0.4 |
| F5 | World view cluster map (`/projects`) with inter-namespace edge aggregation | v0.4 |
| F6 | Schema tab editing (`PUT /api/projects/:namespace/schema` + validation feedback) | v0.4 |
| F7 | Pack toggle (`POST /api/projects/:namespace/packs/:name/toggle`) | v0.5 |

F1 and F2 unblock the primary use case: a user working in one project sees only that project's
entities, tasks, and agent activity without cross-project noise. F1 requires the `namespace` param
to be surfaced on existing gateway routes and the `khive-mcp` verb surface (ADR-020 amendment).
F3–F7 are additive and can ship independently.

## Rationale

### Why project-first instead of global views with optional namespace filter

A namespace filter dropdown on global views is a common degenerate design: users forget to set it,
cross-namespace records appear unexpectedly, and the default "no filter" state returns meaningless
aggregates across unrelated projects. The project-first model makes the scope explicit and
structural — you cannot accidentally see khive-oss tasks on the atlas task board.

The world view (`/projects`) is the correct level for cross-namespace comparisons. It is a
deliberate choice to enter that view, not an accidental omission of a filter.

### Why supersede ADR-047 and ADR-045 rather than amend them

ADR-047 and ADR-045 were designed for a flat-namespace world. Their route structure (`/kg`,
`/tasks`, `/swarm`) has no project scope and no mechanism to add one without restructuring the
entire URL tree. Amending each ADR to add a `:namespace` segment would effectively rewrite them;
a clean supersession with explicit absorption is clearer and produces a single normative reference.

The component implementations from ADR-047 and ADR-045 are reused without modification, as noted
throughout this ADR. Supersession refers to the frontend architecture and route structure, not to
the component designs.

### Why TanStack Query instead of SWR

Explained in D12. The decision is bounded to the data-fetching library; it does not affect
component APIs or data shapes. The five-tab workspace introduces heterogeneous stale times (5s for
swarm, 60s for schema/packs, 10s for tasks) that SWR's global `refreshInterval` cannot accommodate
cleanly without per-hook overrides.

### Why the Packs tab is read-only in phase 1

Pack toggling requires restarting the `khive-mcp` child process or implementing hot-reload of the
`VerbRegistry` — both non-trivial server-side changes. Read-only introspection requires only a
query against the running registry state, which can be derived from the `VerbRegistry`'s loaded
packs at startup. The toggle feature (phase 7) is deferred until the server-side hot-reload
mechanism is designed.

### Why namespace is a per-call parameter (Option B) rather than a handshake

Option A (one `khive-mcp` process per namespace) does not scale: 10 projects means 10 child
processes, 10 MCP handshakes, and 10 SQLite WAL files in concurrent use. Option C (`set_namespace`
handshake before each operation) introduces state in what is otherwise a stateless request-response
protocol — any dropped call or race condition leaves the session in the wrong namespace. Option B
(namespace as a per-call argument) is stateless, composable with batch calls, and consistent with
how the gate and namespace enforcement work inside `khive-mcp` today.

### Why `GET /api/projects` reads from a config file in phase 1

The `khive-mcp` verb surface does not expose a `namespaces()` verb today. Adding one is tracked as
a follow-up issue. Reading from `~/.khive/projects.json` (written by `khive kg init`) is a
pragmatic bootstrap that ships with zero verb-surface changes. Once a proper verb exists, the
gateway route switches to calling it without any frontend change.

## Alternatives Considered

| Alternative | Pros | Cons | Why rejected |
| --- | --- | --- | --- |
| Namespace filter on existing flat routes (`/kg?namespace=X`) | Minimal URL change; no route restructuring | Filter is optional → accidental cross-namespace views; no structural scope enforcement | Project-first scope is required to eliminate cross-namespace noise |
| Separate tab set per page (ADR-047 + ADR-045 as standalone) | Preserves existing route structure | No unifying workspace concept; duplicates namespace state across three separate pages | Single workspace with shared namespace context is architecturally cleaner |
| One `khive-mcp` process per namespace (Option A) | Clean namespace isolation in the server | N processes, N connections, N WAL files; does not scale beyond a handful of projects | Resource cost grows linearly; Option B is stateless and scales better |
| `set_namespace` handshake per session (Option C) | No per-call overhead | Stateful protocol; race condition risk in concurrent batch calls; complex error recovery | Stateful sessions are fragile; per-call namespace is simpler and safer |
| SWR (continue from ADR-047/045) | Consistent with prior ADRs | Cannot cleanly express per-query stale times at 5-tab heterogeneous cadences | TanStack Query's structured query key and per-query staleTime are better fits |
| World view as a tab inside a project (not a separate route) | Simpler navigation model | World view is inherently cross-namespace; nesting it inside a namespace-scoped workspace is a conceptual contradiction | `/projects` as a top-level route makes the cross-namespace scope unambiguous |

## Consequences

### Positive

- Every view in the frontend is unambiguously scoped to one project. Cross-namespace records are
  never accidentally shown.
- The component implementations from ADR-047 (KG, Tasks) and ADR-045 (Swarm) are reused
  without modification — only their data calls gain a `namespace` parameter.
- The project workspace model maps directly to how developers already think about projects (each
  repo is a project; each project has a `.khive/` directory).
- The world view cluster map makes cross-repo connections visible without mixing data from
  different projects in a single list or graph.
- The Packs tab gives operators first-class visibility into what vocabulary is available in a
  project without reading source code or config files.
- The Schema tab exposes `schema.yaml` in the UI, making ontology review possible without a
  text editor.

### Negative

- The URL structure (`/projects/:namespace/kg`) is longer than the prior ADR-047 structure
  (`/kg`). Existing bookmarks to `/kg` and `/tasks` from ADR-047 break. Mitigation: redirect
  `/kg` and `/tasks` to `/projects/local/kg` and `/projects/local/tasks` respectively, where
  `local` is the default namespace.
- `GET /api/projects` in phase 1 requires a `~/.khive/projects.json` config file managed by
  `khive kg init`. If the user has namespaces in the database that are not in this file (created
  without going through `khive kg init`), those namespaces do not appear in the project picker.
  Mitigation: a future `namespaces()` verb removes this limitation; document the requirement in
  the `khive kg init` help text.
- TanStack Query replaces SWR, introducing a dependency change. Both are React data-fetching
  libraries with similar bundle sizes (~15 KB gzipped for TanStack Query vs. ~5 KB for SWR).
  The larger bundle is justified by the per-query stale time configuration.
- The `PUT /api/projects/:namespace/schema` route writes files to disk from the HTTP layer.
  This is a side effect beyond the DSL verb surface. The gateway runs on localhost in phase 1;
  for cloud deployments this route must be gated behind write authorization (ADR-034 scope).

### Neutral

- ADR-047's `EntityBrowser`, `PropertyInspector`, `NeighborhoodGraph`, `PathFinder`, and
  `DensityHeatmap` components are unchanged. ADR-045's `SwarmContext`, `AgentCard`, `HandoffDag`,
  `CycleTimeline`, `BottleneckHeatmap`, `DriftAlerts`, and `ThroughputSparkline` are unchanged.
  This ADR is an organizational wrapper, not a component redesign.
- The Deno gateway gains new routes but its core architecture (Hono + MCP client + DSL
  translation, per ADR-044) is unchanged. Each new route follows the same pattern: translate REST
  parameters into a DSL string and call the `request` MCP tool.
- Phase 7 pack toggling requires server-side `VerbRegistry` hot-reload, which is outside the
  scope of this ADR and must be designed in a separate ADR before implementation.

## Implementation Plan

### Phase 1 (F1) — Project picker + namespace-aware entity list

| Step | File | Change |
| ---- | ---- | ------ |
| 1 | `deno/src/api/projects.ts` | `GET /api/projects` (reads `~/.khive/projects.json`); `GET /api/projects/:ns/stats` |
| 2 | `deno/src/api/entities.ts` + all other routes | Add `namespace` query param forwarding to all existing DSL calls |
| 3 | `deno/src/types/api.ts` | `ProjectSummary` type |
| 4 | `frontend/lib/api/projects.ts` | `fetchProjects()`, `fetchProjectStats()` |
| 5 | `frontend/app/_components/ProjectPicker.tsx` | Namespace dropdown + localStorage persistence |
| 6 | `frontend/app/projects/[namespace]/layout.tsx` | `ProjectLayout` with tab bar (KG / Tasks / Swarm / Packs / Schema) |
| 7 | `frontend/app/projects/[namespace]/kg/page.tsx` | `KgTab` wrapping existing `EntityBrowser` + namespace-aware queries |
| 8 | `frontend/app/projects/page.tsx` | Placeholder world view (entity count per project, no cluster map) |

### Phase 2 (F2) — Namespace-scoped Tasks + Swarm

| Step | File | Change |
| ---- | ---- | ------ |
| 9 | `frontend/app/projects/[namespace]/tasks/page.tsx` | `TasksTab` wrapping `KanbanBoard` + namespace param on all task queries |
| 10 | `frontend/app/projects/[namespace]/swarm/page.tsx` | `SwarmTab` wrapping `SwarmOverview` + namespace param on all swarm queries |
| 11 | `frontend/app/projects/[namespace]/swarm/[agent]/page.tsx` | `AgentDrilldownPage` scoped to namespace |

### Phase 3 (F3) — Packs tab

| Step | File | Change |
| ---- | ---- | ------ |
| 12 | `deno/src/api/projects.ts` | `GET /api/projects/:ns/packs` — reads `VerbRegistry` state |
| 13 | `frontend/lib/packs/PackCard.tsx` | Pack card with verb list, kind pills, edge rules |
| 14 | `frontend/app/projects/[namespace]/packs/page.tsx` | `PacksTab` |

### Phase 4 (F4) — Schema tab + remote linking

| Step | File | Change |
| ---- | ---- | ------ |
| 15 | `deno/src/api/projects.ts` | `GET /api/projects/:ns/schema` — parse and return `schema.yaml` |
| 16 | `frontend/lib/schema/` | `EntityKindsSection`, `EdgeRelationsTable`, `PropertiesSection`, `RemotesSection` |
| 17 | `frontend/app/projects/[namespace]/schema/page.tsx` | `SchemaTab` (read-only) |

### Phase 5 (F5) — World view cluster map

| Step | File | Change |
| ---- | ---- | ------ |
| 18 | `frontend/lib/world/` | `WorldView`, `ProjectClusterNode`, `CrossRepoEdge` |
| 19 | `frontend/app/projects/page.tsx` | Replace placeholder with full React Flow cluster map |

### Phase 6 (F6) — Schema editing

| Step | File | Change |
| ---- | ---- | ------ |
| 20 | `deno/src/api/projects.ts` | `PUT /api/projects/:ns/schema` — write + validate |
| 21 | `frontend/lib/schema/SchemaEditor.tsx` | YAML/JSON modal editor + 422 error display |

## Open Questions

1. **`namespaces()` verb.** `GET /api/projects` reads from a config file in phase 1. A proper
   `namespaces()` verb on the DSL surface would eliminate the config file dependency and surface
   all namespaces regardless of how they were created. Filed as follow-up; this ADR does not
   block on it.

2. **Schema edit authorization.** `PUT /api/projects/:namespace/schema` writes files to disk.
   In single-user local deployments this is acceptable. For cloud deployments (ADR-034 scope),
   this route must be gated behind a namespace-owner authorization check. The ADR-034 `namespace_owner`
   Rego contract is the right hook; this is deferred to when ADR-034 is accepted.

3. **Pack hot-reload.** Phase 7 (pack toggle) requires the running `khive-mcp` to reload its
   `VerbRegistry` without a restart. This is a non-trivial server-side change. A design is needed
   before phase 7 implementation begins; a follow-up ADR will specify it.

4. **Multi-user concurrent schema edits.** If two users edit `schema.yaml` concurrently via
   `PUT /api/projects/:namespace/schema`, the second write clobbers the first. A last-writer-wins
   policy is acceptable for phase 6 (single-user localhost). For collaborative deployments, an
   ETag/conditional-PUT mechanism is needed. Deferred.

5. **World view edge aggregation.** Cross-repo edges are detected from `edges.ndjson` (ADR-048
   format) or from the live SQLite database's edge table where `target LIKE '<remote>:%'`. The
   gateway implementation of cross-namespace edge aggregation is unspecified here; it is
   implementation detail for phase 5.

## References

- [ADR-003](ADR-003-four-layer-architecture.md): Four-layer architecture — frontend / Deno / MCP / crates
- [ADR-011](ADR-011-deno-mcp-only-server.md): Deno gateway entry point
- [ADR-020](ADR-020-request-dsl.md): Request DSL — namespace added as per-call parameter
- [ADR-025](ADR-025-pack-standard.md): Pack standard — `Pack::VERBS`, `Pack::NOTE_KINDS`, `Pack::ENTITY_KINDS` introspected for Packs tab
- [ADR-026](ADR-026-gtd-pack.md): GTD pack — task lifecycle backing the Tasks tab
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single-tool MCP surface — `request` is the only MCP tool
- [ADR-038](ADR-038-events-surface.md): Events surface — swarm telemetry event stream (phase 2)
- [ADR-044](ADR-044-http-api-layer.md): HTTP API layer — this ADR extends its route table
- [ADR-045](ADR-045-swarm-telemetry-dashboard.md): Swarm Telemetry Dashboard — **superseded by this ADR** (absorbed into Swarm tab)
- [ADR-047](ADR-047-kg-explorer-gtd-board.md): KG Explorer and GTD Board — **superseded by this ADR** (absorbed into KG and Tasks tabs)
- [ADR-048](ADR-048-git-native-kg-versioning.md): Git-Native KG Versioning — namespace model and `schema.yaml` that this frontend surfaces
- [React Flow v12](https://reactflow.dev/): Graph canvas for NeighborhoodGraph, HandoffDag, and WorldView
- [TanStack Query v5](https://tanstack.com/query/v5): Data fetching and cache management
- [Hono](https://hono.dev): Deno gateway HTTP framework
