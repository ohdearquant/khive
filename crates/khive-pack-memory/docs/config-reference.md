# Recall Config Reference

Design rationale extracted from `crates/khive-pack-memory/src/config.rs` doc-comments. The
doc-comments in source remain the complete field contract (type, default, env-var fallback);
this file only records why a given bound exists.

## `ann_ready_timeout_ms`

Recall and the daemon's boot-time background warm (`warm_existing_memory_indexes`) both funnel
through `ensure_ann_for_model`'s per-model single-flight lock. Without a bound, a recall landing
while boot warm holds that lock for a from-scratch corpus build would wait out the full build —
300s+ observed in production (#836). This field bounds that wait so the recall degrades to
FTS-only instead of hanging.
