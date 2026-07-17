# Transaction registry — `tx_registry`

`tx_registry` (`src/tx_registry.rs`, ADR-091 Plank 0) is a process-wide,
observe-only registry of currently-open SQL transaction spans. Every
caller-controllable transaction span (`WriterGuard::transaction`,
`atomic_unit`'s own registered span, and the raw `BEGIN IMMEDIATE`/`COMMIT`
batch-writer spans) registers on open and deregisters via `TxHandle`'s
`Drop`. Nothing in this plank enforces anything from the registry — it exists
so a checkpoint task can name which caller, if any, is holding a WAL snapshot
open.

## `TxId`

Identifier for one registered span. The wrapped value is public so consumers
of `oldest()` can detect when the registry's oldest entry has *changed
identity* between two observations — distinct from "is it still above a
threshold" — without needing a live registration of their own (e.g.
`khive-db`'s `TxAgeSweepState` pure state-machine unit tests construct
synthetic ids directly). Equality is the only operation this type supports;
the numeric value carries no meaning beyond "same span" vs. "different span".

## Poisoned-lock recovery

`register`, `oldest`, and `snapshot` all recover a poisoned `Mutex` via
`.unwrap_or_else(|poisoned| poisoned.into_inner())` rather than propagating
the panic or substituting an empty result. This is observe-only telemetry: a
poisoned lock (some other holder panicked mid-critical-section) must not make
the registry silently stop tracking new spans, or a subsequent WAL-pressure
diagnosis could read a false "no open transactions" signal — `None` from
`oldest()` or an empty `Vec` from `snapshot()` would read identically to the
genuinely-empty case, which is exactly the moment this diagnostic matters
most. This replaced a previous `if let Ok(..)` pattern that silently dropped
the write on poison.

## `oldest()` and re-arming latched state

The returned `TxId` lets callers distinguish "the same span is still oldest
and still above a threshold" from "a *different* span became oldest between
two observations" — the latter must re-arm any latched escalation state,
since a departed entry's threshold-crossing history says nothing about its
replacement. `khive-db`'s `TxAgeSweepState` is the consumer this exists for.
