# ADR-045: Swarm Telemetry Dashboard

**Status**: superseded — superseded by ADR-049, which moved to khive-cloud ADR-030\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

Agent swarms running in khive follow a recurring pipeline pattern: gap-analyst scans the knowledge
graph, expander retrieves and adds entities, polisher normalizes and links them, and gap-analyst
resurveys to close remaining gaps. This cycle repeats until the graph converges.

Currently there is no visibility into a running swarm. Operators cannot see which agents are
active, how quickly they are processing tasks, where handoffs are stalling, or whether individual
agents have elevated error or revert rates. GitHub issue #70 identifies swarm telemetry as the
primary motivating use case for the khive frontend.

The data needed for telemetry already exists in the GTD substrate (ADR-026): every agent task is a
note with `kind="task"`, carrying `assignee`, `status`, `priority`, `depends_on`, and `tags`. The
`tags` field captures provenance (`from:gap-analyst`, `cycle:3`) and the `depends_on` edges encode
inter-task ordering. What is missing is a frontend layer that derives and renders meaningful
telemetry from this existing substrate.

ADR-038 establishes `list(kind="event")` as the queryable event stream. ADR-026 establishes the
GTD task lifecycle and the `tasks` verb. ADR-003 positions `frontend/` as a Next.js 15 + React 19
layer calling the Deno HTTP gateway (not the MCP binary directly). The KG versioning frontend
design (established in `docs/design/kg-versioning-frontend.md`) sets the technology stack and
component API conventions this ADR inherits.

### Data model available in the substrate

| Telemetry signal     | How it is derived from the GTD substrate                                        |
| -------------------- | ------------------------------------------------------------------------------- |
| Per-agent throughput | `tasks(assignee=X, status=done)` count over time windows                        |
| Active queue depth   | `tasks(assignee=X, status__in=[active,next])` count                             |
| Handoff DAG          | follow `tags` containing `from:X` + `depends_on` chain between tasks            |
| Cycle progression    | group tasks by `tags` containing `cycle:N`; count status distribution per cycle |
| Mean duration        | `completed_at − created_at` on `done` tasks, grouped by assignee                |
| Drift signals        | tasks transitioned _back_ from `done`/`active` to `inbox`/`waiting` (revert)    |
| Error rate           | `status=cancelled` tasks per assignee over a time window                        |

No schema changes are required. Telemetry is computed exclusively from existing GTD fields.

## Decision

### D1: Route structure

Two routes are added under `frontend/app/swarm/`:

```
/swarm              — SwarmOverviewPage (grid of all active agents)
/swarm/:agent       — AgentDrilldownPage (per-agent history and queue)
```

Both routes use Next.js 15 App Router with React Server Components for initial data fetching and
Client Components for interactive charts and real-time updates.

### D2: Component tree

```
SwarmOverviewPage
├── SwarmContext (React Context — shared polling state)
├── SwarmOverview
│   ├── AgentCard[]          — one per active assignee
│   │   └── ThroughputSparkline
│   ├── HandoffDag           — React Flow graph of agent-to-agent handoffs
│   ├── CycleTimeline        — stacked bar chart (Recharts)
│   └── DriftAlerts          — list of agents with elevated revert rates
└── BottleneckHeatmap        — colored grid of queue depth by agent × time bucket

AgentDrilldownPage
├── AgentDrilldownContext
├── AgentQueueTable          — active and next tasks for the agent
├── AgentThroughputChart     — tasks-completed-per-hour line chart
└── AgentTaskList            — scrollable task history with status badges
```

### D3: Component APIs

**`SwarmContext`** (React Context)

```tsx
interface SwarmState {
  agents: AgentSummary[];
  handoffs: HandoffEdge[];
  cycles: CycleBucket[];
  heatmap: HeatmapCell[][];
  driftAlerts: DriftAlert[];
  lastUpdated: number; // Unix ms
  polling: boolean;
}

type SwarmAction =
  | { type: "SET_AGENTS"; agents: AgentSummary[] }
  | { type: "SET_HANDOFFS"; edges: HandoffEdge[] }
  | { type: "SET_CYCLES"; cycles: CycleBucket[] }
  | { type: "SET_HEATMAP"; cells: HeatmapCell[][] }
  | { type: "SET_DRIFT"; alerts: DriftAlert[] }
  | { type: "TICK"; ts: number };
```

