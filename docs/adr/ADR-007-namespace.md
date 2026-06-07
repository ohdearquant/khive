# ADR-007: Namespace

**Status**: accepted\
**Date**: 2026-05-23\
**Authors**: Ocean, lambda:khive

## Context

Namespace is khive's logical isolation primitive. Every entity, note, event, and edge record
carries a namespace. Queries are namespace-scoped. In a hosted deployment, namespace isolation
failure is a data breach.

The isolation model must satisfy:

1. **OSS simplicity.** Single-user local deployment should work with zero configuration. No
   tenant IDs, no auth tokens, no isolation layers to understand.
2. **Cloud correctness.** In hosted multi-tenant deployment, one tenant's data must be
   unreachable from another tenant's context. Accidental namespace fallback must be impossible
   to express.
3. **Federation safety.** A single verb may fan out to multiple backends (ADR-029, Substrate Coordinator).
   Namespace enforcement must propagate through every backend call, not just the entry point.
4. **Type-level enforcement.** Convention fails; types hold. The design should make isolation
   breaks impossible to express in Rust, not merely documented as forbidden.
5. **Wire compatibility.** Namespace is stored as a TEXT column in SQLite. Changes to the
   `Namespace` representation must not require database migrations for existing deployments.

## Decision

### Opaque newtype with validated factories

`Namespace` is a string-backed newtype with no public unchecked constructor. Callers
construct namespaces through validated factories.

```rust
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct Namespace(String);

impl Namespace {
    pub const LOCAL: &'static str = "local";

    pub fn parse(value: &str) -> Result<Self, NamespaceError> {
        validate_namespace(value)?;
        Ok(Self(String::from(value)))
    }

    pub fn local() -> Self {
        Self(String::from(Self::LOCAL))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_inner(self) -> String {
        self.0
    }
}

impl TryFrom<String> for Namespace { /* validates through Namespace::parse */ }
impl TryFrom<&str> for Namespace { /* validates through Namespace::parse */ }
```

`Namespace` is a string-backed newtype with no public unchecked constructor. The
shipped public construction surface is `Namespace::parse(&str)`, `Namespace::local()`,
`TryFrom<String>`, and `TryFrom<&str>`. Earlier `project(slug)`, `tenant(id)`,
`system(name)`, and `from_trusted_unchecked` examples are deferred sketches, not
accepted API. Hosted deployments derive namespace strings from authenticated
context, parse them, and mint `NamespaceToken` through runtime/gate dispatch;
caller input never receives an unchecked namespace factory.

### No `Default`

`Namespace` does not implement `Default`.

Single-user OSS deployments obtain `Namespace::local()` from runtime configuration, not
from the namespace type itself:

```rust
pub struct RuntimeConfig {
    pub default_namespace: Namespace,
}

impl RuntimeConfig {
    pub fn local_dev() -> Self {
        Self {
            default_namespace: Namespace::local(),
        }
    }
}
```

Hosted deployments must mint namespaces from authenticated tenant context before verb
dispatch. If `Default` existed, a misconfigured cloud tenant could accidentally fall into
`"local"` and access other `"local"`-namespaced data.

### Structural validation

Structural invariants are enforced at `Namespace` construction time. A valid `Namespace`
value is definitionally well-formed.

```text
Structural validation (at construction):
- non-empty
- length-bounded (max 256 characters)
- valid character set (alphanumeric, '-', '_', ':', '.')
- no trailing separator
- no empty path segments ("local::project" is invalid)
- tenant namespace contains valid UUID
- reserved prefixes ("system:", "tenant:") controlled by factories
```

### `NamespaceToken`: type-level authorization proof

`NamespaceToken` is a non-forgeable proof that structural validation and semantic
authorization have both occurred. It is minted only by the auth/runtime layer.

