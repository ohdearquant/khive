# ADR-049: Persistent Warm Runtime over a Unix Socket

**Status**: Accepted
**Date**: 2026-05-30
**Authors**: khive maintainers
**Depends on**: [ADR-016](./ADR-016-request-dsl.md),
[ADR-027](./ADR-027-dynamic-pack-loading.md),
[ADR-031](./ADR-031-multi-engine-retrieval.md)

## Context

`kkernel mcp` normally speaks MCP JSON-RPC over stdio. If every client connection builds
a fresh process, process-local indexes, registries, and other warm runtime state are
reconstructed repeatedly. A long-lived owner can amortize that work while preserving the
stdio transport expected by MCP clients.

Warmup must not delay transport readiness. A pack that can operate with a degraded cold path
should become incrementally warmer without blocking unrelated requests.

## Decision

Use one shipped binary in two MCP modes:

- `kkernel mcp`: a thin stdio MCP process; and
- `kkernel mcp --daemon`: a long-lived Unix-domain-socket server owning the runtime and
  pack registry.

The two modes use the same parser, registry, handlers, configuration resolution, and
response serialization. Only the transport and process lifetime differ.

### 1. Thin client and auto-spawn

In stdio mode, the request handler forwards frames to the daemon:

```text
kkernel mcp (stdio)                kkernel mcp --daemon
  request(ops=...)  --frame-->     VerbRegistry::dispatch
                    <--frame--     response
```

If no responsive socket exists and daemon use is enabled, the client starts
`current_exe mcp --daemon` as a detached child and polls for readiness within a bounded
deadline. Socket bind occurs before background warmup, so readiness does not depend on warm
completion.

`KHIVE_NO_DAEMON=1` disables spawn and uses local dispatch. A daemon that is simply absent
may also fall back locally. A spawn that is attempted and positively fails returns a stable
`respawn_failed` error; it does not silently hide a broken installation.

### 2. Background warmup

`VerbRegistry::call_warm_all()` invokes each selected pack's
`PackRuntime::warm()`. The daemon starts that work after binding the socket.

Warm implementations must publish their usable state atomically. Requests arriving before
completion either use a defined cold path or return a typed capability-unavailable result;
they do not wait on an unbounded restore. A failed warm task records health and may be
retried according to its component policy without stopping transport service.

### 3. Socket protocol

The socket and process marker live in the configured runtime directory. The directory is
owner-only, and both artifacts are created with owner-only permissions.

Frames use:

```text
4-byte big-endian unsigned length
JSON payload bytes
```

The maximum frame size is 8 MiB in each direction. The request payload is the serialized
`RequestParams` plus transport-owned request identity and namespace context. The response
payload is byte-identical to local registry dispatch for the same request and presentation
settings.

On startup, the daemon removes a socket only after proving its recorded process is absent or
the endpoint is unresponsive. On SIGTERM or SIGINT it stops accepting, drains in-flight
requests up to `KHIVE_DRAIN_TIMEOUT_SECS` (default 10 seconds), removes its artifacts, and
exits.

### 4. Per-request context

The daemon validates configuration compatibility and applies request identity as specified
by ADR-096. It must not reuse one request's identity, namespace, or presentation state for
another.

A client-daemon mismatch is classified before fallback:

| `FallbackReason`     | Default behavior                                      |
| -------------------- | ----------------------------------------------------- |
| `no_socket`          | Quiet local fallback when no recovery attempt failed. |
| `protocol_mismatch`  | Local fallback with warning.                          |
| `parse_failure`      | Local fallback with warning.                          |
| `config_mismatch`    | Local fallback with error telemetry.                  |
| `namespace_mismatch` | Local fallback with error telemetry.                  |

`KHIVE_DAEMON_STRICT=1` rejects every would-be fallback with a structured error containing
the stable reason code. Strict mode is disabled by default.

### 5. Scope boundary

This ADR adds:

- one long-lived runtime owner;
- a thin stdio proxy;
- length-prefixed local transport;
- background pack warmup; and
- classified fallback behavior.

It does not add:

- an HTTP listener or SDK protocol;
- a remote authentication plane;
- a new pack-specific snapshot format;
- a second handler implementation;
- public daemon-administration verbs; or
- a guarantee that every pack has a cold degraded mode.

The owner-only local socket has the same host trust boundary as the stdio process. Remote or
multi-principal exposure requires a separate authenticated transport decision.

## Failure behavior

- **No socket and daemon disabled**: dispatch locally.
- **No socket and spawn succeeds**: wait for bounded readiness, then forward.
- **Spawn or pre-bind child exit**: return `respawn_failed`.
- **Protocol or parse mismatch**: apply the classified fallback policy.
- **Configuration or namespace mismatch**: record a strict-violation metric; reject in
  strict mode.
- **Warm task failure**: keep serving defined cold capabilities and expose daemon health.
- **Frame exceeds limit**: reject before allocation or dispatch.
- **Drain timeout**: cancel remaining tracked work and complete cleanup.

## Verification

Tests must prove:

- local and daemon dispatch return byte-identical responses;
- readiness precedes warm completion;
- concurrent frames cannot mix request context;
- frame-size and partial-frame handling are bounded;
- owner-only permissions are applied;
- stale artifact cleanup cannot remove a live daemon socket;
- each fallback reason has stable serialization;
- strict mode rejects every fallback;
- confirmed spawn failure never falls back;
- daemon-disabled operation requires no socket; and
- shutdown drains requests and removes runtime artifacts.

## Alternatives considered

| Alternative                        | Reason rejected                                                    |
| ---------------------------------- | ------------------------------------------------------------------ |
| Background warmup without a daemon | Avoids blocking but repeats warmup for each process.               |
| Faster snapshots only              | Reduces one restore but does not change repeated process lifetime. |
| Separate daemon executable         | Duplicates packaging and risks parser or registry drift.           |
| Silent unconditional fallback      | Hides configuration and recovery faults.                           |

## Consequences

### Positive

- Warm state is amortized across client connections.
- Socket readiness is independent of background warm completion.
- Local and daemon paths share one dispatch implementation.
- Daemonless environments retain a supported local path.

### Negative

- Auto-spawn, stale-artifact cleanup, and drain add process-management surface.
- A long-lived process retains warm memory until shutdown.
- Compatibility checks and fallback classification require stable protocol metadata.

## References

- [ADR-016](./ADR-016-request-dsl.md): forwarded request boundary
- [ADR-027](./ADR-027-dynamic-pack-loading.md): selected pack registry
- [ADR-031](./ADR-031-multi-engine-retrieval.md): runtime warm hook
- [ADR-096](./ADR-096-warm-daemon-per-request-identity.md): per-request daemon identity
