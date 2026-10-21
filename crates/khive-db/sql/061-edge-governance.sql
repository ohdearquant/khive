-- ADR159 §1:120–140: append-only decisions, bound to one graph incarnation.
-- IDs use the graph_edges TEXT representation; INTEGER timestamps are Unix microseconds.
CREATE TABLE IF NOT EXISTS edge_governance_decisions (
    decision_id TEXT PRIMARY KEY NOT NULL, -- ADR159 §1:127.
    edge_id TEXT NOT NULL, -- ADR159 §1:127; no graph FK: history survives hard delete (§3:247).
    edge_namespace TEXT NOT NULL, -- ADR159 §1:128–129.
    source_id TEXT NOT NULL, -- ADR159 §1:128–129.
    target_id TEXT NOT NULL, -- ADR159 §1:128–129.
    relation TEXT NOT NULL CHECK (relation = 'supersedes'), -- ADR159 §1:128–129.
    target_backend TEXT CHECK (target_backend IS NULL), -- ADR159 §2:213–233, local-only.
    disposition TEXT NOT NULL CHECK (disposition IN ('authorized', 'rejected')), -- ADR159 §1:130.
    via TEXT NOT NULL CHECK (via IN
        ('owner_direct', 'reviewed_proposal', 'migration_review', 'cutover_fence')), -- ADR159 §1:131–132.
    authorizer_kind TEXT NOT NULL CHECK (length(authorizer_kind) > 0), -- ADR159 §1:133.
    authorizer_id TEXT NOT NULL CHECK (length(authorizer_id) > 0), -- ADR159 §1:133.
    applied_by TEXT, -- ADR159 §1:134–135; not an authority source.
    authority_action TEXT NOT NULL CHECK (authority_action = 'memory.supersede'), -- ADR159 §1:136, §2:193–195.
    authority_subject_id TEXT NOT NULL CHECK (length(authority_subject_id) > 0), -- ADR159 §1:136–137.
    provider_id TEXT NOT NULL CHECK (length(provider_id) > 0), -- ADR159 §1:137.
    provider_revision TEXT NOT NULL CHECK (length(provider_revision) > 0), -- ADR159 §1:137.
    proposal_id TEXT, -- ADR159 §1:138.
    review_event_id TEXT, -- ADR159 §1:138, reviewed-path CHECK below.
    decided_at INTEGER NOT NULL, -- ADR159 §1:139–140.
    -- ADR159 §1:139–140 / §6:389–392; ADR046 A2:914–918.
    -- The internal fence code is not an additional proposal-payload enum variant.
    reason_code TEXT NOT NULL CHECK (reason_code IN
        ('migration_authorized', 'migration_rejected_unowned',
         'migration_rejected_disputed', 'revocation', 'late_arrival_unreviewed')),
    -- ADR159 §1:138 and §2:201–210: reviewed evidence requires its review event.
    CHECK (via NOT IN ('reviewed_proposal', 'migration_review')
        OR (review_event_id IS NOT NULL AND length(review_event_id) > 0))
);

-- ADR159 §1:142–161: append-only event ledger; no FK to a deletable graph/note row.
CREATE TABLE IF NOT EXISTS edge_governance_invalidations (
    invalidation_id INTEGER PRIMARY KEY, -- ADR159 §1:147.
    decision_id TEXT NOT NULL REFERENCES edge_governance_decisions(decision_id), -- ADR159 §1:147–148.
    edge_id TEXT NOT NULL, -- ADR159 §1:148.
    -- ADR159 §1:149–151 / §3:237–248. Merge rewires share the observable tuple transition.
    -- merge_rewire is reserved in this closed vocabulary, not emitted by native triggers.
    cause TEXT NOT NULL CHECK (cause IN
        ('endpoint_or_relation_or_backend_rewrite', 'merge_rewire',
         'edge_liveness_transition', 'endpoint_note_liveness_transition',
         'hard_delete', 'revoked_by_decision')),
    invalidated_at INTEGER NOT NULL -- ADR159 §1:152.
);
-- ADR159 §1:154–161: spent-decision lookup without constraining event multiplicity.
CREATE INDEX IF NOT EXISTS idx_edge_governance_invalidations_decision
    ON edge_governance_invalidations(decision_id);