State is managed via `useReducer`. SWR handles cache and revalidation. `SwarmContext` is the
single data boundary — leaf components consume via `useSwarmContext()` and do not issue their own
fetches.

**`AgentCard`**

```tsx
interface AgentSummary {
  name: string; // assignee string e.g. "gap-analyst"
  activeTasks: number;
  nextTasks: number;
  completedLast1h: number;
  meanDurationMs: number | null;
  revertRate: number; // 0.0–1.0
  lastActiveAt: number | null; // Unix ms
}

interface AgentCardProps {
  agent: AgentSummary;
  selected?: boolean;
  onClick?: () => void;
}
```

Renders a card with: agent name, active/next badge counts, throughput sparkline for the last hour
(12 × 5-minute buckets), mean duration, and a revert rate chip (green < 0.05, yellow < 0.15, red
≥ 0.15).

**`ThroughputSparkline`**

```tsx
interface ThroughputSparklineProps {
  buckets: { ts: number; count: number }[]; // 5-minute buckets, newest last
  height?: number; // default 40
  width?: number; // default 80
}
```

Lightweight SVG sparkline (no Recharts dependency — inline `<polyline>` element). Each bucket is
one X unit; Y is scaled to the max in the series. Blank if all counts are zero.

**`HandoffDag`** (React Flow container)

```tsx
interface HandoffEdge {
  fromAgent: string;
  toAgent: string;
  taskCount: number; // tasks that carried tags "from:X" and were assigned to Y
  lastHandoffAt: number | null; // Unix ms
}

interface HandoffDagProps {
  edges: HandoffEdge[];
  selectedAgent?: string; // highlights edges incident to this agent
}
```

Converts `HandoffEdge[]` into React Flow `Node[]` (one node per unique agent name) and `Edge[]`
(one edge per `(fromAgent, toAgent)` pair, label = task count). Layout: React Flow's `ELKLayout`
with `layered` algorithm; falls back to `dagre` if ELK is unavailable. Edge thickness encodes
`taskCount` (thin: 1–5, medium: 6–20, thick: 21+). Follows the same React Flow conventions as
`BranchDag` in `kg-versioning-frontend.md`.

Node structure:

```tsx
interface AgentNodeData {
  name: string;
  activeTasks: number;
  isSelected: boolean;
}
```

**`CycleTimeline`**

```tsx
interface CycleBucket {
  cycleLabel: string; // "cycle:1", "cycle:2", …; "none" for untagged tasks
  counts: { status: string; count: number }[];
}

interface CycleTimelineProps {
  cycles: CycleBucket[];
  height?: number; // default 200
}
```

Stacked bar chart (Recharts `BarChart`). X axis: cycle labels. Y axis: task count. Bars stacked by
status (`active`, `done`, `waiting`, `cancelled`). Color per status: active = blue, done = green,
waiting = amber, cancelled = gray. Matches Tailwind CSS 4 semantic colors.

**`BottleneckHeatmap`**

```tsx
interface HeatmapCell {
  agentName: string;
  timeBucket: string; // ISO-8601 hour e.g. "2026-05-20T14:00Z"
  queueDepth: number; // count of active+next tasks at the start of that bucket
}

interface BottleneckHeatmapProps {
  cells: HeatmapCell[][];
  agents: string[]; // ordered list for row labels
  buckets: string[]; // ordered list for column labels
}
```

CSS grid layout (no canvas). Each cell is a `<div>` with a background color from a 5-step scale
(0 = neutral-100, 1–3 = blue-200, 4–7 = blue-400, 8–15 = orange-400, 16+ = red-500). A tooltip
on hover shows exact agent name, time bucket, and queue depth.

**`DriftAlerts`**

```tsx
interface DriftAlert {
  agentName: string;
  revertCount: number;
  windowHours: number; // over which window this rate was measured
  revertRate: number;
}

interface DriftAlertsProps {
  alerts: DriftAlert[];
}
```

A sorted list (highest `revertRate` first) of agents with `revertRate ≥ 0.05`. Each row: agent
name, revert count, rate badge (color-coded as in `AgentCard`), window label.

