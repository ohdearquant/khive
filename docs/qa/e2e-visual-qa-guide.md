# E2E Visual QA Guide — khive Frontend

**Purpose**: Structured checklist for critic-driven visual QA of the khive frontend dashboard.
Run after implementation PRs land. Each section has pass/fail criteria with screenshots expected.

---

## Prerequisites

```bash
# 1. Build khive-mcp binary
cd crates && cargo build --release -p khive-mcp

# 2. Seed test data (entities, edges, tasks)
python3 tests/smoke_test.py          # basic verb coverage
python3 tests/contract_test.py       # behavioral contracts

# 3. Start Deno HTTP server
cd deno && deno task server           # http://localhost:8000

# 4. Start Next.js frontend
cd frontend && npm install && npm run dev  # http://localhost:3000
```

---

## 1. Smoke — App Loads

| #   | Check                               | Pass Criteria                                             |
| --- | ----------------------------------- | --------------------------------------------------------- |
| 1.1 | Navigate to `http://localhost:3000` | Page renders without blank screen or React error boundary |
| 1.2 | Console errors                      | Zero `ERR_*` or `Unhandled` in browser console            |
| 1.3 | API connectivity                    | Network tab shows successful `GET /api/entities` (200 OK) |
| 1.4 | Loading states                      | Skeleton/spinner shows before data arrives (not blank)    |

---

## 2. KG Explorer (`/kg`)

### 2.1 Entity Browser

| #     | Check               | Pass Criteria                                                               |
| ----- | ------------------- | --------------------------------------------------------------------------- |
| 2.1.1 | Entity list renders | Table/grid shows entities with name, kind, created_at                       |
| 2.1.2 | Kind filter         | Selecting "concept" shows only concepts; "all" shows all                    |
| 2.1.3 | Search              | Typing in search bar filters entities; results match `search()` verb output |
| 2.1.4 | Pagination          | "Load more" or page controls work; no duplicate entries                     |
| 2.1.5 | Empty state         | With no entities: shows helpful "No entities yet" message, not blank        |
| 2.1.6 | Entity detail       | Clicking an entity opens detail panel with properties, tags, description    |

### 2.2 Graph Visualization

| #     | Check                 | Pass Criteria                                                      |
| ----- | --------------------- | ------------------------------------------------------------------ |
| 2.2.1 | Neighborhood view     | Selecting an entity shows 1-hop neighbors as a node graph          |
| 2.2.2 | Edge labels           | Edges show relation type (extends, depends_on, etc.)               |
| 2.2.3 | Edge colors           | Different relations use visually distinct colors                   |
| 2.2.4 | Node expand           | Double-click a neighbor expands its edges (progressive disclosure) |
| 2.2.5 | Pan/zoom              | Graph is pannable and zoomable (scroll wheel + drag)               |
| 2.2.6 | No overlapping labels | Layout algorithm prevents label overlap at default zoom            |

### 2.3 Path Finder

| #     | Check                | Pass Criteria                                                    |
| ----- | -------------------- | ---------------------------------------------------------------- |
| 2.3.1 | Two-entity selection | Two autocomplete inputs with entity search                       |
| 2.3.2 | Path display         | Shortest path shown as linear chain: A --[rel]--> B --[rel]--> C |
| 2.3.3 | No path              | When no path exists: shows "No path found" message               |
| 2.3.4 | Same entity          | Source = target: shows "Same entity" or single-node path         |

---

## 3. GTD Board (`/tasks`)

### 3.1 Kanban Layout

| #     | Check             | Pass Criteria                                               |
| ----- | ----------------- | ----------------------------------------------------------- |
| 3.1.1 | 6 columns render  | inbox, next, waiting, active, done, cancelled — all visible |
| 3.1.2 | Task cards        | Each card shows: title, priority badge (P0-P3), assignee    |
| 3.1.3 | Column counts     | Header shows task count per column                          |
| 3.1.4 | Empty columns     | Columns with no tasks show placeholder, not collapsed       |
| 3.1.5 | Priority ordering | Within each column, P0 tasks appear before P1, P1 before P2 |

### 3.2 Task Detail

| #     | Check              | Pass Criteria                                                 |
| ----- | ------------------ | ------------------------------------------------------------- |
| 3.2.1 | Click opens detail | Clicking a card opens slide-out drawer/modal                  |
| 3.2.2 | Full content       | Drawer shows: title, content, tags, assignee, priority, dates |
| 3.2.3 | Dependencies       | depends_on chain shown as linked list or mini-graph           |
| 3.2.4 | Close works        | Escape key or X button closes drawer, returns to board        |

### 3.3 Actions (Phase 2)

| #     | Check                 | Pass Criteria                                             |
| ----- | --------------------- | --------------------------------------------------------- |
| 3.3.1 | Drag-and-drop         | Dragging a card between columns transitions its status    |
| 3.3.2 | Transition validation | Invalid transitions (e.g., inbox → done) show error toast |
| 3.3.3 | Bulk select           | Checkbox on cards enables multi-select → bulk action bar  |

---

## 4. Swarm Telemetry (`/swarm`)

### 4.1 Overview

| #     | Check                | Pass Criteria                                                  |
| ----- | -------------------- | -------------------------------------------------------------- |
| 4.1.1 | Agent cards          | One card per unique task assignee                              |
| 4.1.2 | Throughput sparkline | Each card shows a mini line chart of completed tasks over time |
| 4.1.3 | Queue depth          | Cards show current active + next task count                    |
| 4.1.4 | No agents            | With no tasks: shows "No agent activity detected"              |

