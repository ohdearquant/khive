# KG Versioning — Frontend Design

**Status**: draft\
**Date**: 2026-05-19\
**Authors**: khive maintainers
**Target**: Next.js 15 + React 19 (PR #25 scaffold)

---

## Overview

This document specifies the frontend user interface for the KG versioning system introduced in
ADR-015, ADR-042, and ADR-043. The primary users are researchers and agents who manage knowledge
graphs. The interface covers four primary surfaces:

1. **Branch DAG view** — a git-log-style directed acyclic graph of snapshots and branches.
2. **Diff viewer** — side-by-side entity/edge changes between two snapshots.
3. **Merge conflict resolution UI** — structured conflict cards with resolution actions.
4. **Snapshot timeline** — chronological list of commits with metadata.

---

## Technology Stack

| Layer             | Choice                        | Rationale                                                           |
| ----------------- | ----------------------------- | ------------------------------------------------------------------- |
| Framework         | Next.js 15 (App Router)       | Established scaffold in PR #25; server components for data fetching |
| UI library        | React 19                      | Concurrent mode for responsive graph rendering                      |
| Graph rendering   | React Flow v12                | Declarative DAG nodes; built-in minimap, zoom, pan                  |
| Diff highlighting | `diff2html` + custom CSS      | Unified diff format with entity-aware coloring                      |
| State management  | React Context + `useReducer`  | No global state library needed for v0.1 scope                       |
| Data fetching     | `fetch` + SWR                 | Stale-while-revalidate for snapshot lists                           |
| Styling           | Tailwind CSS 4                | Consistent with the PR #25 scaffold                                 |
| Type generation   | `zod` schemas from Rust types | Backend types compile-time validated on the TS side                 |

---

## Screen 1 — Branch DAG View

### Purpose

Show the snapshot history as a visual DAG: nodes are commits, edges are parent relationships,
branch HEAD markers show where named branches currently point.

### Layout

```
┌─────────────────────────────────────────────────────────┐
│  Namespace: local/llm-research   [Branch: main ▼]  [+]  │
├─────────────────────────────────────────────────────────┤
│                                                         │
│  ● sha256:9bc2 (HEAD → main)         "Add RoPE paper"  │
│  │                                   2026-05-19 14:02  │
│  ●─────● sha256:4a7f (experimental)  "Test supersedes" │
│  │     │                             2026-05-19 12:30  │
│  ●     ↓ (abandoned — no merge)                        │
│  │                                                     │
│  ● sha256:1f3d                       "Add FlashAttn-2" │
│  │                                   2026-05-19 11:15  │
│  ● sha256:a01c (genesis)             "Initial import"  │
│                                      2026-05-19 09:00  │
│                                                         │
├─────────────────────────────────────────────────────────┤
│ [Commit]  [Branch…]  [Checkout]  [Merge…]  [Push/Pull]  │
└─────────────────────────────────────────────────────────┘
```

### Component breakdown

**`BranchDag`** (React Flow container)

- Props: `snapshots: KgSnapshot[]`, `branches: KgBranch[]`, `selectedId?: string`
- Converts the snapshot list into React Flow `Node[]` and `Edge[]`.
- Each node: `SnapshotNode` — shows truncated hash (8 chars), message, timestamp, entity/edge counts.
- Branch HEAD labels rendered as colored chips on the HEAD node.
- On node click: sets `selectedId`, triggers diff panel below.

**`SnapshotNode`** (React Flow custom node)

```tsx
interface SnapshotNodeData {
  id: string; // "sha256:..."
  shortId: string; // first 8 chars of hex
  message: string;
  author?: string;
  createdAt: string; // formatted timestamp
  entityCount: number;
  edgeCount: number;
  branchLabels: string[]; // ["main", "experimental"]
  isHead: boolean;
}
```

Renders as a rounded rectangle with:

- Top row: `shortId` (monospace, green if HEAD, grey otherwise) + branch chips
- Middle row: commit message (truncated at 60 chars)
- Bottom row: entity count + edge count + timestamp

**Layout algorithm**: React Flow's `ELKLayout` with `layered` algorithm (hierarchical top-to-bottom,
branches spread horizontally). Fall back to manual `dagre` if ELK is unavailable in the bundle.

**Minimap**: enabled via React Flow's built-in `<MiniMap>`. Positioned bottom-right.

**Controls**: zoom in/out, fit-view buttons from React Flow's `<Controls>`.