### D4: Data fetching — phase 1 (polling)

All data is derived from two GTD verbs on the Deno HTTP gateway: `tasks` (aggregate lists) and
`list(kind="event", ...)` (for lifecycle transition history). No new server-side API routes are
needed.

**Query set per polling tick:**

```ts
// 1. All active+next tasks per known agent (parallel)
async function fetchAgentQueues(namespace: string): Promise<Task[]> {
  const resp = await khiveRequest(
    `[tasks(namespace="${namespace}", status="active", limit=200),
      tasks(namespace="${namespace}", status="next", limit=200)]`,
  );
  return [...resp[0].result, ...resp[1].result];
}

// 2. Completed tasks in the last hour for throughput
async function fetchRecentCompleted(namespace: string): Promise<Task[]> {
  const since = Date.now() - 3600_000; // 1 hour ago
  const resp = await khiveRequest(
    `[tasks(namespace="${namespace}", status="done", limit=500)]`,
  );
  // Filter client-side by completed_at — avoids a since parameter on tasks verb
  return resp[0].result.filter((t: Task) => t.completedAt && t.completedAt > since);
}

// 3. Lifecycle revert events (status transitions backward)
async function fetchRevertEvents(namespace: string): Promise<Event[]> {
  const since = Date.now() - 86_400_000; // 24 hours ago
  const resp = await khiveRequest(
    `[list(kind="event", verb="transition", since=${since * 1000}, limit=200)]`,
  );
  return resp[0].result;
}
```

**Deriving `HandoffEdge[]`:** tasks carry `tags` such as `["from:gap-analyst"]` and are assigned
to `expander`. Iterate all tasks; for each task with a `from:X` tag assigned to agent Y, emit
`{ fromAgent: X, toAgent: Y }`. Group by `(fromAgent, toAgent)` and count.

**Deriving `CycleBucket[]`:** tasks carry `tags` such as `["cycle:3"]`. Group by the cycle tag
and by status, count per group.

**Deriving revert rate:** from lifecycle events, count events where the `data.from_status` ∈
`{active, done}` and `data.to_status` ∈ `{inbox, waiting, next}` per assignee. Divide by total
transition events for that assignee.

### D5: Data fetching — phase 2 (event-driven, ADR-038)

When ADR-038 is accepted and the events surface is live, replace polling with a WebSocket
subscription from the Deno gateway:

```ts
// Deno gateway: server-sent events from EventStore
const es = new EventSource(`/api/events?verb=transition&since=${Date.now() * 1000}`);
es.onmessage = (e) => dispatch({ type: "APPLY_TRANSITION_EVENT", event: JSON.parse(e.data) });
```

The `SwarmContext` reducer handles incremental updates — agent counts are adjusted on each event
without a full re-fetch. The polling path remains as a fallback when WebSocket is unavailable.

### D6: Refresh cadence

```ts
const ACTIVE_SWARM_POLL_MS = 5_000; // 5 seconds — swarm activity detected
const IDLE_POLL_MS = 30_000; // 30 seconds — no active tasks
const CONFIGURABLE_KEY = "khive.swarm.pollIntervalMs"; // future: localStorage config
```

"Active swarm" is defined as any agent with `activeTasks > 0`. `SwarmContext` evaluates this
condition on each tick and switches the interval accordingly. The SWR `refreshInterval` is updated
via `mutate`.

### D7: TypeScript data types

```ts
// Derived from the GTD pack substrate (ADR-026)
interface Task {
  id: string; // 8-char short ID
  fullId: string; // UUID
  title: string;
  status: "inbox" | "next" | "active" | "waiting" | "someday" | "done" | "cancelled";
  priority: "p0" | "p1" | "p2" | "p3";
  assignee: string | null;
  tags: string[];
  dependsOn: string[];
  createdAt: number; // Unix ms
  completedAt: number | null; // Unix ms
}

// Derived from the events surface (ADR-038)
interface TransitionEvent {
  id: string;
  verb: string; // "transition"
  actor: string;
  namespace: string;
  createdAt: number; // Unix ms
  data: {
    taskId: string;
    assignee: string;
    fromStatus: string;
    toStatus: string;
  };
}

// Computed aggregates (client-side derivation, not stored)
interface AgentSummary {
  name: string;
  activeTasks: number;
  nextTasks: number;
  completedLast1h: number;
  meanDurationMs: number | null;
  revertRate: number;
  lastActiveAt: number | null;
}

interface HandoffEdge {
  fromAgent: string;
  toAgent: string;
  taskCount: number;
  lastHandoffAt: number | null;
}

interface CycleBucket {
  cycleLabel: string;
  counts: { status: string; count: number }[];
}

interface HeatmapCell {
  agentName: string;
  timeBucket: string;
  queueDepth: number;
}

interface DriftAlert {
  agentName: string;
  revertCount: number;
  windowHours: number;
  revertRate: number;
}
```

