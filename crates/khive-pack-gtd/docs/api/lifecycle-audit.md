# Lifecycle audit trail (`src/handlers.rs`)

Every `gtd.transition` and `gtd.complete` invocation is recorded into a
`gtd_lifecycle_audit` table for replay and compliance (ADR-019). This
document covers the write path's implementation details that don't belong
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

## Why `ensure_audit_schema` and `write_audit_record` are `pub`

Unlike every other helper in `handlers.rs`, these two are `pub` rather than
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
already committed successfully.

## `CompleteParams` / `TransitionParams` — `pub` structs, private fields

ADR-099 B3: these two structs are `pub` (not module-private) specifically so
`kkernel`'s `--atomic` validation seam (`atomic_apply::validate_atomic_args`)
can deserialize an op's args through the exact same canonical struct that
`handle_complete`/`handle_transition` use internally. That reproduces the
handlers' `deny_unknown_fields` rejection behavior for the atomic path with
zero duplicated field lists to keep in sync. Fields themselves stay private
— the atomic seam only ever needs the `Result<_, _>` deserialization
outcome, never field access on a successfully-parsed value.