```rust
pub struct NamespaceToken {
    namespace: Namespace,
    principal: PrincipalId,
    grants: NamespaceGrants,
    _sealed: private::Sealed,
}

mod private {
    pub struct Sealed;
}

impl NamespaceToken {
    pub(crate) fn mint(
        auth: &AuthContext,
        namespace: Namespace,
        requested: NamespaceAccess,
    ) -> Result<Self, AuthError> {
        auth.authorize_namespace(&namespace, requested)?;
        Ok(Self {
            namespace,
            principal: auth.principal_id(),
            grants: auth.grants_for(&namespace),
            _sealed: private::Sealed,
        })
    }

    pub fn namespace(&self) -> &Namespace {
        &self.namespace
    }
}
```

Semantic authorization (at token minting):

```text
- namespace exists in tenant registry (cloud) or is well-formed (OSS)
- principal owns or has been granted access to namespace
- requested access mode (read / write / admin) is permitted
- cross-namespace access grant exists (if requesting foreign namespace)
```

A `Namespace` value proves structural well-formedness. A `NamespaceToken` proves
authorization for a principal and access mode.

### Runtime enforcement via `NamespaceView`

Agent/user code never receives the raw coordinator or raw storage. It receives a
`NamespaceView`, created from a `NamespaceToken`:

```rust
pub struct NamespaceView<'a> {
    coordinator: &'a SubstrateCoordinator,
    token: NamespaceToken,
}

impl<'a> NamespaceView<'a> {
    pub async fn search(&self, req: SearchRequest) -> Result<SearchResult, RuntimeError> {
        self.coordinator.search(&self.token, req).await
    }

    pub async fn get_entity(&self, id: Uuid) -> Result<Entity, RuntimeError> {
        self.coordinator.get_entity(&self.token, id).await
    }
}
```

All runtime methods that read/write namespace-scoped records require `NamespaceToken`.
In the shipped runtime, `VerbRegistry::dispatch` resolves and validates the operation
namespace, consults the gate, mints `NamespaceToken` at the dispatch boundary, strips
the transport `namespace` field before ordinary pack handlers, and passes
`&NamespaceToken` into pack code. Single-record ID operations still require token
verification after fetching by UUID.

First-party packs may apply documented namespace policy by rebinding a token with
`NamespaceToken::with_namespace`, for example KG graph verbs using the shared default
namespace under the Namespace-by-Layer rule. This is trusted pack policy, not a
caller-visible namespace factory.

The enforcement pattern:

```rust
// Write path
if entity.namespace != *token.namespace() {
    return Err(RuntimeError::NamespaceMismatch);
}

// Read-by-ID path
let record = storage.get(id).await?;
if record.namespace != *token.namespace() {
    return Err(RuntimeError::NamespaceDenied);
}
```

Physical stores remain unscoped persistence connections. They execute what they are told.
Enforcement happens in the runtime/coordinator layer, not at the storage level.

**Timing oracle mitigation**: Error responses for "UUID exists but wrong namespace" and
"UUID does not exist" MUST be identical in type, message, and observable timing. Both
return `RuntimeError::NotFound` with the message "not found in this namespace" — no
indication of whether the record exists in another namespace. This prevents UUID
enumeration attacks against foreign namespaces.

### Namespace vs backend: independent axes

Namespace and backend are independent isolation dimensions.

```text
Namespace answers: "Which principal may access this record?"
Backend answers: "Where is this record physically stored and what operational policy applies?"
```

A namespace may span multiple backends. A backend may contain multiple namespaces.

Authorization is always evaluated against namespace, not backend name. The coordinator
composes both axes during fan-out: it routes to the correct backends AND enforces namespace
filtering on every backend call.

Neither dimension subsumes the other. Namespace isolation is necessary but not sufficient
for full isolation — a record in the correct namespace on the wrong backend is a routing
bug. A record on the correct backend in the wrong namespace is a security bug.

### Hierarchy helper: naming utility, not authorization

The shipped namespace module exposes `has_segment_prefix(child, parent)` as a public
helper for naming-convention checks. It is not a semantic guarantee and MUST NOT be
used for authorization. Authorization decisions use `NamespaceToken` and gate policy.