All types are validated at the API boundary using `zod` schemas generated from these interfaces.

### D8: State management

React Context + `useReducer`. No Redux or Zustand. The pattern follows `VcsContext` in
`kg-versioning-frontend.md`:

```ts
// SwarmContext is the single data boundary for all swarm components
const SwarmContext = createContext<{ state: SwarmState; dispatch: Dispatch<SwarmAction> } | null>(
  null,
);

function useSwarmContext() {
  const ctx = useContext(SwarmContext);
  if (!ctx) throw new Error("useSwarmContext must be used within SwarmProvider");
  return ctx;
}
```

SWR is used for cache management and revalidation. Each polling query is a `useSWR` call keyed by
namespace + query type. The SWR `onSuccess` callback dispatches to `SwarmContext` to merge new
data into the shared state.

### D9: File layout

```
frontend/app/swarm/
├── page.tsx                     — SwarmOverviewPage (server component, initial data fetch)
├── [agent]/
│   └── page.tsx                 — AgentDrilldownPage
└── _components/
    ├── SwarmContext.tsx          — SwarmContext + SwarmProvider + useSwarmContext
    ├── SwarmOverview.tsx
    ├── AgentCard.tsx
    ├── ThroughputSparkline.tsx
    ├── HandoffDag.tsx
    ├── CycleTimeline.tsx
    ├── BottleneckHeatmap.tsx
    ├── DriftAlerts.tsx
    ├── AgentDrilldownContext.tsx
    ├── AgentQueueTable.tsx
    ├── AgentThroughputChart.tsx
    └── AgentTaskList.tsx
```

Shared derivation utilities:

```
frontend/lib/swarm/
├── derive.ts       — computeAgentSummaries(), deriveHandoffs(), deriveCycles(), deriveHeatmap()
├── queries.ts      — fetchAgentQueues(), fetchRecentCompleted(), fetchRevertEvents()
└── types.ts        — Task, TransitionEvent, AgentSummary, HandoffEdge, CycleBucket, HeatmapCell, DriftAlert
```

## Rationale

### Why derive from GTD tasks rather than a dedicated telemetry store?

A purpose-built telemetry store would duplicate data that already exists in the GTD substrate.
Every task is timestamped, status-tagged, assignee-attributed, and lifecycle-tracked. Maintaining
two synchronized data sources for the same facts introduces consistency risk and additional
migration surface. Derivation from the existing substrate is the correct approach: telemetry is a
view, not additional state (ADR principle documented in `CLAUDE.md` §"Data vs. view").

### Why React Context + SWR and not Redux?

The KG versioning frontend design established this pattern for the same reasoning: the scope of
shared state is bounded to swarm-related pages, Redux's ceremony is disproportionate for this
scope, and SWR handles the cache and revalidation concerns that would otherwise motivate Redux
Toolkit Query. This ADR inherits the established pattern rather than introducing a second state
management model in the same frontend.

### Why polling first instead of WebSocket from the start?

Phase 1 polling uses the existing `tasks` and `list(kind="event")` verbs with no new server-side
infrastructure. ADR-038 (events surface) is currently proposed, not accepted. Designing the
dashboard to depend on an unaccepted ADR would couple this ADR's delivery to ADR-038's acceptance
timeline. The polling path ships independently; the WebSocket upgrade in phase 2 is a progressive
enhancement (swap the data source, keep the same component tree and state shape).

### Why 5-second polling for active swarms?

