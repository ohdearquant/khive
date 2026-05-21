# ADR-046: Frontend Authentication and Real-Time Event Delivery

**Status**: moved — see khive-cloud ADR-031\
**Date**: 2026-05-20\
**Authors**: Ocean, lambda:khive

## Context

The khive dashboard (a Next.js application in `products/khive-dashboard/`) needs two
cross-cutting capabilities that affect every route:

1. **Authentication** — who is allowed to see and modify what.
2. **Real-time updates** — when the underlying data changes, the UI reflects it without a manual
   reload.

Both concerns interact with the existing authorization model. The `Gate` trait
([ADR-029](ADR-029-authorization-gate.md)) and its hard-enforcement layer
([ADR-035](ADR-035-hard-enforcement-and-audit-persistence.md)) operate at the MCP / verb dispatch
level. The HTTP gateway layer that the dashboard talks to must propagate authentication context
into that dispatch path — specifically, the `namespace` claim carried in every JWT must flow
through to `GateRequest.namespace` so the single-dispatch-site invariant
([ADR-027](ADR-027-single-tool-mcp-surface.md)) is not bypassed.

The Events surface ([ADR-038](ADR-038-events-surface.md)) provides queryable event records that
are the natural source for real-time change signals; the real-time delivery mechanism must be
compatible with that surface.

### Scope of this ADR

This ADR specifies:

- The three authentication tiers and their progressive deployment model.
- The JWT claim structure and session model.
- The frontend auth flow (login, storage, refresh, middleware).
- The namespace isolation contract in the HTTP layer.
- The three-phase real-time delivery strategy (polling → WebSocket → SSE).
- Wire schemas for WebSocket messages and SSE events.
- Optimistic update semantics.

This ADR does **not** specify:

- The HTTP gateway crate architecture (deferred; that is a separate platform ADR).
- OAuth provider integrations beyond the interface boundary (deferred to the OAuth provider
  integration ADR).
- The `SessionStore` SQL implementation (ADR-034 was rejected for OSS; the `SessionStore` contract and its SQL implementation are specified in khive-cloud ADR-031).

## Decision

### A. Authentication Tiers

Three tiers in progressive order. A deployment uses the highest tier its infrastructure supports.
Each tier is a strict superset of the previous one's behavior.

#### Tier 1 — Local (dev / personal, no authentication)

- Binding: `localhost` only. The server MUST reject non-loopback requests at the transport
  level; the check is not deferred to the gate.
- Auth model: `AllowAllGate` (ADR-029 default). Every request carries
  `ActorRef { kind: "anonymous", id: "local" }`.
- No login page. No JWT. No cookie.
- Intended users: solo developers, CLI-first workflows, CI environments.
- Upgrade path: switching from Tier 1 to Tier 2 requires setting `KHIVE_AUTH_TIER=api_key` and
  provisioning at least one API key.

#### Tier 2 — API Key (remote single-tenant)

- Auth model: `Authorization: Bearer <key>` header → API key validated against
  `_khive_api_keys` table → namespace resolved from the key record → JWT issued.
- Key provisioning: `khive auth create-key --namespace <ns> --role <role>`.
- Roles: `admin` (full verb access), `viewer` (read-only verbs: `get`, `list`, `search`,
  `neighbors`, `traverse`, `query`), `agent` (verb-scoped: configured per key in
  `allowed_verbs: Vec<String>`).
- Intended users: personal cloud deployments, agent integrations, dashboard remote access.

#### Tier 3 — OAuth (multi-tenant, v2)

- Auth model: GitHub OAuth → tenant mapping → JWT issued with tenant namespace.
- Deferred to v2. This ADR specifies the interface boundary; the OAuth provider integration
  is a separate ADR.
- The JWT structure and session model defined below are compatible with OAuth; no structural
  change will be required when Tier 3 lands.

### B. Session Model

Sessions are stateless JWT tokens issued by the HTTP gateway. A `SessionStore` interface is the
persistence boundary (ADR-034 was rejected for OSS — `SessionStore` is not an OSS gate trait;
it is specified as cloud middleware in khive-cloud ADR-031 and implemented at the gateway layer);
the HTTP gateway calls `SessionStore::create` on login and `SessionStore::validate` on each
subsequent request.

#### JWT Claim Structure