---

## Screen 2 — Diff Viewer

### Purpose

Show what changed between two snapshots: entities added, removed, or modified; edges added,
removed, or weight-modified. Triggered by selecting two nodes in the DAG view or from the snapshot
timeline.

### Layout

```
┌───────────────────────────────────────────────────────┐
│  Diff: sha256:1f3d → sha256:9bc2                      │
│  [Entities (12 changed)]  [Edges (3 changed)]  [Raw]  │
├───────────────────────────────────────────────────────┤
│ ADDED                                                 │
│  + [concept] RoPE                                     │
│    properties: {domain: "positional-encoding"}        │
│  + [person] Su et al.                                 │
│                                                       │
│ MODIFIED                                              │
│  ~ [concept] FlashAttention                           │
│    description:  "IO-aware exact attention"           │
│                → "IO-aware exact attention algorithm" │
│    tags:         ["attention"]                        │
│                → ["attention", "cuda"]                │
│                                                       │
│ REMOVED                                               │
│  - [document] Draft FlashAttention-3 preprint         │
│                                                       │
│ EDGES                                                 │
│  + RoPE ──[introduced_by]──→ Su et al.  (weight 1.0) │
│  ~ FlashAttention-2 ──[extends]──→ FlashAttention    │
│    weight: 0.8 → 1.0                                  │
└───────────────────────────────────────────────────────┘
```

### Component breakdown

**`DiffViewer`** (container)

- Props: `diff: GraphDiff` (ADR-017 format), `from: string`, `to: string`
- Segments operations by kind: `entity_add`, `entity_remove`, `entity_modify`,
  `edge_add`, `edge_remove`, `edge_modify`, `property_set`, `property_unset`.
- Tabs: "Entities", "Edges", "Raw JSON".

**`EntityDiffCard`**

- Renders one entity change.
- Added: green left border, `+` prefix, all fields shown.
- Removed: red left border, `−` prefix, all fields shown.
- Modified: yellow left border, `~` prefix. Shows only changed fields with inline before/after.

**`PropertyDiff`** (inline)

- Renders a `{field}: old → new` row with the old value struck-through and the new value bold.
- For JSON property values: uses `diff2html` for deep object diffs.

**`EdgeDiffLine`**

- Compact one-liner: `source ──[relation]──→ target  weight: old → new`.
- Color-coded by change type.

**Selection for diff**: in the DAG view, users click one node (baseline) then `Shift-click` a second
node (target). The toolbar shows `[Compare selected]`. On desktop, the diff panel opens in a split
pane to the right of the DAG.

---

## Screen 3 — Merge Conflict Resolution UI

### Purpose

Present structured conflict cards when `merge_branch` returns `{ status: "conflicts", conflicts: [...] }`.
Allow the user to resolve each conflict one at a time and finalize the merge.

### Layout

```
┌──────────────────────────────────────────────────────────┐
│  Merge: experimental → main                              │
│  3 conflicts must be resolved before merge can complete  │
├──────────────────────────────────────────────────────────┤
│  [1/3] PropertyMismatch                      [Skip]      │
│                                                          │
│  Entity: FlashAttention (sha256:aaaa0001...)             │
│  Property: year                                          │
│                                                          │
│  ◉ Ours:   2023                                         │
│  ○ Theirs: 2022                                         │
│  ○ Custom: [________________]                            │
│                                                          │
│  [Resolve →]                                             │
├──────────────────────────────────────────────────────────┤
│  [2/3] NameConflict                          [Skip]      │
│                                                          │
│  Entity: bbbb0002...                                     │
│                                                          │
│  ◉ Ours:   "FlashAttention v2"                          │
│  ○ Theirs: "Flash Attention 2"                          │
│  ○ Custom: [________________]                            │
│                                                          │
│  [Resolve →]                                             │
├──────────────────────────────────────────────────────────┤
│  [3/3] DanglingEdge                                      │
│                                                          │
│  Edge: cccc0003 ──[extends]──→ dddd0004                  │
│  Missing endpoint: dddd0004 (deleted in theirs)          │
│                                                          │
│  ○ Keep edge (restore entity dddd0004)                   │
│  ◉ Delete edge                                           │
│  ○ Reroute to: [entity search…]                          │
│                                                          │
│  [Resolve →]                                             │
├──────────────────────────────────────────────────────────┤
│                                                          │
│  Progress: ████████░░  2 / 3 resolved                    │
│                                                          │
│  [Finalize Merge]   (disabled until all resolved)        │
└──────────────────────────────────────────────────────────┘
```

