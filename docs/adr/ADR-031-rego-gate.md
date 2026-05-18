# ADR-031: Rego Policy Backend — `khive-gate-rego`

**Status**: accepted\
**Date**: 2026-05-18\
**Authors**: Ocean, lambda:khive

## Context

[ADR-029](ADR-029-authorization-gate.md) introduced the `Gate` trait with `AllowAllGate` as the
permissive default and named three impl paths. The first non-default impl this ADR ships is
`RegoGate` — a [Rego](https://www.openpolicyagent.org/) policy backend powered by
[`regorus`](https://crates.io/crates/regorus) (Microsoft's MIT/Apache-2.0 Rust Rego engine).

Two design questions had to land before code:

1. **Where does the impl live?** ADR-029 originally said "behind a `rego` feature flag in
   `khive-gate`." A feature flag pulls regorus into the dependency graph for every consumer of
   `khive-gate`, even those who never enable it.
2. **What is the policy contract?** The `GateRequest` JSON shape is already locked by ADR-029.
   What rule path do policies expose? What output shape do they return? How are obligations
   represented in Rego?

This ADR resolves both.

## Decision

### Separate crate `khive-gate-rego` (Apache-2.0)

`RegoGate` ships in a sibling crate, not behind a feature flag on `khive-gate`. Rationale:

- **Pattern parity.** `khive-pack-kg` / `khive-pack-gtd` are sibling crates implementing the
  `Pack` trait. Gate impls follow the same shape: one trait crate, N sibling impl crates.
- **No feature-flag tax.** Consumers add `khive-gate-rego` to deps or they don't. No `cargo
  tree` pollution, no docs gymnastics about which features expose which types.
- **Symmetric with downstream impls.** `khive-cloud-gate` (BUSL) and any future capability-
  witness backend live separately because of license. `RegoGate` living separately for compile
  isolation completes the pattern.
- **Independent versioning.** Breaking changes to the Rego contract (e.g., new entrypoint
  convention) only bump `khive-gate-rego`, not `khive-gate`.

`khive-types` was considered as a home for the trait per a v1 of this discussion. Rejected:
`khive-types` is substrate-narrow (data types only — `Entity`, `Note`, `Event`, `Namespace`,
`Pack`). The `Gate` trait is a runtime-layer concern consumed only by `khive-runtime`. Bloating
`khive-types` with it would couple every storage-layer crate to a trait they never use.

### Policy contract

Policies receive `GateRequest` as JSON on `input`:

```text
input.actor.kind            string  "user" | "agent" | "lambda" | "anonymous" | ...
input.actor.id              string
input.namespace             string
input.verb                  string
input.args                  any     (the verb's raw args)
input.context.session_id    string  optional
input.context.timestamp     string  optional, RFC3339
input.context.source        string  optional ("mcp", "cli", ...)
```

The field shape is the public contract from [ADR-029](ADR-029-authorization-gate.md) — renaming a
field is a breaking change there, not here.

Policies MUST define a `decision` rule under package `khive.gate`, returning an object that
matches the JSON projection of `GateDecision`:

```rego
package khive.gate

import rego.v1

# Fail-closed default. Without this, missing rule = evaluation error.
default decision := {"decision": "deny", "reason": "no rule matched"}

decision := {"decision": "allow", "obligations": []} if {
    input.verb == "search"
}

decision := {
    "decision": "allow",
    "obligations": [{"kind": "audit", "tag": sprintf("verb.%s", [input.verb])}],
} if {
    input.actor.kind == "user"
    input.namespace  == "ocean"
}

decision := {"decision": "deny", "reason": "anonymous callers cannot write"} if {
    input.actor.kind == "anonymous"
    input.verb       == "create"
}
```

Output shape (tagged by `decision`):

```json
{"decision": "allow", "obligations": [{"kind": "audit", "tag": "..."}]}
{"decision": "deny",  "reason": "..."}
```

Obligation kinds — `audit`, `rate_limit`, `custom` — match `Obligation`'s serde shape from
`khive-gate`. Advisory in v0.2, enforced in v0.3 + audit envelope (ADR-032 planned).

### Default entrypoint

`data.khive.gate.decision`. Override with `RegoGate::with_entrypoint("data.your.path.rule")` if
your policy uses a different package — useful for embedding existing OPA policies. The default
is a constant — `khive_gate_rego::DEFAULT_ENTRYPOINT` — to keep the documented path and the
code in sync.

### Loading policies

Two constructors:

- `RegoGate::from_policy_str(&str)` — single inline source. Best for tests and small inline
  policies bundled with the binary.
- `RegoGate::from_dir(&Path)` — load all `*.rego` files under a directory (non-recursive,
  sorted by filename for deterministic order across platforms). Best for ops folks managing
  policies as files.

Both return `Result<Self, GateError>`. Parse/compile errors come back as `GateError::Policy`;
runtime evaluation errors come back as `GateError::Evaluation`.

### Engine concurrency

`regorus::Engine::eval_rule` requires `&mut self`. `RegoGate` holds the engine inside a `Mutex`,
serializing evaluations on the dispatch hot path.

This is acceptable while the gate is advisory ([ADR-029](ADR-029-authorization-gate.md) v0.2 —
the dispatcher logs deny reasons but doesn't block). When enforcement lands in v0.3 and
contention becomes measurable, options are: (a) `regorus::CompiledPolicy` precompiled once,
evaluated cheaply per request; (b) an engine pool. Defer that until benchmarks show the lock
matters.

### `Send + Sync` for storage in `RuntimeConfig`

`Mutex<regorus::Engine>` is `Send + Sync` provided `Engine: Send`. regorus 0.10 satisfies this.
`Arc<RegoGate>` therefore satisfies `Arc<dyn Gate>`'s requirements directly.

## Rationale

### Why Rego as the OSS policy backend

Already covered in [ADR-029](ADR-029-authorization-gate.md) §"Why Rego, not a homegrown policy
DSL." Short version: OPA is the cross-industry standard, `regorus` is a maintained Rust engine,
policies are portable across khive-oss, khive-cloud, and any other OPA consumer.

### Why expose `DEFAULT_ENTRYPOINT` as a `pub const`

External tooling (policy linters, doc generators, IDE integrations) needs to know the
convention. Hiding it inside the type forces consumers to hardcode the string; exporting it
keeps a single source of truth.

### Why `Mutex` over `RwLock`

`eval_rule` mutates the engine — there is no read-only eval. `RwLock` would acquire write-mode
on every call, making it functionally identical to `Mutex` but with more overhead.

### Why fail-closed defaults are the policy author's responsibility

The trait contract says: return a `GateDecision`. A policy that defines no `decision` rule
returns `null` from regorus, which `RegoGate::check` surfaces as
`GateError::Evaluation("policy returned shape that isn't a GateDecision: ...")`. We could
default to `Deny { reason: "no rule matched" }` inside `RegoGate`, but that masks the
configuration bug — the policy author intended to write a rule, didn't, and would never find
out. Surfacing the error makes the bug loud.

The example policies (and the docs) always include a `default decision := {...}` line — that
is the documented best practice.

## Alternatives Considered

| Alternative                                            | Pros                                               | Cons                                                   | Why rejected                                              |
| ------------------------------------------------------ | -------------------------------------------------- | ------------------------------------------------------ | --------------------------------------------------------- |
| Feature flag inside `khive-gate`                       | One crate                                          | All consumers see regorus in `cargo tree`              | Pollutes dep graph for non-Rego users                     |
| Trait in `khive-types`                                 | Single root for the trait                          | Couples storage-layer crates to a runtime concern      | Wrong layer; bloats the narrow types crate                |
| Compile policies via `compile_with_entrypoint` upfront | Faster eval; potentially `&self` instead of `&mut` | More complex; ties RegoGate to regorus internals       | Defer until benchmarks justify it                         |
| Default entrypoint via env var                         | Ops-friendly                                       | Magic at runtime; harder to test; same string repeated | `with_entrypoint(...)` keeps it explicit at construction  |
| Return `Allow{}` when rule missing                     | Lenient                                            | Hides policy bugs; opposite of fail-closed             | Loud error > silent miscompile                            |
| Return `Deny{}` when rule missing                      | Fail-closed default                                | Hides policy bugs (author didn't intend a deny here)   | Same as above — better to surface the missing rule        |
| Recursive `from_dir`                                   | Friendly for nested policy hierarchies             | Surprising load order; collisions across files         | Add later if a real use case appears; flat is predictable |

## Consequences

### Positive

- OSS users get production-grade policy enforcement (advisory in v0.2, enforced in v0.3) via a
  policy language with a well-documented ecosystem.
- Policies authored against `khive-gate-rego` can be reused as-is by `khive-cloud-gate`
  wrapping `RegoGate` inside `LionGate<RegoGate>`. One policy, two enforcement engines.
- No feature-flag complexity. Either `khive-gate-rego` is in `Cargo.toml` or it isn't.
- The crate's public API is small: `RegoGate`, `DEFAULT_ENTRYPOINT`. Trait + types live
  upstream in `khive-gate`. Easy to audit.

### Negative

- regorus brings ~50K LOC of policy engine + the OPA stdlib into the binary. Acceptable for
  deployments that want Rego; the alternative (`khive-gate`-only with `AllowAllGate`) stays
  ~250 LOC.
- The `Mutex<Engine>` serializes evaluations. Hot-path performance is bounded by single-
  threaded eval throughput. Numbers TBD; revisit if benchmarks show contention.
- Policy authors must learn Rego. Mitigated by OPA's existing learning material and the
  documented contract above.

### Neutral

- The lock-step between this ADR and ADR-029 means a breaking change to `GateRequest` /
  `GateDecision` cascades through both.
- `tempdir` is rolled by hand in tests (no `tempfile` dep) to avoid a transitive dependency
  for one test helper. Will revisit if more tests need filesystem fixtures.

## Implementation Status

| Step                                                          | Where                                                         | Status                      |
| ------------------------------------------------------------- | ------------------------------------------------------------- | --------------------------- |
| `khive-gate-rego` crate scaffold + Cargo.toml                 | `crates/khive-gate-rego/`                                     | done                        |
| `RegoGate::from_policy_str` / `from_dir` / `with_entrypoint`  | `crates/khive-gate-rego/src/lib.rs`                           | done                        |
| `impl Gate for RegoGate` — eval round-trip                    | `crates/khive-gate-rego/src/lib.rs`                           | done                        |
| `Gate::impl_name()` default + override                        | `crates/khive-gate/src/lib.rs` + `khive-gate-rego/src/lib.rs` | done                        |
| Integration tests (allow / deny / obligations / errors / dir) | `crates/khive-gate-rego/tests/integration.rs`                 | done                        |
| Example policies                                              | `crates/khive-gate-rego/tests/fixtures/`                      | done                        |
| Audit envelope wiring (`EventKind::GateCheck`)                | TBD                                                           | ADR-032 (planned)           |
| Hard enforcement at dispatch site (deny → error)              | `crates/khive-runtime/src/pack.rs`                            | deferred to v0.3            |
| Engine pool / `CompiledPolicy` for contention                 | `crates/khive-gate-rego/`                                     | deferred (benchmark-driven) |

## Open Questions

1. **Where do operational policies live?** Per-deployment is the user's choice — env var
   pointing at a directory, baked-in literal, fetched from a remote store. khive-mcp's CLI
   currently doesn't expose a policy path; that's a v0.3 wiring concern.
2. **`Obligation::Custom` shape consistency.** Custom obligations carry arbitrary JSON. If
   patterns emerge (e.g., everyone uses `{"kind": "log", "level": "info"}`), promote them to
   first-class variants in `khive-gate` and update this ADR.
3. **Policy hot-reload.** Production deployments may want to swap policies without restart.
   The current `RegoGate` is immutable post-construction. Hot-reload would need a `swap_engine`
   method or a higher-level wrapper. Defer until requested.

## References

- [ADR-029](ADR-029-authorization-gate.md): Authorization gate trait + AllowAllGate default
- [ADR-027](ADR-027-single-tool-mcp-surface.md): Single dispatch site where the gate is
  consulted
- Open Policy Agent: <https://www.openpolicyagent.org/>
- Rego language: <https://www.openpolicyagent.org/docs/latest/policy-language/>
- `regorus`: <https://github.com/microsoft/regorus> (license: MIT / Apache-2.0 / BSD-3-Clause)
- `tests/fixtures/allow_search.rego`, `tests/fixtures/namespace_scoped.rego` — example policies
