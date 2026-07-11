# khive-gate Design

## ADR Compliance

### ADR-018: Authorization Gate

Full spec: `docs/adr/ADR-018-authorization-gate.md` (repo root).

This crate implements the public types and default gate defined by ADR-018:

- **`Gate` trait** — the authorization hook consulted before each verb dispatch.
  The `check` method returns `Ok(GateDecision)` or `Err(GateError)`. A `Deny`
  decision blocks dispatch; an `Err` (infrastructure failure) is fail-open per
  the ADR — dispatch proceeds with a tracing warning, no audit event emitted.

- **`impl_name()`** — stable string identifier per `Gate` implementation.
  Default returns `std::any::type_name::<Self>()`. Concrete impls override for
  clarity (e.g., `"AllowAllGate"`, `"RegoGate"`). Surfaced in audit events so
  downstream tooling can distinguish gate implementations without parsing type
  names.

- **`AllowAllGate`** — the permissive runtime default. Every request is allowed
  with no obligations. Intended for personal/local deployments. Replace in
  `RuntimeConfig.gate` for any deployment requiring real authorization.

- **`AuditEvent`** — structured audit record emitted once per gate consultation.
  The JSON field names (`actor`, `namespace`, `verb`, `decision`, `deny_reason`,
  `obligations`, `gate_impl`, `session_id`, `timestamp`) are a **stable public
  contract**. Adding fields is non-breaking; removing or renaming requires an
  ADR amendment.

- **`Obligation::Custom { value }`** — the struct-like variant (with named field
  `value`) is mandatory. A newtype variant cannot merge serde's internally-tagged
  `kind` discriminator into a non-object payload; scalar and array values fail at
  runtime with a newtype shape. The struct form prevents this at compile time.

- **`GateRequest` JSON projection** — field names (`input.actor`, `input.namespace`,
  `input.verb`, `input.args`) are the policy input contract (e.g., for Rego). Any
  field rename is a breaking change.

### ADR-007: Namespace as Attribution-Only Open String - Dumb Storage, Single Gate, Operator-Configured Read Visibility - Namespace

`GateRequest.namespace` must reflect exactly what the runtime sees, with no coercion
at the gate layer. Coercing an empty namespace to a default inside the gate would
create an authorization blind spot. The runtime and gate resolve namespace through
the same logic.

## Consistency Notes

- **`Obligation::RateLimit` and `Custom` are declared but not enforced in v0.**
  Policy authors can return these obligations; the runtime records them in the audit
  event but does not enforce rate limits or custom semantics. A future ADR adds
  per-actor rate-limit enforcement.

- **`AuditEvent` obligations field always serializes** (even as `[]` on Deny) so
  non-Rust consumers do not need to special-case absence vs. emptiness. The
  `#[serde(default)]` annotation handles deserialization of absent fields, but
  serialization always includes the array.

- **Empty `ActorRef.kind` / `ActorRef.id` and empty `GateRequest.verb` are accepted
  by serde without validation.** Callers must validate before use. A future
  validation layer in the gate or dispatch path will reject empty fields. Tests
  document this boundary explicitly so the failure mode is visible if validation
  is added.
