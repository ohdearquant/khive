# ADR-007: Namespace as Open String (Simplified for OSS)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A multi-tenant KG needs namespace isolation — Tenant A's queries don't see Tenant B's data. The
implementation needs to:

1. Be filterable at the storage layer (SQL queries can carry `WHERE namespace = ?`).
2. Be derivable from the request context (auth, project hierarchy, etc.).
3. Be cheap to compare and pass around.

A full-blown capability system (Actor / Principal / Capability tied to namespaces, with
cryptographic proofs and cross-tenant authorization) is what hosted multi-tenant SaaS deployments
need. For khive's primary use case — a researcher running it locally — that machinery is overkill.

## Decision

**`Namespace` is a `String` newtype with no capability machinery. Default is `"local"`.**

```rust
pub struct Namespace(String);

impl Namespace {
    pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
    pub fn local() -> Self { Self("local".to_string()) }
    pub fn as_str(&self) -> &str { &self.0 }
    pub fn is_child_of(&self, parent: &Namespace) -> bool {
        self.0.len() > parent.0.len()
            && self.0.starts_with(parent.as_str())
            && self.0.as_bytes().get(parent.0.len()) == Some(&b':')
    }
}

impl Default for Namespace {
    fn default() -> Self { Self::local() }
}
```

Hierarchical namespaces use `:` as the separator: `"local"`, `"local:project-alpha"`,
`"local:project-alpha:team-1"`.

## Rationale

### Why simplified

Users typically run khive on their own machine. There's no shared environment requiring strict
tenant isolation:

- Single user, single tenant (typically).
- Trust boundary is the user's own machine.
- Hierarchical organization is for UX (project / sub-project), not security.

A `String` with conventions covers this. A capability-based namespace model can be added later when
multi-tenant deployment becomes a real requirement (see Open Questions).

### Why default to `"local"` (not empty string)?

Empty namespace is ambiguous — does it mean "global" or "uninitialized"? `"local"` is explicit:
"this is local single-user data." If we add a hosted scenario later, namespaces like
`"tenant-abc123"` are clearly different from local.

### Why hierarchical via `:`?

For research KGs, users naturally want to organize by project: `"local:llm-research"`,
`"local:optical-flow"`, etc. The `:` convention:

- Allows simple prefix matching for "all projects under X."
- Is URI-like — familiar to users (`scheme:path`).
- Doesn't conflict with filesystem path characters or URL separators.

### Why an opaque String wrapper (not a parsed structure)?

The structure of namespace strings is a _convention_, not a contract. Different deployments may use
different conventions:

- `"local:project:team"` for hierarchical
- `"tenant-uuid"` for hosted
- `"workspace-name"` for single-flat

Forcing a parsed structure (e.g., `enum Namespace { Local, Project(String), ... }`) would lock users
into one convention. The String wrapper allows flexibility while still being type-safe.

### Why no capability/actor system?

Capabilities (Actor → Namespace bindings) make sense when:

- Multiple actors share a process.
- The actor identity needs cryptographic proof.
- Cross-namespace access is sometimes authorized.

None of these apply to a local research workflow. When hosted multi-tenant deployment becomes a real
requirement, the namespace type already hides its implementation — adding a capability model later
does not break callers.

## Alternatives Considered

| Alternative                                                                    | Pros                | Cons                                                   | Why rejected             |
| ------------------------------------------------------------------------------ | ------------------- | ------------------------------------------------------ | ------------------------ |
| Capability-based isolation (Actor / Principal / Capability with crypto proofs) | Type-safe isolation | Massive complexity for a single-user research workflow | Wrong tool at this scope |
| Drop namespaces entirely                                                       | Simplest possible   | No path to multi-project organization                  | Breaks hierarchical UX   |
| Parsed `enum Namespace`                                                        | Type-safe structure | Locks in one convention                                | Inflexibility            |
| UUID-based namespaces                                                          | Globally unique     | Human-unfriendly, no hierarchy                         | Bad UX                   |

## Consequences

### Positive

- Simple type that's cheap to pass around and compare.
- Users can choose their own naming conventions.
- Hierarchical organization via prefix matching.
- Easy to migrate to a more sophisticated system later (the String wrapper hides the
  implementation).

### Negative