### Component breakdown

**`MergeConflictResolver`** (stateful container)

- Props: `conflicts: MergeConflict[]`, `onFinalize: (resolutions: Resolution[]) => void`
- State: `resolutions: Map<number, Resolution>`, `current: number`
- Renders conflict cards in sequence; each card marks itself resolved and advances to the next.
- `[Finalize Merge]` button is enabled only when `resolutions.size === conflicts.length`.
- On finalize: calls `merge_branch` with `force: true` (the user has resolved conflicts in the
  live state via the API calls emitted on each resolution).

**`ConflictCard`** (per-conflict)

- Polymorphic on `conflict.type` (`property_mismatch`, `name_conflict`, `kind_conflict`,
  `modify_delete`, `edge_modify_delete`, `dangling_edge`).
- For scalar conflicts (`PropertyMismatch`, `NameConflict`, `KindConflict`): three radio options —
  "Ours", "Theirs", "Custom" with an editable text field.
- For existence conflicts (`ModifyDelete`): two options — "Keep (cancel deletion)" or "Delete".
- For `DanglingEdge`: three options — "Keep edge (restore entity)", "Delete edge", "Reroute to
  another entity" (with an entity search typeahead).

**State machine per conflict card**:

```
idle → selecting → confirmed → api_call → resolved
                            ↗ api_error → selecting (retry)
```

When the user clicks `[Resolve →]`, the card calls the backend API to apply the resolution
(e.g., `update(kind="entity", id=..., name="FlashAttention v2")`) then marks itself confirmed.
If the API call fails, the card returns to `selecting` with an error banner.

**`MergeProgress`**

- Shows a progress bar `resolved / total`.
- Lists unresolved conflicts as clickable chips (jump to that card).

---

## Screen 4 — Snapshot Timeline

### Purpose

A compact linear list of commits on a branch. Supplementary to the DAG for branches with long
linear history. Optimized for scrolling through 100+ commits.

### Layout

```
┌──────────────────────────────────────────────────────┐
│  Branch: main   [Namespace: local/llm-research]      │
│  [Search commits…]           Showing 20 of 47        │
├──────────────────────────────────────────────────────┤
│  sha256:9bc2  Add RoPE paper           May 19 14:02  │
│               agent:paper-reader  47 entities         │
│  sha256:1f3d  Add FlashAttn-2 study   May 19 11:15  │
│               agent:paper-reader  43 entities         │
│  sha256:a01c  Initial import          May 19 09:00  │
│               agent:importer     20 entities          │
│  [Load 20 more…]                                     │
└──────────────────────────────────────────────────────┘
```

### Component breakdown

**`SnapshotTimeline`**

- Props: `snapshots: KgSnapshot[]`, `hasMore: boolean`, `onLoadMore: () => void`
- Virtualized list (`react-virtual` or `@tanstack/virtual`) for large histories.
- Each row: `SnapshotRow` — hash chip, message, author, timestamp, entity count.
- Click row: opens diff viewer comparing this snapshot to its parent.
- Row context menu: `[Checkout]`, `[Create branch from here]`, `[View full diff]`.

**`SnapshotSearch`**

- Client-side filter on loaded commits (message substring, author).
- For large histories, defers to server-side search via the `log` endpoint with a `search`
  parameter.

---

## State Architecture

```
VcsContext (React Context)
├── namespace: string
├── currentBranch: string
├── branches: KgBranch[]
├── snapshots: KgSnapshot[]   (current branch history)
├── selectedFrom?: string     (snapshot id for diff)
├── selectedTo?: string       (snapshot id for diff)
├── activeDiff?: GraphDiff
├── activeMerge?: MergeConflict[]
└── dispatch: VcsAction → void

VcsAction:
  | { type: "SET_BRANCH"; branch: string }
  | { type: "SELECT_SNAPSHOT"; id: string }
  | { type: "COMPARE_SNAPSHOTS"; from: string; to: string }
  | { type: "START_MERGE"; conflicts: MergeConflict[] }
  | { type: "RESOLVE_CONFLICT"; index: number; resolution: Resolution }
  | { type: "FINALIZE_MERGE" }
  | { type: "CHECKOUT"; snapshotId: string }
```