### 4.2 Handoff DAG

| #     | Check            | Pass Criteria                                   |
| ----- | ---------------- | ----------------------------------------------- |
| 4.2.1 | DAG renders      | Agent-to-agent edges derived from `from:X` tags |
| 4.2.2 | Edge thickness   | Proportional to handoff count                   |
| 4.2.3 | Direction arrows | Clear directional arrows on edges               |

### 4.3 Bottleneck Heatmap

| #     | Check         | Pass Criteria                                                  |
| ----- | ------------- | -------------------------------------------------------------- |
| 4.3.1 | Grid renders  | Rows = agents, columns = time buckets                          |
| 4.3.2 | Color scale   | Darker = more queued tasks; legend present                     |
| 4.3.3 | Accessibility | Color differences also conveyed by intensity (colorblind-safe) |

---

## 5. KG Versioning (`/vcs`)

### 5.1 Branch DAG

| #     | Check             | Pass Criteria                             |
| ----- | ----------------- | ----------------------------------------- |
| 5.1.1 | Branch list       | Shows all branches with head commit hash  |
| 5.1.2 | DAG visualization | Commit history as node graph (React Flow) |
| 5.1.3 | Current branch    | Active branch highlighted                 |

### 5.2 Diff Viewer

| #     | Check          | Pass Criteria                                                 |
| ----- | -------------- | ------------------------------------------------------------- |
| 5.2.1 | Snapshot diff  | Selecting two snapshots shows added/removed/modified entities |
| 5.2.2 | Entity changes | Side-by-side property comparison                              |
| 5.2.3 | Edge changes   | Added/removed edges listed with endpoints                     |

### 5.3 Merge UI

| #     | Check         | Pass Criteria                                                  |
| ----- | ------------- | -------------------------------------------------------------- |
| 5.3.1 | Conflict list | Shows conflicting entities with both versions                  |
| 5.3.2 | Resolution    | "Accept ours" / "Accept theirs" / "Manual edit" per conflict   |
| 5.3.3 | Merge result  | After resolution: merged snapshot created, branch head updated |

---

## 6. HTTP API Layer

### 6.1 REST Endpoints

| #     | Check                     | Pass Criteria                                                 |
| ----- | ------------------------- | ------------------------------------------------------------- |
| 6.1.1 | `GET /api/entities`       | Returns JSON array of entities                                |
| 6.1.2 | `GET /api/entities/:id`   | Returns single entity by UUID                                 |
| 6.1.3 | `POST /api/entities`      | Creates entity, returns created object                        |
| 6.1.4 | `GET /api/search?query=X` | Returns search results                                        |
| 6.1.5 | `POST /api/request`       | DSL passthrough works                                         |
| 6.1.6 | Error shape               | Invalid requests return `{ok: false, error: {code, message}}` |

### 6.2 Auth (when enabled)

| #     | Check                   | Pass Criteria                                    |
| ----- | ----------------------- | ------------------------------------------------ |
| 6.2.1 | No key → 401            | Missing `Authorization` header returns 401       |
| 6.2.2 | Bad key → 401           | Invalid key returns 401 with clear error         |
| 6.2.3 | Valid key → 200         | Valid key returns data scoped to key's namespace |
| 6.2.4 | Cross-namespace blocked | Key for ns-A cannot read ns-B data               |

---

## 7. Cross-Cutting

### 7.1 Responsive Layout

| #     | Check            | Pass Criteria                                       |
| ----- | ---------------- | --------------------------------------------------- |
| 7.1.1 | Desktop (1920px) | Full layout, sidebar navigation, content area       |
| 7.1.2 | Laptop (1280px)  | Sidebar collapses to icons; content still readable  |
| 7.1.3 | Tablet (768px)   | Single-column layout; hamburger menu for navigation |

### 7.2 Performance

| #     | Check          | Pass Criteria                                                   |
| ----- | -------------- | --------------------------------------------------------------- |
| 7.2.1 | Initial load   | LCP < 2s on localhost                                           |
| 7.2.2 | Navigation     | Route transitions < 500ms (no full reload)                      |
| 7.2.3 | Large datasets | 1000 entities: table renders without lag; virtualized if needed |

### 7.3 Error Handling

| #     | Check           | Pass Criteria                                                |
| ----- | --------------- | ------------------------------------------------------------ |
| 7.3.1 | Server down     | Shows "Cannot connect to server" banner, not crash           |
| 7.3.2 | 500 errors      | Toast notification with error message; UI remains functional |
| 7.3.3 | Network timeout | Loading indicator; retry button after timeout                |

---

## QA Execution Protocol

1. **Critic agent** opens each route in Chrome (via chrome-devtools MCP)
2. Takes screenshot at each checkpoint
3. Records pass/fail per item with evidence (screenshot path or console output)
4. Files issues for any FAIL items
5. Summary: `{total, passed, failed, blocked}` with issue links

## Blocking vs Non-blocking

- **Blocking** (must fix before release): Sections 1, 6.1 (smoke + API)
- **High priority**: Sections 2.1, 3.1, 4.1 (core views render)
- **Medium**: Sections 2.2, 2.3, 3.2, 4.2-4.3, 5 (interactive features)
- **Low**: Section 7 (polish)
