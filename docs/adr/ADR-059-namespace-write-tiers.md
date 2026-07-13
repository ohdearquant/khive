# ADR-059: Namespace Write Tiers and Cross-Namespace Link Access Control

**Status**: Withdrawn — superseded before acceptance by ADR-007 Rev 2 (2026-06-16).

This ADR introduced a three-tier visibility model (shared / private / proposal-only) and
associated per-namespace write enforcement. ADR-007 Rev 2 establishes that namespace is
attribution-only and that all packs store in the single shared "local" namespace; actor
identity is a view-layer tag filter, never a namespace partition. The write-tier model
re-introduced the actor-as-namespace coupling that the "one shared brain" decision removed, making the core mechanism of this ADR incompatible with the ratified design.
The subagent propose-only tier, which is operationally separable, may be proposed as a future
ADR scoped to the propose/review verb lifecycle (ADR-046) rather than to namespace scoping.
The ADR number 059 is burned — this file is preserved as a record of the withdrawn design.

**Date**: 2026-06-15
**Authors**: khive maintainers
**Withdrawn**: 2026-06-16 (superseded by ADR-007 Rev 2)
**Depends on**:

- ADR-007 (Namespace contract — attribution-only, attribution vs isolation axes; now Rev 3)
- ADR-017 (Pack standard — pack-extensible edge endpoints)
- ADR-018 (Authorization gate — single dispatch-site enforcement)
- ADR-046 (Event-sourced proposals — the mutation path for subagents)
- ADR-050 (KG token namespace contract — pack honors token without override)
- ADR-053 (Authorization gate actor store — ActorRef.kind precedent)
- ADR-057 (Comm actor-addressed delivery — actor-label vs namespace-string distinction)

**Related issues**: #75 (actor identity on every request), #57 (cross-namespace comm), #13 (AllowAll gate)

---

## Context

Three lambda orchestrators (lambda:beta, lambda:alpha, lambda:gamma) and an arbitrary number
of ephemeral subagents share one SQLite knowledge graph via the khive MCP server. All sessions
currently run as namespace `"local"` -- the fall-back when no `[actor] id` is configured. This
is a deliberate stop-gap: a prior attempt to give each lambda a distinct namespace identifier
in `khive.toml` broke all cross-read access because `ensure_namespace` enforces strict
primary-namespace equality on write-guarded paths and the data corpus (approximately 12,380
notes, 2,744 entities) lives entirely in `"local"`.

Design intent (2026-06-15):

- All lambdas may read and write to a **shared** cooperative KG namespace.
- All lambdas may also maintain a **private** namespace for their own working state.
- Subagents (ephemeral task-runners spawned by lambdas) may read from visible namespaces but
  must not directly mutate; they submit proposals via the ADR-046 lifecycle which a lambda
  approves or rejects.
- An edge whose endpoints span namespaces is a **cross-namespace link**. The edge itself is
  stored in the creator's namespace and the visibility filter must check all three namespace
  columns (edge, source endpoint, target endpoint) to avoid leaks.

Review on 2026-06-14 produced three binding corrections incorporated
here:

1. Edge namespace is stored on the edge (scheme B1), not derived from endpoint namespaces.
   The visibility filter MUST check all three namespace columns. Checking only the two
   endpoint columns would make a private edge between two shared-namespace nodes globally
   visible -- a real leak.
2. A link whose creator cannot reach both endpoints is rejected at link time. The error code
   is `NotFound`, not `Forbidden`. `Forbidden` is a UUID-existence oracle and violates the
   fail-closed rule.
3. The legacy `"local"` namespace must map to a private `lambda:alpha` namespace plus a fresh
   empty shared namespace -- not become the shared namespace. Making `"local"` the shared
   namespace would expose operator working memory (tasks, episodic memories, session notes) to
   every cooperating agent, which is a privacy leak and pollutes a clean demo environment.

### What this ADR does and does not guarantee

This ADR defines a **visibility boundary for cooperating agents running on the same SQLite
database**. Agents that honor the token contract cannot access records outside their visible
set. This is sufficient for the multi-lambda cooperative use case in a single-machine
deployment.

This ADR does NOT establish a **security boundary against a hostile local actor**. Any process
with filesystem access to the SQLite file can read or write records directly. Authentication
against an independent authority, row-level encryption, and mTLS caller propagation are
multi-actor isolation concerns addressed in ADR-053 and its successors. The phrase "access control" in
this document always refers to the visibility-boundary sense, never to hardened security.

---

## Decision

### 1. Namespace Tiers