-- ADR159 §1:163–175: bound serving projection, deliberately separate from authority history.
CREATE TABLE IF NOT EXISTS edge_governance_active (
    -- ADR159 §1:166 / §3:247: projection may cascade, history must be recorded BEFORE DELETE.
    edge_id TEXT PRIMARY KEY NOT NULL REFERENCES graph_edges(id) ON DELETE CASCADE,
    decision_id TEXT NOT NULL UNIQUE REFERENCES edge_governance_decisions(decision_id), -- ADR159 §1:166.
    edge_namespace TEXT NOT NULL, -- ADR159 §1:167.
    source_id TEXT NOT NULL, -- ADR159 §1:167.
    target_id TEXT NOT NULL, -- ADR159 §1:167.
    relation TEXT NOT NULL CHECK (relation = 'supersedes'), -- ADR159 §1:167 / 128–129.
    target_backend TEXT CHECK (target_backend IS NULL), -- ADR159 §2:213–233.
    stamped_at INTEGER NOT NULL -- ADR159 §1:168.
);
-- ADR159 §1:169–170 / §4:301–304: covering index begins with target_id.
CREATE INDEX IF NOT EXISTS idx_edge_governance_active_target
    ON edge_governance_active(target_id, source_id, edge_id);

-- ADR159 §1:177–181 / §7:424–431: per-store activation receipt, not edge authority.
CREATE TABLE IF NOT EXISTS edge_governance_state (
    singleton INTEGER PRIMARY KEY CHECK (singleton = 1), -- ADR159 §1:177, singleton.
    schema_version INTEGER NOT NULL CHECK (schema_version = 61), -- ADR159 §7:429–431.
    status TEXT NOT NULL CHECK (status IN ('inactive', 'active')), -- ADR159 §1:178.
    activation_epoch INTEGER CHECK (activation_epoch >= 0), -- ADR159 §1:178.
    activated_at INTEGER, -- ADR159 §1:179.
    inventoried_edges INTEGER CHECK (inventoried_edges >= 0), -- ADR159 §1:179.
    authorized_edges INTEGER CHECK (authorized_edges >= 0), -- ADR159 §1:179.
    rejected_edges INTEGER CHECK (rejected_edges >= 0), -- ADR159 §1:179.
    late_arrival_edges INTEGER CHECK (late_arrival_edges >= 0), -- ADR159 §1:179 / §6:389–398.
    provider_mode TEXT CHECK (provider_mode IN ('single-principal', 'multi-actor')), -- ADR159 §7:410–419.
    provider_version TEXT, -- ADR159 §1:179–180.
    trigger_manifest TEXT, -- ADR159 §7:424–431; receipt representation/activation remains deferred.
    -- ADR159 §6:383 / §7:424–438: inactive has no fabricated receipt; active requires the complete receipt.
    CHECK ((status = 'inactive'
        AND activation_epoch IS NULL AND activated_at IS NULL
        AND inventoried_edges IS NULL AND authorized_edges IS NULL
        AND rejected_edges IS NULL AND late_arrival_edges IS NULL
        AND provider_mode IS NULL AND provider_version IS NULL AND trigger_manifest IS NULL)
        OR (status = 'active'
        AND activation_epoch IS NOT NULL AND activated_at IS NOT NULL
        AND inventoried_edges IS NOT NULL AND authorized_edges IS NOT NULL
        AND rejected_edges IS NOT NULL AND late_arrival_edges IS NOT NULL
        AND provider_mode IS NOT NULL AND provider_version IS NOT NULL AND trigger_manifest IS NOT NULL))
);
-- ADR159 §6:383: schema deployment is dormant and selects no provider mode.
-- Tail replay must retain any existing activation receipt.
INSERT INTO edge_governance_state (singleton, schema_version, status)
VALUES (1, 61, 'inactive')
ON CONFLICT(singleton) DO NOTHING;