A gap-analyst → expander → polisher cycle produces roughly one task completion per agent every
10–30 seconds at typical operation. Five-second polling catches each transition within one to two
poll intervals, giving the operator a near-real-time view without excessive load. The 30-second
idle rate avoids unnecessary database reads when no swarm is running.

### Why React Flow for the handoff DAG?

React Flow is already the established choice for DAG visualization in this codebase
(`BranchDag` in `kg-versioning-frontend.md`). Re-using the same library avoids a second graph
rendering dependency, reuses the ELK/dagre layout infrastructure, and ensures visual consistency
between the VCS branch DAG and the swarm handoff DAG.

### Why inline SVG sparklines and not Recharts for `ThroughputSparkline`?

Sparklines within `AgentCard` are 40 × 80 px data glyphs — their purpose is to convey trend at a
glance, not precise values. A full Recharts instance per card adds significant bundle weight for
zero functional gain. The inline `<polyline>` implementation is approximately 20 lines of
TypeScript and needs no additional dependency.

## Alternatives Considered

| Alternative                                         | Pros                                              | Cons                                                                                           | Why rejected                                                                                         |
| --------------------------------------------------- | ------------------------------------------------- | ---------------------------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Dedicated telemetry table in the database           | Purpose-built queries; no client-side aggregation | Data duplication; migration surface; consistency risk between GTD tasks and telemetry counters | Violates data-vs-view principle; derives correctly from GTD substrate                                |
| Redux for state management                          | More explicit; DevTools support                   | Disproportionate ceremony for bounded scope; second state model in the same frontend           | React Context + SWR is established in this codebase (KG versioning design)                           |
| WebSocket-only (skip polling phase)                 | Real-time from day one                            | ADR-038 not yet accepted; WebSocket gateway not yet implemented                                | Polling ships now; WebSocket is a progressive enhancement in phase 2                                 |
| D3 for HandoffDag                                   | Maximum layout flexibility                        | Imperative API; second graph library in the codebase                                           | React Flow already established; consistency with BranchDag                                           |
| Recharts for sparklines in AgentCard                | Consistent chart library                          | ~80 KB per card in a grid; sparkline needs are a 20-line SVG polyline                          | Bundle cost outweighs the benefit for inline glyphs                                                  |
| Canvas-based heatmap                                | Better performance for large grids                | Inaccessible (no keyboard / screen-reader support); harder to inspect                          | CSS grid + div cells are accessible and sufficient for ≤ 50 agents × 24 time buckets                 |
| Dedicated `/api/swarm` aggregation endpoint in Deno | Server-side derivation; less client code          | Additional Deno surface; derivation logic not reusable by CLI                                  | Client-side derivation from existing verbs is correct at OSS data volumes; extract to Deno if needed |

## Consequences

### Positive

- Swarm operators gain a real-time view of agent activity, handoffs, cycle progression, and
  bottlenecks without any schema changes.
- The telemetry derives entirely from the GTD substrate, so it is always consistent with the
  authoritative task data.
- The phase 2 upgrade to WebSocket is a progressive enhancement — it does not require a rewrite
  of the component tree or state model.
- React Flow reuse for the handoff DAG ensures visual and behavioral consistency with the VCS
  branch DAG in the same frontend.
- The CSS-grid heatmap is keyboard-navigable and screen-reader compatible without additional
  ARIA scaffolding.

### Negative

- Client-side derivation of `HandoffEdge`, `CycleBucket`, and revert rates requires fetching up
  to 700 task records per poll tick (500 completed + 200 active/next). At OSS swarm scales
  (typically < 50 concurrent tasks), this is acceptable. For large swarms, server-side
  aggregation in a Deno endpoint may be required.
- The 5-second polling rate on active swarms generates six MCP calls per tick (parallel `tasks`
  - `list(kind="event")` batches). At typical personal-deployment load this is negligible. Cloud
    deployments with many concurrent users should expose a server-side aggregated endpoint.
- Phase 1 derivation of revert rate requires client-side filtering of lifecycle transition events
  by `data.fromStatus` / `data.toStatus`. If ADR-038 outcome filtering is added server-side in
  v0.2, this logic can move upstream.

### Neutral

- The dashboard has no write surface. It observes the GTD substrate but does not modify tasks.
  Agents retain full task lifecycle control.
