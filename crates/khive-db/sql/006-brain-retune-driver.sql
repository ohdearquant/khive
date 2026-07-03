-- V6: ADR-081 recall retune driver — bounded-mass fold gate + cross-session
-- serve ledger.
--
-- brain_implicit_mass: per-accounting-key (profile_id, namespace, target_id)
-- decayed implicit feedback mass accumulator (ADR-081 §2). One row per key;
-- read-decayed-written by the fold gate, inside one BEGIN IMMEDIATE
-- transaction per event, so the decayed mass is not re-derived from the full
-- brain_event_log on every fold.
--
-- last_effective_weight is the weight the most recent event at this key
-- actually folded (0 or the nominal implicit weight) — an audit/observability
-- column distinct from mass (whose value alone cannot disambiguate a passed
-- event from a clamped one: both a fold and a clamp can leave mass at or
-- below the cap).
CREATE TABLE IF NOT EXISTS brain_implicit_mass (
    profile_id            TEXT NOT NULL,
    namespace             TEXT NOT NULL DEFAULT 'default',
    target_id             TEXT NOT NULL,
    mass                  REAL NOT NULL,
    last_event_at         INTEGER NOT NULL,
    last_effective_weight REAL NOT NULL DEFAULT 0.0,
    PRIMARY KEY (profile_id, namespace, target_id)
);

-- brain_serve_ledger: brain-owned record of recall serves and their
-- scorer-assigned grades (ADR-081 §4 normative schema).
--
-- accounting_profile_id is a generated column: COALESCE(served_by_profile_id,
-- resolved_profile_id). served_by_profile_id wins when both are set (the
-- serve-time stamp is authoritative); resolved_profile_id/resolved_at are the
-- score-time fallback retained as the drift audit trail until the serve-time
-- stamp ships (ADR-081 §5, out of scope for this migration/PR).
CREATE TABLE IF NOT EXISTS brain_serve_ledger (
    id                    TEXT PRIMARY KEY,
    namespace             TEXT NOT NULL DEFAULT 'default',
    consumer_kind         TEXT NOT NULL,
    served_by_profile_id  TEXT,
    resolved_profile_id   TEXT,
    resolved_at           INTEGER,
    accounting_profile_id TEXT GENERATED ALWAYS AS
        (COALESCE(served_by_profile_id, resolved_profile_id)) VIRTUAL,
    target_id             TEXT NOT NULL,
    query_class           TEXT NOT NULL,
    query_raw             TEXT NOT NULL,
    served_at             INTEGER NOT NULL,
    grade                 TEXT,
    graded_at             INTEGER,
    scorer_run_id         TEXT
);

-- Serve-row uniqueness (ADR-081 §4): one serve per (namespace, target_id,
-- query_class, served_at).
CREATE UNIQUE INDEX IF NOT EXISTS idx_brain_serve_ledger_unique
    ON brain_serve_ledger(namespace, target_id, query_class, served_at);

-- Suppression reads: recent serves for a (target, query_class) pair.
CREATE INDEX IF NOT EXISTS idx_brain_serve_ledger_suppression
    ON brain_serve_ledger(target_id, query_class, served_at);

-- Mass-query reads: the ADR-081 §2 accounting key, read from the ledger side
-- (attribution audit / grade backfill lookups), distinct from the
-- brain_implicit_mass accumulator's own primary key.
CREATE INDEX IF NOT EXISTS idx_brain_serve_ledger_accounting
    ON brain_serve_ledger(accounting_profile_id, namespace, target_id);

-- brain_scorer_dedup: the ADR-081 §2/§6 idempotency claim table. The dedup
-- key is (scorer_run_id, serve_ledger_id) — one run may legitimately grade
-- multiple serve rows for the same target, and each row's grade folds as its
-- own event, but the same (run, row) pair must fold at most once. The fold
-- gate claims a row here with `INSERT OR IGNORE` inside the same
-- `BEGIN IMMEDIATE` transaction that checks and writes `brain_implicit_mass`
-- (fold_gate.rs), so the claim and the fold are atomic together: a
-- conflicting insert (0 rows affected) means a prior call already claimed
-- this pair, and the caller must treat this emission as a no-op before
-- touching mass or appending a feedback event.
CREATE TABLE IF NOT EXISTS brain_scorer_dedup (
    scorer_run_id   TEXT NOT NULL,
    serve_ledger_id TEXT NOT NULL,
    claimed_at      INTEGER NOT NULL,
    PRIMARY KEY (scorer_run_id, serve_ledger_id)
);
