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
backend infrastructure faults. The dispatcher decides how errors affect availability; a gate that
must fail closed should convert evaluation uncertainty into an explicit denial.