```typescript
interface KhiveJwtClaims {
  // Standard claims
  iss: string; // "khive" — issuer constant
  sub: string; // actor id (API key id or OAuth subject)
  iat: number; // issued-at (Unix timestamp, seconds)
  exp: number; // expiry (Unix timestamp, seconds)
  jti: string; // JWT ID — unique token identifier, used for revocation

  // khive-specific claims
  namespace: string; // tenant namespace — injected on every DSL op
  role: "admin" | "viewer" | "agent"; // access level
  allowed_verbs?: string[]; // present only when role = "agent"; enforced by the gate
  tier: 1 | 2 | 3; // authentication tier that issued this token
}
```

JWT signing: RS256 with a server-managed key pair. The public key is published at
`GET /api/.well-known/jwks.json` for external verifier compatibility. HS256 is available for
single-server deployments where key distribution is not a concern; the algorithm is
server-configured, not client-chosen.

#### Token Lifetimes

| Token type    | Default TTL                    | Renewability                                                          |
| ------------- | ------------------------------ | --------------------------------------------------------------------- |
| Access token  | 15 minutes                     | Renewed via refresh token; no interaction required                    |
| Refresh token | 7 days                         | Single-use rotation; a new refresh token is issued on each use        |
| API key token | Configurable (default 30 days) | Re-issued on key re-validation; revocable via `khive auth revoke-key` |

Short-lived access tokens limit the blast radius of a leaked token. Refresh token rotation
ensures a stolen refresh token is detected: using a previously rotated token triggers
revocation of the entire token family (the "refresh token reuse" detection pattern).

### C. Frontend Auth Flow

#### Login Page

```
┌─────────────────────────────────────────────┐
│  Tier 2: Enter API key                      │
│  [ api_key_input            ] [Sign In]     │
│  ─────────────────────────────────────────  │
│  Tier 3 (v2): [Sign in with GitHub]         │
└─────────────────────────────────────────────┘
```

API key flow:

1. User submits key to `POST /api/auth/login` with body `{ "api_key": "<key>" }`.
2. Server validates the key, resolves namespace and role, calls `SessionStore::create`.
3. Server sets two `httpOnly` cookies:
   - `khive_access`: JWT access token (`SameSite=Strict`, `Secure` in production)
   - `khive_refresh`: refresh token (`SameSite=Strict`, `Secure`, `Path=/api/auth/refresh`)
4. Server responds `200 { "namespace": "...", "role": "..." }` — no token in the body.
5. Next.js stores `namespace` and `role` in React context (not localStorage) for UI rendering.

**Why `httpOnly` cookies over `localStorage`:** `localStorage` is readable by any JavaScript
on the page — a single XSS injection exfiltrates all tokens. `httpOnly` cookies are inaccessible
to JavaScript by design. The `SameSite=Strict` attribute prevents CSRF. This is the OWASP
recommended pattern for session token storage in browser applications.

Refresh flow:

1. Next.js middleware detects an expired access token (by checking `exp` from the decoded,
   but unverified, payload — verification happens server-side only).
2. Middleware calls `POST /api/auth/refresh` with the `khive_refresh` cookie.
3. Server validates the refresh token, issues a new access+refresh pair (rotation), sets new
   cookies.
4. Original request is retried transparently.
5. On rotation failure (invalid/expired/reused refresh token), both cookies are cleared and
   the user is redirected to `/login`.

Logout flow:

1. `POST /api/auth/logout` — server calls `SessionStore::expire(jti)`, clears both cookies.

#### Next.js Middleware

Next.js Edge Middleware runs before every request to `/api/*` routes and every protected page.
The middleware:

1. Reads `khive_access` from the request cookies.
2. Decodes the JWT header and payload (without verifying — verification is on the API route
   server side via `SessionStore::validate`).
3. If `exp < now + 30s` (within 30 seconds of expiry): calls the refresh endpoint transparently
   before forwarding.
4. If no cookie, expired and refresh failed, or malformed: redirects to `/login`.
5. Injects `namespace` and `role` as request headers (`x-khive-namespace`,
   `x-khive-role`) for downstream API routes.