```rust
// khive-types/src/namespace.rs — naming-convention helper, NOT authorization
pub fn has_segment_prefix(child: &Namespace, parent: &Namespace) -> bool {
    let c = child.as_str();
    let p = parent.as_str();
    c.len() > p.len()
        && c.starts_with(p)
        && c.as_bytes().get(p.len()) == Some(&b':')
}
```

Authorization decisions use `NamespaceToken`, not string-prefix checks.

### Pack namespace policy: out of scope

ADR-007 defines the namespace primitive and enforcement model. Pack-specific namespace
behavior (memory pack scoped to agent namespace, lore pack as global read-only) belongs
in ADR-017 (Pack Standard) or a future pack capability ADR.

Any pack namespace policy must compile down to `NamespaceToken` / `NamespaceView`
permissions. Packs must not bypass namespace enforcement or construct namespaces from
raw strings.

## Rationale

### Why no public constructor?

`Namespace::new(arbitrary_string)` allows typos (`""`, `"local:"`, `"tenant:not-a-uuid"`)
and namespace guessing attacks in hosted deployments. Factories enforce invariants at
construction time. Every call site that currently passes an arbitrary string must go through
validation — this is good breakage because it identifies every unvalidated namespace entry
point.

### Why no Default?

`Default` produces `Namespace::local()`. In a cloud deployment, any code path that reaches
`Default::default()` without going through auth falls into the `"local"` namespace. A
misconfigured cloud tenant reading `"local"` data is a data breach. Moving the default to
runtime configuration makes the OSS path explicit (`RuntimeConfig::local_dev()`) and the
cloud path impossible to accidentally bypass.

### Why NamespaceToken (not just ingress validation)?

Ingress validation (option A from the question file) is a single chokepoint at verb
dispatch. Any code path that skips verb dispatch — internal maintenance, background tasks,
admin operations, future hot paths — can access any namespace without validation. A
token makes bypass impossible to express in the type system: if you don't have a
`NamespaceToken`, you cannot call namespace-scoped operations.

### Why independent axes (not namespace-primary)?

Backend is not merely an implementation detail once federation exists. A namespace can span
`main.db` and `archive.db`. A backend can hold namespaces from different tenants. The
coordinator must compose both: route to the right backends AND filter by namespace. Treating
backend as "just an implementation detail" encourages developers to assume backend placement
is irrelevant to isolation — but placing tenant A's data on tenant B's dedicated backend is
a placement bug even if namespace filtering would prevent reads.

### Why remove hierarchy from core type?

`is_child_of` performs a string-prefix check. It has no semantic relationship to
authorization. In cloud deployments, tenant namespaces are UUIDs — there is no hierarchy.
The method would be dead code on half the deployment surface and a security footgun on the
other half.

### Why read-by-ID still requires token?

UUID is globally unique, but namespace-scoped. Storage fetches by UUID without namespace
filtering (the UUID is sufficient for the lookup). But the runtime must verify the result
belongs to the caller's namespace before returning it. Without this check, an attacker who
guesses or observes a UUID from another namespace can read that record.

## Consequences

### Positive

- Namespace isolation enforced by the Rust type system, not convention.
- Cloud deployment cannot accidentally fall into `"local"`.
- Every namespace-scoped code path requires an authorized token.
- Read-by-ID verifies namespace after fetch — no UUID-guessing bypass.
- Hierarchy helpers cannot be confused with authorization.
- Pack namespace policy deferred to the right layer (ADR-017, Pack Standard).

### Negative

- Removing `Default` and the public constructor breaks existing call sites.
  Mitigated: these are the call sites that need audit — the breakage is diagnostic.
- `NamespaceToken` adds a parameter to every runtime/coordinator method.
  Mitigated: the parameter is the authorization proof — omitting it would be the bug.
- Two-layer validation (structural + semantic) is more complex than single-layer.
  Mitigated: each layer is simple and independently testable.

### Neutral

- SQLite TEXT column unchanged. Namespace strings stored the same way.
- `Namespace::local()` still works for OSS. The factory is the same; the default is
  moved to config.