Two tiers are defined. A single deployment always has exactly one shared namespace and zero or
more private namespaces.

| Tier                 | Identifier convention | Owner                     |
| -------------------- | --------------------- | ------------------------- |
| Shared               | `shared` (default)    | All participating lambdas |
| Private (per-lambda) | `lambda:<name>`       | The named lambda only     |

The shared namespace is the cooperative KG: research entities, cross-project concepts, edges
linking them. Private namespaces hold working memory, session notes, task queues, and episodic
memories that belong to one lambda.

No other tiers exist in this ADR. A subagent does not own a namespace; it borrows its
sponsoring lambda's token with a visibility set appropriate to its task.

### 2. Actor Kinds

Two actor kinds are recognized for namespace write-tier enforcement. They map directly to the
`ActorRef.kind` field already defined in `khive-gate/src/actor.rs`.

| Kind     | `ActorRef.kind` value | Who                                         |
| -------- | --------------------- | ------------------------------------------- |
| Lambda   | `"lambda"`            | Standing orchestrators (alpha, beta, gamma) |
| Subagent | `"subagent"`          | Ephemeral task-runners spawned by a lambda  |

The `"anonymous"` kind (the current default) is treated as a lambda for backward
compatibility: unauthenticated local sessions retain full write access. A future ADR may
tighten this once `[actor] kind` is declared in `khive.toml`.

### 3. Permission Matrix

| Action                           |        Lambda (shared ns)        | Lambda (own private ns) | Lambda (other private ns) | Subagent (shared ns) | Subagent (own private ns) |
| -------------------------------- | :------------------------------: | :---------------------: | :-----------------------: | :------------------: | :-----------------------: |
| Read (get / list / search)       |               Yes                |           Yes           | If in visible_namespaces  |         Yes          |            Yes            |
| Write (create / update / delete) |               Yes                |           Yes           |            No             |  No -- propose only  |    No -- propose only     |
| Link (cross-namespace edge)      | Yes, if both endpoints reachable |           Yes           |            No             |  No -- propose only  |    No -- propose only     |
| Propose (ADR-046)                |               Yes                |           Yes           |            No             |         Yes          |            Yes            |
| Review / apply proposal          |         Yes (any lambda)         |           Yes           |            No             |          No          |            No             |

"Propose only" for subagents means: the verb `propose` succeeds; the verbs `create`, `update`,
`delete`, `link`, and `annotates` return `NotFound` (fail-closed, no existence oracle).

### 4. Token Shape Changes

#### 4.1 ActorConfig additions (engine_config.rs)

Two new fields are added to `ActorConfig` in `khive-runtime/src/engine_config.rs`:

```toml
[actor]
id           = "lambda:alpha"        # primary write namespace (existing)
kind         = "lambda"              # NEW: "lambda" | "subagent" | "anonymous"
visible_namespaces  = ["shared"]     # existing: readable beyond primary
writable_namespaces = ["shared"]     # NEW: namespaces this actor may write to
                                     #       beyond its own primary namespace
```

`kind` defaults to `"anonymous"` when absent, preserving backward compatibility.
`writable_namespaces` defaults to `[]` when absent, meaning only the primary namespace is
writable. This preserves the current behavior for all existing configurations.

#### 4.2 NamespaceToken additions (config.rs)

`NamespaceToken` gains a `writable` field parallel to `visible`:

```rust
pub struct NamespaceToken {
    namespace: Namespace,          // primary write namespace (existing)
    visible:   Vec<Namespace>,     // read visibility set (existing)
    writable:  Vec<Namespace>,     // NEW: full write-authorized namespaces
    actor:     ActorRef,           // (existing, already carries .kind)
    _sealed:   private::Sealed,
}
```

Construction invariant: the primary `namespace` is always in both `visible` and `writable`.
`mint_with_visibility` is extended to accept `extra_writable: Vec<Namespace>` alongside
`extra_visible`.

#### 4.3 ActorRef.kind propagation

`ActorConfig.kind` is resolved at token-mint time and stored in `ActorRef.kind` within the
token. The runtime reads `token.actor().kind` to enforce propose-only for subagents. The
field already exists (`khive-gate/src/actor.rs:14`); this ADR defines its values and
enforcement semantics.

### 5. Write Enforcement

#### 5.1 New check: `ensure_namespace_writable`

A new helper is added to `KhiveRuntime` in `operations.rs`, parallel to `ensure_namespace_visible`:

