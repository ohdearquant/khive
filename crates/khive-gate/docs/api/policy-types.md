# Policy request and decision types

The gate wire types are the stable JSON boundary between runtime dispatch and authorization policy.
They validate invariants both in constructors and during deserialization.

## `ActorRef`

An actor has non-empty `kind` and `id` strings. `try_new` reports which field is empty, while `new`
panics and is intended for trusted literals. `anonymous()` produces `kind = "anonymous"` and
`id = "local"` for unauthenticated local use.

`binding_id()` deliberately returns `None` for that anonymous actor. Treating its literal `"local"`
ID as an authenticated binding key would let an unauthenticated caller match a binding that older,
pre-actor-aware callers could never match.

## `GateRequest`

The serialized fields `actor`, `namespace`, `verb`, `args`, and `context` are policy input and are a
public compatibility contract. `verb` must be non-empty; `actor` and `namespace` enforce their own
invariants. `try_new` returns `GateValidationError`, `new` panics for trusted inputs, and
`with_context` attaches session, timestamp, and transport-source metadata.

## `GateDecision`

The internally tagged JSON form uses `"decision": "allow"` or `"deny"`. Allow decisions carry an
obligation array; deny decisions require a non-empty reason. Fallible `try_deny` is appropriate for
policy output, while `deny` panics on an empty trusted literal.

## `Obligation`

`Audit` is persisted in the dispatch audit event when an event store is wired and otherwise reaches
tracing. `RateLimit` requires positive `window_secs` and `max`, but rate-limit enforcement is not
implemented in v0. `Custom` accepts arbitrary JSON and is likewise descriptive rather than enforced.

The `Custom { value }` struct-like variant is required by serde's internally tagged representation:
a newtype payload cannot merge the `kind` discriminator into scalar, array, or null JSON.

## Validation errors

`GateValidationError` covers empty actor kind, actor ID, verb, deny reason, and audit tag, plus zero
rate-limit window or maximum. Custom deserializers route wire input through the same validation as
the constructors, preventing invalid values from bypassing the public API.