- Wire format unchanged — namespaces are strings in JSON/MCP.

## Implementation

- `khive-types/src/namespace.rs`: `Namespace` struct with factories, `TryFrom<String>`,
  `NamespaceError`. No `Default`. No `new(String)`. No `is_child_of`.
- `khive-runtime/src/runtime.rs`: `NamespaceToken` with sealed constructor,
  `NamespaceView` wrapper. Token minting via `VerbRegistry::dispatch`.
- `khive-types/src/namespace.rs`: `has_segment_prefix` utility for OSS hierarchical
  naming convention (lives with the `Namespace` type, not in the runtime).
- Runtime methods: all namespace-scoped operations take `&NamespaceToken`. Read-by-ID
  methods verify `record.namespace == token.namespace()` after fetch.

---

## Amendment: OSS vs Cloud Namespace Models (2026-05-25)

**Authors**: lambda:khive (Wave 4 — k-actor-config)

### Two products, two models

The original ADR describes the complete enforcement model that will apply in cloud (hosted,
multi-tenant) deployments. OSS single-binary deployments use a deliberately lighter model.
These are different products and the distinction is intentional.

```text
OSS model:   config-default actor + per-op override + no enforcement
Cloud model: NamespaceToken from authenticated session + full enforcement
```

OSS never enforces namespace isolation — it is a single-user local tool. The namespace
field exists to give agents logical grouping and to prevent accidental cross-session
contamination, not to provide security.

### OSS namespace resolution (priority order)

When `khive-mcp` starts, it resolves the default namespace for the session in the
following priority order (highest wins):

1. `--actor <id>` CLI flag (or `KHIVE_ACTOR` env var)
2. `--namespace <id>` CLI flag (or `KHIVE_NAMESPACE` env var) — legacy alias for `--actor`
3. `[actor] id` in the config file (`--config` / `KHIVE_CONFIG` / `khive.toml` /
   `.khive/config.toml` / `~/.khive/config.toml`)
4. Hard default: `"local"`

Every verb that does not supply an explicit `namespace` argument inherits the session
default. Verbs that supply `namespace` explicitly use that value unconditionally — the gate
is still consulted (as required by ADR-018), but the OSS default `AllowAllGate` allows all
requests, so there is no denying enforcement. Cloud deployments swap in `TenantGate`, which
rejects namespace requests that do not match the authenticated session JWT.

### TOML config format

```toml
[actor]
id = "lambda:myproject"           # sets default_namespace for this session
display_name = "My Project Agent" # optional, advisory only
```

`display_name` is stored in the config struct and available for display/logging; it has no
effect on namespace routing or enforcement.

### Why no enforcement in OSS

1. **Single-user deployment.** There is no second principal to protect against. The user
   who sets `--actor lambda:foo` is the same user who could set `--actor lambda:bar`. An
   enforcement layer would only add friction with zero security benefit.
2. **AllowAllGate.** The OSS binary uses `AllowAllGate` — every verb dispatch succeeds
   authorization. `NamespaceToken` is still minted (structural validation still fires), but
   the gate never rejects. Cloud swaps in `TenantGate` which verifies the session JWT and
   enforces that the requested namespace matches the authenticated tenant.
3. **Contamination prevention, not isolation.** The main value of `--actor` in OSS is
   preventing accidental cross-session contamination: two agents sharing a single `khive-mcp`
   instance with the same `"local"` default will interleave their tasks. Giving each agent
   its own actor ID (`lambda:agent-a`, `lambda:agent-b`) keeps their records separate without
   any security boundary.

### Cloud enforcement (future)

Cloud deployments replace the `AllowAllGate` with a real gate that:

- Receives the session JWT from the incoming connection context
- Extracts the tenant namespace from the token claims
- Rejects any verb that requests a namespace not permitted by the JWT

The `NamespaceToken` minting in `VerbRegistry::dispatch` is unchanged — only the gate
implementation differs. OSS and cloud share the same type system; enforcement is a
deployment-time configuration, not a code change.

### Consequences of this amendment