-- ADR159 §1:124–125: decision history is append-only, including after graph hard deletion.
CREATE TRIGGER IF NOT EXISTS edge_governance_decisions_no_update
BEFORE UPDATE ON edge_governance_decisions BEGIN
    SELECT RAISE(ABORT, 'edge governance decisions are append-only');
END;
-- ADR159 §1:124–125 / §3:247: history is never removed with the graph incarnation.
CREATE TRIGGER IF NOT EXISTS edge_governance_decisions_no_delete
BEFORE DELETE ON edge_governance_decisions BEGIN
    SELECT RAISE(ABORT, 'edge governance decisions are append-only');
END;
-- ADR159 §1:142–161: invalidation history is append-only and decisions stay permanently spent.
CREATE TRIGGER IF NOT EXISTS edge_governance_invalidations_no_update
BEFORE UPDATE ON edge_governance_invalidations BEGIN
    SELECT RAISE(ABORT, 'edge governance invalidations are append-only');
END;
-- ADR159 §1:142–161: deleting the ledger cannot re-mint a spent incarnation.
CREATE TRIGGER IF NOT EXISTS edge_governance_invalidations_no_delete
BEFORE DELETE ON edge_governance_invalidations BEGIN
    SELECT RAISE(ABORT, 'edge governance invalidations are append-only');
END;

-- ADR159 §1:154–161 / §Verification:520–522: no projection insertion can reuse a spent decision.
CREATE TRIGGER IF NOT EXISTS edge_governance_active_unspent_insert
BEFORE INSERT ON edge_governance_active
WHEN EXISTS (SELECT 1 FROM edge_governance_invalidations WHERE decision_id = NEW.decision_id)
BEGIN
    SELECT RAISE(ABORT, 'edge governance decision is spent');
END;

-- ADR159 §1:154–161 / §3:257–266: replacing a decision obeys the same spent guard.
CREATE TRIGGER IF NOT EXISTS edge_governance_active_unspent_update
BEFORE UPDATE OF decision_id ON edge_governance_active
WHEN OLD.decision_id IS NOT NEW.decision_id
    AND EXISTS (SELECT 1 FROM edge_governance_invalidations WHERE decision_id = NEW.decision_id)
BEGIN
    SELECT RAISE(ABORT, 'edge governance decision is spent');
END;

-- ADR159 §3:240–245 / 250–269: null-safe tuple/liveness transitions, not assignments.
CREATE TRIGGER IF NOT EXISTS edge_governance_graph_update
AFTER UPDATE OF namespace, source_id, target_id, relation, target_backend, deleted_at ON graph_edges
WHEN OLD.namespace IS NOT NEW.namespace
    OR OLD.source_id IS NOT NEW.source_id
    OR OLD.target_id IS NOT NEW.target_id
    OR OLD.relation IS NOT NEW.relation
    OR OLD.target_backend IS NOT NEW.target_backend
    OR OLD.deleted_at IS NOT NEW.deleted_at
BEGIN
    INSERT INTO edge_governance_invalidations (decision_id, edge_id, cause, invalidated_at)
    SELECT decision_id, edge_id,
        CASE WHEN OLD.namespace IS NOT NEW.namespace
            OR OLD.source_id IS NOT NEW.source_id
            OR OLD.target_id IS NOT NEW.target_id
            OR OLD.relation IS NOT NEW.relation
            OR OLD.target_backend IS NOT NEW.target_backend
        THEN 'endpoint_or_relation_or_backend_rewrite' ELSE 'edge_liveness_transition' END,
        CAST(strftime('%s', 'now') AS INTEGER) * 1000000
    FROM edge_governance_active WHERE edge_id = OLD.id;
    DELETE FROM edge_governance_active WHERE edge_id = OLD.id;
END;

