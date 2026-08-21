# Gate evaluation contract

`Gate` is the synchronous, thread-safe authorization boundary consulted immediately before verb
dispatch.

## `Gate::check`

Implementations receive a validated `GateRequest` and return either a policy `GateDecision` or a
`GateError`. A policy denial is data (`Ok(GateDecision::Deny)`), not an infrastructure error.
Implementations must be `Send + Sync + Debug`, and `GateRef` is the shared `Arc<dyn Gate>` handle.

## `Gate::impl_name`

The default is `std::any::type_name::<Self>()`. Audit consumers use this value to distinguish
backend and wrapper decisions without parsing Rust types; implementations may override it with a
stable short name.

## `AllowAllGate`

The runtime default allows every request with no obligations. It is suitable for trusted personal
or local deployments, not deployments that require actor isolation. Enforcement backends include
the sibling `khive-gate-rego` crate and downstream capability or wrapper implementations.

## Error boundary

`GateError::Policy` reports policy parsing or evaluation failures and `GateError::Internal` reports
backend infrastructure faults. The dispatcher audits every `GateError` as gate unavailability,
returns `RuntimeError::GateUnavailable`, and never invokes the operation. An implementation should
convert evaluation uncertainty into `Ok(GateDecision::Deny)` only when it intends to report an
explicit policy refusal rather than an infrastructure outage.
