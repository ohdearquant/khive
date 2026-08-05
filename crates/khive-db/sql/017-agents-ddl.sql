-- V17: durable agent-process table (ADR-142 §1 "Process model").
--
-- One row per runtime-owned agent-process record. This table is the sole
-- durable source of truth for agent lifecycle state; it is distinct from
-- the session pack's checkpoint/transcript storage, which this table only
-- references by id (`checkpoint_session_id`).
--
-- Column choices:
--   * `agent_id` is the primary key: an opaque, immutable handle assigned at
--     spawn (ADR-142 §1 "Actor provenance"), never reused.
--   * `state`/`terminal_reason` are TEXT with CHECK constraints against the
--     closed enums in the shared contract (`AgentState`/`TerminalReason`) so
--     an invalid value fails at the storage boundary, not just in Rust.
--     `terminal_reason` is required exactly when `state = 'terminal'` and
--     forbidden otherwise, matching "exactly one `terminal_reason` once it
--     reaches `terminal`" (ADR-142 §1).
--   * `owner_visible_namespaces` is a JSON array of strings (TEXT, validated
--     with `json_valid`), not a child table. The field is an immutable
--     snapshot written once at spawn and always read back whole with the
--     rest of the record (ADR-142 §1 "Actor provenance") — there is no
--     query path that filters agents by individual namespace membership, so
--     a normalized child table would add a join with no read benefit. This
--     matches the existing convention for whole-record JSON blobs elsewhere
--     in this schema (e.g. `entities.properties`, `notes.properties`).
--   * `spawned_at`/`state_changed_at` are INTEGER microseconds since the
--     Unix epoch, matching every other timestamp column in this schema
--     (e.g. `events.created_at`).
--   * `checkpoint_cursor` is INTEGER (nullable): "the position of the last
--     message captured in that checkpoint" (ADR-142 §1) — an ordinal, not a
--     timestamp.
--
-- Constraint choices — the two properties this migration exists to hold:
--
--   1. At most one non-terminal record per (provider, provider_session_id).
--      Enforced with a partial UNIQUE index scoped to non-terminal states
--      and a non-null `provider_session_id`. SQLite's single-writer
--      serialization (every write on this store runs through the pool's
--      one writer connection/task) means the store-level
--      check-then-insert in `agents.rs` never races against a concurrent
--      writer, but the index is kept anyway as a schema-level backstop: it
--      is the thing that actually makes a violation impossible even if a
--      future write path bypasses the store's own pre-check, and it costs
--      nothing at this table's write volume.
--   2. Idempotency replay is keyed on the pair (owner_actor,
--      idempotency_key), never the key alone. Enforced with a UNIQUE index
--      on that pair (scoped to non-null `idempotency_key`), which is also
--      the exact index `find_by_idempotency` needs for its lookup.

CREATE TABLE IF NOT EXISTS agents (
    agent_id                  TEXT PRIMARY KEY,
    state                     TEXT NOT NULL
        CHECK (state IN ('spawned', 'running', 'suspended', 'terminal')),
    terminal_reason           TEXT
        CHECK (terminal_reason IS NULL OR terminal_reason IN
            ('completed', 'failed', 'killed', 'abandoned', 'host_restart')),
    provider                  TEXT NOT NULL,
    provider_session_id       TEXT,
    checkpoint_session_id     TEXT,
    checkpoint_cursor         INTEGER,
    owner_actor               TEXT NOT NULL,
    owner_peer_class          TEXT NOT NULL,
    owner_write_namespace     TEXT NOT NULL,
    owner_visible_namespaces  TEXT NOT NULL
        CHECK (json_valid(owner_visible_namespaces)),
    spawn_fingerprint         TEXT NOT NULL,
    spawned_at                INTEGER NOT NULL,
    state_changed_at          INTEGER NOT NULL,
    idempotency_key           TEXT,
    CHECK (
        (state = 'terminal' AND terminal_reason IS NOT NULL)
        OR (state != 'terminal' AND terminal_reason IS NULL)
    )
);

CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_live_provider_session
    ON agents(provider, provider_session_id)
    WHERE state != 'terminal' AND provider_session_id IS NOT NULL;

CREATE UNIQUE INDEX IF NOT EXISTS idx_agents_idempotency_pair
    ON agents(owner_actor, idempotency_key)
    WHERE idempotency_key IS NOT NULL;

CREATE INDEX IF NOT EXISTS idx_agents_state
    ON agents(state);