```typescript
// middleware.ts — pseudocode
import { NextRequest, NextResponse } from "next/server";
import { decodeJwtPayload, isExpiring } from "@/lib/jwt";

export async function middleware(req: NextRequest): Promise<NextResponse> {
  const access = req.cookies.get("khive_access")?.value;
  if (!access) return redirectToLogin(req);

  const payload = decodeJwtPayload(access); // no verification — payload inspection only
  if (!payload) return redirectToLogin(req);

  if (isExpiring(payload, 30)) {
    const refreshed = await refreshTokens(req);
    if (!refreshed) return redirectToLogin(req);
    return refreshed; // new cookies already set
  }

  const headers = new Headers(req.headers);
  headers.set("x-khive-namespace", payload.namespace);
  headers.set("x-khive-role", payload.role);
  return NextResponse.next({ request: { headers } });
}

export const config = {
  matcher: ["/api/:path*", "/dashboard/:path*"],
};
```

### D. Namespace Isolation in the HTTP Layer

Every API route handler MUST:

1. Read `namespace` from the `x-khive-namespace` header injected by middleware (trusted — set
   by server-side middleware, not by the client).
2. Pass `namespace` as an explicit parameter on every DSL operation forwarded to the MCP
   dispatch path.
3. Never accept a `namespace` parameter from the request body or query string.

This ensures the JWT `namespace` claim is the sole source of tenant scoping. A compromised
client cannot access a foreign namespace by crafting a DSL operation with a different
`namespace` field — the route handler overwrites it.

```typescript
// Example: API route enforcing namespace injection
// pages/api/entities/list.ts (pseudocode)
export async function GET(req: Request): Promise<Response> {
  const namespace = req.headers.get("x-khive-namespace")!; // set by middleware
  const result = await khiveDispatch({
    ops: `list(kind="entity", namespace="${namespace}", limit=50)`,
  });
  return Response.json(result);
}
```

The `namespace` override rule applies equally to all verbs, including mutations. A `create`
that arrives without an attacker-supplied `namespace` field cannot escape the caller's
namespace.

### E. Real-Time Event Delivery

Three phases in progressive order. Each phase is operational before the next begins. Phases do
not require the previous to be decommissioned — a client may fall back to an earlier phase if
the later one is unavailable.

#### Phase 1 — SWR Polling (immediate, no server changes required)

SWR (`swr` npm package) provides stale-while-revalidate semantics: the UI shows the last known
data immediately and refreshes in the background.

Configuration:

```typescript
import useSWR from "swr";

const { data, error, isValidating } = useSWR(
  "/api/entities?kind=task&namespace=<ns>",
  fetcher,
  {
    refreshInterval: isWindowActive() ? 5_000 : 30_000, // 5s active, 30s idle
    revalidateOnFocus: true,
    revalidateOnReconnect: true,
    dedupingInterval: 2_000,
  },
);
```

The `isWindowActive` function checks `document.hasFocus()` and the Page Visibility API
(`document.visibilityState === "visible"`) to reduce polling overhead when the tab is
backgrounded.

Long-polling variant: for lower overhead, `POST /api/events/poll` with
`{ "since": <last_event_timestamp_us>, "filter": {...} }` blocks until at least one matching
event arrives (max hold time 25 seconds) or the hold window expires. This is an optional
server-side enhancement; the SWR client does not change.

Phase 1 does not require ADR-038 to be implemented, but benefits from it: using
`list(kind="event", since=<cursor>)` as the polling target is more efficient than
polling entity/note lists directly.

#### Phase 2 — WebSocket (primary real-time channel, depends on ADR-038)

The WebSocket endpoint is:

```
WS /api/events/ws?filter=<url_encoded_json>
```

Authentication: the server validates `khive_access` from the cookie on the HTTP upgrade
request (cookies are sent on the upgrade by default in browsers). No separate auth step.

##### WebSocket Message Schema

Server → Client messages:

```typescript
// Event notification (the primary message type)
interface WsEventMessage {
  type: "event";
  id: string; // event UUID
  seq: number; // monotonically increasing sequence number within this connection
  verb: string; // the verb that produced this event
  namespace: string; // always the caller's namespace
  outcome: "success" | "denied" | "error";
  actor: string; // actor id string
  substrate: "note" | "entity" | "event";
  timestamp_us: number; // created_at in microseconds UTC
  data?: unknown; // optional: the AuditEvent.data payload, if any
}

// Heartbeat (sent every 30 seconds to keep proxies alive)
interface WsHeartbeat {
  type: "heartbeat";
  timestamp_us: number;
}

// Connection confirmation on upgrade
interface WsConnected {
  type: "connected";
  session_id: string; // assigned by server; used in last-event-id recovery
  namespace: string;
}

// Error from the server (malformed filter, permission denied, etc.)
interface WsError {
  type: "error";
  code: string; // e.g. "invalid_filter", "permission_denied"
  message: string;
}
```