- `RuntimeConfig::default_namespace` is the single place where the OSS actor default
  is stored. All config-loading code sets it; the runtime never reads `[actor]` directly.
- `khive-runtime/src/engine_config.rs` gains `ActorConfig { id, display_name }` as a
  `[actor]` section in `KhiveConfig`.
- `runtime_config_from_khive_config` applies `actor.id` to `default_namespace` when present.
- `khive-mcp` binary gains `--actor` / `KHIVE_ACTOR` as the preferred namespace override,
  with `--namespace` retained as a legacy alias.
- `KhiveConfig::load_with_home_fallback` implements the 4-tier config search path.

---

## Amendment: Namespace-by-Layer Rule (2026-05-27)

**Authors**: Ocean, lambda:khive

### Problem

When different projects (lionagi, khive, lattice) each run MCP with `--actor lambda:{project}`,
their KG entities land in separate namespaces. This makes shared concepts invisible across
projects, creates duplicates, and prevents cross-project edges — defeating the purpose of a
shared knowledge graph.

### Rule: KG uses shared namespace, scoped packs use actor namespace

| Pack layer                      | Namespace                 | Rationale                                             |
| ------------------------------- | ------------------------- | ----------------------------------------------------- |
| **KG** (entities, edges, notes) | `local` (default, shared) | One "LoRA" entity — all projects link to it via edges |
| **Memory** (remember/recall)    | `lambda:{project}`        | Scoped episodic/semantic memory per agent             |
| **GTD** (assign/next/complete)  | `lambda:{project}`        | Scoped task queues per orchestrator                   |
| **Comm** (send/inbox)           | `lambda:{project}`        | Scoped messaging between agents                       |
| **Brain** (profiles/feedback)   | `lambda:{project}`        | Scoped priors per agent context                       |
| **Schedule** (agenda/remind)    | `lambda:{project}`        | Scoped schedules per agent                            |
| **Knowledge** (learn/cite)      | `local` (shared)          | Extends KG — same shared namespace                    |

**Invariant**: `create`, `link`, `search`, `list`, `get`, `neighbors`, `traverse`, `query`,
`knowledge.learn`, `knowledge.cite` MUST use the default shared namespace. Agents MUST NOT
override the namespace for KG operations with `lambda:*` actor namespaces.

### Why not one namespace for everything?

Memory recall under `lambda:lionagi` returns only lionagi's working memories. If all memory
went to `local`, every agent would recall every other agent's episodic notes — noise that
degrades recall precision. The separation is correct for scoped operational data and wrong
for shared structural knowledge.

### Cross-project connection model

Projects connect through the edge ontology, not through namespace sharing:

```text
Entity: "LoRA" (concept, namespace=local)
  ←implements— "lattice-lora" (project, namespace=local)
  ←depends_on— "lionagi-finetune" (project, namespace=local)
  ←introduced_by— "Hu et al. 2021" (concept, namespace=local)
```

Each project's `project` entity lives in the shared graph. The `implements`, `depends_on`,
`competes_with` edges ARE the cross-project synergy layer.

### Implementation

1. **MCP server**: KG pack verbs always use `RuntimeConfig::default_namespace` (already
   `Namespace::local()` by default). The `--actor` flag affects memory/gtd/comm/brain
   namespace selection but NOT KG verbs.
2. **Pack dispatch**: Each pack determines its own namespace policy. KG pack ignores actor
   override for entity/edge/note operations. Memory/GTD/Comm packs use actor namespace.
3. **Plugin skills**: All KG plugin skills (digest, expand, polish, gap, explore, etc.)
   document that they operate in the shared namespace.
4. **Agent instructions**: Agents using `--actor lambda:{project}` must understand that KG
   operations are cross-project by design.

### Consequences

- No duplicate entities across projects — one canonical concept per name.
- Cross-project edges work without namespace gymnastics.
- Memory/task isolation preserved — agents don't pollute each other's working state.
- Requires pack-level namespace routing (KG pack → shared, memory pack → actor) rather than
  a single session-wide namespace.
