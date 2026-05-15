# ADR-007: Namespace as Open String (Simplified for OSS)

**Status**: accepted\
**Date**: 2026-05-15\
**Authors**: Ocean, lambda:khive

## Context

A multi-tenant KG needs namespace isolation — Tenant A's queries don't see Tenant B's data. The
implementation needs to:

1. Be enforceable at the storage layer (every SQL query carries `WHERE namespace = ?`).
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
        self.0.starts_with(&format!("{}/", parent.0))
    }
}

impl Default for Namespace {
    fn default() -> Self { Self::local() }
}
```

Hierarchical namespaces use `/` as the separator: `"local"`, `"local/project-alpha"`,
`"local/project-alpha/team-1"`.

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

### Why hierarchical via `/`?

For research KGs, users naturally want to organize by project: `"local/llm-research"`,
`"local/optical-flow"`, etc. The `/` convention:

- Allows simple prefix matching for "all projects under X."
- Is filesystem-like — familiar to users.
- Doesn't conflict with any existing identifier characters.

### Why an opaque String wrapper (not a parsed structure)?

The structure of namespace strings is a _convention_, not a contract. Different deployments may use
different conventions:

- `"local/project/team"` for hierarchical
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

- Migration to typed namespaces (if ever needed) is a transparent change at the storage layer —
  every SQL query already uses `WHERE namespace = ?` as a string parameter.

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

- All store traits accept `&str` for namespace parameters.
- All SQL queries include `WHERE namespace = ?`.
- No namespace validation at the storage layer.

In services (when ported):

- Validate namespace format at ingress (e.g., regex, length limits) before passing to storage.
- Derive namespace from auth context for multi-tenant scenarios.

## References

- ADR-003: Four-Layer Architecture (namespace flows top-down through layers)
- ADR-004: Substrate Observables (every observable is namespace-scoped)
- `crates/khive-types/src/namespace.rs`: implementation