-- ADR159 §3:243 / 247 / 267–269: ledger append precedes projection FK cascade.
CREATE TRIGGER IF NOT EXISTS edge_governance_graph_delete
BEFORE DELETE ON graph_edges
BEGIN
    INSERT INTO edge_governance_invalidations (decision_id, edge_id, cause, invalidated_at)
    SELECT decision_id, edge_id, 'hard_delete',
        CAST(strftime('%s', 'now') AS INTEGER) * 1000000
    FROM edge_governance_active WHERE edge_id = OLD.id;
    DELETE FROM edge_governance_active WHERE edge_id = OLD.id;
END;

-- ADR159 §3:246 / 252–269: both endpoint-note liveness directions invalidate incident markers.
CREATE TRIGGER IF NOT EXISTS edge_governance_note_update
AFTER UPDATE OF deleted_at ON notes
WHEN OLD.deleted_at IS NOT NEW.deleted_at
BEGIN
    INSERT INTO edge_governance_invalidations (decision_id, edge_id, cause, invalidated_at)
    SELECT decision_id, edge_id, 'endpoint_note_liveness_transition',
        CAST(strftime('%s', 'now') AS INTEGER) * 1000000
    FROM edge_governance_active WHERE source_id = OLD.id OR target_id = OLD.id;
    DELETE FROM edge_governance_active WHERE source_id = OLD.id OR target_id = OLD.id;
END;

-- ADR159 §3:247 / 267–269: endpoint hard delete records once before later incident-edge deletes.
CREATE TRIGGER IF NOT EXISTS edge_governance_note_delete
BEFORE DELETE ON notes
BEGIN
    INSERT INTO edge_governance_invalidations (decision_id, edge_id, cause, invalidated_at)
    SELECT decision_id, edge_id, 'hard_delete',
        CAST(strftime('%s', 'now') AS INTEGER) * 1000000
    FROM edge_governance_active WHERE source_id = OLD.id OR target_id = OLD.id;
    DELETE FROM edge_governance_active WHERE source_id = OLD.id OR target_id = OLD.id;
END;

-- ADR159 §7:424–431: immutable names + SHA256 of installed SQL after whitespace collapse/trim.
-- The terminal statement delimiter is not part of sqlite_schema.sql. Native parity requires owner-queue validation.
-- ADR159-TRIGGER-SHA256 edge_governance_decisions_no_update a7d67310d78479046d8d253b8a9a7261284e21948fa4565d3430dfdc68d72c3a
-- ADR159-TRIGGER-SHA256 edge_governance_decisions_no_delete 937a152f4f998e8f491ddc050fc1332fe04f52bb2e06d1af1184bd7ee23f6220
-- ADR159-TRIGGER-SHA256 edge_governance_invalidations_no_update 6de3da121cc29abfc9d1b1eb772c74bf6c84be230ba9f00841de5b91ee024337
-- ADR159-TRIGGER-SHA256 edge_governance_invalidations_no_delete 4962d393999be3f37acb7cc406399ac2d9b2a93e76d00e8df11d33ee6840625d
-- ADR159-TRIGGER-SHA256 edge_governance_graph_update 0f68647ef161ac30e40ee67f9e1defed7b883ce1e9bdd7d54ac955046a3f92bc
-- ADR159-TRIGGER-SHA256 edge_governance_graph_delete 7e11b604e090c0b774f51b7df44ed1c80e8132319e64e9527c459c29f76a6724
-- ADR159-TRIGGER-SHA256 edge_governance_note_update 30a726a24910f3c66e8e51c6d05ecb66c04a1cf4ba21767b7ae073396f625263
-- ADR159-TRIGGER-SHA256 edge_governance_note_delete 9c267fe3aac117ede8983cf87401d1554a1c828214d4c556f36fbc19738808a1
-- ADR159-TRIGGER-SHA256 edge_governance_active_unspent_insert 43ba23e898e2b6e4ad2b434ee7223cd7eae644c739dd1ab45bab938fd1a6518c
-- ADR159-TRIGGER-SHA256 edge_governance_active_unspent_update 05b07be6bc051ef6bdf0f4dde3e3f9cbf84f69fdb5441affedb4eb5c137ba603
