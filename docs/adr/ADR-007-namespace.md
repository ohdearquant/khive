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
    pub fn local() -> Self {
        Self("local".to_owned())
    }

    pub fn project(slug: &str) -> Result<Self, NamespaceError> {
        validate_slug(slug)?;
        Ok(Self(format!("local:{slug}")))
    }

    pub fn tenant(id: Uuid) -> Self {
        Self(format!("tenant:{id}"))
    }

    pub fn system(name: &str) -> Result<Self, NamespaceError> {
        validate_slug(name)?;
        Ok(Self(format!("system:{name}")))
    }

    pub fn parse(s: &str) -> Result<Self, NamespaceError> {
        validate_namespace_string(s)?;
        Ok(Self(s.to_owned()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_trusted_unchecked(s: String) -> Self {
        Self(s)
    }
}

impl TryFrom<String> for Namespace {
    type Error = NamespaceError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Namespace::parse(&value)
    }
}
```

**No public `Namespace::new(String)`.** Every construction site goes through a factory that
enforces structural invariants. `from_trusted_unchecked` is `pub(crate)` only — for
deserialization from trusted storage where the value was validated on write.

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

All runtime and coordinator methods that read/write namespace-scoped records require
`NamespaceToken`. Single-record ID operations still require token verification — after
fetching by UUID, runtime compares the record's namespace to the token before returning:

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

### Hierarchy helpers: not on the core type

`is_child_of` (the prefix-based hierarchy check) is removed from `Namespace`. It is a
naming-convention utility, not a semantic guarantee. Leaving it on the core security type
invites misuse as an authorization primitive:

```rust
// DANGEROUS — looks like auth, isn't
if requested.is_child_of(&caller) { allow(); }
```

Hierarchy helpers move to a separate utility:

```rust
// namespace_path.rs — naming-convention helper, NOT authorization
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
- `khive-runtime/src/auth.rs` (or `namespace_token.rs`): `NamespaceToken` with sealed
  constructor, `NamespaceView` wrapper. `AuthContext` for token minting.
- `khive-runtime/src/namespace_path.rs`: `has_segment_prefix` utility for OSS hierarchical
  naming convention.
- Runtime methods: all namespace-scoped operations take `&NamespaceToken`. Read-by-ID
  methods verify `record.namespace == token.namespace()` after fetch.