```rust
pub(crate) fn ensure_namespace_writable(
    record_ns: &str,
    token: &NamespaceToken,
) -> RuntimeResult<()> {
    if token.actor().kind == "subagent" {
        return Err(RuntimeError::NotFound("not found in this namespace".into()));
    }
    for ns in token.writable_namespaces() {
        if record_ns == ns.as_str() {
            return Ok(());
        }
    }
    Err(RuntimeError::NotFound("not found in this namespace".into()))
}
```

The error is `NotFound` in all cases, not `PermissionDenied`, to preserve the
fail-closed no-existence-oracle rule.

#### 5.2 Call site changes

The existing `ensure_namespace` (strict primary-equality check) at write paths is replaced
with `ensure_namespace_writable`. This affects the following call sites in `operations.rs`:

| Line (approx.) | Operation                       | Change                                            |
| -------------- | ------------------------------- | ------------------------------------------------- |
| 2272--2286     | `get_by_ids` entity/note/event  | No change (read path, uses `ensure_namespace`)    |
| 2310--2322     | `hard_delete` entity/note/event | `ensure_namespace` -> `ensure_namespace_writable` |
| 2358           | `complete_note`                 | `ensure_namespace` -> `ensure_namespace_writable` |
| 2514           | `update_entity`                 | `ensure_namespace` -> `ensure_namespace_writable` |
| 2598           | `update_note`                   | `ensure_namespace` -> `ensure_namespace_writable` |
| 2660           | `delete_entity`                 | `ensure_namespace` -> `ensure_namespace_writable` |

`create_entity` and `create_note` do not call `ensure_namespace` today; they stamp
`token.namespace().as_str()` directly onto new records. After this ADR, the target
namespace for a create is chosen from `token.namespace()` (primary write namespace) -- no
change needed. If a lambda wishes to create a record in the shared namespace, it sets its
primary namespace to shared, or uses an explicit `namespace=` parameter validated against
`ensure_namespace_writable`.

#### 5.3 Subagent propose routing

When a subagent calls `create`, `update`, `delete`, or `link`, `ensure_namespace_writable`
returns `NotFound` immediately. The verb handler returns `{ok: false, error: "..."}` without
reaching storage. The subagent must use `propose(title, description, changeset)` (ADR-046)
instead.

A lambda reviewing a proposal applies the changeset through its own token, which passes
`ensure_namespace_writable`. The apply path in the proposal worker is unchanged.

### 6. Cross-Namespace Links (Scheme B1)

#### 6.1 Edge namespace storage

An edge is stored with its own `namespace` column equal to the creator's primary write
namespace at link time. This is scheme B1 (mirror decision, 2026-06-14: confirmed over
scheme B2 where edge namespace is derived from endpoints).

No change to the edge schema is required; the `namespace` column already exists on the
`edges` table.

#### 6.2 Three-column visibility filter

The existing `list_edges`, `neighbors`, and `traverse` operations filter edges by namespace.
After this ADR, the filter condition is:

```sql
edge.namespace IN (visible_set)
AND source_entity.namespace IN (visible_set)
AND target_entity.namespace IN (visible_set)
```

All three namespace columns must be in the caller's visible set. Checking only the two
endpoint columns would make a private edge between two shared nodes globally visible to any
caller who can see the shared namespace -- a real leak. Checking only the edge column would
make an edge invisible even when both endpoints are reachable (unnecessary opacity).

This is an additive change to the query filter. The ADR-002 endpoint kind contract
(which `(source_kind, relation, target_kind)` triples are legal) is unaffected. Namespace
scope and entity-kind scope are orthogonal validation axes.

#### 6.3 Link-time cross-namespace rejection

Before creating an edge, the `link` verb handler calls `ensure_namespace_visible` for both
the source and target entity. If either check fails, the edge creation is aborted with
`NotFound` for that entity. The response is indistinguishable from the entity not existing.

```rust
// Pseudocode -- implementation lives in khive-pack-kg/src/handlers.rs
Self::ensure_namespace_visible(&source_entity.namespace, token)?;
Self::ensure_namespace_visible(&target_entity.namespace, token)?;
// Only after both checks pass:
self.runtime.create_edge(token, ...).await
```

The error is `NotFound`, not `Forbidden`. Returning `Forbidden` would confirm that the UUID
exists in a namespace the caller cannot reach (existence oracle).

Additionally, `ensure_namespace_writable` is checked against `edge.namespace` (the creator's
primary namespace) before any storage write, consistent with all other write paths.

### 7. GraphStore Trait Scope

