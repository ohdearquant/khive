-- ADR-118: the fresh-tail exact leg's per-model tail scans (global-scope
-- consumers, e.g. the memory pack) filter by embedding_model/kind/field and
-- range over seq WITHOUT a namespace predicate. The existing
-- idx_ann_write_log_ns_model_seq index is namespace-leading, so it cannot
-- serve these scans as an index range seek — the query degrades to a full
-- table scan. This index leads with embedding_model so an
-- (embedding_model, kind, field) equality prefix plus a seq range seek stays
-- an index-only operation regardless of namespace.
CREATE INDEX IF NOT EXISTS idx_ann_write_log_model_kind_field_seq
    ON ann_write_log (embedding_model, kind, field, seq);
