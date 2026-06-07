# khive-gate-rego Design

## ADR Compliance

### ADR-018: Authorization Gate

This crate is the reference Rego backend for the khive authorization gate defined in ADR-018.

Key design decisions and constraints:

- **Fail-open on dispatch errors.** When `Gate::check` returns `Err(GateError)`, the runtime
  treats it as an infrastructure failure, logs a warning, and proceeds. This is the ADR-018
  "fail-open on gate Err" behavior. To prevent unintended access, always declare a
  `default decision := {"decision": "deny", ...}` so unmatched requests deny explicitly
  rather than relying on the fail-open path.

- **Fail-closed on load errors.** Policy syntax/parse errors and empty policy directories are
  detected at construction time (`from_policy_str` / `from_dir` return `Err`), not at dispatch.
  This surfaces misconfiguration at boot rather than silently failing open at runtime.

- **Engine mutex serialization.** `regorus::Engine::eval_rule` requires `&mut self`, so the
  engine is held behind a `Mutex`. This serializes evaluations on the dispatch hot path.
  If hard-enforcement workloads show contention, revisit with compiled policy or an engine pool.

- **Entrypoint validation at construction.** `try_with_entrypoint` rejects empty,
  whitespace-only, or non-`data.`-prefixed entrypoints before the gate is installed.
  This prevents a misconfigured entrypoint from causing fail-open dispatch errors at runtime.
  `with_entrypoint` is the infallible variant for programmatic use with already-validated paths.

- **Deterministic policy load order.** `from_dir` sorts `.rego` files by name before loading,
  ensuring consistent behavior across platforms when policies depend on import order.

## Consistency Notes

- The `README.md` and `docs/protocol.md` retain ADR-018 cross-references for external readers
  navigating from documentation to the authoritative design record. Only the `.rs` source files
  have had ADR citations removed.
- The fail-open behavior described here matches the production runtime behavior in
  `khive-runtime`. Any change to the gate error handling policy must be coordinated with
  the runtime gate dispatch path.