All API calls are made from the `VcsContext` reducer (not directly from components) using the
MCP `request` DSL via a thin TypeScript client. Example:

```ts
async function fetchBranchHistory(namespace: string, branch: string): Promise<KgSnapshot[]> {
  const resp = await khiveRequest(`[log(namespace="${namespace}", branch="${branch}", limit=50)]`);
  return resp[0].result as KgSnapshot[];
}
```

---

## Data Types (TypeScript)

Mirroring the Rust types from `khive-vcs`:

```ts
interface KgSnapshot {
  id: string; // "sha256:..."
  namespace: string;
  parentId: string | null;
  message: string;
  author: string | null;
  createdAt: number; // Unix microseconds
  entityCount: number;
  edgeCount: number;
}

interface KgBranch {
  namespace: string;
  name: string;
  headId: string;
  createdAt: number;
  updatedAt: number;
}

type MergeConflict =
  | { type: "name_conflict"; entityId: string; ours: string; theirs: string }
  | { type: "kind_conflict"; entityId: string; ours: string; theirs: string }
  | { type: "property_mismatch"; entityId: string; key: string; ours: unknown; theirs: unknown }
  | {
    type: "modify_delete";
    entityId: string;
    modifiedIn: "ours" | "theirs";
    deletedIn: "ours" | "theirs";
  }
  | {
    type: "edge_modify_delete";
    sourceId: string;
    targetId: string;
    relation: string;
    modifiedIn: "ours" | "theirs";
    deletedIn: "ours" | "theirs";
  }
  | {
    type: "dangling_edge";
    sourceId: string;
    targetId: string;
    relation: string;
    missingEndpoint: string;
  };
```

Zod schemas validate these at the API boundary before they enter React state.

---

## Accessibility

- All interactive elements have `aria-label` attributes.
- Conflict cards have `role="region"` and `aria-live="polite"` for screen-reader announcements
  when a resolution is applied.
- Graph nodes are keyboard-navigable (Tab to focus, Enter to select, Shift+click for diff
  comparison has a keyboard alternative: Space to mark first selection, Shift+Space for second).
- Color coding is not the sole differentiator for diff types — icons and text labels accompany all
  color cues.

---

## Responsive Behavior

The primary target is desktop (1280px+). The DAG view and diff panel use a two-column split on
desktop; on mobile (< 768px) they stack vertically with tabs.

The merge conflict resolution UI is designed mobile-first (single-column, full-width cards) since
conflict resolution may happen on a tablet while reading source papers.

---

## Open Questions

1. **Graph layout for large histories**: React Flow + ELK handles up to ~500 nodes well. Beyond
   that, a virtualized DAG is needed. At what snapshot count should the DAG be replaced with the
   timeline view as the default? Recommendation: auto-switch at 200+ commits.

2. **Real-time updates**: if two agents commit to the same namespace while the user has the DAG
   open, the view will be stale. Add WebSocket polling (or Server-Sent Events) to refresh the
   snapshot list when the active branch HEAD changes. Defer to v0.2.

3. **Diff of properties with large JSON values**: a `properties` field that holds a 100-key object
   needs a collapsible view. The current design shows the full `PropertyDiff` inline. Add a
   `[Expand]` toggle for property values > 5 keys.

4. **Conflict resolution API calls**: when the user resolves a conflict by choosing "Ours" on a
   `PropertyMismatch`, the UI calls `update(kind="entity", id=..., properties={...})`. This must
   not overwrite other properties the user has not resolved yet. Use per-key `property_set` ops
   (ADR-017) rather than wholesale `properties` replace.

5. **Snapshot checkout confirmation**: `checkout` is destructive (replaces live namespace state).
   The UI should show a confirmation modal listing uncommitted-change count before executing.

---

## References

- ADR-015: KG Versioning Model — `commit`, `branch`, `checkout`, `merge_branch`, `log` tool signatures
- ADR-017: Graph Diff Format — `GraphDiff` JSON structure rendered in the diff viewer
- ADR-042: KG Versioning Implementation — `KgSnapshot`, `KgBranch` Rust types
- ADR-043: KG Merge Algorithm — `MergeConflict` enum rendered as conflict cards
- [React Flow v12 docs](https://reactflow.dev/)
- [diff2html](https://diff2html.xyz/) — diff rendering library
- [Tailwind CSS v4](https://tailwindcss.com/) — utility CSS framework
- [Zod](https://zod.dev/) — runtime type validation