The current `GraphStore` trait methods accept a single `namespace: &str` parameter for
list and search operations. To support multi-namespace visible sets, these signatures must
accept `namespaces: &[&str]` (or an equivalent `IN`-clause parameter). This is a
breaking change to the trait contract and requires updates to all `GraphStore`
implementations.

This change is a cost callout (identified 2026-06-14) and is tracked in a
separate implementation ticket. The ADR specifies the required end state; the migration path
is implementation-owned.

### 8. ANN Index Multi-Namespace Gather

The Vamana ANN index (`khive-vamana`) is keyed per-namespace. Shared and private namespaces
have separate index files. A search that spans multiple visible namespaces must gather results
from each index separately and apply RRF fusion to produce a unified ranked list.

This is a cost callout. The current single-index recall path in `operations.rs:1947-2011` must
be extended to iterate over visible namespaces, query each index, and fuse results. The
`memory.recall` verb is the primary consumer.

### 9. Migration of the Legacy `"local"` Namespace

Existing data in the `"local"` namespace must not be deleted or moved. The migration is a
namespace relabel via SQL UPDATE, applied as a numbered `VersionedMigration` (ADR-015).

The relabel policy:

| Record kind                                                                | Relabel target               | Rationale                                          |
| -------------------------------------------------------------------------- | ---------------------------- | -------------------------------------------------- |
| `memory` notes (episodic / semantic)                                       | `lambda:alpha` (private)     | Operator working memory; not for subagents to read |
| `task` notes                                                               | `lambda:alpha` (private)     | lambda:alpha task queue                            |
| `message` notes                                                            | `lambda:alpha` (private)     | Comm pack messages are actor-addressed internally  |
| `scheduled_event` notes                                                    | `lambda:alpha` (private)     | Schedule pack intent is per-actor                  |
| All other note kinds (observation, insight, question, decision, reference) | OPEN FORK F1                 | See Forks                                          |
| All entities (concept, document, project, etc.)                            | OPEN FORK F1                 | See Forks                                          |
| All edges                                                                  | Follow source entity relabel | Edge namespace = creator's namespace               |

The fresh shared namespace (`shared` by default, configurable) begins empty. Lambdas
populate it intentionally over time by creating records with their primary namespace set to
`shared`, or by using the `namespace=` explicit argument on supported verbs.

The `"local"` identifier is not retired. Any existing config or process that passes
`namespace="local"` will target a now-private-but-renamed namespace. Once all active sessions
are reconfigured to use `lambda:alpha` or `shared`, `"local"` becomes an orphan and can be
aliased in config.

**Data preservation invariant**: no record UUID changes. No edge is deleted. The migration is
UPDATE-only. The data-vs-view principle (CLAUDE.md) applies: changing which namespace a query
returns is a view-layer decision; the migration changes the stored namespace string to align
the data model with the new tier design, not to alter query semantics.

The live SQLite file must never be deleted or `rm`'d. The migration runs through the
VersionedMigration system; adding a version `N+1` pointing at a new `.sql` file that applies
the UPDATE statements.

### 10. Configuration Examples

After this ADR, a typical `khive.toml` for each actor:

```toml
# kkernel serving as lambda:alpha
[actor]
id                  = "lambda:alpha"
kind                = "lambda"
visible_namespaces  = ["shared"]
writable_namespaces = ["shared"]
```

```toml
# kkernel serving as lambda:beta
[actor]
id                  = "lambda:beta"
kind                = "lambda"
visible_namespaces  = ["shared", "lambda:alpha"]   # can read alpha's private ns
writable_namespaces = ["shared"]                   # writes to shared only
```

```toml
# Subagent spawned for a task (token minted by its sponsor lambda, not from config)
# ActorConfig is not used; token is minted by the runtime with kind="subagent"
```

---

## Alternatives Considered

### A1. Keep all lambdas in `"local"`, differentiate only by comm actor label

ADR-057 showed that actor-addressed delivery within a shared namespace is sufficient for comm
routing without per-lambda namespaces. The same argument could be extended to the KG: all
lambdas share `"local"`, and propose-only for subagents is enforced by a config flag rather
than namespace separation.

Rejected because: (a) there is no write isolation between lambdas -- a misbehaving lambda
can overwrite another's private notes; (b) the design requirement is per-lambda
private namespaces for working state; (c) without namespace separation, cross-namespace link
semantics have no surface to operate on.

### A2. Subagent propose-only enforced at token-mint only, not per-verb

The sponsoring lambda mints a token for a subagent that lacks any writable namespace. The
subagent then receives `NotFound` on every write attempt without any additional per-verb check.

