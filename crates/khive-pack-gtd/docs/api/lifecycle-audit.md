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

A `gtd.transition` call where `current == target` normally writes no audit
row — an idempotent no-op is not a lifecycle event. On canonical dispatch, a
no-op that carries a caller-supplied `note` is the exception: the note is
persisted to `properties.transition_note` (last-write-wins), and a same-status
audit row (`from_state == to_state`) is attempted so each overwritten note can
keep a durable trail. Its response carries `note_recorded` and, when the note
write wins, `audit_persisted`. Consumers counting or replaying *real*
transitions must therefore filter `from_state != to_state`; same-status rows
are note events, not lifecycle changes.

ADR-099 atomic v1 encodes every same-status transition as a guarded no-effect
assertion, including calls that supplied `note`. The assertion revalidates the
prepare snapshot inside the commit transaction, while the call still returns
the base no-op shape, persists neither the note nor an audit row, and omits
both status fields. Call canonical `gtd.transition` when a same-status note
must be recorded.

## `CompleteParams` / `TransitionParams` — `pub` structs, private fields

ADR-099 B3: these two structs are `pub` (not module-private) specifically so
`kkernel`'s `--atomic` validation seam (`atomic_apply::validate_atomic_args`)
can deserialize an op's args through the exact same canonical struct that
`handle_complete`/`handle_transition` use internally. That reproduces the
handlers' `deny_unknown_fields` rejection behavior for the atomic path with
zero duplicated field lists to keep in sync. Fields themselves stay private
— the atomic seam only ever needs the `Result<_, _>` deserialization
outcome, never field access on a successfully-parsed value.