- No compile-time enforcement of namespace format. Mitigated: validation at ingress (when accepting
  namespace from user input or API), not at the type level.
- Hierarchical traversal is string-based (less efficient than tree pointers). Mitigated: namespace
  hierarchies are shallow (1-3 levels typical), prefix matching is fast.

### Neutral

- Migration to typed namespaces (if ever needed) is a transparent change — namespace is already
  passed as a string parameter throughout the stack.

## Storage-Layer Design: Namespace as Caller-Supplied Parameter

This section records the Option B decision for how namespace is handled in `khive-storage` and
`khive-db`. It supersedes any earlier wording that implied "stores are namespace-scoped handles."

### Decision

**Stores are unscoped database connections. Namespace is a caller-supplied parameter, not a
store-level boundary.**

```rust
// Option A (rejected): namespace baked into the store handle
struct EntityStore { namespace: Namespace, pool: Pool }

// Option B (chosen): namespace is a call-site parameter
struct EntityStore { pool: Pool }
impl EntityStore {
    async fn query(&self, namespace: &str, ...) -> Vec<Entity> { ... }
    async fn get(&self, id: Uuid) -> Option<Entity> { ... }  // no namespace: UUID is global
}
```

### Rules

1. **Methods that operate on multiple records** (query, count, search, list) take `namespace` as an
   explicit parameter. The caller decides which namespace to query.

2. **Methods that operate on a single record by ID** (get, delete, upsert) do not take a namespace
   parameter. UUID v4 is globally unique across all namespaces — there is no ambiguity, and no
   namespace filter is needed.

3. **Records carry their own namespace.** `Entity.namespace`, `Note.namespace`, `Event.namespace` —
   the record's namespace field is authoritative. `upsert` writes the namespace stored in the record
   as-is; it does not override it from a store-level setting.

4. **Isolation is enforced at the service/runtime layer**, not the storage layer. The runtime
   (`khive-runtime`) ensures that MCP verbs only access namespaces the authenticated caller owns.
   Storage is a dumb persistence layer that executes what it is told.

5. **Exception — EventStore, GraphStore, and VectorStore**: some trait methods on these stores don't
   take per-call namespace (e.g., `count`, `delete`, `get_event`). These stores accept a default
   namespace at construction as a convenience fallback. This is not an enforcement boundary — it is a
   default that can be overridden via filter parameters for reads that need to span namespaces.

### Why not store-level scoping (Option A)?

| Problem                         | Detail                                                                                                                                                                            |
| ------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| False security                  | The store struct has no auth context and cannot make access decisions. Scoping it feels safe but doesn't enforce anything.                                                        |
| API inconsistency               | Some methods take namespace, others use `self.namespace` — callers have to remember which is which.                                                                               |
| Parameter-ignoring anti-pattern | When a method signature accepts a namespace parameter but the implementation ignores it in favor of `self.namespace`, implementors are confused and bugs are introduced silently. |
| Inflexibility                   | A store scoped to namespace X cannot serve a cross-namespace admin query without constructing a second store or bypassing the scope.                                              |

Store-level scoping is the wrong abstraction. Access control belongs to the layer that has the
authority context — the service/runtime — not the layer that has the database connection.

## Implementation

In `khive-types`:

```
crates/khive-types/src/namespace.rs
```

```rust
pub struct Namespace(String);
// + new, local, as_str, is_child_of, Display
```

In `khive-storage`:

- Store traits are unscoped database connections — no `namespace` field on the struct.
- Methods over multiple records accept `namespace: &str` as an explicit caller-supplied parameter.
- Methods over a single record by ID (get, delete, upsert) do not take a namespace parameter.
- No namespace validation at the storage layer.

In `khive-db`:

- SQL queries for multi-record operations include `WHERE namespace = ?` with the caller-supplied
  value.
- SQL queries for single-record operations use `WHERE id = ?` only.

In services (when ported):

- Validate namespace format at ingress (e.g., regex, length limits) before passing to storage.
- Derive namespace from auth context for multi-tenant scenarios.
- Enforce that the authenticated caller's namespace matches the requested namespace before calling
  storage.

## References

- ADR-003: Four-Layer Architecture (namespace flows top-down through layers)
- ADR-004: Substrate Observables (every observable carries a namespace field)
- `crates/khive-types/src/namespace.rs`: implementation