- Two new routes (`/swarm`, `/swarm/:agent`) are added with no changes to existing routes.
- The `frontend/lib/swarm/` derivation utilities are pure functions (no side effects) and are
  fully unit-testable without a running backend.

## Implementation Plan

### Phase 1 — Polling dashboard (target: v0.2)

1. Add `frontend/lib/swarm/types.ts` with `Task`, `TransitionEvent`, and all derived aggregate
   types; validate with `zod` schemas.
2. Add `frontend/lib/swarm/queries.ts` — `fetchAgentQueues`, `fetchRecentCompleted`,
   `fetchRevertEvents` using the existing `khiveRequest` helper.
3. Add `frontend/lib/swarm/derive.ts` — `computeAgentSummaries`, `deriveHandoffs`,
   `deriveCycles`, `deriveHeatmap`. Unit-test with `vitest`.
4. Add `SwarmContext.tsx` with `useReducer` + `useSwarmContext`.
5. Add leaf components bottom-up: `ThroughputSparkline` → `AgentCard` → `DriftAlerts` →
   `BottleneckHeatmap` → `CycleTimeline` → `HandoffDag` → `SwarmOverview`.
6. Add route `frontend/app/swarm/page.tsx` (server component for initial data; hands off to
   `SwarmProvider` + `SwarmOverview`).
7. Add `frontend/app/swarm/[agent]/page.tsx` for per-agent drill-down.
8. Run `deno fmt --check` and Next.js build before PR.

### Phase 2 — Event-driven updates (target: v0.3, requires ADR-038 accepted)

1. Add a Deno SSE endpoint `GET /api/events/stream` that proxies `list(kind="event")` with
   long-polling or SSE.
2. Replace `fetchRevertEvents` polling with an SSE `EventSource` subscriber in
   `SwarmContext.tsx`.
3. Add incremental reducer actions (`APPLY_TRANSITION_EVENT`) for in-place state updates.

### Test coverage targets

| Module                              | Target                                         |
| ----------------------------------- | ---------------------------------------------- |
| `frontend/lib/swarm/derive.ts`      | 90% (pure functions, vitest)                   |
| `SwarmContext.tsx` (reducer)        | 85% (vitest)                                   |
| `AgentCard` + `ThroughputSparkline` | 80% (React Testing Library)                    |
| `HandoffDag`                        | 75% (React Testing Library; ELK layout mocked) |
| `BottleneckHeatmap`                 | 80% (React Testing Library)                    |

## Wireframes

### `/swarm` — Overview

```
┌─────────────────────────────────────────────────────────────────────┐
│  Namespace: local   [Active swarm]   Last updated: 14:32:05    [⟳]  │
├─────────────────────────────────────────────────────────────────────┤
│  AGENTS                                                             │
│  ┌────────────────┐  ┌────────────────┐  ┌────────────────┐        │
│  │ gap-analyst    │  │ expander       │  │ polisher       │        │
│  │ ● 3 active     │  │ ● 7 active     │  │ ● 2 active     │        │
│  │ ○ 1 next       │  │ ○ 4 next       │  │ ○ 0 next       │        │
│  │ ▁▂▃▄▃▂▁▂▃▄     │  │ ▄▄▅▆▇▇▆▅▄▃     │  │ ▂▂▂▃▃▃▂▂▁▁     │        │
│  │ 6/h            │  │ 14/h           │  │ 4/h            │        │
│  └────────────────┘  └────────────────┘  └────────────────┘        │
├─────────────────────────────────────────────────────────────────────┤
│  HANDOFF DAG                                                        │
│                                                                     │
│  [gap-analyst] ──(12)──→ [expander] ──(11)──→ [polisher]           │
│       ↑                                             │               │
│       └────────────────(8)──────────────────────────┘               │
│                                                                     │
├─────────────────────────────────────────────────────────────────────┤
│  CYCLE TIMELINE                    DRIFT ALERTS                     │
│  ██████████░░░░  cycle:1 (done)    ! polisher  revert rate 12%      │
│  ████████████░░  cycle:2 (done)      3 reverts / 24h window         │
│  ████████░░░░░░  cycle:3 (active)                                   │
├─────────────────────────────────────────────────────────────────────┤
│  QUEUE DEPTH HEATMAP (last 6 hours, per agent)                      │
│              14:00  14:30  15:00  15:30  16:00  16:30               │
│  gap-analyst   ░      ▒      ▓      ██     ██     ▓                 │
│  expander      ▒      ▓      ██     ██     ██     ██                │
│  polisher      ░      ░      ▒      ▒      ▓      ▓                 │
└─────────────────────────────────────────────────────────────────────┘
```