Client → Server messages:

```typescript
// Update subscription filter without reconnecting
interface WsSubscribe {
  type: "subscribe";
  filter: WsEventFilter;
}

// Acknowledge receipt (optional; future use for guaranteed delivery)
interface WsAck {
  type: "ack";
  seq: number;
}
```

Filter schema:

```typescript
interface WsEventFilter {
  verbs?: string[]; // filter to specific verbs, e.g. ["create", "update"]
  outcomes?: ("success" | "denied" | "error")[]; // default: all
  substrates?: ("note" | "entity" | "event")[]; // default: all
  actors?: string[]; // filter by actor id
  since_us?: number; // replay events from this timestamp on connect (gap recovery)
}
```

##### Server-Side Subscription Management

The Deno MCP server (ADR-011) maintains a per-connection subscription registry:

```typescript
interface WsConnection {
  id: string; // connection-local UUID
  namespace: string; // from JWT; immutable
  filter: WsEventFilter;
  lastSeq: number; // last seq number sent on this connection
  socket: WebSocket;
}
```

On each new `AuditEvent` appended to `EventStore` (via ADR-035's dispatch-time write), the
server fans out to all connections whose filter matches the event and whose `namespace` equals
the event's namespace. The namespace check is non-negotiable — a connection cannot receive
events from a foreign namespace regardless of the filter.

Fan-out is synchronous within the Deno event loop (no background tasks). At expected
personal-deployment volumes (tens of events per second), synchronous fan-out is sufficient.
At cloud scale, a broadcast channel (Deno `BroadcastChannel` for multi-isolate, or an
external pub-sub for multi-process) is the scale-out path.

##### Gap Recovery

On reconnect, the client sends `since_us` in the filter. The server calls
`list(kind="event", since=<since_us>, namespace=<ns>)` (ADR-038 surface) and replays
matching events before switching to live delivery. The replay uses the same `WsEventMessage`
shape with `seq` continuing from the last acknowledged value.

#### Phase 3 — Server-Sent Events (simpler-deployment alternative)

SSE is an alternative to WebSocket for environments where WebSocket is unavailable (certain
reverse proxies, HTTP/1.1 only deployments, PaaS providers that do not proxy WebSocket
upgrades).

Endpoint:

```
GET /api/events/stream
```

Authentication: `khive_access` cookie (browser sends cookies on SSE `EventSource` requests).

SSE message format:

```
id: <event_uuid>
event: khive_event
data: <JSON-serialized WsEventMessage (same schema, type field = "event")>
```

(Standard SSE format: each field on its own line, blank line between messages.)

The client reconnects with `Last-Event-ID` header set to the last received event UUID. The
server uses this to replay missed events via `list(kind="event", since=...)` on reconnect —
the same gap recovery mechanism as Phase 2.

For `heartbeat` messages, the SSE comment syntax is used:

```
: heartbeat <timestamp_us>
```

SSE comments (lines starting with `:`) are ignored by browsers but keep the connection alive
through proxies.

**Phase 2 vs Phase 3 selection:** the client probes for WebSocket support on first load. If
the upgrade succeeds, WebSocket is used. If the upgrade fails (HTTP 4xx/5xx), the client falls
back to SSE automatically. The server exposes both endpoints; operators may disable WebSocket
via `KHIVE_WS_ENABLED=false` to force SSE.

### F. Optimistic Updates

Optimistic updates apply only to Phase 2 and Phase 3 (real-time channels available). In Phase
1 (polling), updates are confirmed before the UI reflects them.

#### Optimistic Update Protocol

1. User performs an action (e.g., completes a task).
2. The UI immediately applies the expected state change to the local SWR cache (`mutate` with
   `optimisticData`).
3. The API call is dispatched to the server.
4. On success: the next WebSocket/SSE event confirming the mutation is received; SWR revalidates
   and reconciles. If the server state matches the optimistic state, no visible change occurs.
5. On error: SWR reverts to the last confirmed server state; the UI shows an error notification.

Conflict detection via sequence numbers:

```typescript
interface OptimisticOperation {
  id: string; // client-generated UUID for this operation
  verb: string;
  expectedSeq: number; // last event seq seen by this client
  payload: unknown;
}
```

If the server receives an operation with `expectedSeq` that is behind the current
`EventStore` sequence for the namespace, the server responds with
`409 Conflict { "server_seq": <current>, "client_seq": <expected> }`. The client then
revalidates from the server's current state before retrying.

Sequence numbers are namespace-scoped, not global. The sequence counter is derived from the
`EventStore`'s `created_at` column in microseconds — a monotonic value that is already
present in every `Event` record (ADR-004, ADR-038). No new counter table is required.

## Rationale

### Why three authentication tiers rather than a single mandatory auth model

The existing ADR-029 / ADR-035 auth layer is gated behind `AllowAllGate` for local
deployments. Forcing auth on Tier 1 (local) would break the zero-config developer experience
that is central to khive-oss's adoption model. The three tiers match the three deployment
contexts: personal local, personal remote, multi-tenant commercial.

### Why `httpOnly` cookies over `Authorization` header for browser sessions

API clients (CLI, agent scripts) use `Authorization: Bearer <token>` because they control
request construction. Browser applications cannot prevent XSS from reading
`Authorization` headers set via JavaScript, but `httpOnly` cookies are architecturally
inaccessible to JavaScript regardless of XSS. The OWASP Token Storage Cheat Sheet recommends
`httpOnly` cookies as the only browser-side storage option resistant to XSS token theft.

### Why the access token is short-lived (15 minutes)

A stolen access token is valid for at most 15 minutes. Combined with `httpOnly` cookie
storage (which makes theft require more than XSS), the 15-minute window bounds the practical
exploitation window. The refresh token rotation pattern ensures that a stolen refresh token is
detected and revoked the first time it is reused — providing session revocability without
server-side state per access token.

### Why SWR polling before WebSocket

WebSocket implementation requires server-side changes (connection management, fan-out, gap
recovery). SWR polling is a pure-client change and can ship with zero server modifications
beyond the existing `list(kind="event", ...)` surface from ADR-038. Polling at 5s intervals
for the active tab is acceptable latency for the current use cases (task management, KG
updates). WebSocket is the correct target for sub-second update requirements, but those
requirements are not present at v0.1 scale.

### Why both WebSocket and SSE rather than choosing one

WebSocket and SSE have different transport compatibility profiles. WebSocket requires a
protocol upgrade that some HTTP/1.1 proxies and PaaS platforms do not support. SSE is plain
HTTP and works through any proxy. Offering both with automatic client-side fallback means
the dashboard works in more deployment environments without operator configuration. The
server-side implementation shares the same fan-out logic; the only difference is the wire
format.

### Why sequence numbers are derived from `created_at` microseconds rather than a separate counter

A separate sequence counter introduces a schema change and a monotonicity invariant to
maintain. `Event.created_at` is already a microsecond UTC timestamp that is monotonically
increasing within a namespace (SQLite's write serialization guarantees this for single-process
deployments). Using it as the sequence number reuses an existing invariant and avoids new
infrastructure. For multi-process deployments where two writers could produce the same
microsecond timestamp, the gap recovery protocol (replay since `since_us`) is still correct
— the worst case is a small number of duplicate events delivered on reconnect, which the
client deduplicates by event UUID.

### Why namespace isolation is enforced in the HTTP layer (not only in the Gate)

The Gate trait operates at verb dispatch time. The HTTP layer is upstream: it decides which
`namespace` to inject into the DSL operation before the Gate sees it. If the HTTP layer
allowed a caller to supply `namespace` in the request body, a compromised client could name
an arbitrary namespace and the Gate would evaluate the request as if the caller legitimately
owned that namespace. Enforcing namespace injection in the middleware — from the verified JWT
claim only — closes this attack surface before the request reaches dispatch.

## Alternatives Considered

| Alternative                               | Pros                                               | Cons                                                                                    | Why rejected                                                                          |
| ----------------------------------------- | -------------------------------------------------- | --------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------- |
| Single auth tier (always require API key) | Simpler implementation                             | Breaks zero-config local dev; contradicts ADR-029's `AllowAllGate` default              | Local dev friction; adoption cost                                                     |
| Store JWT in `localStorage`               | Accessible to client-side JS; no cookie complexity | XSS can read and exfiltrate the token                                                   | OWASP explicitly recommends against this                                              |
| Long-lived access token (no refresh)      | No refresh token complexity                        | Stolen token valid until expiry; no revocation path shorter than expiry                 | Unacceptable for cloud deployments                                                    |
| WebSocket only (no SSE fallback)          | One real-time path to implement                    | WebSocket blocked by some proxies/PaaS platforms                                        | Limits deployment environments unnecessarily                                          |
| SSE only (no WebSocket)                   | Simpler protocol; proxy-compatible                 | No bidirectional messaging; cannot send `subscribe` filter updates without reconnecting | Reduced real-time flexibility                                                         |
| Polling only (no WebSocket/SSE)           | Smallest implementation surface                    | Latency bounded by poll interval; cannot achieve sub-second updates                     | Insufficient for real-time task updates                                               |
| Separate sequence counter table           | Explicit monotonicity guarantee                    | New schema migration; additional write per event                                        | `created_at` microseconds provide the same guarantee under SQLite write serialization |
| Optimistic updates in Phase 1 (polling)   | Faster perceived UI                                | Revert on mismatch creates visible flicker; no real-time confirmation signal            | Polling interval too long for reliable revert detection                               |
| Accept `namespace` from request body      | Simpler API route implementation                   | Client can escape to foreign namespace                                                  | Security regression; closes the namespace isolation contract                          |

## Consequences

### Positive

- The three-tier model maps directly to the three deployment contexts; operators adopt auth
  incrementally without breaking changes.
- `httpOnly` cookie storage closes the primary browser XSS token-theft vector.
- Refresh token rotation provides revocability with stateless access tokens — the common case
  (no revocation) is fast; the revocation path exists.
- SWR polling is deployable with no server changes; WebSocket and SSE layer on top.
- Namespace injection at the middleware level provides defense-in-depth: even if a
  pack handler forgets to enforce namespace isolation, the JWT-derived namespace was
  already injected by the middleware.
- The real-time event schemas are compatible with ADR-038's `EventFilter` shape; no new
  storage infrastructure is required.

### Negative

- `httpOnly` cookies require HTTPS in production (the `Secure` flag). HTTP-only deployments
  are limited to Tier 1 (localhost).
- Refresh token rotation requires server-side state (`SessionStore` must persist refresh
  token records). ADR-034 was rejected for OSS — session store implementation is specified in
  khive-cloud ADR-031; a SQL-backed implementation is required for Tier 2 and Tier 3 deployments.
- WebSocket fan-out is synchronous in Deno's event loop. At high event volumes, a slow
  WebSocket client blocks fan-out to all clients in the same isolate. The
  `BroadcastChannel`/external pub-sub scale-out path must be prioritized before cloud
  deployment.
- Sequence numbers derived from `created_at` microseconds can produce duplicates in
  multi-process deployments. The client-side deduplication-by-UUID requirement adds
  complexity to the frontend SWR cache management.
- ADR-038's event surface is a prerequisite for Phase 2 gap recovery and Phase 1 event-based
  polling. Deploying Phase 2 before ADR-038 is implemented requires polling entity/note lists
  directly as a fallback, which is less efficient.

### Neutral

- Tier 3 (OAuth) is specified at the interface level only. No server-side changes are
  required until the OAuth provider integration ADR lands. The JWT structure is forward
  compatible.
- The WebSocket and SSE endpoints share the same `WsEventMessage` schema; a single
  serialization layer serves both paths.
- The optimistic update conflict detection (`409 Conflict` with `server_seq`) is consistent
  with ADR-014's curation model: the server is the source of truth; the client proposes,
  the server confirms.

## Implementation Status

| Deliverable                                                             | Location                                         | Status                        |
| ----------------------------------------------------------------------- | ------------------------------------------------ | ----------------------------- |
| Tier 1 local binding (`localhost`-only restriction)                     | HTTP gateway crate (TBD)                         | planned                       |
| API key validation endpoint (`POST /api/auth/login`)                    | HTTP gateway crate (TBD)                         | planned                       |
| JWT issuance (RS256, access + refresh)                                  | `products/khive-dashboard/lib/jwt.ts`            | planned                       |
| `httpOnly` cookie management (set, rotate, clear)                       | Next.js API routes                               | planned                       |
| Next.js Edge Middleware (JWT decode, expiry check, namespace injection) | `products/khive-dashboard/middleware.ts`         | planned                       |
| Namespace injection enforcement in API routes                           | All `products/khive-dashboard/pages/api/` routes | planned                       |
| SWR polling with active/idle refresh intervals                          | `products/khive-dashboard/hooks/useKhiveData.ts` | planned                       |
| WebSocket endpoint (`WS /api/events/ws`)                                | HTTP gateway crate (TBD)                         | planned (depends on ADR-038)  |
| WebSocket connection registry + fan-out                                 | HTTP gateway crate (TBD)                         | planned (depends on ADR-038)  |
| Gap recovery via `list(kind="event", since=...)`                        | HTTP gateway crate (TBD)                         | planned (depends on ADR-038)  |
| SSE endpoint (`GET /api/events/stream`)                                 | HTTP gateway crate (TBD)                         | planned (depends on ADR-038)  |
| Client-side WS/SSE fallback probe                                       | `products/khive-dashboard/lib/realtime.ts`       | planned                       |
| Optimistic update helpers (mutate + revert)                             | `products/khive-dashboard/lib/optimistic.ts`     | planned (depends on Phase 2)  |
| `_khive_api_keys` table migration                                       | `crates/khive-db/src/migrations.rs`              | planned                       |
| SQL-backed `SessionStore` (refresh token persistence)                   | cloud middleware (see khive-cloud ADR-031)       | planned (Tier 2 prerequisite) |

## Open Questions

1. **HTTP gateway crate.** This ADR assumes an HTTP gateway in front of the Deno MCP server.
   The gateway architecture (a new Rust crate, a Deno HTTP server, or a sidecar) is unspecified
   and requires a separate ADR before the login endpoint and WebSocket/SSE endpoints can be
   implemented.

2. **API key storage schema.** The `_khive_api_keys` table schema (key hash, namespace,
   role, `allowed_verbs`, expiry, revocation status) is referenced but not specified. This
   belongs in the migration PR for Tier 2.

3. **Key rotation UX.** The CLI verb `khive auth create-key` is named but not specified.
   The exact parameters, output format, and storage contract belong in the CLI ADR.

4. **WebSocket fan-out at cloud scale.** Deno's `BroadcastChannel` is the multi-isolate
   path; an external pub-sub (Redis Streams, NATS) is the multi-process path. The selection
   criterion (number of concurrent connections, number of Deno isolates) is unspecified.
   Track as a prerequisite for cloud deployment of Phase 2.

5. **Optimistic update rollback UI.** The `409 Conflict` handling specifies that the client
   revalidates from the server state and retries. The UX for surfacing the conflict to the
   user (silent retry, toast notification, modal) is left to the dashboard implementation.

6. **Tier 3 OAuth provider mapping.** GitHub OAuth produces a user id and email. The mapping
   from GitHub user to khive namespace (1:1, 1:N, org-level) is unspecified. This belongs in
   the OAuth provider integration ADR.

## References

- [ADR-004](ADR-004-substrate-observables.md): Event as the third substrate — `created_at`
  timestamp used as real-time sequence cursor
- [ADR-011](ADR-011-deno-mcp-only-server.md): Deno MCP server — the WebSocket / SSE host
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single dispatch site — the namespace
  injection invariant this ADR enforces upstream
- [ADR-029](ADR-029-authorization-gate.md): Authorization gate — `Gate` trait, `AllowAllGate`
  default, `GateRequest.namespace` as the contract
- [ADR-034](ADR-034-identity-session-metering-hooks.md): Identity, session, and metering hooks
  — rejected for OSS; `SessionStore` is a cloud middleware concern specified in khive-cloud ADR-031,
  not an OSS gate trait
- [ADR-035](ADR-035-hard-enforcement-and-audit-persistence.md): Hard authorization enforcement
  — `PermissionDenied` error, `EventStore` wiring at dispatch site
- [ADR-038](ADR-038-events-surface.md): Events surface — `list(kind="event", since=...)` used
  for polling and gap recovery; prerequisite for Phase 2 and Phase 3
- OWASP Token Storage Cheat Sheet: <https://cheatsheetseries.owasp.org/cheatsheets/HTML5_Security_Cheat_Sheet.html#local-storage>
- RFC 7517 / RFC 7518: JSON Web Key (JWK) and JSON Web Algorithms (JWA)
- RFC 6750: Bearer Token Usage in HTTP
- MDN: EventSource API — <https://developer.mozilla.org/en-US/docs/Web/API/EventSource>
- W3C Server-Sent Events specification