This is architecturally cleaner and avoids adding `actor.kind` checks inside verb handlers.
The issue is that `with_namespace` (used today by comm and memory pack fanout) can upgrade a
subagent's token to write-capable in a different namespace. Per-verb enforcement adds a
defense-in-depth layer. Both mechanisms should be present.

This ADR specifies both: token-level writable set (primary enforcement) and actor.kind check
in `ensure_namespace_writable` (defense-in-depth). The two are redundant for an honest
runtime and additive for a misconfigured one.

### A3. Make `"local"` the shared namespace

All existing data becomes shared immediately. No migration required.

Rejected (2026-06-14): lambda:alpha's working memory (tasks, episodic
notes, session state) is not appropriate shared content. Dumping it into the shared namespace
pollutes the cooperative KG and exposes operator context to every cooperating agent.

### A4. Database-level enforcement (per-row ownership column, SQLite ATTACH)

Add a `owner_kind` column to every substrate table. Storage queries enforce ownership. Or use
SQLite ATTACH to give each namespace a separate file with filesystem-level permissions.

Rejected: this requires schema changes to all three substrate tables (entities,
notes, edges), adds storage-layer coupling, and still provides no hardened security against
a process with filesystem access. The visibility-boundary model is sufficient for cooperative
agents and does not need storage-layer enforcement in single-actor deployments.
Isolation in multi-actor deployments uses per-actor database files
(ADR-028 routing) rather than row-level ownership.

---

## Consequences

### Benefits

- Lambda orchestrators can maintain private working memory that subagents and other lambdas
  cannot overwrite.
- The cooperative KG (shared namespace) is populated intentionally, not as a side effect of
  all-in-`"local"` operation.
- Subagent proposals are auditable and reversible via the ADR-046 lifecycle; direct mutations
  from ephemeral agents are eliminated.
- Cross-namespace edges are governed: the three-column filter prevents edge-induced namespace
  leaks.
- The legacy `"local"` corpus is preserved without UUID changes; migration is a relabel.

### Costs and Risks

- **GraphStore trait breaking change**: accepting `namespaces: &[&str]` instead of a single
  namespace string is a breaking change to all GraphStore implementations. This is the largest
  implementation cost.
- **ANN multi-index gather**: recall across shared and private namespaces requires fusing
  results from two Vamana index files. The current single-index path in `operations.rs`
  must be extended.
- **Migration SQL**: UPDATE-ing namespace strings on approximately 15,000 records requires
  careful transaction design and index rebuild after the migration.
- **Config rollout order**: the shared namespace must be empty when lambdas start writing to
  it; all three lambda configs must be updated in a single coordinated deployment.
- **Backward compatibility**: any existing test or integration that creates records with
  `namespace="local"` and reads them back will continue to work (local is still valid); tests
  that explicitly verify namespace values will need updating after migration.

---

## Open Questions

See the local namespace-write-tiers fork list for the full set of options and decision
guidance. The questions that require maintainer judgment before code lands are:

- **F1**: Which existing `"local"` note kinds and entities map to shared vs private?
- **F2**: How is the shared namespace identifier chosen and where is it declared?
- **F3**: Is subagent propose-only enforced at token-mint time, at per-verb time, or both?
- **F4**: Does the migration run automatically at server start or require an explicit operator
  command?

---

## Implementation Notes (non-normative)

No implementation is prescribed in this ADR. The following notes are for the implementer
agent when this ADR is approved.

Files that change:

| File                                        | Change                                                                            |
| ------------------------------------------- | --------------------------------------------------------------------------------- |
| `crates/khive-runtime/src/engine_config.rs` | Add `kind`, `writable_namespaces` to `ActorConfig`                                |
| `crates/khive-runtime/src/config.rs`        | Add `writable: Vec<Namespace>` to `NamespaceToken`; extend `mint_with_visibility` |
| `crates/khive-runtime/src/operations.rs`    | Add `ensure_namespace_writable`; update 6 write-path call sites                   |
| `crates/khive-runtime/src/runtime.rs`       | Thread `writable_namespaces` from config into token mint                          |
| `crates/khive-pack-kg/src/handlers.rs`      | Three-column visibility filter in link handler; cross-ns endpoint check           |
| `crates/khive-db/sql/`                      | New numbered migration SQL for `"local"` namespace relabel                        |
| `crates/khive-db/src/migrations.rs`         | Register new `VersionedMigration`                                                 |
| `crates/khive-storage/src/`                 | Update `GraphStore` trait signatures to accept namespace slice                    |
| All `GraphStore` impls                      | Update to match new trait signature                                               |
