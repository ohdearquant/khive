# ADR-044: HTTP API Layer — Deno Server REST Wrapper over the Request DSL

**Status**: Proposed\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

khive currently ships one user-facing binary: `khive-mcp`, a stdio MCP server. Agents reach it
over the MCP protocol ([ADR-027](ADR-027-single-tool-mcp-surface.md)); there is no HTTP surface.

Two consumers need HTTP access:

1. **Frontend dashboard** — the Next.js app (`frontend/`) under development (PR #25). Browsers
   cannot speak stdio MCP; they need a conventional `fetch`-based API.
2. **Non-MCP integrations** — CLI scripts, external tools, and future SaaS metering agents that
   benefit from HTTP semantics (caching, proxying, standard auth headers, load balancers).

[ADR-011](ADR-011-deno-mcp-only-server.md) already allocates `deno/src/server.ts` as the HTTP
gateway entry point, and establishes that the Deno server talks to `khive-mcp` over stdio MCP.
That ADR does not specify the REST surface, auth model, CORS policy, or wire shape — this ADR
fills that gap. Issue [#70](https://github.com/ohdearquant/khive/issues/70) tracks the work.

### Constraints

- **No new Rust binary.** The HTTP layer lives entirely in Deno (TypeScript), per ADR-011. Adding
  a second Rust binary (e.g., `axum` server) contradicts the single-binary Rust principle and
  creates a maintenance split.
- **Request DSL is the canonical verb surface.** [ADR-020](ADR-020-request-dsl.md) defines the
  batch-capable verb DSL. The HTTP layer must map to it rather than invent a parallel call path.
  New verbs and pack additions are automatically reachable via HTTP with no HTTP-layer changes.
- **Single `request` MCP tool.** [ADR-027](ADR-027-single-tool-mcp-surface.md) makes `request`
  the only MCP tool. The Deno server's MCP client wrapper calls exactly one tool; the verb
  dispatch is `khive-mcp`'s responsibility.

## Decision

### D1: Transport — Deno HTTP server using Hono, wrapping `khive-mcp` via stdio MCP

`deno/src/server.ts` launches a [Hono](https://hono.dev) application on port 8000. It spawns
`khive-mcp` as a child process and communicates with it over the stdio MCP transport using the
`@modelcontextprotocol/sdk` client library, as planned in ADR-011. Every HTTP route translates
its parameters into a `request(ops="…")` DSL string and calls the single MCP tool.

The translation layer (`deno/src/api/dsl.ts`) is a pure function:

```
(route_params) → ops_string → MCP_call → JSON_response → HTTP_response
```

No business logic lives in the HTTP layer. All validation, namespace enforcement, and verb
dispatch happen inside `khive-mcp`.

### D2: REST API surface

All routes live under `/api`. Convention: collections are plural, identifiers follow `:id`.

#### Entity routes

| Method   | Path                | Request DSL op                                                        | Notes                                                                                           |
| -------- | ------------------- | --------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------- |
| `GET`    | `/api/entities`     | `list(kind="entity", entity_kind=<kind>, limit=<n>, offset=<o>)`      | `kind` defaults to `"entity"`; granular kinds (`concept`, `project`, …) passed as `entity_kind` |
| `GET`    | `/api/entities/:id` | `get(id=":id")`                                                       | UUID or 8-char short ID                                                                         |
| `POST`   | `/api/entities`     | `create(kind="entity", entity_kind=<body.kind>, name=<body.name>, …)` | Body fields forwarded as named args                                                             |
| `PATCH`  | `/api/entities/:id` | `update(id=":id", …body)`                                             | Partial update; returns updated entity                                                          |
| `DELETE` | `/api/entities/:id` | `delete(id=":id")`                                                    | Soft-delete by default; `?hard=true` for hard delete                                            |

#### Edge routes

| Method   | Path             | Request DSL op                                                                                                  |
| -------- | ---------------- | --------------------------------------------------------------------------------------------------------------- |
| `GET`    | `/api/edges`     | `list(kind="edge", source_id=<q>, target_id=<q>, relation=<q>)`                                                 |
| `POST`   | `/api/edges`     | `link(source_id=<body.source_id>, target_id=<body.target_id>, relation=<body.relation>, weight=<body.weight?>)` |
| `DELETE` | `/api/edges/:id` | `delete(id=":id")`                                                                                              |

#### Task routes (GTD pack; only available when `KHIVE_PACKS=kg,gtd`)

| Method | Path                        | Request DSL op                                                  |
| ------ | --------------------------- | --------------------------------------------------------------- |
| `GET`  | `/api/tasks`                | `tasks(status=<q?>, assignee=<q?>, priority=<q?>, limit=<n>)`   |
| `POST` | `/api/tasks`                | `assign(title=<body.title>, priority=<body.priority?>, …)`      |
| `POST` | `/api/tasks/:id/complete`   | `complete(id=":id", result=<body.result?>)`                     |
| `POST` | `/api/tasks/:id/transition` | `transition(id=":id", status=<body.status>, note=<body.note?>)` |

#### Search

| Method | Path          | Request DSL op                 |
| ------ | ------------- | ------------------------------ |
| `GET`  | `/api/search` | `search(kind=<q?>, query=<q>)` |

#### Graph traversal

| Method | Path                          | Request DSL op                                               |
| ------ | ----------------------------- | ------------------------------------------------------------ |
| `GET`  | `/api/traverse`               | `traverse(roots=["<id>"], max_depth=<n?>, relations=<r[]?>)` |
| `GET`  | `/api/entities/:id/neighbors` | `neighbors(node_id=":id", direction=<q?>, relations=<r[]?>)` |

#### Raw DSL passthrough

| Method | Path           | Purpose                                                                                                                                         |
| ------ | -------------- | ----------------------------------------------------------------------------------------------------------------------------------------------- |
| `POST` | `/api/request` | Body `{ops: string}` forwarded verbatim to `request` MCP tool. For agents and power users who need the full DSL without the convenience routes. |

#### Example: entity list

Request:

```
GET /api/entities?entity_kind=concept&limit=25&offset=0
Authorization: Bearer <api-key>
```

Translated DSL:

```
list(kind="entity", entity_kind="concept", limit=25, offset=0)
```

Response `200 OK`:

```json
{
  "ok": true,
  "result": {
    "items": [
      { "id": "a1b2c3d4", "full_id": "a1b2c3d4-...", "kind": "concept", "name": "FlashAttention", ... }
    ],
    "total": 1,
    "limit": 25,
    "offset": 0
  }
}
```

#### Example: raw DSL

Request:

```
POST /api/request
Content-Type: application/json
Authorization: Bearer <api-key>

{
  "ops": "[create(kind=\"entity\", entity_kind=\"concept\", name=\"LoRA\"), create(kind=\"entity\", entity_kind=\"concept\", name=\"QLoRA\")]"
}
```

Response `200 OK`:

```json
{
  "ok": true,
  "result": {
    "results": [
      { "ok": true, "result": { "id": "...", "name": "LoRA" } },
      { "ok": true, "result": { "id": "...", "name": "QLoRA" } }
    ],
    "summary": { "total": 2, "succeeded": 2, "failed": 0 }
  }
}
```

### D3: Auth model

Two tiers, both using the `Authorization: Bearer <token>` header:

**Local dev (no auth):** When `KHIVE_AUTH_DISABLED=true` (default in dev mode), the server
accepts requests with no `Authorization` header and routes them to the default namespace. This
matches the MCP behavior where stdio access implies local trust. The server logs a startup
warning when auth is disabled.

**API key auth:** When auth is enabled, the server extracts the bearer token and resolves it to a
namespace via a namespace resolution function in `deno/src/auth/keys.ts`. In v0.1, key-to-namespace
mapping is read from a config file (`~/.khive/api-keys.json` or the path in `KHIVE_API_KEYS_FILE`).
The resolved namespace is forwarded to `khive-mcp` as a `namespace` arg on each DSL op.

```
Authorization: Bearer khive-sk-abc123
→ namespace resolution → "lambda:khive"
→ list(kind="entity", namespace="lambda:khive", ...)
```

**Future — OAuth (multi-tenant):** When khive moves to multi-tenant SaaS, OAuth 2.0 access tokens
replace API keys. The namespace resolution function becomes a DB lookup against the tenant store.
The HTTP layer does not change; only `auth/keys.ts` is replaced. This is a cloud middleware
concern (ADR-034 was rejected for OSS; identity management is a cloud middleware concern).

### D4: CORS policy

The server applies CORS middleware (Hono's built-in `cors()` middleware) with the following policy:

```typescript
cors({
  origin: (origin) => {
    const allowed = (Deno.env.get("KHIVE_CORS_ORIGINS") ?? "http://localhost:3000")
      .split(",")
      .map((s) => s.trim());
    return allowed.includes(origin) ? origin : null;
  },
  allowMethods: ["GET", "POST", "PATCH", "DELETE", "OPTIONS"],
  allowHeaders: ["Content-Type", "Authorization"],
  maxAge: 86400,
});
```

`localhost:3000` (Next.js dev server) is allowed by default. Production deployments set
`KHIVE_CORS_ORIGINS` to their domain(s). Wildcard origins (`*`) are not permitted — API keys
would be trivially exposed to cross-site requests.

### D5: Error shape

All errors use a consistent envelope, matching the per-op error shape from
[ADR-020](ADR-020-request-dsl.md):

```json
{ "ok": false, "error": { "code": "NOT_FOUND", "message": "entity a1b2c3d4 not found" } }
```

HTTP status codes map to error codes as follows:

| Code                    | HTTP status | When                                                                          |
| ----------------------- | ----------- | ----------------------------------------------------------------------------- |
| `BAD_REQUEST`           | 400         | Malformed DSL, missing required fields                                        |
| `UNAUTHORIZED`          | 401         | Missing or invalid API key when auth is enabled                               |
| `FORBIDDEN`             | 403         | Gate deny (ADR-029/ADR-032)                                                   |
| `NOT_FOUND`             | 404         | UUID not found or soft-deleted                                                |
| `CONFLICT`              | 409         | Duplicate create (if the runtime returns a uniqueness error)                  |
| `UNPROCESSABLE_ENTITY`  | 422         | Valid DSL that fails verb-level validation (unknown kind, bad relation, etc.) |
| `INTERNAL_SERVER_ERROR` | 500         | Unexpected MCP transport failure or Deno runtime error                        |

The server wraps MCP tool errors (per-op `{ok: false, error: "..."}`) into the HTTP error
envelope. A failed single-op call returns a 4xx status. A batch call via `POST /api/request`
returns `200 OK` with the per-op `ok` discriminants in the results array — individual op failures
do not become HTTP errors (consistent with ADR-020 batch semantics).

### D6: WebSocket event stream (phase 2, depends on ADR-038)

A WebSocket endpoint at `WS /api/events` is deferred to phase 2 and is conditional on
[ADR-038](ADR-038-events-surface.md) shipping. The planned shape:

- Clients connect with an optional `Authorization: Bearer <token>` header.
- Filter parameters passed as query string: `?verb=create&since=<microseconds_utc>`.
- The server polls `list(kind="event", ...)` on a configurable interval (default: 1s) and pushes
  new events to connected clients as newline-delimited JSON.
- Server-Sent Events (SSE) via `GET /api/events` is an alternative for clients that cannot
  upgrade to WebSocket; the same filter parameters apply.

The phase-1 server does not implement this endpoint. The route is reserved and returns `501 Not
Implemented` with `{"ok":false,"error":{"code":"NOT_IMPLEMENTED","message":"event stream
requires ADR-038"}}` until phase 2 ships.

### D7: Module layout

```
deno/
├── deno.json           # existing: tasks + fmt config
└── src/
    ├── server.ts       # Hono app entry; registers all route groups; starts MCP child
    ├── api/
    │   ├── entities.ts # GET/POST/PATCH/DELETE /api/entities[/:id]
    │   ├── edges.ts    # GET/POST/DELETE /api/edges[/:id]
    │   ├── tasks.ts    # GET/POST /api/tasks; POST /api/tasks/:id/complete|transition
    │   ├── search.ts   # GET /api/search
    │   ├── traverse.ts # GET /api/traverse; GET /api/entities/:id/neighbors
    │   ├── request.ts  # POST /api/request (raw DSL passthrough)
    │   ├── events.ts   # WS+SSE /api/events (phase 2; stub returns 501)
    │   └── dsl.ts      # Pure helper: route_params → ops_string
    ├── auth/
    │   └── keys.ts     # Bearer token → namespace resolution
    ├── mcp/
    │   └── client.ts   # MCP child process spawn + request(ops=...) call
    └── types/
        └── api.ts      # TypeScript types for request/response shapes
```

The `mcp/client.ts` module is the single point of contact with `khive-mcp`. All route handlers
call it through the same interface:

```typescript
interface McpClient {
  request(ops: string): Promise<McpResult>;
}
```

### D8: Startup behavior

`deno task server` (defined in `deno/deno.json`):

1. Reads config from environment (`KHIVE_PORT`, `KHIVE_CORS_ORIGINS`, `KHIVE_AUTH_DISABLED`,
   `KHIVE_MCP_BIN`, `KHIVE_PACKS`).
2. Spawns `khive-mcp` as a child process (path from `KHIVE_MCP_BIN`, defaulting to `khive-mcp`
   on `PATH`). Passes `--pack` flags derived from `KHIVE_PACKS`.
3. Performs an MCP handshake. If the handshake fails within 5 seconds, exits with a descriptive
   error.
4. Registers Hono routes and starts listening on `KHIVE_PORT` (default: 8000).
5. On `SIGTERM` / `SIGINT`: sends the MCP `shutdown` message, waits up to 2 seconds for the
   child process to exit, then terminates.

## Rationale

### Why Deno + Hono, not a separate Rust HTTP server

ADR-011 already made the language decision: the server layer is Deno. Adding a Rust HTTP crate
(`axum`) would:

- Contradict ADR-011's "exactly one Rust binary" principle.
- Require duplicating the request routing logic in a second language.
- Lose the type-sharing benefit between `deno/src/types/` and `frontend/`.

Hono is the HTTP framework designated in ADR-011 (§"Open Questions", first item). It runs on Deno
natively, is edge-deployable (Cloudflare Workers, Deno Deploy), and has a minimal footprint.

### Why route → DSL translation, not direct runtime calls

The alternative is for the Deno server to call `khive-runtime` methods directly (via Deno FFI or
a second Rust library). This approach:

- Couples the HTTP layer to the Rust storage internals.
- Requires a Rust FFI crate (`khive-ffi`) to be built and kept in sync.
- Violates the layering established by ADR-003 (Deno does not reach below the MCP boundary).

Route → DSL → `request` MCP tool preserves the single dispatch site (ADR-027), the gate
evaluation (ADR-029), and the namespace enforcement already in `khive-mcp`. The HTTP layer
remains pure glue.

### Why REST routes in addition to `POST /api/request`

The raw DSL passthrough (`POST /api/request`) gives agents and scripts full access. But the
Next.js frontend needs idiomatic REST for:

- **Code readability.** `GET /api/entities?entity_kind=concept` is self-documenting in a
  React component; `request(ops="list(kind=\"entity\",entity_kind=\"concept\")")` is not.
- **Browser tooling.** Browser devtools, network inspectors, and service workers reason about
  REST semantics (GET is cacheable, DELETE is destructive).
- **Standard auth middleware.** Route-level middleware is easier to scope with REST paths than
  with a single-endpoint DSL.

### Why two-tier auth (local dev + API key), not OAuth first

OAuth requires a tenant store, a redirect flow, and session management — all absent in v0.1.
API keys are a one-file config change and are sufficient for single-tenant and early multi-tenant
use. The auth module is isolated in `auth/keys.ts` so the OAuth replacement (a cloud middleware
concern — ADR-034 was rejected for OSS; identity/session handling is a cloud middleware concern) does not
touch route handlers.

### Why WebSocket / SSE is deferred to phase 2

The event stream is only meaningful once `list(kind="event")` ships (ADR-038). Designing the
streaming protocol before the underlying query surface exists risks coupling the implementation
to an unfinished spec. The `501` stub reserves the path without blocking the dashboard on the
event feature.

## Alternatives Considered

| Alternative                                                         | Pros                                  | Cons                                                                                                                 | Why rejected                                                                     |
| ------------------------------------------------------------------- | ------------------------------------- | -------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------------------- |
| Rust axum HTTP server (`khive-http` crate)                          | Native Rust, no Deno spawn overhead   | Second Rust binary, no type sharing with frontend, contradicts ADR-011                                               | Contradicts established language decision; maintenance split                     |
| Deno server calling `khive-runtime` directly via FFI                | Eliminates MCP round-trip overhead    | Violates ADR-003 layer boundary; requires `khive-ffi` crate; bypasses gate (ADR-029) and namespace enforcement       | Wrong architectural layer; couples server to storage internals                   |
| Single `POST /api/request` endpoint only (no REST routes)           | Minimal surface; no translation layer | Browser fetch patterns are awkward; no HTTP caching semantics; frontend code less readable                           | REST routes add ~200 LOC and significantly improve frontend developer experience |
| GraphQL endpoint                                                    | Strongly typed, introspectable        | Requires schema generation from Rust types; adds a dependency for the schema layer; overkill for a read/write KG API | Not worth the complexity at this scale; REST is sufficient                       |
| No HTTP layer — frontend uses MCP directly via a browser MCP bridge | Consistent protocol for all callers   | Browsers cannot speak stdio MCP; a WebSocket MCP bridge is a custom protocol on top of HTTP anyway                   | Not practical; HTTP is the browser's native protocol                             |

## Consequences

### Positive

- The Next.js frontend gets a standard `fetch`-based API with typed route paths.
- No new Rust code — the HTTP layer is Deno TypeScript, consistent with ADR-011.
- DSL passthrough (`POST /api/request`) gives agents full access; convenience routes give the
  frontend idiomatic REST access. Both reach the same dispatch path.
- Gate evaluation (ADR-029/ADR-032), namespace enforcement, and audit event emission (ADR-035)
  apply to HTTP calls automatically — they happen inside `khive-mcp`, not in the HTTP layer.
- Future pack additions (new verbs) are automatically reachable via `/api/request` without
  HTTP-layer changes. New REST convenience routes can be added on demand.

### Negative

- Every HTTP call incurs a Deno-to-MCP stdio round trip (~0.5–2 ms on localhost). Acceptable
  for dashboard workloads; not suitable for tight loops (use MCP directly for agent batch work).
- Local dev auth (`KHIVE_AUTH_DISABLED=true`) is a footgun if accidentally enabled in
  production. Documentation and deployment tooling must make this configuration visible.
- The `auth/keys.ts` file-based key store is a temporary solution. It does not support key
  rotation, expiry, or per-key rate limiting without manual file edits. Replacement is a cloud
  middleware concern (ADR-034 was rejected for OSS).

### Neutral

- The `deno task server` command already exists as a placeholder from ADR-011. This ADR fills
  in its implementation contract.
- `khive-mcp` is unchanged. The HTTP layer is a new consumer, not a modification to the Rust
  runtime.

## Implementation

| Step                     | File                                                 | Change                                                                       |
| ------------------------ | ---------------------------------------------------- | ---------------------------------------------------------------------------- |
| 1. Hono app + MCP client | `deno/src/server.ts`, `deno/src/mcp/client.ts`       | Spawn `khive-mcp`; handshake; export `McpClient`                             |
| 2. DSL builder           | `deno/src/api/dsl.ts`                                | Pure function: `buildOps(verb, params)` → DSL string                         |
| 3. Entity routes         | `deno/src/api/entities.ts`                           | Five handlers; import `McpClient` + `buildOps`                               |
| 4. Edge routes           | `deno/src/api/edges.ts`                              | Three handlers                                                               |
| 5. Task routes           | `deno/src/api/tasks.ts`                              | Four handlers; guard on `KHIVE_PACKS` containing `gtd`                       |
| 6. Search + traverse     | `deno/src/api/search.ts`, `deno/src/api/traverse.ts` | Two handlers each                                                            |
| 7. Raw DSL passthrough   | `deno/src/api/request.ts`                            | Forward `body.ops` verbatim                                                  |
| 8. Events stub           | `deno/src/api/events.ts`                             | Return `501`; log `"events pending ADR-038"`                                 |
| 9. Auth middleware       | `deno/src/auth/keys.ts`                              | Token → namespace; CORS middleware registration                              |
| 10. Types                | `deno/src/types/api.ts`                              | Shared TS types for all request/response shapes                              |
| 11. Deno task            | `deno/deno.json`                                     | Add `"server": "deno run --allow-net --allow-env --allow-run src/server.ts"` |

The smoke test (`tests/smoke_test.py`) gains a companion (`tests/http_smoke_test.ts`) that
starts the server, issues one request per route group, and asserts `200 OK` or `201 Created`.

## Open Questions

1. **MCP transport upgrade.** The server uses stdio MCP to `khive-mcp` in v0.1. If the MCP
   spec standardizes an HTTP transport, the client module (`mcp/client.ts`) switches to it
   without touching route handlers.
2. **Rate limiting.** `Obligation::RateLimit` from ADR-029 is not enforced at the HTTP layer
   in v0.1. Enforcement is a cloud middleware concern (ADR-034 was rejected for OSS); when rate limiting
   lands, the Hono middleware layer is the natural insertion point.
3. **Request ID / tracing.** The server should propagate a request ID header
   (`X-Request-Id`) into `GateContext.session_id` for correlation with audit events. Not
   specified here; deferred to the audit observability work.
4. **Versioning.** The API is unversioned in v0.1 (`/api/`, no `/v1/`). If breaking changes
   land, a version prefix will be added. Omitted now to avoid premature constraint.

## References

- [ADR-003](ADR-003-four-layer-architecture.md): Four-layer architecture — HTTP sits in the Deno
  server layer, above the MCP boundary
- [ADR-011](ADR-011-deno-mcp-only-server.md): Deno server + MCP-only — establishes the Deno
  HTTP gateway entry point and the single Rust binary constraint
- [ADR-020](ADR-020-request-dsl.md): Request DSL — verb syntax that HTTP routes translate to
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single-tool MCP surface — the single dispatch
  site that HTTP calls route through
- [ADR-029](ADR-029-authorization-gate.md): Authorization gate — gate evaluation (DENY → 403)
  happens inside `khive-mcp`, not in the HTTP layer
- [ADR-034](ADR-034-identity-session-metering-hooks.md): Identity, session, metering —
  rejected for OSS; OAuth/session/metering are cloud middleware concerns, not OSS gate traits
- [ADR-038](ADR-038-events-surface.md): Events surface — prerequisite for the WebSocket/SSE
  event stream (phase 2)
- Hono: <https://hono.dev>
- GitHub issue [#70](https://github.com/ohdearquant/khive/issues/70): HTTP API for frontend
