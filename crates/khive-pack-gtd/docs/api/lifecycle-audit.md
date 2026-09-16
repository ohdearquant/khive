# Lifecycle audit trail (`src/handlers.rs`)

Every state-changing `gtd.transition` and `gtd.complete` invocation attempts
to append to a `gtd_lifecycle_audit` table for replay and compliance (ADR-019).
This document covers the write path's implementation details that don't belong
in the caller-facing contract on `handle_transition`/`handle_complete`.

## `ensure_audit_schema` — why per-call, not `OnceLock`

The DDL (`CREATE TABLE IF NOT EXISTS gtd_lifecycle_audit` plus its index) is
applied on every call rather than gated behind a global `OnceLock`. Each
`KhiveRuntime::memory()` instance in tests creates a fresh in-memory
database that needs its own schema bootstrap — a process-wide `OnceLock`
would only run the DDL once and leave every subsequent fresh test database
without the audit table. In production this per-call DDL is idempotent and
cheap: SQLite skips an `IF NOT EXISTS` table creation near-instantly once
the table already exists.

## Why the lifecycle-audit helpers are `pub`

Unlike every other helper in `handlers.rs`, these are `pub` rather than
module-private. The ADR-099 `--atomic` CLI surface's `gtd.transition`/
`gtd.complete` prepare functions live in `kkernel` (a crate that already
depends on both `khive-runtime` and `khive-pack-gtd` — see that crate's
`atomic_apply` module doc for the crate-direction rationale). The B3 GAP-5
fix applies this exact function as a deferred post-commit effect, so atomic
transitions/completes write the same best-effort lifecycle audit row the
canonical MCP handlers do, instead of re-deriving the DDL and `INSERT`
statement a second time in `kkernel`.

Audit writes are best-effort: a failure to write the audit row is logged and
does not fail the transition/complete call itself, since the state change
already committed successfully. `write_audit_record_with_status` returns a
boolean, and the canonical and atomic real-transition response builders expose
it as `audit_persisted`. The original public `write_audit_record` remains a
unit-returning compatibility wrapper. This keeps the auxiliary append non-fatal
without silently claiming a complete audit trail.

## Same-status rows: canonical and atomic behavior

A `gtd.transition` call where `current == target` writes neither a task change nor
an audit row, including when a caller supplies `note`. Canonical dispatch returns
`note_recorded=false` for a supplied note. Atomic v1 also carries a guarded
no-effect assertion that revalidates the prepare snapshot during commit.

## Dependency refusals and explicit override

Shared `prepare_complete` and `prepare_transition` check dependency readiness only
for transitions to `done`, before preparing any lifecycle mutation or audit effect.
Both canonical and atomic adapters forward the published `ignore_dependencies`
boolean, default false. Refusals preserve the task and create no audit row;
successful overrides retain the normal lifecycle audit behavior. Cancellation is
permitted with unresolved dependencies. See [the dependency contract](../design.md#completion-dependencies)
for the preparation-time scope and error details.

## `CompleteParams` / `TransitionParams` — `pub` structs, private fields

ADR-099 B3: these two structs are `pub` (not module-private) specifically so
`kkernel`'s `--atomic` validation seam (`atomic_apply::validate_atomic_args`)
can deserialize an op's args through the exact same canonical struct that
`handle_complete`/`handle_transition` use internally. That reproduces the
handlers' `deny_unknown_fields` rejection behavior for the atomic path with
zero duplicated field lists to keep in sync. Fields themselves stay private
— the atomic seam only ever needs the `Result<_, _>` deserialization
outcome, never field access on a successfully-parsed value.
