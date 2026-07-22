# ADR-068: Local Process and Database Ownership

**Status**: Proposed\
**Date**: 2026-06-23\
**Authors**: khive maintainers\
**Depends on**: [ADR-007](./ADR-007-namespace.md),
[ADR-009](./ADR-009-backend-architecture.md),
[ADR-028](./ADR-028-pack-scoped-backends.md),
[ADR-049](./ADR-049-khived-daemon.md)

---

## Context

The public runtime uses SQLite for local persistence. SQLite write serialization, WAL state,
checkpointing, and connection ownership are scoped to a database file. Clear ownership is needed
to avoid concurrent daemons independently managing the same WAL and derived indexes.

Namespaces remain logical attribution and query labels. They do not replace operating-system file
permissions or the database ownership rule defined here.

## Decision

### 1. One local daemon owns one configured database

A running daemon has exclusive write ownership of its configured SQLite database and associated
derived-index files. Other clients send requests to that daemon instead of opening the same files
for writes.

The command-line path may open the database directly only when no daemon owns it. Startup must fail
clearly when the ownership lock cannot be acquired.

### 2. Ownership covers sidecar state

The same owner manages:

- the SQLite database, WAL, and shared-memory files;
- ANN segment files derived from that database;
- temporary build files and atomic replacement markers;
- checkpoint and recovery operations.

Sidecar paths are resolved from the configured backend data directory. They must not be placed in a
global shared directory unrelated to that backend.

### 3. Startup is fail-closed

Before serving requests, the process must:

1. resolve the database path to a stable absolute path;
2. create or validate the parent directory;
3. acquire the ownership lock;
4. open the database and run migrations;
5. recover or discard incomplete derived-index builds;
6. start request handling only after those checks succeed.

A failure at any step releases acquired resources and does not expose a partially initialized
server.

### 4. Shutdown preserves recoverability

Graceful shutdown stops accepting new requests, waits for in-flight writes, completes or aborts
derived-index publication, performs the configured checkpoint action, closes the database, and
releases the ownership lock.

Crash recovery relies on SQLite WAL recovery and atomic publication of derived indexes. An
incomplete derived index is rebuilt; it is never treated as authoritative data.

### 5. Network isolation is not specified here

This ADR defines local file and process ownership only. It does not define a shared server, remote
authentication protocol, row-level security model, or resource-accounting service. Any future
network-accessible topology requires a separate security and operations specification.

## Invariants

1. At most one daemon writes a configured database at a time.
2. Direct CLI writes do not run concurrently with a daemon owner.
3. Derived-index state is owned and recovered with its source database.
4. Namespace labels are not presented as physical security boundaries.
5. A partially initialized process never accepts requests.

## Consequences

### Positive

- SQLite and WAL ownership are unambiguous.
- Local clients share one warm runtime and one connection pool.
- Derived indexes cannot be published independently of their source database owner.
- The scope of the public topology is explicit.

### Tradeoffs

- Multiple local databases require separate daemon processes.
- Supervisors must manage one lock and lifecycle per configured database.
- Direct maintenance commands need a daemon RPC path or an explicit offline window.

## Testing requirements

- A second daemon cannot acquire ownership of an active database.
- Direct-write commands fail while the daemon holds the lock.
- Startup failure releases the ownership lock.
- Crash recovery never publishes an incomplete derived index.
- Graceful shutdown drains writes before closing the database.

## References

- [ADR-007](./ADR-007-namespace.md): logical namespace semantics
- [ADR-009](./ADR-009-backend-architecture.md): storage boundary
- [ADR-028](./ADR-028-pack-scoped-backends.md): backend assignment
- [ADR-049](./ADR-049-khived-daemon.md): daemon lifecycle