### `/swarm/:agent` — Per-agent drill-down

```
┌──────────────────────────────────────────────────────────────┐
│  ← Swarm   Agent: expander   [Active — 7 tasks]              │
├──────────────────────────────────────────────────────────────┤
│  THROUGHPUT (last 2 hours)                                   │
│  15 │          ●                                             │
│  10 │      ●  / \  ●                                        │
│   5 │  ●  / \/    \/                                        │
│   0 └─────────────────────────────────────────              │
│     14:00              15:00              16:00              │
├──────────────────────────────────────────────────────────────┤
│  ACTIVE QUEUE                                                │
│  #a1b2c3d4  [p1] Expand: RoPE positional encoding  active   │
│  #b2c3d4e5  [p1] Expand: GQA grouped-query attn    active   │
│  #c3d4e5f6  [p2] Expand: KV cache compression      next     │
│  [7 more…]                                                   │
├──────────────────────────────────────────────────────────────┤
│  RECENT COMPLETIONS  (last 20)                               │
│  #d4e5f6a7  Expand: FlashAttention-2         done   12m ago  │
│  #e5f6a7b8  Expand: LoRA fine-tuning         done   18m ago  │
│  …                                                           │
└──────────────────────────────────────────────────────────────┘
```

## Open Questions

1. **Namespace selection**: the overview page shows one namespace at a time. Should it support
   multi-namespace swarms (e.g., `local/llm-research` and `local/rl-research` running in
   parallel)? Recommendation: single-namespace in v0.2; a namespace switcher dropdown in v0.3.

2. **Agent name discovery**: the current design fetches `tasks(status="active")` and collects
   the unique `assignee` values. If a swarm has not started any tasks yet, the agent grid is
   empty. A future `config` or `swarm_spec` entity in the KG could declare expected agents
   before they begin. Defer to v0.3.

3. **Cycle tag convention**: deriving `CycleBucket` assumes tasks carry a `cycle:N` tag. This
   convention is not enforced by ADR-026. If agents do not emit cycle tags, the timeline shows
   only the `"none"` bucket. Document the tagging convention in an agent deployment guide;
   enforce optionally via the Deno gateway in a future ADR.

4. **Heatmap bucket granularity**: the current design uses 30-minute time buckets. For swarms
   running over multiple days, the number of columns becomes unwieldy. Add a bucket-size
   selector (`15m`, `30m`, `1h`, `1d`) in a follow-up iteration.

5. **Export / reporting**: operators may want to export swarm telemetry (cycle summary, per-agent
   throughput) as a JSON or CSV snapshot. Defer to v0.3; the derivation utilities in
   `frontend/lib/swarm/derive.ts` can be reused for a server-side export endpoint.

## References

- ADR-003: Four-Layer Architecture — frontend/Deno/MCP/crates separation; frontend calls Deno gateway via HTTP
- ADR-004: Substrate Observables — Event as the third substrate (lifecycle event stream)
- ADR-026: GTD Pack — task lifecycle, `tasks` verb, `assignee`, `status`, `tags`, `depends_on` fields
- ADR-038: Events Surface — `list(kind="event")` phase 2 data source; lifecycle transition events
- [KG Versioning Frontend Design](../design/kg-versioning-frontend.md) — component API conventions, React Flow usage, VcsContext pattern, technology stack
- [React Flow v12 docs](https://reactflow.dev/) — DAG node/edge rendering, ELKLayout
- [Recharts](https://recharts.org/) — `BarChart` for `CycleTimeline`
- [SWR](https://swr.vercel.app/) — data fetching and cache revalidation pattern
- [Zod](https://zod.dev/) — runtime type validation at API boundary
- GitHub issue #70 — swarm telemetry as motivating use case for the frontend
